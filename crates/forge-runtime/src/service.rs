use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use forge_config::Config;
use forge_core::{
    CompletionRequest, DecisionRouter, Event, EventKind, ExecutionProvider, ForgeError, Message,
    ModelProvider, ProjectGraph, RiskLevel, RoutingRequest, RunState, SessionStore, Skill,
    SkillMeta, SkillRegistry, ToolCall, ToolResult,
};
use forge_needle::NeedleEngine;
use forge_session::{JsonlSessionStore, new_run_id, new_session_id};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::tools::{ToolDispatcher, ToolOutcome, minimum_dispatch_risk, tool_definitions};

/// Skill registry for runtimes without skills (tests).
pub struct NullSkillRegistry;

impl SkillRegistry for NullSkillRegistry {
    fn list(&self) -> Vec<SkillMeta> {
        Vec::new()
    }

    fn activate(&self, name: &str) -> Result<Skill, ForgeError> {
        Err(ForgeError::skill(format!("unknown skill: {name}")))
    }
}

/// Factory resolving a model provider for a routed model name.
pub type ModelFactory =
    Arc<dyn Fn(&str) -> Result<Arc<dyn ModelProvider>, ForgeError> + Send + Sync>;

/// Router name recorded for a run answered by the direct-dispatch fast
/// path instead of the model loop.
const NEEDLE_DISPATCH: &str = "needle-dispatch";

/// The two `decide` options of the fast-path guardrail. Order matters:
/// only `GUARD_SAFE` (index 0) lets a call through, and `HashBackend`
/// breaks ties in favour of the first option.
///
/// The phrasing is deliberate. `HashBackend` scores an option by how many
/// of its tokens appear in the question (see `hash_backend.rs`), so the
/// question shares exactly one token — "operation" — with *both* options:
/// a benign call therefore ties 1–1 and the safe option wins on order,
/// while any destructive word in the tool name or arguments ("rm",
/// "delete", …) lifts the risky option to 2 and the call is refused. A
/// real backend reads the same two strings as plain English.
const GUARD_SAFE: &str = "safe operation";
const GUARD_RISKY: &str =
    "destructive operation: delete, remove, rm, rmdir, overwrite, truncate, drop, format, kill";

/// A tool call the brain picked, already executed, ready to be reported.
struct FastPathDispatch {
    call: ToolCall,
    outcome: ToolOutcome,
    confidence: f64,
}

/// Result of a completed run.
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub session_id: String,
    pub text: String,
    pub turns: u32,
    pub tool_calls: usize,
    pub events: Vec<Event>,
}

/// What [`AgentService::fork_session`] created.
#[derive(Debug, Clone, Serialize)]
pub struct ForkOutcome {
    /// The new session.
    pub session_id: String,
    pub source_session_id: String,
    /// 1-based position of the last copied source event (after snapping).
    pub at_position: u64,
    /// The last run included in the fork.
    pub at_run_id: String,
    /// Event lines copied from the source, excluding the fork marker.
    pub events_copied: usize,
}

/// Optional parameters for a run.
#[derive(Debug, Default)]
pub struct RunOptions {
    /// Pre-generated run id (defaults to a fresh ULID).
    pub run_id: Option<String>,
    /// Session to run in (defaults to a fresh session).
    pub session_id: Option<String>,
    /// Turn budget override (defaults to `config.max_turns`).
    pub max_turns: Option<u32>,
}

/// Everything one run needs beyond the ids. Built by
/// [`AgentService::run_with_options`] for a fresh prompt and by
/// [`AgentService::resume`] for a continuation.
struct RunPlan {
    /// Recorded in `run_started`, and therefore what a later replay reads
    /// back as this run's user turn.
    prompt: String,
    /// The text routing, skill matching and graph context key on. On a
    /// resume this is the session's original ask, so a continuation is
    /// routed and seeded like the work it continues rather than like the
    /// word "continue".
    task: String,
    /// Conversation replayed from the session log, prepended to this run's
    /// messages. Empty for a fresh run.
    history: Vec<Message>,
    /// The run this one continues, for the `input_received` link marker.
    resumed_from: Option<String>,
}

impl RunPlan {
    /// A fresh run: the prompt is the task and there is no history.
    fn fresh(prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        Self {
            task: prompt.clone(),
            prompt,
            history: Vec::new(),
            resumed_from: None,
        }
    }
}

/// The prompt a resumed run records and sends. `forge resume` takes no new
/// instruction, so the conversation replayed above this line *is* the
/// context and this is the nudge that makes the model act on it.
const RESUME_PROMPT: &str = "Continue the work in the conversation above.";

/// One run, as [`AgentService::list_runs`] reports it.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    /// `None` for a run this process started that has not written its first
    /// event yet.
    pub session_id: Option<String>,
    pub state: RunState,
    pub started_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    /// Highest `seq` recorded for the run (0 when it has no events).
    pub last_seq: u64,
}

/// A coherent view of one run: everything recorded so far, plus everything
/// from now on, with no gap and no duplicate.
///
/// Returned by [`AgentService::attach`]. `backlog` is the run's events at
/// the moment of attaching; [`recv`](Self::recv) yields the ones that come
/// after, skipping any the backlog already contained.
#[derive(Debug)]
pub struct Attachment {
    pub run_id: String,
    pub session_id: Option<String>,
    pub backlog: Vec<Event>,
    /// The run's state as of the backlog.
    pub state: RunState,
    /// `None` when no further events can arrive in this process: the run is
    /// terminal, or it belongs to another process (whose events reach this
    /// one only through the store).
    live: Option<broadcast::Receiver<Event>>,
    last_seq: u64,
}

impl Attachment {
    /// The next live event after the backlog, or `None` once no more can
    /// arrive.
    ///
    /// Events already present in the backlog are skipped by `seq`, which is
    /// what makes "subscribe, then read the log" gap-free *and*
    /// duplicate-free: subscribing first means nothing appended in between
    /// is missed, and the `seq` filter means nothing is delivered twice.
    pub async fn recv(&mut self) -> Option<Event> {
        let live = self.live.as_mut()?;
        loop {
            match live.recv().await {
                // Already in the backlog. (`seq` 0 means a v1 event, which
                // cannot be compared — deliver it and let the caller see
                // it; live events are always store-assigned anyway.)
                Ok(event) if event.seq != 0 && event.seq <= self.last_seq => continue,
                Ok(event) => {
                    self.last_seq = self.last_seq.max(event.seq);
                    return Some(event);
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(
                        run = %self.run_id,
                        missed,
                        "attached event stream lagged; read the session log for the gap"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// True when this attachment can still deliver live events.
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }
}

/// How many terminal runs stay recognisable in memory after their tracking
/// entries are pruned. Past this the oldest are forgotten and the session
/// store answers instead — which it always can.
const REMEMBERED_TERMINAL_RUNS: usize = 256;

/// How many terminal runs [`AgentService::list_runs`] reports. Live runs are
/// never dropped; a long-lived session directory must not turn `list_runs`
/// into an unbounded response.
const LISTED_TERMINAL_RUNS: usize = 20;

/// Bounded record of runs this process finished, so a run whose tracking
/// entries were pruned is still distinguishable from one never heard of.
#[derive(Default)]
struct FinishedRuns {
    states: HashMap<String, RunState>,
    order: VecDeque<String>,
}

impl FinishedRuns {
    fn record(&mut self, run_id: &str, state: RunState) {
        if self.states.insert(run_id.to_string(), state).is_none() {
            self.order.push_back(run_id.to_string());
        }
        while self.order.len() > REMEMBERED_TERMINAL_RUNS {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.states.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// Input channel state for a run. `Closed` is a tombstone: closing before
/// the run starts (e.g. stdin already at EOF) must still close the
/// receiver the loop will take, so approval waits fail instead of hanging.
enum InputState {
    Open {
        sender: mpsc::Sender<String>,
        receiver: Option<mpsc::Receiver<String>>,
    },
    Closed,
}

impl InputState {
    fn open() -> Self {
        let (sender, receiver) = mpsc::channel(32);
        Self::Open {
            sender,
            receiver: Some(receiver),
        }
    }
}

/// Transport-neutral agent runtime: routing → model → tool loop → events.
/// CLI and server share this type.
///
/// Cancellation works in-process (a per-run `CancellationToken`) and
/// cross-process: `cancel()` also writes `.forge/runs/<run_id>.cancel`,
/// which a running loop polls at every turn/tool checkpoint — so CLI
/// `forge cancel` can stop a `forge run` in another process.
pub struct AgentService {
    model: Arc<dyn ModelProvider>,
    router: Arc<dyn DecisionRouter>,
    execution: Arc<dyn ExecutionProvider>,
    skills: Arc<dyn SkillRegistry>,
    sessions: Arc<JsonlSessionStore>,
    config: Config,
    graph: Option<Arc<dyn ProjectGraph>>,
    /// Resolves a provider for the routed model name; defaults to the
    /// single configured model for every selection.
    model_factory: Option<ModelFactory>,
    /// On-device brain for the direct-dispatch fast path. `None` (the
    /// default) means every run goes through the model loop.
    needle: Option<Arc<NeedleEngine>>,
    /// Per-run tracking, pruned when a run reaches a terminal state (see
    /// [`AgentService::finish_run`]).
    broadcasters: Mutex<HashMap<String, broadcast::Sender<Event>>>,
    inputs: Mutex<HashMap<String, InputState>>,
    cancel_tokens: Mutex<HashMap<String, CancellationToken>>,
    /// Bounded tombstones for pruned runs, so `send_input` can refuse a
    /// finished run instead of resurrecting its channel.
    finished: Mutex<FinishedRuns>,
}

impl AgentService {
    pub fn new(
        model: Arc<dyn ModelProvider>,
        router: Arc<dyn DecisionRouter>,
        execution: Arc<dyn ExecutionProvider>,
        skills: Arc<dyn SkillRegistry>,
        sessions: Arc<JsonlSessionStore>,
        config: Config,
    ) -> Self {
        Self {
            model,
            router,
            execution,
            skills,
            sessions,
            config,
            graph: None,
            model_factory: None,
            needle: None,
            broadcasters: Mutex::new(HashMap::new()),
            inputs: Mutex::new(HashMap::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
            finished: Mutex::new(FinishedRuns::default()),
        }
    }

    /// Attach a project graph for context seeding and graph tools.
    pub fn with_graph(mut self, graph: Option<Arc<dyn ProjectGraph>>) -> Self {
        self.graph = graph;
        self
    }

    /// Attach a factory resolving a provider for the routed model name
    /// (e.g. a `[models]` entry with its own `base_url`). Without one, the
    /// configured model serves every selection.
    pub fn with_model_factory(mut self, factory: ModelFactory) -> Self {
        self.model_factory = Some(factory);
        self
    }

    /// Attach the on-device Needle brain, enabling the direct-dispatch
    /// fast path (see [`AgentService::needle_fast_path`]). Callers pass the
    /// engine only when it is genuinely usable — `forge_needle::engine_if_available`
    /// is the seam that decides that. `None` keeps the plain model loop
    /// exactly as it is, so a build without a brain behaves identically.
    pub fn with_needle(mut self, engine: Option<Arc<NeedleEngine>>) -> Self {
        self.needle = engine;
        self
    }

    pub fn model(&self) -> &Arc<dyn ModelProvider> {
        &self.model
    }

    pub fn execution(&self) -> &Arc<dyn ExecutionProvider> {
        &self.execution
    }

    pub fn skills(&self) -> &Arc<dyn SkillRegistry> {
        &self.skills
    }

    pub fn sessions(&self) -> &Arc<JsonlSessionStore> {
        &self.sessions
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn broadcaster(&self, run_id: &str) -> broadcast::Sender<Event> {
        self.broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run_id.to_string())
            .or_insert_with(|| broadcast::channel(64).0)
            .clone()
    }

    /// A run's broadcaster if one exists, without creating it. Callers that
    /// only want to *publish to whoever is listening* use this: creating a
    /// channel for a run nobody is running is how the map used to grow.
    fn existing_broadcaster(&self, run_id: &str) -> Option<broadcast::Sender<Event>> {
        self.broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned()
    }

    /// Subscribe to the live event stream of a run. This is the
    /// transport-neutral seam the server's SSE endpoint consumes.
    ///
    /// For a run already known to be terminal this hands back an
    /// immediately-closed receiver rather than registering a broadcaster
    /// nothing will ever publish to — the caller's own replay of the stored
    /// events is the complete answer for such a run (and see
    /// [`attach`](Self::attach) for that backlog in one call).
    pub fn subscribe(&self, run_id: &str) -> broadcast::Receiver<Event> {
        if self.terminal_state(run_id).is_some() {
            let (sender, receiver) = broadcast::channel(1);
            drop(sender);
            return receiver;
        }
        self.broadcaster(run_id).subscribe()
    }

    /// Terminal state of a run, if it is known to have one: this process's
    /// tombstones first, then the session store (which also covers runs
    /// started by another process).
    fn terminal_state(&self, run_id: &str) -> Option<RunState> {
        if let Some(state) = self
            .finished
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .states
            .get(run_id)
            .copied()
        {
            return Some(state);
        }
        let events = self.events(run_id).ok()?;
        if events.is_empty() {
            return None;
        }
        let state = RunState::of_events(&events);
        state.is_terminal().then_some(state)
    }

    /// A run reached a terminal state: drop its in-memory tracking.
    ///
    /// `inputs`, `broadcasters` and `cancel_tokens` are keyed by run id and
    /// nothing ever removed them, so a long-lived process (`forge serve`,
    /// `forge mcp`, `forge acp`) grew by three entries per run, forever.
    /// Pruning is safe because everything still wanted about a finished run
    /// lives in the session store: [`attach`](Self::attach) serves its
    /// backlog from there, and the bounded tombstone remembers *that* it
    /// finished so [`send_input`](Self::send_input) can refuse it.
    ///
    /// Called *after* the terminal event is emitted, so subscribers still
    /// receive it: dropping the map's sender clone leaves already-buffered
    /// events readable, and receivers only see `Closed` afterwards.
    fn finish_run(&self, run_id: &str, state: RunState) {
        self.finished
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(run_id, state);
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        self.broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        tracing::debug!(run_id, state = state.as_str(), "run tracking pruned");
    }

    /// Per-run input channel sender (created on demand). Used by
    /// `send_input` (server input endpoint / CLI stdin feeder) and by the
    /// approval pause inside the loop.
    fn input_sender(&self, run_id: &str) -> Result<mpsc::Sender<String>, ForgeError> {
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        match inputs
            .entry(run_id.to_string())
            .or_insert_with(InputState::open)
        {
            InputState::Open { sender, .. } => Ok(sender.clone()),
            InputState::Closed => Err(ForgeError::session(format!(
                "input channel for run {run_id} is closed"
            ))),
        }
    }

    /// The loop takes the receiver once, at run start. A tombstoned
    /// (already closed) channel yields an immediately-closed receiver.
    fn take_input_receiver(&self, run_id: &str) -> mpsc::Receiver<String> {
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        match inputs
            .entry(run_id.to_string())
            .or_insert_with(InputState::open)
        {
            InputState::Open { sender, receiver } => match receiver.take() {
                Some(receiver) => receiver,
                None => {
                    // Receiver already taken (one loop per run id, so this
                    // shouldn't happen); replace with a fresh live channel.
                    let (new_sender, new_receiver) = mpsc::channel(32);
                    *sender = new_sender;
                    new_receiver
                }
            },
            InputState::Closed => {
                let (sender, receiver) = mpsc::channel(32);
                drop(sender);
                receiver
            }
        }
    }

    /// Deliver user input to a run and record an `InputReceived` event
    /// (when the run is already persisted).
    ///
    /// A run that has already finished is a typed error, not a silent
    /// no-op: before pruning existed, this created a fresh input channel for
    /// a dead run id — leaking an entry and swallowing the message into a
    /// queue with no reader.
    pub fn send_input(&self, run_id: &str, message: impl Into<String>) -> Result<(), ForgeError> {
        if let Some(state) = self.terminal_state(run_id) {
            return Err(ForgeError::session(format!(
                "run {run_id} is {}; not accepting input",
                state.as_str()
            )));
        }
        let message = message.into();
        let sender = self.input_sender(run_id)?;
        if let Ok(Some(session)) = self.sessions.find_run(run_id) {
            let stored = self.sessions.append(Event::new(
                run_id,
                &session,
                EventKind::InputReceived {
                    message: message.clone(),
                },
            ))?;
            if let Some(broadcaster) = self.existing_broadcaster(run_id) {
                let _ = broadcaster.send(stored);
            }
        }
        sender
            .try_send(message)
            .map_err(|e| ForgeError::session(format!("input queue for run {run_id} is full: {e}")))
    }

    /// Attach to a run: its events so far *and* the ones still to come, in
    /// one call, with no gap and no duplicate.
    ///
    /// This is the primitive a UI uses to join a run late — after the ids
    /// were handed out, after a reattach, after a restart. The ordering is
    /// the whole point: for a run this process is running, the live
    /// subscription is taken *before* the stored backlog is read, so an
    /// event appended in between arrives on the channel rather than falling
    /// between the two reads; [`Attachment::recv`] then drops anything the
    /// backlog already contained, by `seq`.
    ///
    /// A terminal run attaches to its backlog with no live stream. A run
    /// owned by *another* process attaches to its stored backlog and a live
    /// stream that will stay silent — that process's events reach this one
    /// only through the store, which is the documented limit of in-process
    /// attach.
    pub fn attach(&self, run_id: &str) -> Result<Attachment, ForgeError> {
        let terminal = self.terminal_state(run_id);
        let tracked = self
            .broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(run_id);

        // Subscribe first, but only for a run we already track — attaching
        // must never create a broadcaster for an id that turns out to be
        // unknown.
        let live = match (&terminal, tracked) {
            (None, true) => Some(self.broadcaster(run_id).subscribe()),
            _ => None,
        };

        let session_id = self.sessions.find_run(run_id)?;
        let backlog: Vec<Event> = match &session_id {
            Some(session) => self
                .sessions
                .events_for(session)?
                .into_iter()
                .filter(|e| e.run_id == run_id)
                .collect(),
            None => Vec::new(),
        };
        if backlog.is_empty() && terminal.is_none() && !tracked {
            return Err(ForgeError::session(format!("unknown run: {run_id}")));
        }

        let state = terminal.unwrap_or_else(|| RunState::of_events(&backlog));
        let last_seq = backlog.iter().map(|e| e.seq).max().unwrap_or(0);
        Ok(Attachment {
            run_id: run_id.to_string(),
            session_id,
            backlog,
            state,
            live,
            last_seq,
        })
    }

    /// Runs worth showing: every live one, plus the most recent
    /// [`LISTED_TERMINAL_RUNS`] that have finished. Newest activity first.
    ///
    /// Live runs are never dropped from the list — a run you could still
    /// talk to must not be hidden by a busy history — while terminal ones
    /// are bounded, because a long-lived project accumulates them without
    /// limit.
    pub fn list_runs(&self) -> Result<Vec<RunSummary>, ForgeError> {
        let mut summaries: Vec<RunSummary> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for info in self.sessions.list_sessions()? {
            let events = self.sessions.events_for(&info.session_id)?;
            let mut runs: Vec<String> = Vec::new();
            for event in &events {
                if !runs.contains(&event.run_id) {
                    runs.push(event.run_id.clone());
                }
            }
            for run_id in runs {
                let own: Vec<&Event> = events.iter().filter(|e| e.run_id == run_id).collect();
                // A fork marker is provenance, not a run.
                if own
                    .iter()
                    .all(|e| matches!(e.kind, EventKind::SessionForked { .. }))
                {
                    continue;
                }
                let state = self
                    .finished
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .states
                    .get(&run_id)
                    .copied()
                    .unwrap_or_else(|| {
                        // Only the last event decides; borrow it rather than
                        // cloning the whole run to ask.
                        own.last().map_or(RunState::Running, |last| {
                            RunState::of_events(std::slice::from_ref(*last))
                        })
                    });
                seen.insert(run_id.clone());
                summaries.push(RunSummary {
                    run_id,
                    session_id: Some(info.session_id.clone()),
                    state,
                    started_at: own.first().map(|e| e.ts),
                    last_event_at: own.last().map(|e| e.ts),
                    last_seq: own.iter().map(|e| e.seq).max().unwrap_or(0),
                });
            }
        }

        // Runs this process started that have not written an event yet.
        let tracked: Vec<String> = self
            .broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for run_id in tracked {
            if seen.contains(&run_id) {
                continue;
            }
            summaries.push(RunSummary {
                run_id,
                session_id: None,
                state: RunState::Running,
                started_at: None,
                last_event_at: None,
                last_seq: 0,
            });
        }

        // Newest activity first; a run with no events yet is the newest
        // thing there is.
        summaries.sort_by_key(|s| std::cmp::Reverse(s.last_event_at));
        let (live, terminal): (Vec<RunSummary>, Vec<RunSummary>) =
            summaries.into_iter().partition(|s| !s.state.is_terminal());
        let mut out = live;
        out.extend(terminal.into_iter().take(LISTED_TERMINAL_RUNS));
        Ok(out)
    }

    /// Close a run's input channel. The loop treats a closed channel
    /// during an approval wait as "no approval possible" and fails the
    /// run cleanly instead of hanging.
    pub fn close_input(&self, run_id: &str) {
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run_id.to_string(), InputState::Closed);
    }

    fn cancel_token(&self, run_id: &str) -> CancellationToken {
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run_id.to_string())
            .or_default()
            .clone()
    }

    fn runs_dir(&self) -> PathBuf {
        // sessions live in <root>/.forge/sessions → markers in .forge/runs
        match self.sessions.root().parent() {
            Some(forge_dir) => forge_dir.join("runs"),
            None => PathBuf::from(".forge/runs"),
        }
    }

    fn cancel_marker(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(format!("{run_id}.cancel"))
    }

    /// In-process token or cross-process marker file.
    fn cancel_requested(&self, run_id: &str) -> bool {
        let token_fired = self
            .cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .is_some_and(CancellationToken::is_cancelled);
        token_fired || self.cancel_marker(run_id).exists()
    }

    /// Append an event to the store (which assigns its sequence number),
    /// broadcast the stored event to subscribers, and collect it.
    fn emit(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        event: Event,
    ) -> Result<(), ForgeError> {
        let stored = self.sessions.append(event)?;
        // No subscribers yet is normal for the CLI; not an error.
        let _ = sender.send(stored.clone());
        collected.push(stored);
        Ok(())
    }

    /// Record one model response verbatim, so the conversation can be
    /// replayed later (see [`crate::replay`]). This is the *replay* stream;
    /// the `tool_*`/`completed` events remain the short observability
    /// summaries every adapter already reads.
    ///
    /// A response with neither text nor tool calls records nothing: there
    /// is no message to replay, and an empty assistant turn in the history
    /// is noise a provider may well reject.
    fn emit_assistant_message(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        run_id: &str,
        session_id: &str,
        response: &forge_core::CompletionResponse,
    ) -> Result<(), ForgeError> {
        if response.content.is_empty() && response.tool_calls.is_empty() {
            return Ok(());
        }
        self.emit(
            sender,
            collected,
            Event::new(
                run_id,
                session_id,
                EventKind::AssistantMessage {
                    text: response.content.clone(),
                    tool_calls: response.tool_calls.clone(),
                },
            ),
        )
    }

    /// Run a prompt through the agent loop with fresh run/session ids.
    pub async fn run(&self, prompt: &str) -> Result<RunOutcome, ForgeError> {
        self.run_with_options(prompt, RunOptions::default()).await
    }

    /// Run a prompt with explicit options.
    pub async fn run_with_options(
        &self,
        prompt: &str,
        options: RunOptions,
    ) -> Result<RunOutcome, ForgeError> {
        let run_id = options.run_id.unwrap_or_else(new_run_id);
        let session_id = options.session_id.unwrap_or_else(new_session_id);
        self.run_tracked(
            RunPlan::fresh(prompt),
            &run_id,
            &session_id,
            options.max_turns,
        )
        .await
    }

    /// [`run_inner`](Self::run_inner) plus the bookkeeping every run owes:
    /// whatever it returns, its tracking entries are pruned and its outcome
    /// recorded. The classification is typed — [`RunState::of_result`] reads
    /// the error's variant, never its message.
    async fn run_tracked(
        &self,
        plan: RunPlan,
        run_id: &str,
        session_id: &str,
        max_turns: Option<u32>,
    ) -> Result<RunOutcome, ForgeError> {
        let result = self.run_inner(plan, run_id, session_id, max_turns).await;
        self.finish_run(run_id, RunState::of_result(&result));
        result
    }

    /// Start a run on a tokio task without blocking the caller. Returns
    /// the pre-generated `(run_id, session_id)` and the task handle so
    /// transports (the REST server) can return ids immediately and abort
    /// the task on cancel. Events are persisted and broadcast as usual.
    pub fn start_run(
        self: &Arc<Self>,
        prompt: impl Into<String>,
        session_id: Option<String>,
    ) -> (
        String,
        String,
        tokio::task::JoinHandle<Result<RunOutcome, ForgeError>>,
    ) {
        self.start_run_with_options(
            prompt,
            RunOptions {
                session_id,
                ..RunOptions::default()
            },
        )
    }

    /// [`start_run`](Self::start_run) with explicit [`RunOptions`] — the
    /// MCP adapter needs a per-call turn budget, which the REST adapter
    /// has no way to express. Resuming is [`resume`](Self::resume)'s job.
    pub fn start_run_with_options(
        self: &Arc<Self>,
        prompt: impl Into<String>,
        options: RunOptions,
    ) -> (
        String,
        String,
        tokio::task::JoinHandle<Result<RunOutcome, ForgeError>>,
    ) {
        let run_id = options.run_id.unwrap_or_else(new_run_id);
        let session_id = options.session_id.unwrap_or_else(new_session_id);
        // Create the broadcast channel now so subscribers connecting right
        // after the ids are handed out miss nothing.
        self.broadcaster(&run_id);
        let service = Arc::clone(self);
        let prompt = prompt.into();
        let (rid, sid) = (run_id.clone(), session_id.clone());
        let max_turns = options.max_turns;
        let handle = tokio::spawn(async move {
            service
                .run_tracked(RunPlan::fresh(prompt), &rid, &sid, max_turns)
                .await
        });
        (run_id, session_id, handle)
    }

    /// Try to answer a prompt with one local tool call instead of the
    /// model loop: the brain picks the tool *and* fills its arguments, a
    /// second brain call vets the result, and the call is dispatched
    /// through [`ToolDispatcher`] — the same path the loop uses, so
    /// `ExecutionProvider` approval gating applies unchanged.
    ///
    /// Every gate must hold, and any failure returns `None` to mean "run
    /// normally": this is an optimization, never a behaviour change.
    ///  1. a brain is attached and the run isn't cancelled;
    ///  2. `tool_call` produced a call within `router_timeout_ms`, from the
    ///     same tool list the model would have been offered (the caller
    ///     only calls this when that list is non-empty, i.e. when the
    ///     resolved model is tool-capable — a chat-only provider gets the
    ///     plain-completion path and the fast path must not widen that);
    ///  3. its confidence is at least `router_confidence_threshold`;
    ///  4. its arguments parse as a JSON *object* (what the dispatcher
    ///     reads arguments out of);
    ///  5. the call is *read-only*: `minimum_dispatch_risk` says
    ///     `RiskLevel::Safe`, the one classification no approval policy can
    ///     gate;
    ///  6. the guardrail `decide` answers `GUARD_SAFE`, confidently and in
    ///     time;
    ///  7. the dispatch itself succeeded.
    ///
    /// Both brain calls are bounded by `router_timeout_ms`: a slow or hung
    /// engine costs a run that budget once and then behaves as if no brain
    /// were attached — it can never hang a run.
    ///
    /// Gate 5 is the load-bearing safety gate: **no approval prompt can
    /// ever originate from the fast path**, under any `approval` policy.
    /// Without it, `check_approval` would run *inside* the fast path —
    /// blocking on stdin on a terminal before any event exists, and turning
    /// a user's "n" into a plain error that the fast path would silently
    /// swallow, letting the loop request the very same tool and prompt a
    /// second time. Restricting dispatch to reads removes that whole class:
    /// `Safe` operations return from `check_approval` before the policy is
    /// even consulted. Anything else — writes, edits, deletes, commands —
    /// falls through *before* the execution provider is touched at all.
    ///
    /// Nothing is emitted from here, so a decline leaves no trace and no
    /// side effect for the loop to contradict. The cost is that a
    /// dispatched call's `tool_*` events are written just after the work
    /// rather than just before it, for the few milliseconds one local
    /// read takes.
    async fn needle_fast_path(
        &self,
        prompt: &str,
        run_id: &str,
        tools: &[forge_core::ToolDefinition],
    ) -> Option<FastPathDispatch> {
        let engine = self.needle.as_ref()?;
        if self.cancel_requested(run_id) {
            // Let the loop's own checkpoint report the cancellation.
            return None;
        }
        let budget = Duration::from_millis(self.config.router_timeout_ms);
        let tools_json = serde_json::to_string(tools).ok()?;

        let call =
            match tokio::time::timeout(budget, engine.tool_call(prompt.to_string(), tools_json))
                .await
            {
                Ok(Ok(Some(call))) => call,
                Ok(Ok(None)) => return None,
                Ok(Err(e)) => {
                    tracing::debug!(run_id, error = %e, "needle fast path unavailable");
                    return None;
                }
                Err(_) => {
                    tracing::debug!(run_id, "needle fast path timed out picking a tool");
                    return None;
                }
            };
        if call.confidence < self.config.router_confidence_threshold {
            return None;
        }
        let arguments = match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
            Ok(value) if value.is_object() => value,
            _ => return None,
        };

        let tool_call = ToolCall::new(format!("{NEEDLE_DISPATCH}-1"), &call.name, arguments);
        if minimum_dispatch_risk(&tool_call) != Some(RiskLevel::Safe) {
            tracing::debug!(
                run_id,
                tool = %tool_call.name,
                "needle fast path skipped: not a read-only operation"
            );
            return None;
        }

        let question = format!(
            "Classify this tool operation: {} with arguments {}",
            call.name, call.arguments_json
        );
        let options = vec![GUARD_SAFE.to_string(), GUARD_RISKY.to_string()];
        let verdict = match tokio::time::timeout(budget, engine.decide(question, options)).await {
            Ok(Ok(verdict)) => verdict,
            _ => return None,
        };
        if verdict.choice != GUARD_SAFE
            || verdict.confidence < self.config.router_confidence_threshold
        {
            tracing::debug!(
                run_id,
                tool = %call.name,
                choice = %verdict.choice,
                confidence = verdict.confidence,
                "needle fast path declined by the guardrail"
            );
            return None;
        }

        let dispatcher = ToolDispatcher::new(self.execution.clone(), self.graph.clone());
        match dispatcher.dispatch(&tool_call).await {
            Ok(outcome) if !outcome.result.is_error => Some(FastPathDispatch {
                call: tool_call,
                outcome,
                confidence: call.confidence,
            }),
            // A tool error means the operation did not complete, so the loop
            // is where it belongs: a model may recover from it. (An
            // `ApprovalRequired` cannot reach here — gate 5 admits only
            // `Safe` operations — but it is handled the same way for free.)
            _ => {
                tracing::debug!(
                    run_id,
                    tool = %tool_call.name,
                    "needle fast path handed the call back to the agent loop"
                );
                None
            }
        }
    }

    async fn run_inner(
        &self,
        plan: RunPlan,
        run_id: &str,
        session_id: &str,
        max_turns: Option<u32>,
    ) -> Result<RunOutcome, ForgeError> {
        let RunPlan {
            prompt,
            task,
            history,
            resumed_from,
        } = plan;
        let prompt = prompt.as_str();
        let task = task.as_str();
        let run_id = run_id.to_string();
        let session_id = session_id.to_string();
        let max_turns = max_turns.unwrap_or(self.config.max_turns);
        let sender = self.broadcaster(&run_id);
        let token = self.cancel_token(&run_id);
        let mut input_rx = self.take_input_receiver(&run_id);
        let mut collected = Vec::new();
        let mut tool_call_count = 0usize;

        let fail = |collected: &mut Vec<Event>, error: ForgeError| -> ForgeError {
            let event = Event::new(
                &run_id,
                &session_id,
                EventKind::Error {
                    message: error.to_string(),
                },
            );
            let _ = self.emit(&sender, collected, event);
            error
        };

        self.emit(
            &sender,
            &mut collected,
            Event::new(
                &run_id,
                &session_id,
                EventKind::RunStarted {
                    provider: self.model.name().to_string(),
                    model: self.config.model.clone(),
                    prompt: prompt.to_string(),
                },
            ),
        )?;

        if let Some(old_run_id) = &resumed_from {
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::InputReceived {
                        message: format!("resume of run {old_run_id}"),
                    },
                ),
            )?;
        }

        // Candidates: the [models] table plus the configured model.
        let mut candidates: Vec<String> = self.config.model_entries().keys().cloned().collect();
        if !candidates.contains(&self.config.model) {
            candidates.push(self.config.model.clone());
        }
        candidates.sort();
        let routing_request = RoutingRequest {
            task: task.to_string(),
            required_capabilities: Vec::new(),
            candidates,
        };
        let decision = match self.router.route(&routing_request).await {
            Ok(decision) => decision,
            Err(e) => return Err(fail(&mut collected, e)),
        };
        tracing::info!(
            run_id,
            model = %decision.selected_model,
            confidence = decision.confidence,
            fallback = decision.fallback_used,
            "routing decision"
        );
        self.emit(
            &sender,
            &mut collected,
            Event::new(
                &run_id,
                &session_id,
                EventKind::RoutingDecisionMade {
                    router: decision.router_name.clone(),
                    selected_model: decision.selected_model.clone(),
                    confidence: decision.confidence,
                    fallback_used: decision.fallback_used,
                    reason: decision.reason.clone(),
                },
            ),
        )?;

        // Build the conversation: system preamble (skills, graph context),
        // then the replayed history of this session, then the user prompt.
        // System first is what providers expect, and the history is a real
        // user/assistant/tool transcript that must arrive in its own order.
        let mut messages = Vec::new();
        for meta in self.skills.match_task(task) {
            match self.skills.activate(&meta.name) {
                Ok(skill) => {
                    self.emit(
                        &sender,
                        &mut collected,
                        Event::new(
                            &run_id,
                            &session_id,
                            EventKind::SkillActivated {
                                name: skill.meta.name.clone(),
                                path: skill.meta.path.clone(),
                            },
                        ),
                    )?;
                    messages.push(Message::system(format!(
                        "Active skill `{}` instructions:\n{}",
                        skill.meta.name, skill.instructions
                    )));
                }
                Err(e) => {
                    tracing::warn!(skill = %meta.name, error = %e, "skill activation failed")
                }
            }
        }
        if let Some(graph) = &self.graph {
            let hits = graph.context(task, 5);
            if !hits.is_empty() {
                let listing = hits
                    .iter()
                    .map(|h| format!("- {} (score {})", h.path, h.score))
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(Message::system(format!(
                    "Relevant project files (from the project graph):\n{listing}"
                )));
            }
        }
        let resuming = !history.is_empty();
        messages.extend(history);
        messages.push(Message::user(prompt));

        // Resolve the provider for the routed model (defaults to the
        // configured one).
        let model = match &self.model_factory {
            Some(factory) => match factory(&decision.selected_model) {
                Ok(provider) => provider,
                Err(e) => return Err(fail(&mut collected, e)),
            },
            None => self.model.clone(),
        };
        tracing::debug!(model = %model.name(), "model resolved for run");

        let tools = if model.capabilities().tools {
            tool_definitions()
        } else {
            Vec::new()
        };

        // Fast path: the on-device brain answers a well-defined prompt with
        // one local tool call, before the model is called. Only for a fresh
        // prompt — a resume continues a conversation, so re-running the
        // original prompt's tool would be wrong — and only when the run
        // actually has tools, i.e. the resolved provider is tool-capable:
        // a chat-only model's run is a plain completion and the fast path
        // must not turn it into tool execution. All other gates and the
        // dispatch itself live in `needle_fast_path`; `None` means "run
        // normally", and nothing has been emitted or executed by then.
        if !resuming
            && !tools.is_empty()
            && let Some(fast) = self.needle_fast_path(prompt, &run_id, &tools).await
        {
            tracing::info!(
                run_id,
                tool = %fast.call.name,
                confidence = fast.confidence,
                "needle dispatched a tool call directly (no model call)"
            );
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::RoutingDecisionMade {
                        router: NEEDLE_DISPATCH.to_string(),
                        selected_model: "none".to_string(),
                        confidence: fast.confidence,
                        fallback_used: false,
                        reason: format!(
                            "needle filled and dispatched `{}` on device; no model call",
                            fast.call.name
                        ),
                    },
                ),
            )?;
            // Replay record: the brain stood in for the model, so the
            // conversation this run contributes is "assistant asked for
            // this call" + its result. A later resume continues from it.
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::AssistantMessage {
                        text: String::new(),
                        tool_calls: vec![fast.call.clone()],
                    },
                ),
            )?;
            let args_summary: String = fast.call.arguments.to_string().chars().take(120).collect();
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::ToolCallRequested {
                        tool: fast.call.name.clone(),
                        args_summary,
                    },
                ),
            )?;
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::ToolStarted {
                        name: fast.call.name.clone(),
                    },
                ),
            )?;
            if let Some(path) = &fast.outcome.file_changed {
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(
                        &run_id,
                        &session_id,
                        EventKind::FileChanged { path: path.clone() },
                    ),
                )?;
            }
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::ToolCompleted {
                        name: fast.call.name.clone(),
                        success: true,
                    },
                ),
            )?;
            let text = fast.outcome.result.content;
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::ToolResult {
                        call_id: fast.call.id.clone(),
                        tool: fast.call.name.clone(),
                        output: forge_core::cap_tool_output(&text),
                        is_error: false,
                    },
                ),
            )?;
            let summary: String = text.chars().take(80).collect();
            self.emit(
                &sender,
                &mut collected,
                Event::new(&run_id, &session_id, EventKind::Completed { summary }),
            )?;
            return Ok(RunOutcome {
                run_id,
                session_id,
                text,
                // No model turn ran; the one tool call is the whole run.
                turns: 0,
                tool_calls: tool_call_count + 1,
                events: collected,
            });
        }

        if tools.is_empty() {
            // Single-turn path: providers without tool support behave
            // exactly as a plain completion.
            let request = CompletionRequest::new(decision.selected_model.clone(), messages);
            let response = match model.complete(request).await {
                Ok(response) => response,
                Err(e) => return Err(fail(&mut collected, e)),
            };
            self.emit_assistant_message(&sender, &mut collected, &run_id, &session_id, &response)?;
            let summary: String = response.content.chars().take(80).collect();
            self.emit(
                &sender,
                &mut collected,
                Event::new(&run_id, &session_id, EventKind::Completed { summary }),
            )?;
            return Ok(RunOutcome {
                run_id,
                session_id,
                text: response.content,
                turns: 1,
                tool_calls: tool_call_count,
                events: collected,
            });
        }

        let dispatcher = ToolDispatcher::new(self.execution.clone(), self.graph.clone());
        let selected = decision.selected_model.clone();
        let mut turn = 0u32;
        let final_text = loop {
            turn += 1;
            if turn > max_turns {
                return Err(fail(
                    &mut collected,
                    ForgeError::agent(format!("max turns ({max_turns}) exhausted")),
                ));
            }
            if self.cancel_requested(&run_id) {
                // cancel() already recorded the Cancelled event.
                return Err(ForgeError::cancelled("cancellation requested"));
            }

            let request = CompletionRequest::new(selected.clone(), messages.clone())
                .with_tools(tools.clone());
            let response = match model.complete(request).await {
                Ok(response) => response,
                Err(e) => return Err(fail(&mut collected, e)),
            };

            self.emit_assistant_message(&sender, &mut collected, &run_id, &session_id, &response)?;

            if response.tool_calls.is_empty() {
                let summary: String = response.content.chars().take(80).collect();
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(&run_id, &session_id, EventKind::Completed { summary }),
                )?;
                break response.content;
            }

            let mut assistant = Message::assistant_tool_calls(response.tool_calls.clone());
            assistant.content.clone_from(&response.content);
            messages.push(assistant);

            for call in &response.tool_calls {
                if self.cancel_requested(&run_id) {
                    return Err(ForgeError::cancelled("cancellation requested"));
                }
                tool_call_count += 1;
                let args_summary: String = call.arguments.to_string().chars().take(120).collect();
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(
                        &run_id,
                        &session_id,
                        EventKind::ToolCallRequested {
                            tool: call.name.clone(),
                            args_summary,
                        },
                    ),
                )?;
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(
                        &run_id,
                        &session_id,
                        EventKind::ToolStarted {
                            name: call.name.clone(),
                        },
                    ),
                )?;

                let outcome = match dispatcher.dispatch(call).await {
                    Ok(outcome) => outcome,
                    Err(ForgeError::ApprovalRequired { description, risk }) => {
                        self.emit(
                            &sender,
                            &mut collected,
                            Event::new(
                                &run_id,
                                &session_id,
                                EventKind::ApprovalRequested {
                                    command: description.clone(),
                                    risk,
                                },
                            ),
                        )?;
                        match self
                            .await_approval(&run_id, &mut input_rx, &token, &description, risk)
                            .await
                        {
                            Ok(true) => {
                                self.emit(
                                    &sender,
                                    &mut collected,
                                    Event::new(
                                        &run_id,
                                        &session_id,
                                        EventKind::ApprovalDecided {
                                            command: description,
                                            approved: true,
                                        },
                                    ),
                                )?;
                                match dispatcher.dispatch_approved(call).await {
                                    Ok(outcome) => outcome,
                                    Err(e) => return Err(fail(&mut collected, e)),
                                }
                            }
                            Ok(false) => {
                                self.emit(
                                    &sender,
                                    &mut collected,
                                    Event::new(
                                        &run_id,
                                        &session_id,
                                        EventKind::ApprovalDecided {
                                            command: description,
                                            approved: false,
                                        },
                                    ),
                                )?;
                                ToolOutcome {
                                    result: ToolResult::error(
                                        call.id.clone(),
                                        call.name.clone(),
                                        "approval denied".to_string(),
                                    ),
                                    file_changed: None,
                                }
                            }
                            Err(e @ ForgeError::ApprovalRequired { .. }) => {
                                // Input channel closed with no answer:
                                // record the denial, then fail cleanly.
                                self.emit(
                                    &sender,
                                    &mut collected,
                                    Event::new(
                                        &run_id,
                                        &session_id,
                                        EventKind::ApprovalDecided {
                                            command: description,
                                            approved: false,
                                        },
                                    ),
                                )?;
                                return Err(fail(&mut collected, e));
                            }
                            // Cancellation: the Cancelled event was already
                            // recorded by cancel(); no Error event.
                            Err(e) => return Err(e),
                        }
                    }
                    Err(e) => return Err(fail(&mut collected, e)),
                };

                if let Some(path) = &outcome.file_changed {
                    self.emit(
                        &sender,
                        &mut collected,
                        Event::new(
                            &run_id,
                            &session_id,
                            EventKind::FileChanged { path: path.clone() },
                        ),
                    )?;
                }
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(
                        &run_id,
                        &session_id,
                        EventKind::ToolCompleted {
                            name: call.name.clone(),
                            success: !outcome.result.is_error,
                        },
                    ),
                )?;
                // Replay record: the result verbatim (capped), as the
                // model is about to see it.
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(
                        &run_id,
                        &session_id,
                        EventKind::ToolResult {
                            call_id: call.id.clone(),
                            tool: call.name.clone(),
                            output: forge_core::cap_tool_output(&outcome.result.content),
                            is_error: outcome.result.is_error,
                        },
                    ),
                )?;
                messages.push(Message::tool(
                    call.id.clone(),
                    outcome.result.content.clone(),
                ));
            }
            self.emit(
                &sender,
                &mut collected,
                Event::new(&run_id, &session_id, EventKind::TurnCompleted { turn }),
            )?;
        };

        Ok(RunOutcome {
            run_id,
            session_id,
            text: final_text,
            turns: turn,
            tool_calls: tool_call_count,
            events: collected,
        })
    }

    /// Wait for an approval decision on the run's input channel.
    /// Returns Ok(true/false) on an explicit answer. A closed channel
    /// (stdin EOF, nobody listening) fails the run cleanly with the
    /// original approval-required error instead of hanging; cancellation
    /// aborts the wait.
    async fn await_approval(
        &self,
        run_id: &str,
        rx: &mut mpsc::Receiver<String>,
        token: &CancellationToken,
        description: &str,
        risk: RiskLevel,
    ) -> Result<bool, ForgeError> {
        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(text) => {
                            let answer = text.trim().to_lowercase();
                            return Ok(matches!(answer.as_str(), "y" | "yes" | "approve"));
                        }
                        None => {
                            return Err(ForgeError::ApprovalRequired {
                                description: description.to_string(),
                                risk,
                            });
                        }
                    }
                }
                () = token.cancelled() => {
                    return Err(ForgeError::cancelled("cancellation requested"));
                }
                _ = ticker.tick() => {
                    if self.cancel_marker(run_id).exists() {
                        return Err(ForgeError::cancelled("cancellation requested"));
                    }
                }
            }
        }
    }

    /// Record cancellation for a run (or session): cancel the in-process
    /// token, write the cross-process marker file, and append the
    /// `Cancelled` event. Unknown ids are a typed error.
    pub fn cancel(&self, run_or_session_id: &str) -> Result<(), ForgeError> {
        let session_id = match self.sessions.find_run(run_or_session_id)? {
            Some(session) => Some(session),
            None => self
                .sessions
                .list_sessions()?
                .iter()
                .any(|s| s.session_id == run_or_session_id)
                .then(|| run_or_session_id.to_string()),
        };
        let Some(session_id) = session_id else {
            return Err(ForgeError::session(format!(
                "unknown run or session: {run_or_session_id}"
            )));
        };

        // In-process + cross-process cancellation signals.
        self.cancel_token(run_or_session_id).cancel();
        let marker = self.cancel_marker(run_or_session_id);
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
        }
        std::fs::write(&marker, b"cancelled\n").map_err(ForgeError::Io)?;

        let stored = self.sessions.append(Event::new(
            run_or_session_id,
            &session_id,
            EventKind::Cancelled {
                reason: "cancelled by user".to_string(),
            },
        ))?;
        // Publish to whoever is listening; never register a broadcaster for
        // a run that is not (or no longer) live.
        if let Some(broadcaster) = self.existing_broadcaster(run_or_session_id) {
            let _ = broadcaster.send(stored);
        }
        // Pruning is deliberately left to the run itself: a live loop is
        // still polling the token this cancel just fired, and taking it out
        // from under it would leave only the marker file to notice. A run
        // that is not live here has nothing to prune — `cancel` creates no
        // tracking entries.
        Ok(())
    }

    /// Resume a completed run: start a NEW run in the same session, whose
    /// conversation is the session's history replayed from the event log —
    /// every prior run's prompts, assistant messages, tool calls and tool
    /// results, in order — followed by a continuation instruction. An
    /// `InputReceived` marker event links the new run to the old one.
    ///
    /// Everything up to and including the target run is replayed. Later
    /// runs of the same session are not: resuming run *n* means continuing
    /// from *n*, and a run started after it is a different branch (see
    /// [`fork_session`](Self::fork_session) for keeping both).
    ///
    /// Runs recorded before event schema v3 have no verbatim assistant/tool
    /// payloads, so their turns replay from the truncated `completed`
    /// summary and the resume is logged as degraded. A run with no prompt at
    /// all (v1) still cannot be resumed.
    pub async fn resume(&self, session_or_run_id: &str) -> Result<RunOutcome, ForgeError> {
        let session_id = match self.sessions.find_run(session_or_run_id)? {
            Some(session) => session,
            None if self.session_exists(session_or_run_id)? => session_or_run_id.to_string(),
            None => {
                return Err(ForgeError::session(format!(
                    "unknown run or session: {session_or_run_id}"
                )));
            }
        };
        let events = self.sessions.events_for(&session_id)?;

        // The target run: the id itself when it names a run, else the
        // session's latest real run. A `session_forked` marker is
        // provenance, not a run, so it never becomes the resume target —
        // otherwise the first resume of every fork would look at a run with
        // no prompt.
        let target_run = if events.iter().any(|e| e.run_id == session_or_run_id) {
            session_or_run_id.to_string()
        } else {
            events
                .iter()
                .rev()
                .find(|e| !matches!(e.kind, EventKind::SessionForked { .. }))
                .map(|e| e.run_id.clone())
                .ok_or_else(|| ForgeError::session("session has no runs"))?
        };
        let run_events: Vec<&Event> = events.iter().filter(|e| e.run_id == target_run).collect();

        let task = run_events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::RunStarted { prompt, .. } if !prompt.is_empty() => Some(prompt.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                ForgeError::agent(format!(
                    "run {target_run} predates event schema v2 (no prompt recorded); cannot resume"
                ))
            })?;

        // Only completed runs resume.
        match run_events.last().map(|e| &e.kind) {
            Some(EventKind::Completed { .. }) => {}
            Some(EventKind::Error { .. } | EventKind::Cancelled { .. }) => {
                return Err(ForgeError::agent(format!(
                    "run {target_run} ended without completion; cannot resume"
                )));
            }
            _ => {
                return Err(ForgeError::agent(format!(
                    "run {target_run} is still in progress or empty; cannot resume"
                )));
            }
        }

        // Replay everything up to the end of the target run.
        let cut = events
            .iter()
            .rposition(|e| e.run_id == target_run)
            .map_or(events.len(), |i| i + 1);
        let replay = crate::replay::conversation_from_events(&events[..cut]);
        // The budget comes from the *configured* model's advertised window:
        // routing happens inside the run, after the history is assembled, so
        // a `[models]` entry with a different context window is approximated
        // by the default. The budget is an estimate either way (see
        // `history_budget_chars`) and the model enforces the real limit.
        let budget = crate::replay::history_budget_chars(&self.model.capabilities());
        let replayed = replay.messages.len();
        let history = crate::replay::fit_to_budget(replay.messages, budget);
        tracing::info!(
            session = %session_id,
            run = %target_run,
            replayed,
            kept = history.len(),
            degraded = replay.degraded,
            "replaying session history for resume"
        );

        self.run_tracked(
            RunPlan {
                prompt: RESUME_PROMPT.to_string(),
                task,
                history,
                resumed_from: Some(target_run),
            },
            &new_run_id(),
            &session_id,
            None,
        )
        .await
    }

    /// Fork a session: create a NEW session whose event log is a prefix of
    /// `source`, so both can be continued independently.
    ///
    /// `at` selects the cut point and accepts either spelling:
    ///
    /// * an integer — a 1-based position in the source log, exactly as
    ///   `forge session show --json` lists the events;
    /// * a run id — fork after that run.
    ///
    /// `None` forks the whole log.
    ///
    /// **Snapping.** A cut inside a run is allowed but snapped *forward* to
    /// the end of the run containing it. A half-run prefix would replay as
    /// a conversation with an assistant tool call and no result, which is
    /// not a state any model should be handed; a run boundary is the only
    /// semantically clean place to branch. (The source log's `seq` is
    /// per-run and therefore cannot address a session-wide point on its
    /// own, which is why the position is a log position.)
    ///
    /// The prefix is copied verbatim and the source is never touched, so
    /// the fork is self-contained: it can be resumed, cancelled and forked
    /// again with no reference back. A `session_forked` marker event under
    /// its own run id records the provenance.
    ///
    /// **One consequence of copying:** the copied run ids exist in two
    /// sessions. `resume <run-id>` therefore resolves to whichever session
    /// [`JsonlSessionStore::find_run`] finds first — sessions are listed by
    /// id and session ids are ULIDs, so that is the older session, i.e. the
    /// source. Name the fork's *session* id to continue the fork.
    pub fn fork_session(
        &self,
        source_session_id: &str,
        at: Option<&str>,
    ) -> Result<ForkOutcome, ForgeError> {
        let events = self.sessions.events_for(source_session_id)?;
        if events.is_empty() {
            return Err(ForgeError::session(format!(
                "unknown or empty session: {source_session_id}"
            )));
        }

        // Resolve `at` to an index into `events` that must be included.
        let anchor = match at {
            None => events.len() - 1,
            Some(value) => match value.parse::<usize>() {
                Ok(0) => {
                    return Err(ForgeError::session(
                        "--at is a 1-based log position; 0 is not an event",
                    ));
                }
                Ok(position) if position <= events.len() => position - 1,
                Ok(position) => {
                    return Err(ForgeError::session(format!(
                        "session {source_session_id} has {} events; no position {position}",
                        events.len()
                    )));
                }
                // Not a number: a run id.
                Err(_) => events
                    .iter()
                    .rposition(|e| e.run_id == value)
                    .ok_or_else(|| {
                        ForgeError::session(format!(
                            "session {source_session_id} has no run {value}"
                        ))
                    })?,
            },
        };

        // Snap forward to the end of the run that owns the anchor.
        let at_run_id = events[anchor].run_id.clone();
        let cut = events
            .iter()
            .rposition(|e| e.run_id == at_run_id)
            .unwrap_or(anchor);
        let lines = cut + 1;

        let session_id = new_session_id();
        let events_copied = self
            .sessions
            .copy_prefix(source_session_id, &session_id, lines)?;
        #[allow(clippy::cast_possible_truncation)]
        let at_position = lines as u64;
        self.sessions.append(Event::new(
            new_run_id(),
            &session_id,
            EventKind::SessionForked {
                from_session: source_session_id.to_string(),
                at_position,
            },
        ))?;

        tracing::info!(
            source = source_session_id,
            session = %session_id,
            at_position,
            run = %at_run_id,
            events_copied,
            "forked session"
        );
        Ok(ForkOutcome {
            session_id,
            source_session_id: source_session_id.to_string(),
            at_position,
            at_run_id,
            events_copied,
        })
    }

    /// All events belonging to one run (for the server events endpoint).
    pub fn events(&self, run_id: &str) -> Result<Vec<Event>, ForgeError> {
        match self.sessions.find_run(run_id)? {
            Some(session) => Ok(self
                .sessions
                .events_for(&session)?
                .into_iter()
                .filter(|e| e.run_id == run_id)
                .collect()),
            None => Err(ForgeError::session(format!("unknown run: {run_id}"))),
        }
    }

    fn session_exists(&self, session_id: &str) -> Result<bool, ForgeError> {
        Ok(self
            .sessions
            .list_sessions()?
            .iter()
            .any(|s| s.session_id == session_id))
    }
}

#[cfg(test)]
mod tests;
