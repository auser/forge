use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use forge_config::Config;
use forge_context::{
    CONTEXT_PLAN_VERSION, ContextComponents, ContextPlanDraft, ContextPlanSummary, ContextSize,
    ContextStore, stable_prefix,
};
use forge_core::{
    CompletionRequest, DecisionRouter, Event, EventKind, ExecutionProvider, ForgeError, Message,
    ModelProvider, ProjectGraph, RiskLevel, RoutingRequest, RunState, SessionStore, Skill,
    SkillMeta, SkillRegistry, TOOL_POLICY_SCHEMA_VERSION, ToolCall, ToolResult,
};
use forge_needle::NeedleEngine;
use forge_session::{
    Decider, DecisionLog, DecisionLogHandle, JsonlSessionStore, Outcome, RecordDraft, Stage,
    new_run_id, new_session_id,
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::budget::{SpendTracker, completion_cost};
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

/// Budget for the *first* `tool_call` in a process — see
/// [`AgentService::needle_fast_path`] for the measurements behind it.
///
/// Small on purpose. A cold `libneedle` needs ~8 s for that call and cannot win
/// whatever we allow it, so the only question is how fast we give up; the
/// backends that *can* answer (an already-warm engine, or `HashBackend`) do so
/// in microseconds, far inside this. Not configurable: it is a property of the
/// engine's one-time setup cost, not a preference.
const FIRST_TOOL_CALL_PROBE: Duration = Duration::from_millis(250);

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

/// A tool call the brain picked and vetted, ready for policy evaluation.
struct FastPathDispatch {
    call: ToolCall,
    confidence: f64,
}

/// Run-local quota state. It is deliberately created inside `run_inner`, so
/// concurrent runs on the same service never share counters.
struct ToolRunQuotas<'a> {
    limits: &'a std::collections::BTreeMap<String, forge_config::ToolLimitConfig>,
    attempts: HashMap<String, u64>,
}

impl<'a> ToolRunQuotas<'a> {
    fn new(config: &'a Config) -> Self {
        Self {
            limits: &config.tool_limits,
            attempts: HashMap::new(),
        }
    }

    /// Reserve an attempt before dispatch. Invalid calls consume quota because
    /// validation happens in the dispatcher; calls rejected here do not.
    fn reserve(&mut self, tool: &str) -> Result<(), String> {
        let Some(limit) = self.limits.get(tool) else {
            return Ok(());
        };
        let used = self.attempts.entry(tool.to_string()).or_default();
        if *used >= limit.per_run {
            return Err(format!(
                "per-run quota exceeded for tool `{tool}`: limit is {}; choose another action or finish the run",
                limit.per_run
            ));
        }
        *used += 1;
        Ok(())
    }

    async fn dispatch(
        dispatcher: &ToolDispatcher,
        call: &ToolCall,
        reservation: Result<(), String>,
    ) -> Result<ToolOutcome, ForgeError> {
        if let Err(message) = reservation {
            return Ok(ToolOutcome {
                result: ToolResult::error(call.id.clone(), call.name.clone(), message),
                file_changed: None,
            });
        }
        dispatcher.dispatch(call).await
    }
}

/// The plumbing a pause-for-approval inside the agent loop needs: how to
/// emit events for this run, and how to wait for the answer. Bundled so
/// the gates that use it (the budget gate today) don't each grow a
/// seven-parameter signature.
struct RunGate<'a> {
    sender: &'a broadcast::Sender<Event>,
    collected: &'a mut Vec<Event>,
    run_id: &'a str,
    session_id: &'a str,
    input_rx: &'a mut mpsc::Receiver<String>,
    token: &'a CancellationToken,
}

#[derive(Clone, Copy)]
struct ContextBoundaries {
    system_end: usize,
    skills_end: usize,
    graph_end: usize,
    prompt_index: usize,
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

/// A run started on its own task: the ids a transport can answer with
/// immediately, plus the handle it needs to await or abort.
///
/// A struct rather than a tuple because the call is now fallible — a session
/// with a run already in flight is refused — and `Result<(String, String,
/// JoinHandle<…>), _>` reads as noise at every call site.
#[derive(Debug)]
pub struct StartedRun {
    pub run_id: String,
    pub session_id: String,
    pub handle: tokio::task::JoinHandle<Result<RunOutcome, ForgeError>>,
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
    /// Skill names to activate explicitly, in addition to any the task
    /// matches lexically. An unknown name is a typed error at the entry
    /// point — the caller asked for it, so never run without it.
    pub activate_skills: Vec<String>,
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
    /// Skills the caller named explicitly. Empty on a resume, which has no
    /// options and re-runs discovery on the original task.
    activate_skills: Vec<String>,
}

impl RunPlan {
    /// A new instruction: the prompt is the task, and `history` is whatever
    /// the session it lands in already contains (empty for a fresh session).
    fn new_prompt(
        prompt: impl Into<String>,
        history: Vec<Message>,
        activate_skills: Vec<String>,
    ) -> Self {
        let prompt = prompt.into();
        Self {
            task: prompt.clone(),
            prompt,
            history,
            resumed_from: None,
            activate_skills,
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

/// Sessions with a run in flight in this process, `session_id -> run_id`.
///
/// Held behind an `Arc` rather than inline in [`AgentService`] so a
/// [`SessionClaim`] can outlive the borrow that created it and travel into a
/// spawned task.
#[derive(Default)]
struct LiveSessions {
    runs: Mutex<HashMap<String, String>>,
}

impl LiveSessions {
    /// Claim `session_id` for `run_id`, or report who holds it.
    fn claim(self: &Arc<Self>, session_id: &str, run_id: &str) -> Result<SessionClaim, ForgeError> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = runs.get(session_id) {
            return Err(ForgeError::session_busy(session_id, active));
        }
        runs.insert(session_id.to_string(), run_id.to_string());
        Ok(SessionClaim {
            sessions: Arc::clone(self),
            session_id: session_id.to_string(),
        })
    }

    fn release(&self, session_id: &str) {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }
}

/// A session held for the lifetime of one run: while this value exists, no
/// second run can start in that session.
///
/// Release is `Drop` rather than an explicit call at the end of the run so
/// that every way a run can end frees the session — a normal finish, an
/// error, a panic inside the loop, and the aborted task the REST adapter's
/// cancel produces. A claim that leaked would wedge its session permanently.
struct SessionClaim {
    sessions: Arc<LiveSessions>,
    session_id: String,
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        self.sessions.release(&self.session_id);
        tracing::debug!(session = %self.session_id, "session claim released");
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
    context_store: Option<Arc<dyn ContextStore>>,
    artifact_store: Option<Arc<dyn forge_context::ArtifactStore>>,
    compression_enabled: bool,
    /// Stable coding contract and project guidance supplied by the host.
    /// Kept separate from replay: it is current run context, not conversation.
    system_context: Vec<Message>,
    /// Resolves a provider for the routed model name; defaults to the
    /// single configured model for every selection.
    model_factory: Option<ModelFactory>,
    /// On-device brain for the direct-dispatch fast path. `None` (the
    /// default) means every run goes through the model loop.
    needle: Option<Arc<NeedleEngine>>,
    /// Whether the engine's tool surface has been installed yet. The install is
    /// a property of the engine, not of any one run, so it is paid once per
    /// process — see [`AgentService::needle_fast_path`].
    needle_warmed: AtomicBool,
    /// Per-run tracking, pruned when a run reaches a terminal state (see
    /// [`AgentService::finish_run`]).
    broadcasters: Mutex<HashMap<String, broadcast::Sender<Event>>>,
    inputs: Mutex<HashMap<String, InputState>>,
    cancel_tokens: Mutex<HashMap<String, CancellationToken>>,
    /// Bounded tombstones for pruned runs, so `send_input` can refuse a
    /// finished run instead of resurrecting its channel.
    finished: Mutex<FinishedRuns>,
    /// One live run per session (see [`AgentService::claim_session`]).
    live_sessions: Arc<LiveSessions>,
    /// What forge decided, appended beside the transcripts as
    /// `<session-id>.decisions.jsonl` — decision shape only, never prompt
    /// text or tool arguments (see `forge_session::decisions`).
    decision_log: Arc<DecisionLog>,
    /// Per-session turn counter for the decision log, seeded from the
    /// store on first touch (one parse per session per process) and then
    /// kept in memory. A turn is a run today; the counter exists so the
    /// log's `turn` column stops being a hardcoded 1 on every record.
    session_turns: Mutex<HashMap<String, u32>>,
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
        // The log lives beside the transcripts: sessions.root() is the
        // directory holding `<session-id>.jsonl`.
        let decision_log = Arc::new(DecisionLog::new(sessions.root().to_path_buf()));
        let compression_enabled = config.context_compression.enabled;
        Self {
            model,
            router,
            execution,
            skills,
            sessions,
            config,
            graph: None,
            context_store: None,
            artifact_store: None,
            compression_enabled,
            system_context: Vec::new(),
            model_factory: None,
            needle: None,
            needle_warmed: AtomicBool::new(false),
            broadcasters: Mutex::new(HashMap::new()),
            inputs: Mutex::new(HashMap::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
            finished: Mutex::new(FinishedRuns::default()),
            live_sessions: Arc::new(LiveSessions::default()),
            decision_log,
            session_turns: Mutex::new(HashMap::new()),
        }
    }

    /// Attach a project graph for context seeding and graph tools.
    pub fn with_graph(mut self, graph: Option<Arc<dyn ProjectGraph>>) -> Self {
        self.graph = graph;
        self
    }

    /// Attach best-effort context accounting. Its failure never blocks a model call.
    pub fn with_context_store(mut self, store: Option<Arc<dyn ContextStore>>) -> Self {
        self.context_store = store;
        self
    }

    /// Attach local sanitized tool artifacts. Absent storage still redacts outputs.
    pub fn with_artifact_store(
        mut self,
        store: Option<Arc<dyn forge_context::ArtifactStore>>,
    ) -> Self {
        self.artifact_store = store;
        self
    }

    /// Disable content compression without changing CONTEXT-2 artifact behavior.
    pub fn with_compression_enabled(mut self, enabled: bool) -> Self {
        self.compression_enabled = enabled;
        self
    }

    /// Attach host/project guidance that starts every model conversation.
    pub fn with_system_context(mut self, messages: Vec<Message>) -> Self {
        self.system_context = messages;
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

    /// True when this process is running (or about to run) the run:
    /// `start_run` registers the broadcaster before spawning the loop, and
    /// `finish_run` removes it first when pruning, so this is the one flag
    /// that means "our loop, live".
    fn is_tracked(&self, run_id: &str) -> bool {
        self.broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(run_id)
    }

    /// A receiver on a channel with no sender: nothing can ever arrive.
    fn closed_stream() -> broadcast::Receiver<Event> {
        let (sender, receiver) = broadcast::channel(1);
        drop(sender);
        receiver
    }

    /// Can this process still publish events for `run_id`?
    ///
    /// True when the run is already ours, or when nothing is known about it
    /// at all — an id handed out but not yet started. Both
    /// [`subscribe`](Self::subscribe) and
    /// [`input_sender`](Self::input_sender) need that second case: ACP
    /// subscribes *before* `start_run_with_options` so an early approval
    /// cannot be emitted with nobody listening, and `forge run` queues piped
    /// stdin before calling `run_with_options`.
    ///
    /// False for the two cases that used to grow the maps with entries
    /// nothing would publish to or prune: a run this process already
    /// finished, and a run that is live *in another process* (it has stored
    /// events but is not ours — its events never reach this channel).
    fn may_become_live(&self, run_id: &str) -> bool {
        if self.is_tracked(run_id) {
            return true;
        }
        if self.tombstoned_state(run_id).is_some() {
            return false;
        }
        // Unknown to the store as well: nothing has run under this id, so it
        // may still be about to start here.
        self.events(run_id).map(|e| e.is_empty()).unwrap_or(true)
    }

    /// Subscribe to the live event stream of a run. This is the
    /// transport-neutral seam the server's SSE endpoint consumes, and the
    /// one ACP calls *before* starting a run so no early event is emitted
    /// with nobody listening.
    ///
    /// A run that can no longer produce events here — already finished, or
    /// live in another process — gets an immediately-closed receiver instead
    /// of registering a broadcaster nothing will ever publish to and nothing
    /// will ever prune. Those runs are read from the session store: see
    /// [`attach`](Self::attach), which returns the backlog and the live
    /// stream together.
    pub fn subscribe(&self, run_id: &str) -> broadcast::Receiver<Event> {
        if !self.may_become_live(run_id) {
            return Self::closed_stream();
        }
        self.broadcaster(run_id).subscribe()
    }

    /// Terminal state this process recorded for a run when it pruned it.
    /// Cheap (one lock, no IO), which is why it is the check that runs
    /// inside [`input_sender`](Self::input_sender)'s critical section.
    fn tombstoned_state(&self, run_id: &str) -> Option<RunState> {
        self.finished
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .states
            .get(run_id)
            .copied()
    }

    /// Terminal state of a run, if it is known to have one: this process's
    /// tombstones first, then the session store (which also covers runs
    /// started by another process).
    fn terminal_state(&self, run_id: &str) -> Option<RunState> {
        if let Some(state) = self.tombstoned_state(run_id) {
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
    ///
    /// **Order matters.** The tombstone is recorded *first*, before any map
    /// entry disappears: [`input_sender`](Self::input_sender) re-checks it
    /// under the inputs lock, so recording it up front is what stops a run
    /// terminating mid-`send_input` from getting a resurrected channel.
    ///
    /// Each lock is taken and released on its own; nothing here nests, so
    /// `input_sender`'s nesting cannot deadlock against it.
    fn finish_run(&self, run_id: &str, state: RunState) {
        self.finished
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(run_id, state);
        self.broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        tracing::debug!(run_id, state = state.as_str(), "run tracking pruned");
    }

    /// Per-run input channel sender. Used by `send_input` (server input
    /// endpoint / CLI stdin feeder) and by the approval pause inside the
    /// loop.
    ///
    /// Creating the channel on demand is deliberate and load-bearing:
    /// `forge run` generates the run id and spawns its stdin feeder *before*
    /// calling `run_with_options`, so `echo y | forge run …` legitimately
    /// queues an approval for a run that has not started. What must never
    /// happen is creating one for a run that has already **finished** — the
    /// resurrection that swallowed the message and leaked the entry.
    ///
    /// The tombstone is therefore re-checked *while holding the inputs
    /// lock*. That closes the window [`send_input`](Self::send_input)'s
    /// earlier check leaves open: [`finish_run`](Self::finish_run) records
    /// the tombstone before it prunes anything, so a run that terminates
    /// between the two is already tombstoned by the time creation is
    /// considered. Nesting order is `inputs` → {`finished`, `broadcasters`},
    /// and this is the only place these locks nest; `finish_run` holds one at
    /// a time and so can never be the other half of a cycle.
    fn input_sender(&self, run_id: &str) -> Result<mpsc::Sender<String>, ForgeError> {
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        match inputs.get(run_id) {
            Some(InputState::Open { sender, .. }) => return Ok(sender.clone()),
            Some(InputState::Closed) => {
                return Err(ForgeError::session(format!(
                    "input channel for run {run_id} is closed"
                )));
            }
            None => {}
        }
        if let Some(state) = self.tombstoned_state(run_id) {
            return Err(ForgeError::session(format!(
                "run {run_id} is {}; not accepting input",
                state.as_str()
            )));
        }
        if !self.may_become_live(run_id) {
            return Err(ForgeError::session(format!(
                "run {run_id} has no live input channel in this process; not accepting input"
            )));
        }
        match inputs
            .entry(run_id.to_string())
            .or_insert_with(InputState::open)
        {
            InputState::Open { sender, .. } => Ok(sender.clone()),
            // Just inserted as Open; unreachable in practice.
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
        let tracked = self.is_tracked(run_id);

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
    ///
    /// The bound is on the *work*, not just the response: chat calls this
    /// once per submitted line, so it cannot re-read and re-parse every
    /// session's whole JSONL log each time. Sessions are walked
    /// newest-modified first, and once [`LISTED_TERMINAL_RUNS`] terminal
    /// runs are collected, any session file last modified at or before the
    /// current cutoff is skipped unparsed — every event in it predates its
    /// mtime, so nothing in it could displace a run already kept. One
    /// honest consequence: a run that died without writing a terminal
    /// event (state `Running` forever) and whose session file has been
    /// untouched since before the cutoff no longer lists as live. Runs
    /// *this* process tracks are in memory and always listed regardless.
    pub fn list_runs(&self) -> Result<Vec<RunSummary>, ForgeError> {
        let mut summaries: Vec<RunSummary> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        // `last_event_at` of each terminal run collected so far, newest
        // first; the cutoff for skipping old session files unparsed.
        let mut terminal_stamps: Vec<DateTime<Utc>> = Vec::new();

        for (session_id, _path, modified) in self.sessions.session_files_by_recency()? {
            if terminal_stamps.len() >= LISTED_TERMINAL_RUNS {
                let cutoff = terminal_stamps[LISTED_TERMINAL_RUNS - 1];
                if DateTime::<Utc>::from(modified) <= cutoff {
                    continue;
                }
            }
            let events = self.sessions.events_for(&session_id)?;
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
                if state.is_terminal()
                    && let Some(stamp) = own.last().map(|e| e.ts)
                {
                    // Keep newest-first so the cutoff is a plain index.
                    let slot = terminal_stamps
                        .binary_search_by(|probe| stamp.cmp(probe))
                        .unwrap_or_else(|slot| slot);
                    terminal_stamps.insert(slot, stamp);
                }
                summaries.push(RunSummary {
                    run_id,
                    session_id: Some(session_id.clone()),
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

        // Newest activity first, with the just-started (no events yet) runs
        // ahead of everything — they are the newest thing there is. The
        // `is_some()` term is load-bearing: `None < Some(_)`, so
        // `Reverse(None)` is the *maximum* and sorting on the timestamp
        // alone would bury them at the end.
        summaries.sort_by_key(|s| {
            (
                s.last_event_at.is_some(),
                std::cmp::Reverse(s.last_event_at),
            )
        });
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

    /// The run's cancellation token, created if absent. Only the loop calls
    /// this, for its own run — `finish_run` prunes what it creates.
    fn cancel_token(&self, run_id: &str) -> CancellationToken {
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run_id.to_string())
            .or_default()
            .clone()
    }

    /// The run's cancellation token if one exists, without creating it —
    /// what [`cancel`](Self::cancel) uses. A run with no token has no loop
    /// in this process listening for one, and the cross-process marker file
    /// covers it; creating a token for it would leave an entry nothing
    /// prunes.
    fn existing_cancel_token(&self, run_id: &str) -> Option<CancellationToken> {
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned()
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

    /// The single post-dispatch boundary, shared by ordinary and Needle tools.
    /// Sanitize the complete result before truncation, storage or publication.
    fn prepare_tool_output(
        &self,
        call: &ToolCall,
        raw: &str,
        source: forge_context::ArtifactSource,
    ) -> (String, Option<EventKind>, Option<EventKind>) {
        if call.name == "retrieve_tool_output" {
            return (
                self.sessions.redactor().redact_tool_output(&call.name, raw),
                None,
                None,
            );
        }
        let sanitized = forge_context::SanitizedOutput::new(self.sessions.redactor(), raw);
        let text = sanitized.as_str();
        if text.len() <= forge_core::MAX_TOOL_OUTPUT_BYTES {
            return (forge_core::cap_tool_output(text), None, None);
        }
        let capped = forge_core::cap_tool_output(text);
        let reference = self.artifact_store.as_ref().and_then(|store| {
            let source =
                forge_context::SanitizedArtifactSource::new(source, self.sessions.redactor())
                    .ok()?;
            store.put(source, &sanitized).ok()
        });
        match reference {
            Some(reference) => {
                let mut output = format!(
                    "{capped}\n[Complete sanitized output: retrieve_tool_output handle={}]\n",
                    reference.handle
                );
                let decision = if self.compression_enabled {
                    // Compare savings against what append will actually persist, but
                    // retain the original fallback so it traverses that boundary once.
                    let persisted_baseline = self
                        .sessions
                        .redactor()
                        .redact_tool_output(&call.name, &output);
                    let compressed = forge_context::compress_tool_output(
                        call,
                        &sanitized,
                        &reference,
                        &persisted_baseline,
                    );
                    let mut decision = compressed.decision;
                    if let Some(view) = compressed.view {
                        // Framing can complete a secret pattern absent from the
                        // sanitized source. Never publish a structurally changed view.
                        if self
                            .sessions
                            .redactor()
                            .redact_tool_output(&call.name, &view)
                            == view
                        {
                            output = view;
                        } else {
                            decision.reason = forge_context::CompressionReason::Unsupported;
                            decision.view = decision.baseline;
                            decision.omitted = 0;
                        }
                    }
                    use forge_context::{CompressionKind as K, CompressionReason as R};
                    use forge_core::events::{
                        ToolCompressionKind as EK, ToolCompressionReason as ER, ToolCompressionSize,
                    };
                    let size = |value: forge_context::ContextSize| ToolCompressionSize {
                        chars: value.chars,
                        estimated_tokens: value.estimated_tokens,
                    };
                    Some(EventKind::ToolOutputCompression {
                        event_seq: reference.source.event_seq,
                        version: decision.version,
                        kind: match decision.kind {
                            K::Unknown => EK::Unknown,
                            K::Log => EK::Log,
                            K::Search => EK::Search,
                            K::Json => EK::Json,
                            K::Jsonl => EK::Jsonl,
                            K::Table => EK::Table,
                            K::Diff => EK::Diff,
                        },
                        reason: match decision.reason {
                            R::BelowThreshold => ER::BelowThreshold,
                            R::Unsupported => ER::Unsupported,
                            R::NoSavings => ER::NoSavings,
                            R::Compressed => ER::Compressed,
                        },
                        original: size(decision.original),
                        baseline: size(decision.baseline),
                        view: size(decision.view),
                        omitted: decision.omitted,
                    })
                } else {
                    None
                };
                let event = EventKind::ToolOutputArtifact {
                    handle: reference.handle,
                    source_session_id: reference.source.session_id,
                    source_run_id: reference.source.run_id,
                    call_id: reference.source.call_id,
                    event_seq: reference.source.event_seq,
                };
                (output, Some(event), decision)
            }
            None => (
                format!("{capped}\n[Complete sanitized output unavailable]\n"),
                None,
                None,
            ),
        }
    }

    /// Grants come only from the exact persisted session prefix. Forks physically
    /// copy that prefix, including inherited source identities; no ancestor's
    /// current log or model-supplied text is consulted.
    fn retrieve_tool_output(&self, session_id: &str, call: &ToolCall) -> ToolOutcome {
        let result = (|| {
            let store = self.artifact_store.as_ref().ok_or("artifact unavailable")?;
            let (handle, query) = crate::tools::artifact_query(&call.arguments)?;
            let events = self
                .sessions
                .events_for(session_id)
                .map_err(|_| "artifact unavailable")?;
            let allowed = events
                .into_iter()
                .filter_map(|event| match event.kind {
                    EventKind::ToolOutputArtifact {
                        handle,
                        source_session_id,
                        source_run_id,
                        call_id,
                        event_seq,
                    } => Some(forge_context::ArtifactRef {
                        handle,
                        source: forge_context::ArtifactSource {
                            session_id: source_session_id,
                            run_id: source_run_id,
                            call_id,
                            event_seq,
                        },
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let read = store
                .retrieve(handle, &allowed, query)
                .map_err(|_| "artifact unavailable")?
                .ok_or("artifact unavailable")?;
            let output = serde_json::to_string(&read).map_err(|_| "artifact unavailable")?;
            let output = self
                .sessions
                .redactor()
                .redact_tool_output(&call.name, &output);
            if output.len() > 16 * 1024 {
                return Err("artifact unavailable");
            }
            Ok(output)
        })();
        ToolOutcome {
            result: match result {
                Ok(text) => ToolResult::ok(call.id.clone(), call.name.clone(), text),
                Err(error) => {
                    ToolResult::error(call.id.clone(), call.name.clone(), error.to_owned())
                }
            },
            file_changed: None,
        }
    }

    /// Append an event to the store (which assigns its sequence number),
    /// broadcast the stored event to subscribers, and collect it.
    fn emit(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        event: Event,
    ) -> Result<(), ForgeError> {
        self.emit_stored(sender, collected, event).map(|_| ())
    }

    /// Return the persisted view for consumers that forward event content.
    fn emit_stored(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        event: Event,
    ) -> Result<Event, ForgeError> {
        let stored = self.sessions.append(event)?;
        // No subscribers yet is normal for the CLI; not an error.
        let _ = sender.send(stored.clone());
        collected.push(stored.clone());
        Ok(stored)
    }

    /// Activate one skill by name: emit its `skill_activated` event and
    /// return the system message carrying its instructions. The one
    /// definition of both, so the explicit and lexical activation paths in
    /// [`run_inner`](Self::run_inner) cannot drift — only their failure
    /// policies differ (explicit fails the run, lexical warns and
    /// continues), and that choice stays at the call sites.
    fn activate_skill(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        run_id: &str,
        session_id: &str,
        name: &str,
    ) -> Result<Message, ForgeError> {
        let skill = self.skills.activate(name)?;
        self.emit(
            sender,
            collected,
            Event::new(
                run_id,
                session_id,
                EventKind::SkillActivated {
                    name: skill.meta.name.clone(),
                    path: skill.meta.path.clone(),
                },
            ),
        )?;
        Ok(Message::system(format!(
            "Active skill `{}` instructions:\n{}",
            skill.meta.name, skill.instructions
        )))
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

    /// Record one completion's usage and cost in the decision log, and
    /// accrue both into the spend tracker the budget gate reads. The price
    /// comes from the [`forge_config::CostBook`] — the resolved model
    /// entry's own per-million-token costs first, the cached OpenRouter
    /// catalogue when the entry declares none — so a model with no price
    /// anywhere (or a response with no usage) stays `None`, never 0.0
    /// pretending to be free.
    #[allow(clippy::too_many_arguments)]
    fn account_completion(
        &self,
        decisions: &DecisionLogHandle,
        spend: &mut SpendTracker,
        turn: u32,
        model_name: &str,
        response: &forge_core::CompletionResponse,
        elapsed_ms: u64,
        costs: &forge_config::CostBook,
    ) {
        let cost_usd = completion_cost(response.usage, costs.price(model_name));
        spend.record(response.usage, cost_usd);
        decisions.record_usage(turn, model_name, response.usage, cost_usd, elapsed_ms);
    }

    #[allow(clippy::too_many_arguments)]
    fn record_context_plan(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        run_id: &str,
        session_id: &str,
        ordinal: u32,
        request: &CompletionRequest,
        boundaries: ContextBoundaries,
        max_context: usize,
    ) -> Result<(), ForgeError> {
        let Some(store) = &self.context_store else {
            return Ok(());
        };
        let size = |messages: &[Message]| ContextSize::of_items(messages);
        let mut components = ContextComponents {
            system_guidance: size(&request.messages[..boundaries.system_end]),
            skill_instructions: size(
                &request.messages[boundaries.system_end..boundaries.skills_end],
            ),
            graph_context: size(&request.messages[boundaries.skills_end..boundaries.graph_end]),
            replay_history: size(&request.messages[boundaries.graph_end..boundaries.prompt_index])
                .saturating_add(size(&request.messages[boundaries.prompt_index + 1..])),
            memory: ContextSize::default(),
            current_prompt: size(
                &request.messages[boundaries.prompt_index..=boundaries.prompt_index],
            ),
            tool_schemas: ContextSize::of_items(&request.tools),
            total: ContextSize::default(),
        };
        components.calculate_total();
        let draft = ContextPlanDraft {
            version: CONTEXT_PLAN_VERSION,
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            request_ordinal: ordinal,
            remaining_context_tokens: max_context.saturating_sub(components.total.estimated_tokens),
            components,
            stable_prefix: stable_prefix(&self.system_context, &request.tools),
            reserved_output_tokens: request.max_tokens,
        };
        let kind = match store.record(draft) {
            Ok(plan) => {
                let summary = ContextPlanSummary::from(&plan);
                EventKind::ContextPlanRecorded {
                    plan_id: summary.plan_id,
                    request_ordinal: summary.request_ordinal,
                    stable_prefix_hash: summary.stable_prefix_hash,
                    prefix_changed: summary.prefix_changed,
                    total_estimated_input_tokens: summary.total_estimated_input_tokens,
                    reserved_output_tokens: summary.reserved_output_tokens,
                    plan_path: summary.plan_path,
                }
            }
            Err(error) => {
                let category = match error {
                    ForgeError::Io(_) => "io",
                    ForgeError::Session(_) => "store",
                    _ => "unknown",
                };
                tracing::warn!(error_category = category, "context accounting unavailable");
                EventKind::ContextPlanUnavailable {
                    request_ordinal: ordinal,
                    error_category: category.to_string(),
                    message: "context accounting unavailable".to_string(),
                }
            }
        };
        self.emit(sender, collected, Event::new(run_id, session_id, kind))
    }

    /// Enforce `[budget]` before a model call: when a configured ceiling
    /// has been reached, `on_exceeded = "stop"` fails the turn naming the
    /// ceiling and the spend, and `"prompt"` asks on the run's input
    /// channel — the same path tool approvals take. A channel with nobody
    /// to answer (headless run, closed stdin) degrades `"prompt"` to
    /// `"stop"`: a ceiling that silently lets everything through is worse
    /// than one that halts. Zero-cost (local) models never reach this with
    /// a USD ceiling tripped — they accrue no USD.
    async fn gate_budget(
        &self,
        gate: &mut RunGate<'_>,
        spend: &SpendTracker,
    ) -> Result<(), ForgeError> {
        let Some(trip) = spend.tripped(&self.config.budget) else {
            return Ok(());
        };
        let description = trip.describe();
        if self.config.budget.on_exceeded == "stop" {
            return Err(ForgeError::agent(format!(
                "budget exceeded: {description} (budget.on_exceeded = \"stop\")"
            )));
        }
        self.emit(
            gate.sender,
            gate.collected,
            Event::new(
                gate.run_id,
                gate.session_id,
                EventKind::ApprovalRequested {
                    command: description.clone(),
                    risk: RiskLevel::Risky,
                },
            ),
        )?;
        match self
            .await_approval(
                gate.run_id,
                gate.input_rx,
                gate.token,
                &description,
                RiskLevel::Risky,
            )
            .await
        {
            Ok(approved) => {
                self.emit(
                    gate.sender,
                    gate.collected,
                    Event::new(
                        gate.run_id,
                        gate.session_id,
                        EventKind::ApprovalDecided {
                            command: description.clone(),
                            approved,
                        },
                    ),
                )?;
                if approved {
                    Ok(())
                } else {
                    Err(ForgeError::agent(format!(
                        "budget exceeded: {description}; continuing was denied"
                    )))
                }
            }
            Err(ForgeError::ApprovalRequired { .. }) => {
                // Input channel closed with no answer: nobody can approve,
                // so prompt behaves as stop (mirroring the tool-approval
                // flow above).
                self.emit(
                    gate.sender,
                    gate.collected,
                    Event::new(
                        gate.run_id,
                        gate.session_id,
                        EventKind::ApprovalDecided {
                            command: description.clone(),
                            approved: false,
                        },
                    ),
                )?;
                Err(ForgeError::agent(format!(
                    "budget exceeded: {description} and no one can answer the budget prompt; \
                     treating budget.on_exceeded = \"prompt\" as \"stop\""
                )))
            }
            // Cancellation: the Cancelled event was already recorded by
            // cancel(); no Error event.
            Err(e) => Err(e),
        }
    }

    /// One model call, streamed when the provider supports it.
    ///
    /// Fragments become `assistant_delta` events — rendering-only: the replay
    /// record is the assembled `AssistantMessage` emitted by
    /// [`emit_assistant_message`](Self::emit_assistant_message) from the
    /// returned response, exactly as for a non-streaming call.
    ///
    /// A delta is emitted only up to its last whitespace boundary; the
    /// trailing partial token is carried until more text arrives and flushed
    /// before this returns. The redactor matches whole patterns per payload
    /// (`forge_session`'s one boundary), and a provider chunk can split a
    /// secret mid-token — the carry is what makes "deltas pass through the
    /// one redaction boundary" true rather than vacuous. The boundary is
    /// also *pattern-aware*: a candidate prefix that ends where a
    /// whitespace-spanning pattern (`Bearer <token>`) could still be
    /// completed is held back from the pattern's start, or the token would
    /// leave in a later delta that matches nothing alone.
    async fn complete_streaming(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        run_id: &str,
        session_id: &str,
        model: &Arc<dyn ModelProvider>,
        request: CompletionRequest,
    ) -> Result<forge_core::CompletionResponse, ForgeError> {
        if !model.capabilities().streaming {
            return model.complete(request).await;
        }
        let mut carry = String::new();
        let mut streamed = String::new();
        let mut on_delta = |delta: &str| {
            carry.push_str(delta);
            // Emit the prefix that ends at whitespace; keep the partial token.
            // (`rfind` yields a byte index, so step over the whole char.)
            let Some(mut split) = carry
                .rfind(char::is_whitespace)
                .map(|i| i + carry[i..].chars().next().map_or(1, char::len_utf8))
            else {
                return;
            };
            // Pattern-aware: never emit a prefix that ends where a
            // whitespace-spanning secret pattern could still be completed by
            // later text.
            if let Some(start) = self.sessions.secret_prefix_start(&carry[..split]) {
                split = start;
            }
            if split == 0 {
                return;
            }
            let tail = carry.split_off(split);
            streamed.push_str(&carry);
            let event = Event::new(
                run_id,
                session_id,
                EventKind::AssistantDelta {
                    text: std::mem::take(&mut carry),
                },
            );
            carry = tail;
            // Best-effort: a delta that fails to persist is a rendering gap,
            // never a run failure — the final AssistantMessage carries the
            // same text through the same boundary a moment later.
            if let Err(e) = self.emit(sender, collected, event) {
                tracing::warn!(run_id, error = %e, "assistant delta not persisted");
            }
        };
        let response = model.stream_complete(request, &mut on_delta).await?;
        // The tail: the provider contract says fragments concatenate to
        // `content`, so flush whatever is still carried.
        streamed.push_str(&carry);
        if !carry.is_empty() {
            let event = Event::new(
                run_id,
                session_id,
                EventKind::AssistantDelta { text: carry },
            );
            if let Err(e) = self.emit(sender, collected, event) {
                tracing::warn!(run_id, error = %e, "assistant delta not persisted");
            }
        }
        if streamed != response.content {
            // A provider violating the fragment contract must not corrupt
            // the replay record — the response is what gets recorded.
            tracing::warn!(
                run_id,
                "streamed fragments do not concatenate to the response content; \
                 the final assistant_message is unaffected"
            );
        }
        Ok(response)
    }

    /// Take the session for one run, or refuse: **one live run per session.**
    ///
    /// Two runs in one session are not merely racy, they corrupt data that
    /// outlives them. Both write into the same append-only log, so their
    /// events interleave, and the next replay of that session has to
    /// disentangle them (see [`crate::replay`]) — which it now does, but a
    /// log that never interleaves is better than one that has to be repaired.
    /// They would also each replay a history that does not include the other,
    /// so the two conversations silently diverge.
    ///
    /// Claimed at the *entry points* (`run_with_options`,
    /// `start_run_with_options`, `resume`) rather than deeper in the loop,
    /// because the refusal has to reach the caller **synchronously**: a
    /// transport that has already answered "202, here is your run id" has
    /// nowhere left to report a conflict. The claim then travels into
    /// [`run_tracked`](Self::run_tracked), whose scope is the run's lifetime,
    /// so release needs no bookkeeping of its own.
    ///
    /// Sequential reuse of a session is untouched: the claim is gone before
    /// the run's future resolves, so the next turn, resume or `POST /v1/runs`
    /// naming that session claims it freely.
    ///
    /// The scope is this process. Two `forge resume` processes on one session
    /// directory are still able to interleave, which is exactly why replay
    /// had to be fixed as well as guarded.
    fn claim_session(&self, session_id: &str, run_id: &str) -> Result<SessionClaim, ForgeError> {
        self.live_sessions.claim(session_id, run_id)
    }

    /// Refuse an explicit skill request the registry cannot satisfy.
    ///
    /// Called **first** at every entry point taking [`RunOptions`] — before
    /// the session is claimed, before any task is spawned — because the
    /// refusal must reach the caller synchronously (a transport that has
    /// already answered "202, here is your run id" has nowhere left to
    /// report it; [`claim_session`](Self::claim_session)'s doc makes the same
    /// argument for `SessionBusy`), and because a refused run must leave no
    /// claim, no broadcaster and no events behind. The caller explicitly
    /// asked for these skills, so running without one is never the right
    /// answer — the same "don't silently reinterpret" rule as an unknown
    /// slash command. Lexical discovery keeps its warn-and-continue policy
    /// inside the run: discovery is speculative, this is requested.
    fn validate_activate_skills(&self, names: &[String]) -> Result<(), ForgeError> {
        if names.is_empty() {
            return Ok(());
        }
        let available: Vec<String> = self.skills.list().into_iter().map(|m| m.name).collect();
        for name in names {
            if !available.iter().any(|a| a == name) {
                let available = if available.is_empty() {
                    "none discovered".to_string()
                } else {
                    available.join(", ")
                };
                return Err(ForgeError::skill(format!(
                    "unknown skill: {name} (available: {available})"
                )));
            }
        }
        Ok(())
    }

    /// Run a prompt through the agent loop with fresh run/session ids.
    pub async fn run(&self, prompt: &str) -> Result<RunOutcome, ForgeError> {
        self.run_with_options(prompt, RunOptions::default()).await
    }

    /// Run a prompt with explicit options.
    ///
    /// Naming a session that already has runs **continues** it: the session's
    /// conversation is replayed as the model's history and the prompt is the
    /// next turn. A fresh session (the default) starts with nothing, so
    /// `forge run` is unaffected.
    ///
    /// Naming a session that has a run *in flight* is
    /// [`ForgeError::SessionBusy`] — see
    /// [`claim_session`](Self::claim_session). An unknown name in
    /// `activate_skills` is [`ForgeError::Skill`], refused just as
    /// synchronously — see [`validate_activate_skills`](Self::validate_activate_skills).
    pub async fn run_with_options(
        &self,
        prompt: &str,
        options: RunOptions,
    ) -> Result<RunOutcome, ForgeError> {
        self.validate_activate_skills(&options.activate_skills)?;
        let run_id = options.run_id.unwrap_or_else(new_run_id);
        let session_id = options.session_id.unwrap_or_else(new_session_id);
        let claim = self.claim_session(&session_id, &run_id)?;
        let history = self.session_history(&session_id);
        self.run_tracked(
            RunPlan::new_prompt(prompt, history, options.activate_skills),
            &run_id,
            &session_id,
            options.max_turns,
            claim,
        )
        .await
    }

    /// [`run_inner`](Self::run_inner) plus the bookkeeping every run owes:
    /// whatever it returns, its tracking entries are pruned and its outcome
    /// recorded. The classification is typed — [`RunState::of_result`] reads
    /// the error's variant, never its message.
    ///
    /// `_claim` is the session claim the caller took; holding it here means
    /// the session is released exactly when the run's future ends, however it
    /// ends.
    async fn run_tracked(
        &self,
        plan: RunPlan,
        run_id: &str,
        session_id: &str,
        max_turns: Option<u32>,
        _claim: SessionClaim,
    ) -> Result<RunOutcome, ForgeError> {
        let result = self.run_inner(plan, run_id, session_id, max_turns).await;
        self.finish_run(run_id, RunState::of_result(&result));
        result
    }

    /// Start a run on a tokio task without blocking the caller. Returns
    /// the pre-generated `(run_id, session_id)` and the task handle so
    /// transports (the REST server) can return ids immediately and abort
    /// the task on cancel. Events are persisted and broadcast as usual.
    ///
    /// `Err` only for a session that already has a run in flight (see
    /// [`claim_session`](Self::claim_session)); the run's own failures arrive
    /// through the handle.
    pub fn start_run(
        self: &Arc<Self>,
        prompt: impl Into<String>,
        session_id: Option<String>,
    ) -> Result<StartedRun, ForgeError> {
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
    ///
    /// Like [`run_with_options`](Self::run_with_options), a prompt landing in
    /// a session that already has runs continues that conversation, and a
    /// prompt landing in one with a run *in flight* is refused.
    ///
    /// The session is claimed **before** the task is spawned and before any
    /// tracking entry is created, so a refusal leaves nothing behind and the
    /// caller learns about it in time to answer with a conflict rather than an
    /// accepted run that later fails. An unknown `activate_skills` name is
    /// refused even earlier — before the claim — for the same reason.
    pub fn start_run_with_options(
        self: &Arc<Self>,
        prompt: impl Into<String>,
        options: RunOptions,
    ) -> Result<StartedRun, ForgeError> {
        self.validate_activate_skills(&options.activate_skills)?;
        let run_id = options.run_id.unwrap_or_else(new_run_id);
        let session_id = options.session_id.unwrap_or_else(new_session_id);
        let claim = self.claim_session(&session_id, &run_id)?;
        // Create the broadcast channel now so subscribers connecting right
        // after the ids are handed out miss nothing.
        self.broadcaster(&run_id);
        let service = Arc::clone(self);
        let prompt = prompt.into();
        let (rid, sid) = (run_id.clone(), session_id.clone());
        let max_turns = options.max_turns;
        let activate_skills = options.activate_skills;
        let handle = tokio::spawn(async move {
            let history = service.session_history(&sid);
            service
                .run_tracked(
                    RunPlan::new_prompt(prompt, history, activate_skills),
                    &rid,
                    &sid,
                    max_turns,
                    claim,
                )
                .await
        });
        Ok(StartedRun {
            run_id,
            session_id,
            handle,
        })
    }

    /// The conversation a session already holds, replayed and fitted to the
    /// model's budget. Empty for a fresh session.
    ///
    /// This is what makes the session store the harness's memory for *every*
    /// caller, not just `forge resume`: an ACP session's second turn, a
    /// `forge_run` with a `session_id`, a `POST /v1/runs` naming a session —
    /// all of them continue the conversation they name, which is what those
    /// APIs already claim to do.
    ///
    /// A replay failure is never a run failure: a corrupt or unreadable log
    /// is logged and the run starts fresh, because losing history is worse
    /// than losing the run only if the run survives.
    fn session_history(&self, session_id: &str) -> Vec<Message> {
        let events = match self.sessions.events_for(session_id) {
            Ok(events) if !events.is_empty() => events,
            Ok(_) => return Vec::new(),
            Err(e) => {
                tracing::warn!(session = session_id, error = %e, "could not read session history; starting fresh");
                return Vec::new();
            }
        };
        let replay = crate::replay::conversation_from_events(&events);
        let budget = crate::replay::history_budget_chars(&self.model.capabilities());
        let history = crate::replay::fit_to_budget(replay.messages, budget);
        if !history.is_empty() {
            tracing::info!(
                session = session_id,
                messages = history.len(),
                degraded = replay.degraded,
                "continuing an existing session"
            );
        }
        history
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
    ///     full tool surface — the caller passes it regardless of the
    ///     resolved model's tool capability (the capability gate further
    ///     down governs only what the *model* is offered);
    ///  3. its confidence is at least `router_confidence_threshold`;
    ///  4. its arguments parse as a JSON *object* (what the dispatcher
    ///     reads arguments out of);
    ///  5. the call is *read-only*: `minimum_dispatch_risk` says
    ///     `RiskLevel::Safe`, the one classification no approval policy can
    ///     gate;
    ///  6. the guardrail `decide` answers `GUARD_SAFE`, confidently and in
    ///     time.
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
    /// This only selects a candidate; it neither emits events nor executes.
    /// The caller reserves quota and records the policy before dispatch.
    /// Failed dispatches are logged and supplied to the model for recovery;
    /// only successful dispatches complete the run without a model turn.
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
        let tools_json = serde_json::to_string(tools).ok()?;

        // The first `tool_call` in a process installs and tokenizes the tool
        // surface, and that is expensive in a way no later call is. Measured on
        // an M-series release build with forge's seven real tools
        // (crates/forge-needle/tests/fastpath_latency.rs): the cold call takes
        // ~8 s where warm calls are p50 1.1 s / p90 2.2 s. Loading the weights
        // is not the cause — that is 29 ms.
        //
        // 8 s cannot fit any budget worth giving a *fast* path, so the cold call
        // is never going to be the one that dispatches. Giving it
        // `router_timeout_ms` (5 s) meant every fresh `forge run` waited the
        // full 5 s, logged "timed out picking a tool", and called the model
        // anyway — and `forge run` is one process per invocation, so for the CLI
        // that was *every* run.
        //
        // So the first attempt gets a deliberately tiny budget instead. A
        // backend that can answer instantly still does — an already-warm engine,
        // or `HashBackend` — which matters because skipping the first attempt
        // outright would make the fast path unreachable for every one-shot run.
        // A cold `libneedle` blows the probe and we fall through in
        // milliseconds.
        //
        // Losing the probe does not waste the work: `timeout` drops our
        // receiver, but the engine thread has already picked the job up and runs
        // it to completion, which is exactly the one-time install. It finishes
        // while this turn's model call — which the turn was committed to anyway —
        // is in flight, so the next turn finds a warm engine.
        let first_attempt = !self.needle_warmed.swap(true, Ordering::SeqCst);
        let budget = if first_attempt {
            FIRST_TOOL_CALL_PROBE
        } else {
            Duration::from_millis(self.config.router_timeout_ms)
        };

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
                Err(_) if first_attempt => {
                    tracing::debug!(
                        run_id,
                        "needle fast path probe expired; the tool surface is installing in the \
                         background and later turns will use it. This turn goes to the model."
                    );
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

        Some(FastPathDispatch {
            call: tool_call,
            confidence: call.confidence,
        })
    }

    /// The turn number this run is in its session, for the decision log:
    /// 1 + the runs this session has already seen. Seeded from the store on
    /// first touch (one parse per session per process — a forked session
    /// continues from the runs its copied history holds), then kept in
    /// memory so a busy chat costs nothing per line.
    fn next_turn(&self, session_id: &str) -> u32 {
        let mut turns = self.session_turns.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(turn) = turns.get_mut(session_id) {
            *turn += 1;
            return *turn;
        }
        let seen = self
            .sessions
            .events_for(session_id)
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e.kind, EventKind::RunStarted { .. }))
                    .map(|e| e.run_id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len() as u32
            })
            .unwrap_or(0);
        let turn = seen + 1;
        turns.insert(session_id.to_string(), turn);
        turn
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
            activate_skills,
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
        let mut tool_quotas = ToolRunQuotas::new(&self.config);
        let decisions = DecisionLog::handle(&self.decision_log, &session_id);
        let turn_no = self.next_turn(&session_id);
        // Spend so far, scanned from the decision logs this session (and
        // today, across sessions) has already written — see
        // `crate::budget`. Skipped entirely when no ceiling is configured.
        let mut spend =
            SpendTracker::scan_if_budgeted(self.sessions.root(), &session_id, &self.config.budget);
        // Prices for every completion this run records, resolved once:
        // config first, cached OpenRouter catalogue second (read, never
        // fetched — the hot path sees no network), unpriced stays unpriced.
        let catalogue = forge_config::catalogue::load_for_routing(&self.config);
        let cost_book = forge_config::CostBook::new(
            self.config.model_entries(),
            catalogue.as_ref().map(|cached| &cached.catalogue),
        );

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

        // DECIDE before ROUTE: the on-device brain is offered the full
        // tool surface first, because a dispatch needs no model at all —
        // and choosing a model is only meaningful once a model is known to
        // be answering. (The old order routed first, so every dispatched
        // turn paid for a routing decision it never used, and the log
        // showed a model being "chosen" that was never called.) Needle is
        // offered the full surface regardless of the configured model's
        // tool capability: the capability gate below now governs only what
        // the *model* is offered, never what needle may do instead of it.
        //
        // Only when there is a *new* instruction to answer — a resume
        // continues a conversation with no fresh prompt, so re-running the
        // original prompt's tool would be wrong. Replayed history does not
        // disqualify it: the second turn of a session is still a new
        // instruction, and gating on history instead of on `resumed_from`
        // would silently switch the fast path off for every continuation.
        // An explicit-skill turn is excluded too: the fast path returns
        // before the skill block, so dispatching one would record no
        // activation and inject no instructions — silently dishonoring the
        // one thing the caller asked for. All other gates and the dispatch
        // itself live in `needle_fast_path`; `None` means "run normally",
        // and nothing has been emitted or executed by then.
        //
        // Whatever the fast path answers is recorded — dispatched, declined
        // and (with no engine attached) unavailable alike. A decline that
        // left no record would be invisible in exactly the decline-rate data
        // this log exists to produce.
        let decide_tools = tool_definitions();
        // `tool_definitions()` is a fixed surface, so it is always
        // non-empty and does not gate the fast path.
        let fast = if resumed_from.is_none() && activate_skills.is_empty() {
            let started = std::time::Instant::now();
            let outcome = self.needle_fast_path(prompt, &run_id, &decide_tools).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            decisions.record(
                turn_no,
                RecordDraft {
                    stage: Stage::Decide,
                    decider: if self.needle.is_some() {
                        Decider::Needle
                    } else {
                        Decider::None
                    },
                    question: "tool".to_string(),
                    choice: outcome
                        .as_ref()
                        .map(|f| f.call.name.clone())
                        .unwrap_or_else(|| "none".to_string()),
                    confidence: outcome.as_ref().map(|f| f.confidence),
                    probabilities: std::collections::BTreeMap::new(),
                    candidates: decide_tools.iter().map(|t| t.name.clone()).collect(),
                    outcome: match (&outcome, self.needle.is_some()) {
                        (Some(_), _) => Outcome::Dispatched,
                        (None, true) => Outcome::Declined,
                        (None, false) => Outcome::Unavailable,
                    },
                    elapsed_ms,
                    speculative: false,
                },
            );
            outcome
        } else {
            None
        };
        let mut failed_fast = None;
        if let Some(fast) = fast {
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
            let source = forge_context::ArtifactSource {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                call_id: fast.call.id.clone(),
                event_seq: collected.last().expect("persisted tool request").seq,
            };
            let dispatcher = ToolDispatcher::new(self.execution.clone(), self.graph.clone());
            let reservation = tool_quotas.reserve(&fast.call.name);
            let mut policy = dispatcher
                .evaluate_policy(&fast.call)
                .expect("needle only selects recognized tools");
            if reservation.is_err() {
                policy.disposition = forge_core::ToolPolicyDisposition::Block;
                policy.reason = "per-run tool quota exhausted".to_string();
            }
            self.emit(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::ToolPolicyDecision {
                        tool: fast.call.name.clone(),
                        risk: policy.risk,
                        approval_policy: policy.approval_policy,
                        disposition: policy.disposition,
                        reason: policy.reason,
                        policy_schema: TOOL_POLICY_SCHEMA_VERSION,
                        forge_version: env!("CARGO_PKG_VERSION").to_string(),
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
            let outcome = match ToolRunQuotas::dispatch(&dispatcher, &fast.call, reservation).await
            {
                Ok(outcome) => outcome,
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
                        name: fast.call.name.clone(),
                        success: !outcome.result.is_error,
                    },
                ),
            )?;
            let (text, artifact, compression) =
                self.prepare_tool_output(&fast.call, &outcome.result.content, source);
            for kind in artifact.into_iter().chain(compression) {
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(&run_id, &session_id, kind),
                )?;
            }
            let stored = self.emit_stored(
                &sender,
                &mut collected,
                Event::new(
                    &run_id,
                    &session_id,
                    EventKind::ToolResult {
                        call_id: fast.call.id.clone(),
                        tool: fast.call.name.clone(),
                        output: text.clone(),
                        is_error: outcome.result.is_error,
                    },
                ),
            )?;
            let EventKind::ToolResult { output: text, .. } = stored.kind else {
                unreachable!("session append preserves event kind");
            };
            tool_call_count += 1;
            if outcome.result.is_error {
                failed_fast = Some((fast.call, text));
            } else {
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
                    tool_calls: tool_call_count,
                    events: collected,
                });
            }
        }

        // ROUTE: only reached when needle declined (or is unavailable) and
        // a model will answer — which is what makes choosing one
        // meaningful.
        // Candidates: the [models] table plus the configured model — unless
        // the model was pinned by an explicit user action (`--model`,
        // `FORGE_MODEL`, the chat's `/model`): then the candidate set is
        // exactly that model. An explicit choice is a constraint, not one
        // more suggestion the router may improve on — a user who said
        // `--model gpt-5` must never be "rerouted" onto a different
        // provider (least of all one that then errors).
        let candidates: Vec<String> = if self.config.model_pinned {
            vec![self.config.model.clone()]
        } else {
            let mut candidates: Vec<String> = self.config.model_entries().keys().cloned().collect();
            if !candidates.contains(&self.config.model) {
                candidates.push(self.config.model.clone());
            }
            candidates.sort();
            candidates
        };
        let routing_request = RoutingRequest {
            task: task.to_string(),
            required_capabilities: Vec::new(),
            candidates,
        };
        let route_started = std::time::Instant::now();
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
        decisions.record(
            turn_no,
            RecordDraft {
                stage: Stage::Route,
                // Map from the router that actually answered, not from
                // `fallback_used`: a primary `static` router is not needle
                // having fallen back, and conflating them would corrupt the
                // decline rate this log exists to measure.
                decider: match decision.router_name.as_str() {
                    "needle" | "needle-dispatch" => Decider::Needle,
                    "static" | "cheapest" | "mock" => Decider::Static,
                    _ => Decider::Llm,
                },
                question: "model".to_string(),
                choice: decision.selected_model.clone(),
                confidence: Some(decision.confidence),
                probabilities: std::collections::BTreeMap::new(),
                candidates: routing_request.candidates.clone(),
                outcome: Outcome::Routed,
                elapsed_ms: route_started.elapsed().as_millis() as u64,
                speculative: false,
            },
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
        let mut messages = self.system_context.clone();
        let system_end = messages.len();
        // Explicitly requested skills activate first, in caller order with
        // duplicates collapsed — ahead of, never instead of, lexical
        // discovery. The caller asked for these by name, so a failed
        // activation fails the run (entry validation has already refused
        // unknown names; this surfaces a skill deleted between the two
        // resolutions). They are not budgeted against the lexical match cap.
        let mut activated: Vec<String> = Vec::new();
        for name in &activate_skills {
            if activated.iter().any(|n| n == name) {
                continue;
            }
            match self.activate_skill(&sender, &mut collected, &run_id, &session_id, name) {
                Ok(message) => {
                    messages.push(message);
                    activated.push(name.clone());
                }
                Err(e) => return Err(fail(&mut collected, e)),
            }
        }
        for meta in self.skills.match_task(task) {
            if activated.iter().any(|n| n == &meta.name) {
                continue;
            }
            match self.activate_skill(&sender, &mut collected, &run_id, &session_id, &meta.name) {
                Ok(message) => {
                    messages.push(message);
                    activated.push(meta.name.clone());
                }
                // Discovery is speculative, so it keeps its
                // warn-and-continue policy — the two failure policies
                // differ because the two contracts differ.
                Err(e) => {
                    tracing::warn!(skill = %meta.name, error = %e, "skill activation failed")
                }
            }
        }
        let skills_end = messages.len();
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
        let graph_end = messages.len();
        messages.extend(history);
        let prompt_index = messages.len();
        messages.push(Message::user(prompt));
        let context_boundaries = ContextBoundaries {
            system_end,
            skills_end,
            graph_end,
            prompt_index,
        };
        if let Some((call, output)) = failed_fast {
            messages.push(Message::assistant_tool_calls(vec![call.clone()]));
            messages.push(Message::tool(call.id, output));
        }

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

        // The capability gate: what the *model* is offered. Needle was
        // offered the full surface above regardless of this — a chat-only
        // model receives zero tools, exactly as before the reorder.
        let mut tools = if model.capabilities().tools {
            decide_tools
        } else {
            Vec::new()
        };
        if model.capabilities().tools && self.artifact_store.is_some() {
            tools.push(crate::tools::artifact_tool_definition());
        }

        if tools.is_empty() {
            // Single-turn path: providers without tool support behave
            // exactly as a plain completion.
            match self
                .gate_budget(
                    &mut RunGate {
                        sender: &sender,
                        collected: &mut collected,
                        run_id: &run_id,
                        session_id: &session_id,
                        input_rx: &mut input_rx,
                        token: &token,
                    },
                    &spend,
                )
                .await
            {
                Ok(()) => {}
                // Cancellation: the Cancelled event was already recorded.
                Err(e @ ForgeError::Cancelled(_)) => return Err(e),
                Err(e) => return Err(fail(&mut collected, e)),
            }
            let request = CompletionRequest::new(decision.selected_model.clone(), messages);
            self.record_context_plan(
                &sender,
                &mut collected,
                &run_id,
                &session_id,
                1,
                &request,
                context_boundaries,
                model.capabilities().max_context,
            )?;
            let complete_started = std::time::Instant::now();
            let response = match self
                .complete_streaming(
                    &sender,
                    &mut collected,
                    &run_id,
                    &session_id,
                    &model,
                    request,
                )
                .await
            {
                Ok(response) => response,
                Err(e) => return Err(fail(&mut collected, e)),
            };
            self.account_completion(
                &decisions,
                &mut spend,
                turn_no,
                &decision.selected_model,
                &response,
                complete_started.elapsed().as_millis() as u64,
                &cost_book,
            );
            self.emit_assistant_message(&sender, &mut collected, &run_id, &session_id, &response)?;
            let summary: String = response.content.chars().take(80).collect();
            // Same accounting as the loop's final iteration: one model
            // round-trip is one turn, and the footer counts these events.
            self.emit(
                &sender,
                &mut collected,
                Event::new(&run_id, &session_id, EventKind::TurnCompleted { turn: 1 }),
            )?;
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

            match self
                .gate_budget(
                    &mut RunGate {
                        sender: &sender,
                        collected: &mut collected,
                        run_id: &run_id,
                        session_id: &session_id,
                        input_rx: &mut input_rx,
                        token: &token,
                    },
                    &spend,
                )
                .await
            {
                Ok(()) => {}
                // Cancellation: the Cancelled event was already recorded.
                Err(e @ ForgeError::Cancelled(_)) => return Err(e),
                Err(e) => return Err(fail(&mut collected, e)),
            }

            let request = CompletionRequest::new(selected.clone(), messages.clone())
                .with_tools(tools.clone());
            self.record_context_plan(
                &sender,
                &mut collected,
                &run_id,
                &session_id,
                turn,
                &request,
                context_boundaries,
                model.capabilities().max_context,
            )?;
            let complete_started = std::time::Instant::now();
            let response = match self
                .complete_streaming(
                    &sender,
                    &mut collected,
                    &run_id,
                    &session_id,
                    &model,
                    request,
                )
                .await
            {
                Ok(response) => response,
                Err(e) => return Err(fail(&mut collected, e)),
            };
            self.account_completion(
                &decisions,
                &mut spend,
                turn_no,
                &selected,
                &response,
                complete_started.elapsed().as_millis() as u64,
                &cost_book,
            );

            self.emit_assistant_message(&sender, &mut collected, &run_id, &session_id, &response)?;

            if response.tool_calls.is_empty() {
                let summary: String = response.content.chars().take(80).collect();
                // The final answer is a model round-trip like any other:
                // the turn count (and the footer counting these events)
                // includes it. Dispatch-path runs return before the loop
                // and correctly emit none.
                self.emit(
                    &sender,
                    &mut collected,
                    Event::new(&run_id, &session_id, EventKind::TurnCompleted { turn }),
                )?;
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
                let source = forge_context::ArtifactSource {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    call_id: call.id.clone(),
                    event_seq: collected.last().expect("persisted tool request").seq,
                };
                let reservation = tool_quotas.reserve(&call.name);
                if let Some(mut policy) = dispatcher.evaluate_policy(call) {
                    if reservation.is_err() {
                        policy.disposition = forge_core::ToolPolicyDisposition::Block;
                        policy.reason = "per-run tool quota exhausted".to_string();
                    }
                    self.emit(
                        &sender,
                        &mut collected,
                        Event::new(
                            &run_id,
                            &session_id,
                            EventKind::ToolPolicyDecision {
                                tool: call.name.clone(),
                                risk: policy.risk,
                                approval_policy: policy.approval_policy,
                                disposition: policy.disposition,
                                reason: policy.reason,
                                policy_schema: TOOL_POLICY_SCHEMA_VERSION,
                                forge_version: env!("CARGO_PKG_VERSION").to_string(),
                            },
                        ),
                    )?;
                }
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

                let dispatched = if call.name == "retrieve_tool_output" && reservation.is_ok() {
                    Ok(self.retrieve_tool_output(&session_id, call))
                } else {
                    ToolRunQuotas::dispatch(&dispatcher, call, reservation).await
                };
                let outcome = match dispatched {
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
                let (model_output, artifact, compression) =
                    self.prepare_tool_output(call, &outcome.result.content, source);
                for kind in artifact.into_iter().chain(compression) {
                    self.emit(
                        &sender,
                        &mut collected,
                        Event::new(&run_id, &session_id, kind),
                    )?;
                }
                let stored = self.emit_stored(
                    &sender,
                    &mut collected,
                    Event::new(
                        &run_id,
                        &session_id,
                        EventKind::ToolResult {
                            call_id: call.id.clone(),
                            tool: call.name.clone(),
                            output: model_output.clone(),
                            is_error: outcome.result.is_error,
                        },
                    ),
                )?;
                let EventKind::ToolResult { output, .. } = stored.kind else {
                    unreachable!("session append preserves event kind");
                };
                // Preserve the provider's protocol identifier so it matches the
                // assistant call above; only the output comes from persistence.
                messages.push(Message::tool(call.id.clone(), output));
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

        // In-process + cross-process cancellation signals. The token is
        // fired only if one exists: a run with none has no loop here to
        // interrupt, and the marker file below is what reaches the process
        // that does — creating a token for it would leave a map entry
        // nothing prunes.
        if let Some(token) = self.existing_cancel_token(run_or_session_id) {
            token.cancel();
        }
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
        // Claimed before anything is read, so a resume of a session that is
        // already running is refused rather than started alongside it.
        let resumed_run = new_run_id();
        let claim = self.claim_session(&session_id, &resumed_run)?;
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
                // A resume takes no options: discovery re-runs on the
                // original task, exactly as the run being continued did.
                activate_skills: Vec::new(),
            },
            &resumed_run,
            &session_id,
            None,
            claim,
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
    /// Snapping is to the *anchored* run's boundary, which is not the same as
    /// "the prefix ends at a boundary for every run in it": if two runs of
    /// one session were in flight at once, their events interleave, and
    /// cutting after the anchor's last event can still land mid-way through
    /// the other one. The fork is then honest but partial — replay groups the
    /// prefix by run and repairs the truncated run's unanswered tool calls the
    /// same way it repairs an aborted one (see [`crate::replay`]).
    /// [`claim_session`](Self::claim_session) stops this process producing
    /// such a log at all; a log written by two forge processes at once, or one
    /// written before that guard existed, can still contain it.
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
