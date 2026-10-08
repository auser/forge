//! In-process test doubles for [`crate::app`]: no terminal, no process.
//!
//! [`ScriptedIo`] replaces the terminal with a queue of lines and a
//! captured output buffer, plus an interrupt that a test schedules rather
//! than types; [`FakeHost`] wraps a real [`AgentService`] built from the
//! scripted mock model and either [`MockExecution`] (approvals never park)
//! or [`NativeExecution`] parked (approvals always park), so the whole
//! driver in `app.rs` runs end to end with no TTY and no network. This is
//! the same shape `forge-runtime`'s own `service/tests.rs` uses to build a
//! service — see its `scripted_service` helper — reused here because the
//! chat needs a real `AgentService`, not a second mock of one.
//!
//! Providers are constructed directly, which the `FORGE_TEST_MOCKS` gate
//! does not restrict: that gate is about what *configuration* may select,
//! not what a test may build in Rust.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge_config::Config;
use forge_core::{
    ApprovalPolicy, CompletionRequest, CompletionResponse, ExecutionProvider, ForgeError,
    ModelCapabilities, ModelProvider, Skill, SkillMeta, SkillRegistry, ToolCall,
};
use forge_execution::{ApprovalChannel, MockExecution, NativeExecution};
use forge_providers::{ScriptedMockModel, ScriptedReply, StaticRouter};
use forge_runtime::{AgentService, NullSkillRegistry};
use forge_session::JsonlSessionStore;
use tempfile::TempDir;
use tokio::sync::Notify;

use crate::host::{
    ChatHost, ConfigLine, ContextLine, Environment, HostChange, ModelChoice, NeedleState,
    SkillChoice,
};
use crate::io::{ChatIo, CompletionSnapshot, Interactivity, Line, Prompt, ReadOutcome};

// --- ScriptedIo ----------------------------------------------------------

/// What an interrupt is scheduled on. Checked synchronously, from inside
/// [`ScriptedIoHandle::read`]/`write`/`notify` themselves — the only way a
/// test can fire it "during" whatever the driver is doing without a second
/// thread or a sleep.
enum Trigger {
    None,
    /// Fires the moment the first read resolves with a line.
    AfterFirstPrompt,
    /// Fires the moment the captured output contains this substring.
    OutputContains(String),
}

struct Shared {
    lines: Mutex<VecDeque<String>>,
    output: Mutex<String>,
    interactivity: Interactivity,
    reads_done: AtomicUsize,
    /// Reads that resolved [`ReadOutcome::Eof`] — the count that makes a
    /// batch-mode busy-wait visible to an assertion. See
    /// [`ScriptedIo::eof_reads`].
    eof_reads: AtomicUsize,
    /// The completions of the most recent `Prompt` handed to `read` — what
    /// the editor would have completed against. See
    /// [`ScriptedIo::last_completions`].
    last_completions: Mutex<CompletionSnapshot>,
    trigger: Mutex<Trigger>,
    fired: AtomicBool,
    notify: Notify,
    /// Woken by [`ScriptedIo::push_line`], so an interactive read that
    /// found the queue empty can pick a later-pushed line up instead of
    /// pending for ever.
    pushed: Notify,
}

/// A queue of input lines and a captured output buffer standing in for a
/// terminal. `new` builds an [`Interactivity::Interactive`] queue (an
/// exhausted queue then pends forever, exactly like a real prompt waiting
/// for the next keystroke); [`ScriptedIo::batch`] builds
/// [`Interactivity::Batch`] instead, where an exhausted queue is EOF.
pub struct ScriptedIo {
    shared: Arc<Shared>,
}

impl ScriptedIo {
    pub fn new<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::build(lines, Interactivity::Interactive)
    }

    /// The queue-draining case (§12.3): EOF once the queue is empty rather
    /// than pending forever.
    pub fn batch<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::build(lines, Interactivity::Batch)
    }

    fn build<I, S>(lines: I, interactivity: Interactivity) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            shared: Arc::new(Shared {
                lines: Mutex::new(lines.into_iter().map(Into::into).collect()),
                output: Mutex::new(String::new()),
                interactivity,
                reads_done: AtomicUsize::new(0),
                eof_reads: AtomicUsize::new(0),
                last_completions: Mutex::new(CompletionSnapshot::default()),
                trigger: Mutex::new(Trigger::None),
                fired: AtomicBool::new(false),
                notify: Notify::new(),
                pushed: Notify::new(),
            }),
        }
    }

    /// Schedule the interrupt to fire as soon as the first line has been
    /// read (i.e. right after the first turn is submitted).
    pub fn interrupt_after_first_prompt(&mut self) {
        *self
            .shared
            .trigger
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Trigger::AfterFirstPrompt;
    }

    /// Schedule the interrupt to fire as soon as a written or notified
    /// line makes the captured output contain `needle` — used to interrupt
    /// mid-turn at a point defined by what the transcript says, not by a
    /// line count.
    pub fn interrupt_when_output_contains(&mut self, needle: impl Into<String>) {
        *self
            .shared
            .trigger
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Trigger::OutputContains(needle.into());
    }

    /// A handle sharing this queue and buffer, to hand to [`crate::app::run`]
    /// while keeping this value around to inspect afterwards.
    pub fn handle(&mut self) -> ScriptedIoHandle {
        ScriptedIoHandle(Arc::clone(&self.shared))
    }

    /// Append a line to the queue *after* the chat has started, so a test
    /// can type in reaction to the transcript rather than only up front.
    ///
    /// A fixed script cannot express "and then, once the background job has
    /// reported in, press Ctrl-D": every scripted line is popped within
    /// microseconds of the last, long before anything with real timing in
    /// it has happened. Pushing wakes an interactive read that already
    /// found the queue empty, so the line is picked up even if the chat got
    /// there first.
    pub fn push_line(&mut self, line: impl Into<String>) {
        self.shared
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(line.into());
        self.shared.pushed.notify_one();
    }

    /// How many reads resolved [`ReadOutcome::Eof`].
    ///
    /// A batch-mode driver that keeps an exhausted reader in its `select!`
    /// gets EOF back instantly on every iteration and spins for the whole
    /// length of the turn; this is how a test asserts that it does not,
    /// without measuring CPU. The transcript is identical either way, so
    /// nothing else here can catch that regression.
    pub fn eof_reads(&self) -> usize {
        self.shared.eof_reads.load(Ordering::SeqCst)
    }

    /// The transcript captured so far.
    pub fn output(&self) -> String {
        self.shared
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The completion snapshot of the last `Prompt` `read` was handed —
    /// how a test asserts what the host's data reached the editor without
    /// a terminal in the way.
    pub fn last_completions(&self) -> CompletionSnapshot {
        self.shared
            .last_completions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The [`ChatIo`] implementation itself, separate from [`ScriptedIo`] so the
/// latter survives being handed to [`crate::app::run`] (which takes `ChatIo`
/// by value) and can still be inspected afterwards.
pub struct ScriptedIoHandle(Arc<Shared>);

impl ScriptedIoHandle {
    fn pop_line(&self) -> Option<String> {
        self.0
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// One accepted line's bookkeeping, shared by the queued and the
    /// pushed path so a [`Trigger::AfterFirstPrompt`] cannot depend on
    /// which of the two delivered the first line.
    fn on_line_read(&self, line: String) -> ReadOutcome {
        let n = self.0.reads_done.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            let armed = matches!(
                &*self.0.trigger.lock().unwrap_or_else(|e| e.into_inner()),
                Trigger::AfterFirstPrompt
            );
            if armed {
                self.fire();
            }
        }
        ReadOutcome::Line(line)
    }

    fn record(&self, text: &str, terminated: bool) {
        {
            let mut output = self.0.output.lock().unwrap_or_else(|e| e.into_inner());
            output.push_str(text);
            // A fragment appends to the line in flight; everything else
            // terminates its line — exactly what a terminal's scrollback
            // would show, which is what every app-level assertion reads.
            if terminated {
                output.push('\n');
            }
        }
        self.maybe_fire_on_output();
    }

    fn maybe_fire_on_output(&self) {
        if self.0.fired.load(Ordering::SeqCst) {
            return;
        }
        let trigger = self.0.trigger.lock().unwrap_or_else(|e| e.into_inner());
        if let Trigger::OutputContains(needle) = &*trigger {
            let matched = self
                .0
                .output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(needle.as_str());
            if matched {
                drop(trigger);
                self.fire();
            }
        }
    }

    fn fire(&self) {
        if !self.0.fired.swap(true, Ordering::SeqCst) {
            self.0.notify.notify_one();
        }
    }
}

#[async_trait]
impl ChatIo for ScriptedIoHandle {
    async fn read(&mut self, prompt: Prompt) -> ReadOutcome {
        // Capture what the editor would have seen before answering: the
        // snapshot travels inside the prompt precisely so the completer
        // never calls back into the host, and a test asserting on wiring
        // needs the same view.
        *self
            .0
            .last_completions
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = prompt.completions;
        // A genuine yield, even when a line is already queued: without it
        // a queue with several lines ready at once would be drained in one
        // synchronous burst, never giving the run's own spawned task a
        // chance to run at all (see app.rs's module doc for why the main
        // loop needs the same courtesy on the way out).
        tokio::task::yield_now().await;
        loop {
            if let Some(line) = self.pop_line() {
                return self.on_line_read(line);
            }
            match self.0.interactivity {
                Interactivity::Batch => {
                    self.0.eof_reads.fetch_add(1, Ordering::SeqCst);
                    return ReadOutcome::Eof;
                }
                // A real prompt with nothing typed yet: waits until
                // something is typed, which for a test is
                // [`ScriptedIo::push_line`] and otherwise is never.
                Interactivity::Interactive => self.0.pushed.notified().await,
            }
        }
    }

    fn write(&mut self, line: &Line) {
        self.record(&line.text, !line.fragment);
    }

    fn notify(&mut self, line: &Line) {
        self.record(&line.text, !line.fragment);
    }

    async fn interrupted(&mut self) {
        self.0.notify.notified().await;
    }

    fn interactivity(&self) -> Interactivity {
        self.0.interactivity
    }

    fn shutdown(&mut self) {}
}

// --- FakeHost --------------------------------------------------------------

/// Which [`ExecutionProvider`] a [`FakeHost`] is built over.
enum Execution {
    /// Never asks for approval: every op just runs.
    Mock,
    /// Never asks for approval, and `read_file` returns the given content —
    /// the fixture for `/show`, which needs a known payload.
    MockReading(String),
    /// Approval policy `prompt`, channel [`ApprovalChannel::Parked`]: a
    /// risky op always parks with `ForgeError::ApprovalRequired`, exactly
    /// what the chat's own runtime is built with (design §8.1).
    Writing,
}

/// A [`ChatHost`] wrapping a real [`AgentService`] built from mock
/// providers, plus fixed data for the rest of the trait. `Clone` because
/// [`resuming_a_session_rerenders_its_transcript_through_one_renderer`]
/// (in `app.rs`'s test module) runs two chats over one runtime.
#[derive(Clone)]
pub struct FakeHost {
    service: Arc<AgentService>,
    environment: Environment,
    models: Vec<ModelChoice>,
    skills: Vec<SkillChoice>,
    /// The scripted model the service was built with, when it was one —
    /// kept so a test can assert on what the model was actually sent.
    scripted_model: Option<Arc<ScriptedMockModel>>,
    /// What `project_files` returns — the `@`-completion source, empty by
    /// default (an unbuilt graph degrades to silence).
    paths: Vec<String>,
}

/// The one-skill registry behind [`FakeHost::with_skill_and_script`]: the
/// name is two characters, so `SkillRegistry::match_task` (which tokenizes
/// words of >= 3) can never produce it — any activation of `xy` came
/// through the explicit path.
struct StubSkills;

impl SkillRegistry for StubSkills {
    fn list(&self) -> Vec<SkillMeta> {
        vec![SkillMeta {
            name: "xy".to_string(),
            description: "the two-letter test skill".to_string(),
            path: PathBuf::from("skills/xy/SKILL.md"),
        }]
    }

    fn activate(&self, name: &str) -> Result<Skill, ForgeError> {
        self.list()
            .into_iter()
            .find(|meta| meta.name == name)
            .map(|meta| Skill {
                instructions: "xy INSTRUCTIONS".to_string(),
                meta,
            })
            .ok_or_else(|| ForgeError::skill(format!("unknown skill: {name}")))
    }
}

impl FakeHost {
    /// A service whose model is a [`ScriptedMockModel`] parsed from `json`
    /// (see [`ScriptedMockModel::from_json`] for the shape) and whose
    /// execution never asks for approval.
    pub fn with_script(json: &str) -> (Self, TempDir) {
        let model = Arc::new(ScriptedMockModel::from_json(json).expect("valid script"));
        let recorded = Arc::clone(&model);
        let (mut host, tmp) = Self::build(Execution::Mock, move |_root| model);
        host.scripted_model = Some(recorded);
        (host, tmp)
    }

    /// A service like [`with_script`](Self::with_script), but whose skill
    /// registry offers `xy` — a skill no prompt can match lexically — and
    /// whose snapshot lists it, so `/xy` parses as a skill command.
    pub fn with_skill_and_script(json: &str) -> (Self, TempDir) {
        let model = Arc::new(ScriptedMockModel::from_json(json).expect("valid script"));
        let recorded = Arc::clone(&model);
        let (mut host, tmp) = Self::build_with_skills(
            Execution::Mock,
            move |_root| model,
            Arc::new(StubSkills),
            vec![SkillChoice {
                name: "xy".to_string(),
                description: "the two-letter test skill".to_string(),
            }],
        );
        host.scripted_model = Some(recorded);
        (host, tmp)
    }

    /// A scripted model over a `MockExecution` whose `read_file` returns
    /// `content` — the fixture for `/show`, which needs a known payload.
    /// (`MockExecution::new` serves `Some("")` for reads, which would
    /// exercise only the `(no output)` branch.)
    pub fn with_script_and_read_content(json: &str, content: &str) -> (Self, TempDir) {
        Self::build(Execution::MockReading(content.to_string()), |_root| {
            Arc::new(ScriptedMockModel::from_json(json).expect("valid script"))
        })
    }

    /// A two-reply script whose model sleeps before every reply — long
    /// enough that `/bg`, `/fork` and an interrupt can be typed while the
    /// first turn is still attached. The sleep races a poll of the run's own
    /// cancel marker file (the same one `AgentService::cancel` writes and
    /// `cancel_requested` reads), so a cancelled turn does not have to wait
    /// out the full delay: without that, `service.cancel` recording the
    /// cancellation is not enough to make the *model call* return promptly,
    /// and a driver-side interrupt would still take the full delay to be
    /// reflected in the transcript.
    pub fn with_slow_script() -> (Self, TempDir) {
        Self::build(Execution::Mock, |root| {
            let inner = ScriptedMockModel::new(vec![
                ScriptedReply {
                    text: Some("slow turn done".to_string()),
                    tool_calls: Vec::new(),
                },
                ScriptedReply {
                    text: Some("second slow turn done".to_string()),
                    tool_calls: Vec::new(),
                },
            ]);
            Arc::new(SlowModel {
                inner,
                delay: Duration::from_millis(200),
                runs_dir: root.join(".forge").join("runs"),
            })
        })
    }

    /// A scripted model parsed from `json` whose stream *trickles*: each
    /// chunk lands `chunk_delay` after the last, so "the answer arrives
    /// during the turn" is observable from a test with no TTY
    /// (`with_slow_script`'s `SlowModel` does not override
    /// `stream_complete`, so its turns emit no deltas at all; TICKET-1 D2).
    pub fn with_trickled_script(json: &str, chunk_delay: Duration) -> (Self, TempDir) {
        Self::build(Execution::Mock, move |_root| {
            Arc::new(TrickleModel {
                inner: ScriptedMockModel::from_json(json).expect("valid script"),
                chunk_delay,
            })
        })
    }

    /// A model whose every call fails, as a bad endpoint would.
    pub fn with_unreachable_model() -> (Self, TempDir) {
        Self::build(Execution::Mock, |_root| Arc::new(UnreachableModel))
    }

    /// A model that asks to write `notes.txt`, over a runtime whose
    /// approval always parks (§8.1) — the fixture for the approval
    /// round-trip tests.
    pub fn writing_project() -> (Self, TempDir) {
        Self::build(Execution::Writing, |_root| {
            Arc::new(ScriptedMockModel::new(vec![ScriptedReply {
                text: None,
                tool_calls: vec![ToolCall::new(
                    "call_1",
                    "write_file",
                    serde_json::json!({"path": "notes.txt", "content": "some notes"}),
                )],
            }]))
        })
    }

    /// The runtime this host wraps, for assertions on session/run state
    /// after a chat ends.
    pub fn service(&self) -> Arc<AgentService> {
        Arc::clone(&self.service)
    }

    /// The scripted model the service was built with, when it was one —
    /// for assertions on what the model was actually sent.
    pub fn scripted_model(&self) -> Option<Arc<ScriptedMockModel>> {
        self.scripted_model.clone()
    }

    /// Set the project file list `project_files` reports (the
    /// `@`-completion source), builder-style.
    pub fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.paths = paths;
        self
    }

    fn build(
        execution: Execution,
        model: impl FnOnce(&std::path::Path) -> Arc<dyn ModelProvider>,
    ) -> (Self, TempDir) {
        Self::build_with_skills(execution, model, Arc::new(NullSkillRegistry), Vec::new())
    }

    fn build_with_skills(
        execution: Execution,
        model: impl FnOnce(&std::path::Path) -> Arc<dyn ModelProvider>,
        skill_registry: Arc<dyn SkillRegistry>,
        skills: Vec<SkillChoice>,
    ) -> (Self, TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root: PathBuf = tmp.path().to_path_buf();
        let model = model(&root);
        let execution: Arc<dyn ExecutionProvider> = match execution {
            Execution::Mock => Arc::new(MockExecution::new(&root)),
            Execution::MockReading(content) => {
                Arc::new(MockExecution::new(&root).with_read_content(content))
            }
            Execution::Writing => Arc::new(NativeExecution::with_channel(
                ApprovalPolicy::Prompt,
                &root,
                ApprovalChannel::Parked,
            )),
        };
        let router = Arc::new(StaticRouter::new("scripted-mock"));
        let sessions = Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions")));
        let service = Arc::new(AgentService::new(
            model,
            router,
            execution,
            skill_registry,
            sessions,
            Config::default(),
        ));
        let environment = Environment {
            project_root: root,
            model: "scripted-mock".to_string(),
            router: "static".to_string(),
            approval: "prompt".to_string(),
            needle: NeedleState::Inactive {
                reason: "no engine in tests".to_string(),
            },
        };
        let host = Self {
            service,
            environment,
            models: vec![ModelChoice {
                name: "scripted-mock".to_string(),
                description: "the scripted test model".to_string(),
                active: true,
            }],
            skills,
            scripted_model: None,
            paths: Vec::new(),
        };
        (host, tmp)
    }
}

#[async_trait]
impl ChatHost for FakeHost {
    fn service(&self) -> Arc<AgentService> {
        Arc::clone(&self.service)
    }

    async fn switch(&mut self, _change: HostChange) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn authenticate(&mut self, _provider: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    fn environment(&self) -> Environment {
        self.environment.clone()
    }

    fn models(&self) -> Vec<ModelChoice> {
        self.models.clone()
    }

    fn skills(&self) -> Vec<SkillChoice> {
        self.skills.clone()
    }

    fn project_files(&self) -> Vec<String> {
        self.paths.clone()
    }

    fn config_summary(&self, _key: Option<&str>) -> Vec<ConfigLine> {
        Vec::new()
    }

    async fn graph_context(
        &self,
        _query: &str,
        _steering: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<ContextLine>, ForgeError> {
        Ok(Vec::new())
    }
}

// --- test-only model providers ---------------------------------------------

/// Delays every reply by `delay`, then defers to `inner` — a real sleep, not
/// a hung future, so it always resolves and never needs a second script.
struct SlowModel {
    inner: ScriptedMockModel,
    delay: Duration,
    /// `AgentService`'s own `<root>/.forge/runs` directory: where
    /// `cancel()` writes a `<run-id>.cancel` marker. Polled rather than
    /// slept through blindly, so a cancelled run's model call returns
    /// promptly instead of sitting out the whole delay — these tests never
    /// run two cancellable turns at once, so "any marker" is as good as
    /// matching the run id.
    runs_dir: PathBuf,
}

#[async_trait]
impl ModelProvider for SlowModel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        let deadline = tokio::time::Instant::now() + self.delay;
        let poll = Duration::from_millis(10);
        while tokio::time::Instant::now() < deadline {
            if self.cancel_marker_exists() {
                break;
            }
            tokio::time::sleep(poll).await;
        }
        self.inner.complete(request).await
    }
}

impl SlowModel {
    fn cancel_marker_exists(&self) -> bool {
        std::fs::read_dir(&self.runs_dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    }
}

/// Streams its scripted reply one chunk at a time with a real delay
/// between chunks — the fixture that makes "the answer arrives during the
/// turn" observable from a test (`with_slow_script`'s `SlowModel` does not
/// override `stream_complete`, so its turns emit no deltas at all; TICKET-1
/// D2). The scripted mock answers synchronously, so the chunks are
/// collected first and replayed with sleeps — the staggering is what a
/// test observes, not the collection.
///
/// Deliberately does *not* poll the cancel marker (that is `SlowModel`'s
/// trick for a different job): the tests built on this fixture never
/// interrupt a trickle.
struct TrickleModel {
    inner: ScriptedMockModel,
    chunk_delay: Duration,
}

#[async_trait]
impl ModelProvider for TrickleModel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        self.inner.complete(request).await
    }

    async fn stream_complete(
        &self,
        request: CompletionRequest,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<CompletionResponse, ForgeError> {
        let mut chunks = Vec::new();
        let response = self
            .inner
            .stream_complete(request, &mut |d: &str| chunks.push(d.to_string()))
            .await?;
        for chunk in chunks {
            tokio::time::sleep(self.chunk_delay).await;
            on_delta(&chunk);
        }
        Ok(response)
    }
}

/// Fails every call, as an unreachable endpoint would — no network involved.
struct UnreachableModel;

#[async_trait]
impl ModelProvider for UnreachableModel {
    fn name(&self) -> &str {
        "unreachable"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: false,
            tools: true,
            structured_output: false,
            vision: false,
            max_context: 8192,
        }
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ForgeError> {
        Err(ForgeError::provider("model endpoint unreachable"))
    }
}
