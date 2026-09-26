//! The async driver: `ChatIo` x `ChatHost` x `AgentService`, tied together.
//!
//! Everything else in this crate is a pure function; this is the one module
//! that actually runs anything, by executing the [`Action`]s the
//! [`Controller`] returns against a real `AgentService`. The turn driver is
//! copied from `forge-acp::server::run_turn` (subscribe before starting,
//! drain `try_recv` after settling, print the answer once) with two arms
//! ACP's driver does not need: an outstanding `io.read` (so `/bg` is
//! reachable mid-turn, §6.2) and `io.interrupted()`.
//!
//! **On `read` and `interrupted` racing.** Both are declared `&mut self` on
//! `ChatIo`, so they cannot be two live branches of the *same*
//! `tokio::select!` — two mutable borrows of one `io` would not compile (this
//! was verified against the compiler, not assumed). The fix that keeps the
//! design's intent (an interrupt must be noticed whether it lands on a typed
//! line or between them) without holding two borrows at once: `read` is a
//! real arm of the main `select!`, and right after *every* arm resolves —
//! read, an event, the run settling, a background notice — [`App::interrupted_now`]
//! takes a momentary, non-blocking look at `io.interrupted()` on its own,
//! `io` fully free again by then because the main `select!` already dropped
//! every non-winning branch. `ScriptedIo`'s two interrupt triggers both fire
//! synchronously from inside `read`/`write`/`notify` themselves, so this
//! catches them at exactly the point they are armed for. What it cannot do —
//! and what a genuine concurrent race could — is notice a `SIGINT` while
//! `read` is blocked for a long stretch with nothing else happening at all;
//! closing that gap needs either `read` itself to report an interrupt (as it
//! already does on a TTY, `ReadOutcome::Interrupt`) or a future `ChatIo`
//! change that splits the two responsibilities apart. Piped mode's SIGINT
//! path (`forge-cli`'s job, §12.3) should keep this in mind.
//!
//! **On ordering.** The main `select!` is `biased`, events and the settling
//! handle listed before `read`. Without that, a line already queued (which
//! resolves the instant it is popped) could win a fair race against an
//! event that is merely a scheduler tick away — which is how `y` sent to
//! answer an approval could be popped and misread as a new prompt before
//! the run had even asked the question. Biasing toward the run's own
//! progress, plus a `tokio::task::yield_now()` on the way out of every
//! iteration (so a just-spawned run task gets a turn on a `current_thread`
//! runtime), is what makes the interleaving deterministic instead of a race
//! the test suite would only sometimes lose.

use std::time::Duration;

use forge_core::{Event, EventKind, ForgeError};
use forge_runtime::{Attachment, RunOptions, RunOutcome};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::controller::{Action, Controller, Signal};
use crate::host::{ChatHost, HostChange, NeedleState};
use crate::io::{ChatIo, CompletionSnapshot, Line, Prompt, ReadOutcome};
use crate::render::TranscriptState;

/// How the chat resolves the session it starts in (design §7).
pub enum SessionStart {
    /// A brand new session: no history, no silent resume.
    Fresh,
    /// The project's most recently active session.
    Continue,
    /// A session named explicitly (`--session <id>`, or a driver resuming
    /// one after `/session <id>`).
    Named(String),
}

/// Everything [`run`] needs to begin: where the conversation starts, and an
/// optional first turn to submit before the interactive loop takes over
/// (`forge chat "fix the build"`).
pub struct Start {
    pub session: SessionStart,
    pub first_prompt: Option<String>,
}

impl Start {
    pub fn fresh() -> Self {
        Self {
            session: SessionStart::Fresh,
            first_prompt: None,
        }
    }

    pub fn continue_latest() -> Self {
        Self {
            session: SessionStart::Continue,
            first_prompt: None,
        }
    }

    pub fn named(id: impl Into<String>) -> Self {
        Self {
            session: SessionStart::Named(id.into()),
            first_prompt: None,
        }
    }
}

/// The attached run's event source: a live subscription for a run this
/// driver started, or an attachment's backlog-then-live stream for one
/// joined via `/attach`. Different origins, one interface, so the main loop
/// does not need to know which kind it is driving.
enum RunEvents {
    Live(broadcast::Receiver<Event>),
    Attached(Attachment),
}

/// One notice from a background watcher (design §10.1): the two out-of-band
/// kinds it may print, and the bookkeeping signal that lets the driver
/// decrement the detached count when the job settles.
enum BgMsg {
    Notice(Line),
    Settled(String),
}

/// The driver's whole state: what is attached, what is detached, and the
/// two seams it drives. No conversation state of its own — the session log
/// is the memory (§5) — just the ids of where the conversation is right now.
struct App<Io, Host> {
    io: Io,
    host: Host,
    controller: Controller,
    snapshot: CompletionSnapshot,
    session_id: String,
    run_id: Option<String>,
    events: Option<RunEvents>,
    handle: Option<JoinHandle<Result<RunOutcome, ForgeError>>>,
    transcript: Option<TranscriptState>,
    /// Run ids detached via `/bg`, so `Action::CancelAllJobs` has something
    /// to cancel — the controller counts them but does not name them.
    background: Vec<String>,
    bg_tx: mpsc::UnboundedSender<BgMsg>,
    bg_rx: mpsc::UnboundedReceiver<BgMsg>,
    exit_code: Option<i32>,
}

/// Resolve `start` into a session, print the banner, then loop until the
/// chat exits.
pub async fn run(io: impl ChatIo, host: impl ChatHost, start: Start) -> Result<i32, ForgeError> {
    App::new(io, host).start(start).await
}

impl<Io: ChatIo, Host: ChatHost> App<Io, Host> {
    fn new(io: Io, host: Host) -> Self {
        let interactivity = io.interactivity();
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let mut app = Self {
            io,
            host,
            controller: Controller::new(CompletionSnapshot::default(), interactivity),
            snapshot: CompletionSnapshot::default(),
            session_id: String::new(),
            run_id: None,
            events: None,
            handle: None,
            transcript: None,
            background: Vec::new(),
            bg_tx,
            bg_rx,
            exit_code: None,
        };
        app.refresh_completions();
        app
    }

    // --- startup ------------------------------------------------------

    async fn start(&mut self, start: Start) -> Result<i32, ForgeError> {
        match start.session {
            SessionStart::Fresh => self.session_id = forge_session::new_session_id(),
            SessionStart::Continue => match self.latest_session() {
                Some(id) => {
                    self.session_id = id.clone();
                    self.render_session_transcript(&id, true);
                }
                None => self.session_id = forge_session::new_session_id(),
            },
            SessionStart::Named(id) => {
                self.session_id = id.clone();
                self.render_session_transcript(&id, true);
            }
        }
        self.refresh_completions();
        self.print_banner();
        if let Some(prompt) = start.first_prompt {
            let actions = self.controller.on_line(&prompt);
            self.execute(actions).await;
        }
        self.drive().await
    }

    /// The project's most recently active session: sessions are ULIDs, so
    /// the lexicographically greatest id is also the newest one.
    fn latest_session(&self) -> Option<String> {
        self.host
            .service()
            .sessions()
            .list_sessions()
            .ok()?
            .into_iter()
            .map(|info| info.session_id)
            .max()
    }

    fn print_banner(&mut self) {
        let env = self.host.environment();
        let needle = match &env.needle {
            NeedleState::Active { model_id } => format!("brain active ({model_id})"),
            NeedleState::Inactive { reason } => format!("brain off ({reason})"),
        };
        self.io.write(&Line::plain(format!(
            "forge  {}",
            env.project_root.display()
        )));
        self.io.write(&Line::plain(format!(
            "model {}  router {}  approval {}  {needle}",
            env.model, env.router, env.approval
        )));
        self.io.write(&Line::plain(format!(
            "session {}  /help for commands",
            self.session_id
        )));
    }

    // --- the main loop --------------------------------------------------

    /// One `io.read` is always outstanding, including while a turn runs
    /// (§6.2): the four arms below are `read`, the attached run's events,
    /// its settling, and a detached job's notice. `biased` and the
    /// ordering are load-bearing — see the module doc.
    async fn drive(&mut self) -> Result<i32, ForgeError> {
        loop {
            let prompt = self.prompt();
            tokio::select! {
                biased;
                event = recv_events(self.events.as_mut()), if self.events.is_some() => {
                    match event {
                        Some(event) => self.apply_event(event),
                        // The stream ended on its own with no `JoinHandle`
                        // to tell us so — reached only via `/attach`, since
                        // an owned run's authoritative settlement is the
                        // `handle` arm above instead, and that arm alone
                        // clears `self.events` for that case. Without this,
                        // `self.events` (and the controller's `attached`)
                        // would stay set forever once a followed run
                        // finished, and the chat would never accept another
                        // turn again (Review Focus 4).
                        None if self.handle.is_none() => self.settle_attached_run().await,
                        // An owned run's channel closing early (a `Closed`
                        // broadcast receiver resolves `None` on *every*
                        // subsequent poll, not just once) — clear `events`
                        // so this arm's guard goes false and stops winning
                        // this `biased` race on every iteration; without
                        // that, the still-pending `handle` arm below would
                        // never get polled again at all, an instant, silent,
                        // CPU-bound spin rather than a visible hang. Its
                        // resolution remains the authoritative settlement.
                        None => self.events = None,
                    }
                }
                joined = join_handle(self.handle.as_mut()), if self.handle.is_some() => {
                    self.on_joined(joined).await;
                }
                Some(msg) = self.bg_rx.recv() => {
                    self.on_bg(msg);
                }
                outcome = self.io.read(prompt) => {
                    self.on_read(outcome).await;
                }
            }
            if self.interrupted_now().await {
                self.handle_signal(Signal::Interrupt).await;
            }
            if let Some(code) = self.exit_code {
                self.io.shutdown();
                return Ok(code);
            }
            // Give a just-spawned or just-unblocked run task a turn before
            // asking `io` for the next line again — see the module doc.
            tokio::task::yield_now().await;
        }
    }

    /// A momentary, non-blocking look at whether the user has interrupted:
    /// `interrupted()` races against an immediately-ready fallback, so this
    /// never itself waits. Safe to call here because by this point the main
    /// `select!` has already dropped every branch that did not win,
    /// `io` included — see the module doc for why this cannot instead be a
    /// direct arm of that `select!`.
    async fn interrupted_now(&mut self) -> bool {
        tokio::select! {
            biased;
            () = self.io.interrupted() => true,
            () = std::future::ready(()) => false,
        }
    }

    fn prompt(&self) -> Prompt {
        Prompt {
            text: "> ".to_string(),
            completions: self.snapshot.clone(),
            history: Vec::new(),
        }
    }

    /// Rebuild the Tab-completion snapshot: discovered skills, user-visible
    /// models, and — the two calls that matter here — `list_runs()` and
    /// `sessions().list_sessions()`, both synchronous filesystem reads, the
    /// former re-reading and re-parsing every session's whole JSONL log.
    /// Called once at startup and once after each *submitted line*
    /// (`on_read`'s `Line` arm), not on every loop iteration: completions
    /// are only ever read back out when a `Prompt` is built for the next
    /// `io.read`, so refreshing on every event a running turn emits would
    /// be doing this filesystem work, scaling with session count and log
    /// size, once per event instead of once per prompt.
    fn refresh_completions(&mut self) {
        let skills = self.host.skills().into_iter().map(|s| s.name).collect();
        let models = self.host.models().into_iter().map(|m| m.name).collect();
        let jobs = self
            .host
            .service()
            .list_runs()
            .map(|runs| runs.into_iter().map(|r| r.run_id).collect())
            .unwrap_or_default();
        let sessions = self
            .host
            .service()
            .sessions()
            .list_sessions()
            .map(|infos| infos.into_iter().map(|s| s.session_id).collect())
            .unwrap_or_default();
        self.snapshot = CompletionSnapshot {
            skills,
            models,
            jobs,
            sessions,
        };
        self.controller.set_completions(self.snapshot.clone());
    }

    // --- dispatch --------------------------------------------------------

    async fn on_read(&mut self, outcome: ReadOutcome) {
        match outcome {
            ReadOutcome::Line(line) => {
                let actions = self.controller.on_line(&line);
                self.execute(actions).await;
                // Refresh here, not in the main loop, and not for every
                // event a running turn emits — see `refresh_completions`'s
                // doc for why that would be the wrong frequency.
                self.refresh_completions();
            }
            ReadOutcome::Interrupt => self.handle_signal(Signal::Interrupt).await,
            ReadOutcome::Eof => self.handle_signal(Signal::Eof).await,
            ReadOutcome::Failed(message) => self.emit(Line::bad(format!("error: {message}"))),
        }
    }

    async fn handle_signal(&mut self, signal: Signal) {
        let actions = self.controller.on_signal(signal);
        self.execute(actions).await;
    }

    fn on_bg(&mut self, msg: BgMsg) {
        match msg {
            // Out-of-band by construction (design §10.1): always `notify`,
            // never gated on whether a turn happens to be attached.
            BgMsg::Notice(line) => self.io.notify(&line),
            BgMsg::Settled(run_id) => {
                self.background.retain(|id| id != &run_id);
                self.controller.on_job_settled();
            }
        }
    }

    fn apply_event(&mut self, event: Event) {
        if let EventKind::ApprovalRequested { command, .. } = &event.kind {
            self.controller.on_approval_requested(command);
        }
        let lines = match self.transcript.as_mut() {
            Some(transcript) => transcript.on_event(&event),
            None => Vec::new(),
        };
        for line in lines {
            self.emit(line);
        }
    }

    /// Drain whatever was already queued when the run settled, so the last
    /// tool's result line precedes the answer (mirrors
    /// `forge-acp::server::run_turn`'s own flush).
    fn drain_events(&mut self) {
        let Some(RunEvents::Live(rx)) = self.events.as_mut() else {
            return;
        };
        let mut drained = Vec::new();
        while let Ok(event) = rx.try_recv() {
            drained.push(event);
        }
        for event in drained {
            self.apply_event(event);
        }
    }

    async fn on_joined(
        &mut self,
        joined: Result<Result<RunOutcome, ForgeError>, tokio::task::JoinError>,
    ) {
        self.finish_run(joined).await;
    }

    /// Cancel and *wait* for the attached run to settle (bounded), rather
    /// than firing `service.cancel` and letting the main loop notice the
    /// handle resolving on some later iteration.
    ///
    /// Why this cannot be "fire and forget": `service.cancel` records the
    /// cancellation, but the spawned task still has to run to actually stop
    /// — and while it is doing that, this driver's own read stays
    /// outstanding too. Without blocking here, a line typed right after the
    /// interrupt (as every one of these tests does) can be popped and acted
    /// on *before* the cancelled run has released its session claim: a bare
    /// prompt would queue behind a turn the controller still thinks is
    /// attached, and a lone `/quit` would only get the single "job still
    /// running" warning (§6.3's one-confirmation rule) with nothing left in
    /// the script to supply the second request that actually leaves — a
    /// hang, not a bug in the controller, which the driver must not invite.
    /// Two seconds is the same grace design §5 gives a cancel before giving
    /// up on it and returning to the prompt anyway.
    async fn settle_cancelled_run(&mut self) {
        let Some(mut handle) = self.handle.take() else {
            return;
        };
        match tokio::time::timeout(Duration::from_secs(2), &mut handle).await {
            Ok(joined) => self.finish_run(joined).await,
            Err(_) => {
                self.emit(Line::bad("cancelled (the turn is still unwinding)"));
                self.run_id = None;
                self.events = None;
                self.transcript = None;
                self.controller.on_run_settled();
                if let Some(next) = self.controller.take_queued() {
                    self.start_turn(next).await;
                }
            }
        }
    }

    async fn finish_run(
        &mut self,
        joined: Result<Result<RunOutcome, ForgeError>, tokio::task::JoinError>,
    ) {
        self.drain_events();
        match joined {
            Ok(Ok(outcome)) => {
                // §4.3: print the outcome's text only if nothing textual
                // was rendered live — the fast path's shape, and the rule
                // that keeps an ordinary turn's answer from printing twice.
                let rendered = self
                    .transcript
                    .as_ref()
                    .map(TranscriptState::rendered_assistant_text)
                    .unwrap_or(false);
                if !rendered && !outcome.text.trim().is_empty() {
                    self.emit(Line::plain(String::new()));
                    for line in outcome.text.lines() {
                        self.emit(Line::plain(line));
                    }
                    self.emit(Line::plain(String::new()));
                }
            }
            // A `ForgeError` here already has its own event (`Error` or
            // `Cancelled`, appended by the run itself), already rendered
            // above — printing it again would be a second, redundant line.
            Ok(Err(_)) => {}
            // The task panicked or was aborted: nothing in the log records
            // this, so it is the one case that needs its own line.
            Err(e) => self.emit(Line::bad(format!("error: run task did not finish: {e}"))),
        }
        if let Some(transcript) = self.transcript.take() {
            self.emit(transcript.footer());
        }
        self.run_id = None;
        self.events = None;
        self.handle = None;
        self.controller.on_run_settled();
        if let Some(next) = self.controller.take_queued() {
            self.start_turn(next).await;
        }
    }

    /// A followed run (`/attach`) finished, or stopped being followable, on
    /// its own — no `JoinHandle` involved, so `finish_run` is not the path
    /// here. Same tail as `finish_run`: footer, clear state, tell the
    /// controller, run whatever was queued. There is no `RunOutcome` to
    /// fall back to (this driver never started the run), but that is fine —
    /// whatever text the run had, this driver already rendered live, event
    /// by event, while attached.
    async fn settle_attached_run(&mut self) {
        if let Some(transcript) = self.transcript.take() {
            self.emit(transcript.footer());
        }
        self.run_id = None;
        self.events = None;
        self.controller.on_run_settled();
        if let Some(next) = self.controller.take_queued() {
            self.start_turn(next).await;
        }
    }

    /// A transcript line: `notify` while a turn is attached (it may land
    /// while a prompt is up), `write` between turns. Both take the same
    /// [`Line`], so the transcript is identical either way (§6.2).
    fn emit(&mut self, line: Line) {
        if self.events.is_some() {
            self.io.notify(&line);
        } else {
            self.io.write(&line);
        }
    }

    // --- actions ----------------------------------------------------------

    async fn execute(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Prompt(text) => self.start_turn(text).await,
                Action::Approve(approved) => {
                    if let Some(run_id) = self.run_id.clone() {
                        let message = if approved { "y" } else { "n" };
                        // Unlike `CancelRun`/`CancelAllJobs`, a failure here
                        // is not something the process is leaving anyway or
                        // already-recorded elsewhere: if the run settled in
                        // the narrow window before the answer arrived, the
                        // user's approve/deny would otherwise vanish with
                        // no feedback at all.
                        if let Err(e) = self.host.service().send_input(&run_id, message) {
                            self.emit(Line::bad(format!("error: {e}")));
                        }
                    }
                }
                Action::CancelRun => {
                    if let Some(run_id) = self.run_id.clone() {
                        let _ = self.host.service().cancel(&run_id);
                        self.settle_cancelled_run().await;
                    }
                }
                Action::Background => self.do_background(),
                Action::Host(change) => self.do_host_change(change).await,
                Action::Write(line) => self.emit(line),
                Action::Help => self.do_help(),
                Action::ListModels => self.do_list_models(),
                Action::ShowApproval => self.do_show_approval(),
                Action::ShowConfig(key) => self.do_show_config(key),
                Action::ListSkills => self.do_list_skills(),
                Action::Graph(query) => self.do_graph(query),
                Action::ShowSession => self.do_show_session(),
                Action::NewSession => self.do_new_session(),
                Action::SwitchSession(id) => self.do_switch_session(id),
                Action::Fork(at) => self.do_fork(at),
                Action::ListJobs => self.do_list_jobs(),
                Action::Attach(run_id) => self.do_attach(run_id),
                Action::CancelAllJobs => self.do_cancel_all_jobs(),
                Action::Quit(code) => self.exit_code = Some(code),
            }
        }
    }

    /// The turn driver (design §5), copied from `forge-acp::server::run_turn`:
    /// subscribe *before* starting, so an approval from a fast first tool
    /// call cannot be emitted with nobody listening.
    ///
    /// Yields once after a successful spawn, on purpose: `service.cancel`
    /// resolves a run id by looking it up in the session store, which has
    /// nothing to find until the new task has written its first event.
    /// Without this yield, an interrupt arriving in the same instant the
    /// turn starts (as `ScriptedIo::interrupt_after_first_prompt` deliberately
    /// does, and as a real double-tap easily could) would ask the runtime to
    /// cancel a run it does not know exists yet — silently, since a fallible
    /// cancel here is intentionally best-effort — and the turn would run to
    /// completion uncancelled.
    async fn start_turn(&mut self, prompt: String) {
        let run_id = forge_session::new_run_id();
        let events = self.host.service().subscribe(&run_id);
        let started = self.host.service().start_run_with_options(
            prompt,
            RunOptions {
                run_id: Some(run_id.clone()),
                session_id: Some(self.session_id.clone()),
                ..RunOptions::default()
            },
        );
        match started {
            Ok(started) => {
                self.run_id = Some(run_id);
                self.events = Some(RunEvents::Live(events));
                self.handle = Some(started.handle);
                self.transcript = Some(TranscriptState::new());
                tokio::task::yield_now().await;
            }
            Err(e) => {
                // No concurrency guard here on purpose (design §5, §10.1.1):
                // `AgentService` owns the one-live-run-per-session refusal,
                // and if it ever surfaces it is rendered as a normal failed
                // turn, same as any other typed error.
                self.emit(Line::bad(format!("error: {e}")));
                self.controller.on_run_settled();
            }
        }
    }

    /// `/bg` (design §10.1.1): fork the conversation for the foreground and
    /// leave the detached run in the source session, watched by a task that
    /// emits only the two notice kinds of §10.1.
    fn do_background(&mut self) {
        let Some(run_id) = self.run_id.take() else {
            return;
        };
        let events = self.events.take();
        self.handle = None;
        self.transcript = None;
        let source = self.session_id.clone();
        let note = match self.host.service().fork_session(&source, None) {
            Ok(outcome) => {
                self.session_id = outcome.session_id.clone();
                format!(
                    "this conversation continues in fork {} (the job keeps writing to {source})",
                    outcome.session_id
                )
            }
            // Nothing to copy yet (the backgrounded turn had not written a
            // single event): start fresh rather than fail the detach.
            Err(_) => {
                let fresh = forge_session::new_session_id();
                self.session_id = fresh.clone();
                format!(
                    "this conversation continues in a fresh session {fresh} (nothing to fork yet)"
                )
            }
        };
        self.emit(Line::meta(format!(
            "detached run {run_id} - /jobs to list, /attach {run_id} to follow"
        )));
        self.emit(Line::meta(note));
        self.background.push(run_id.clone());
        if let Some(RunEvents::Live(rx)) = events {
            self.spawn_background_watcher(run_id, rx);
        }
    }

    fn spawn_background_watcher(&self, run_id: String, mut rx: broadcast::Receiver<Event>) {
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // One definition of "this ends a run" — sharing it
                        // is what keeps a watcher for a job in a background
                        // job that never settles if a new terminal kind is
                        // added and only one of the two call sites is
                        // updated for it.
                        let terminal = event.kind.is_terminal();
                        let notice = match &event.kind {
                            EventKind::ApprovalRequested { .. } => {
                                Some(format!("job {run_id} needs approval - /attach {run_id}"))
                            }
                            EventKind::Completed { .. } => Some(format!("job {run_id} completed")),
                            EventKind::Error { .. } => Some(format!("job {run_id} failed")),
                            EventKind::Cancelled { .. } => Some(format!("job {run_id} cancelled")),
                            _ => None,
                        };
                        if let Some(text) = notice {
                            let _ = tx.send(BgMsg::Notice(Line::notice(text)));
                        }
                        if terminal {
                            let _ = tx.send(BgMsg::Settled(run_id.clone()));
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// Cancel every run that would die with this process (design §10.2):
    /// the attached one, if any, plus every detached job. Fire-and-forget —
    /// `service.cancel` already records the cancellation synchronously, and
    /// the process is leaving either way.
    fn do_cancel_all_jobs(&mut self) {
        if let Some(run_id) = self.run_id.take() {
            let _ = self.host.service().cancel(&run_id);
        }
        for run_id in self.background.drain(..) {
            let _ = self.host.service().cancel(&run_id);
        }
        self.events = None;
        self.handle = None;
        self.transcript = None;
    }

    fn do_list_jobs(&mut self) {
        match self.host.service().list_runs() {
            Ok(runs) if runs.is_empty() => self.emit(Line::meta("no runs")),
            Ok(runs) => {
                for run in runs {
                    let marker = if self.run_id.as_deref() == Some(run.run_id.as_str()) {
                        " (this conversation)"
                    } else {
                        ""
                    };
                    self.emit(Line::meta(format!(
                        "{} {}{marker}",
                        run.run_id,
                        run.state.as_str()
                    )));
                }
            }
            Err(e) => self.emit(Line::bad(format!("error: {e}"))),
        }
    }

    /// `/fork` (design §11): continue in the fork. With `--at`, the fork is
    /// shorter than the screen, so its tail is re-rendered; with no `--at`
    /// the fork's history is exactly what is already on screen.
    fn do_fork(&mut self, at: Option<String>) {
        let source = self.session_id.clone();
        match self.host.service().fork_session(&source, at.as_deref()) {
            Ok(outcome) => {
                self.session_id = outcome.session_id.clone();
                self.emit(Line::meta(format!(
                    "forked to session {} ({} events copied)",
                    outcome.session_id, outcome.events_copied
                )));
                if at.is_some() {
                    self.render_session_transcript(&outcome.session_id.clone(), false);
                } else {
                    self.emit(Line::meta(format!(
                        "this conversation continues in the fork; {source} is untouched"
                    )));
                }
            }
            Err(e) => self.emit(Line::bad(format!("error: {e}"))),
        }
    }

    /// `/attach <run-id>` (design §10.2): render the backlog, then stream
    /// live while the run is still live in this process; otherwise say so
    /// and return.
    fn do_attach(&mut self, run_id: String) {
        match self.host.service().attach(&run_id) {
            Ok(attachment) => {
                if let Some(session) = attachment.session_id.clone() {
                    self.session_id = session;
                }
                let mut transcript = TranscriptState::new();
                for event in &attachment.backlog {
                    for line in transcript.on_event(event) {
                        self.emit(line);
                    }
                }
                if attachment.is_live() {
                    self.run_id = Some(run_id);
                    self.transcript = Some(transcript);
                    self.events = Some(RunEvents::Attached(attachment));
                    self.handle = None;
                } else {
                    self.emit(Line::meta(format!(
                        "run {run_id} belongs to another forge process; showing its recorded history only"
                    )));
                    self.controller.on_run_settled();
                }
            }
            Err(e) => {
                self.emit(Line::bad(format!("error: {e}")));
                self.controller.on_run_settled();
            }
        }
    }

    fn do_switch_session(&mut self, id: String) {
        self.session_id = id.clone();
        self.render_session_transcript(&id, true);
    }

    fn do_new_session(&mut self) {
        self.session_id = forge_session::new_session_id();
        self.emit(Line::meta(format!("new session {}", self.session_id)));
    }

    fn do_show_session(&mut self) {
        let runs = self
            .host
            .service()
            .sessions()
            .events_for(&self.session_id)
            .map(|events| {
                events
                    .iter()
                    .map(|e| e.run_id.as_str())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            })
            .unwrap_or(0);
        let root = self.host.environment().project_root;
        self.emit(Line::meta(format!(
            "session {} - {runs} runs - {}",
            self.session_id,
            root.display()
        )));
    }

    fn do_help(&mut self) {
        for line in crate::command::help_lines() {
            self.emit(line);
        }
        for skill in self.host.skills() {
            self.emit(Line::meta(format!("/{} {}", skill.name, skill.description)));
        }
    }

    fn do_list_models(&mut self) {
        for model in self.host.models() {
            let marker = if model.active { " (active)" } else { "" };
            self.emit(Line::meta(format!(
                "{}{marker} - {}",
                model.name, model.description
            )));
        }
    }

    fn do_show_approval(&mut self) {
        let approval = self.host.environment().approval;
        self.emit(Line::meta(format!("approval {approval}")));
    }

    fn do_show_config(&mut self, key: Option<String>) {
        for line in self.host.config_summary(key.as_deref()) {
            self.emit(Line::meta(format!(
                "{} = {} ({})",
                line.key, line.value, line.origin
            )));
        }
    }

    fn do_list_skills(&mut self) {
        for skill in self.host.skills() {
            self.emit(Line::meta(format!(
                "{} - {}",
                skill.name, skill.description
            )));
        }
    }

    fn do_graph(&mut self, query: String) {
        match self.host.graph_context(&query, 10) {
            Ok(hits) if hits.is_empty() => self.emit(Line::meta("no matches")),
            Ok(hits) => {
                for hit in hits {
                    self.emit(Line::meta(format!("{} ({})", hit.path, hit.score)));
                }
            }
            Err(e) => self.emit(Line::bad(format!("error: {e}"))),
        }
    }

    async fn do_host_change(&mut self, change: HostChange) {
        match self.host.switch(change).await {
            Ok(()) => {
                let env = self.host.environment();
                self.emit(Line::meta(format!(
                    "model {} approval {}",
                    env.model, env.approval
                )));
            }
            Err(e) => self.emit(Line::bad(format!("error: {e}"))),
        }
    }

    /// Re-render a session's transcript through the one renderer (§7):
    /// `announce` prints the `resumed session` header first (skipped for a
    /// fork with no `--at`, whose history is already on screen). Bounded at
    /// 200 lines, with a note for anything older.
    fn render_session_transcript(&mut self, session_id: &str, announce: bool) {
        let events = self
            .host
            .service()
            .sessions()
            .events_for(session_id)
            .unwrap_or_default();
        if announce {
            let runs = events
                .iter()
                .map(|e| e.run_id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len();
            self.emit(Line::meta(format!(
                "resumed session {session_id} ({runs} runs)"
            )));
        }
        let mut transcript = TranscriptState::new();
        let mut lines = Vec::new();
        for event in &events {
            lines.extend(transcript.on_event(event));
        }
        const BOUND: usize = 200;
        if lines.len() > BOUND {
            let hidden = lines.len() - BOUND;
            self.emit(Line::meta(format!(
                "... {hidden} earlier lines (forge session show {session_id})"
            )));
            let tail = lines.split_off(lines.len() - BOUND);
            for line in tail {
                self.emit(line);
            }
        } else {
            for line in lines {
                self.emit(line);
            }
        }
    }
}

/// The attached run's next event, or `None` once no more can arrive.
/// `Lagged` is a warning, not a failure: the final text does not come from
/// this stream, so a gap here degrades the transcript, never the answer.
async fn recv_events(events: Option<&mut RunEvents>) -> Option<Event> {
    match events {
        Some(RunEvents::Live(rx)) => loop {
            match rx.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "chat event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
        Some(RunEvents::Attached(attachment)) => attachment.recv().await,
        None => std::future::pending().await,
    }
}

/// Await the attached run's task, if this driver owns one (it does not for
/// a run joined via `/attach`).
async fn join_handle(
    handle: Option<&mut JoinHandle<Result<RunOutcome, ForgeError>>>,
) -> Result<Result<RunOutcome, ForgeError>, tokio::task::JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeHost, ScriptedIo};

    /// One turn: the transcript shows the routing line, the tool call, and
    /// the answer exactly once.
    #[tokio::test]
    async fn a_turn_renders_and_answers_once() {
        let (host, _tmp) = FakeHost::with_script(
            r#"[
            {"tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "alpha.rs"}}]},
            {"text": "alpha.rs defines parse_config"}
        ]"#,
        );
        let mut io = ScriptedIo::new(["explain alpha.rs", "/quit"]);
        let code = run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        assert_eq!(code, 0);
        let out = io.output();
        assert!(out.contains("  * read_file alpha.rs"), "{out}");
        assert_eq!(
            out.matches("alpha.rs defines parse_config").count(),
            1,
            "the answer is printed exactly once:\n{out}"
        );
        // `TurnCompleted` is emitted only for a turn that dispatched a tool
        // call (see `forge-runtime`'s own
        // `scripted_two_turn_run_writes_file_and_emits_full_trail`, which
        // pins the same two-model-call script at one `turn_completed`
        // event) — the transcript's footer counts those events, not model
        // calls, so this two-call script reports one turn, not two.
        assert!(
            out.contains("  = 1 turn"),
            "the footer reports the turns:\n{out}"
        );
    }

    /// The fast path has no assistant text, so the outcome is the answer —
    /// and still only once.
    #[tokio::test]
    async fn a_turn_with_no_assistant_text_falls_back_to_the_run_outcome() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": ""}]"#);
        let mut io = ScriptedIo::new(["say nothing", "/quit"]);
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        // Nothing textual rendered and nothing to fall back to: no blank
        // block, no panic, and the loop kept going.
        assert!(io.output().contains("  = "), "{}", io.output());
    }

    /// Review Focus 1, in the driver: cancel the run, survive, stay usable.
    #[tokio::test]
    async fn an_interrupt_mid_turn_cancels_the_run_and_the_chat_continues() {
        let (host, _tmp) = FakeHost::with_slow_script();
        let mut io = ScriptedIo::new(["something slow", "still here", "/quit"]);
        io.interrupt_after_first_prompt();
        let code = run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        assert_eq!(code, 0, "an interrupt must not change the exit code");
        let out = io.output();
        assert!(out.contains("  ! cancelled"), "{out}");
        assert!(
            out.contains("still here") || out.contains("  = "),
            "the chat accepted another turn afterwards:\n{out}"
        );
    }

    /// Review Focus 3 + §8: the parked mechanism, answered from the chat.
    #[tokio::test]
    async fn an_approval_is_asked_in_the_transcript_and_answered_from_the_prompt() {
        let (host, tmp) = FakeHost::writing_project(); // parked approvals, `prompt`
        let mut io = ScriptedIo::new(["write the notes", "y", "/quit"]);
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        let out = io.output();
        assert!(out.contains("  ! approval needed:"), "{out}");
        assert!(out.contains("    -> approved"), "{out}");
        assert!(
            tmp.path().join("notes.txt").exists(),
            "approved work must happen"
        );
    }

    #[tokio::test]
    async fn a_denied_approval_leaves_the_file_alone_and_the_turn_continues() {
        let (host, tmp) = FakeHost::writing_project();
        let mut io = ScriptedIo::new(["write the notes", "n", "/quit"]);
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        assert!(io.output().contains("    -> denied"), "{}", io.output());
        assert!(
            !tmp.path().join("notes.txt").exists(),
            "denied work must not happen"
        );
    }

    /// Review Focus 2: interrupting the approval must not run the operation.
    #[tokio::test]
    async fn an_interrupt_during_an_approval_cancels_without_running_it() {
        let (host, tmp) = FakeHost::writing_project();
        let mut io = ScriptedIo::new(["write the notes", "/quit"]);
        // The approval question is a transcript line, not a prompt, so the
        // interrupt is scheduled on the output rather than on a prompt.
        io.interrupt_when_output_contains("approval needed");
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        assert!(io.output().contains("  ! cancelled"), "{}", io.output());
        assert!(
            !tmp.path().join("notes.txt").exists(),
            "the pending operation must not have run"
        );
    }

    /// Review Focus 7: a turn that fails is a line, not the end.
    #[tokio::test]
    async fn a_failing_turn_prints_an_error_and_keeps_the_chat_alive() {
        let (host, _tmp) = FakeHost::with_unreachable_model();
        let mut io = ScriptedIo::new(["anything", "/help", "/quit"]);
        let code = run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        assert_eq!(code, 0);
        let out = io.output();
        assert!(out.contains("  ! error:"), "{out}");
        assert!(
            out.contains("/model"),
            "/help still worked afterwards:\n{out}"
        );
    }

    /// One session across turns, which is what makes history real (§7).
    #[tokio::test]
    async fn every_turn_lands_in_one_session() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": "ok"}]"#);
        let service = host.service();
        let mut io = ScriptedIo::new(["first", "second", "/quit"]);
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        let sessions = service.sessions().list_sessions().expect("sessions");
        assert_eq!(sessions.len(), 1, "two turns, one session: {sessions:?}");
    }

    /// `/bg` detaches *and* moves the conversation to a fork: two runs in
    /// one session log replay interleaved, which silently replays a
    /// successful tool call as unanswered and invites the model to retry a
    /// side effect (spec §10.1.1). The runtime enforces the invariant; this
    /// test pins the UI behaviour that keeps the chat clear of it.
    #[tokio::test]
    async fn bg_detaches_and_continues_in_a_fork() {
        let (host, _tmp) = FakeHost::with_slow_script();
        let service = host.service();
        let mut io = ScriptedIo::new(["long job", "/bg", "/jobs", "/quit", "/quit"]);
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        let out = io.output();
        assert!(out.contains("  - detached run"), "{out}");
        assert!(
            out.contains("continues in fork"),
            "the fork is announced:\n{out}"
        );
        assert!(out.contains("running"), "/jobs shows its state:\n{out}");
        assert!(out.contains("job still running"), "quit asks once:\n{out}");
        assert_eq!(
            service.sessions().list_sessions().expect("sessions").len(),
            2,
            "the job keeps its session; the conversation moved to a fork"
        );
    }

    /// The same rule from the other side: a turn is never started in a
    /// session that already has a live run.
    #[tokio::test]
    async fn fork_is_refused_while_a_turn_is_attached_and_points_at_bg() {
        let (host, _tmp) = FakeHost::with_slow_script();
        let mut io = ScriptedIo::new(["long job", "/fork", "/quit", "/quit"]);
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        let out = io.output();
        assert!(
            out.contains("/bg"),
            "the refusal names the way forward:\n{out}"
        );
        assert!(
            !out.contains("  - forked to session"),
            "no fork happened:\n{out}"
        );
    }

    #[tokio::test]
    async fn fork_continues_in_the_new_session_and_leaves_the_source_alone() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": "ok"}]"#);
        let service = host.service();
        let mut io = ScriptedIo::new(["first", "/fork", "second", "/quit"]);
        run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        let out = io.output();
        assert!(out.contains("  - forked to session"), "{out}");
        assert!(out.contains("is untouched"), "{out}");
        let sessions = service.sessions().list_sessions().expect("sessions");
        assert_eq!(
            sessions.len(),
            2,
            "the fork is a second session: {sessions:?}"
        );
    }

    #[tokio::test]
    async fn resuming_a_session_rerenders_its_transcript_through_one_renderer() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": "the first answer"}]"#);
        let service = host.service();
        let mut first = ScriptedIo::new(["first", "/quit"]);
        run(first.handle(), host.clone(), Start::fresh())
            .await
            .expect("first chat");
        let session = service.sessions().list_sessions().expect("sessions")[0]
            .session_id
            .clone();

        let mut second = ScriptedIo::new(["/quit"]);
        run(second.handle(), host, Start::named(&session))
            .await
            .expect("second chat");
        let out = second.output();
        assert!(out.contains("  - resumed session"), "{out}");
        assert!(
            out.contains("the first answer"),
            "history is re-rendered:\n{out}"
        );
    }

    /// Review Focus 4: `/attach` follows a run that finishes on its own —
    /// no `Ctrl-C`, no `/bg`, nothing the driver did. Without
    /// `App::settle_attached_run`, `self.events` (and the controller's
    /// `attached`) would stay set forever once the followed run's stream
    /// closed, and every turn typed afterwards would queue behind a run
    /// that will never report as finished. The run is started directly
    /// through the service, bypassing the chat, so it is genuinely live
    /// when `/attach` reaches it and finishes while this driver is
    /// streaming it — not already terminal by the time `/attach` runs.
    #[tokio::test]
    async fn an_attached_run_that_finishes_on_its_own_frees_the_chat_to_keep_going() {
        let (host, _tmp) = FakeHost::with_slow_script();
        let service = host.service();
        let started = service
            .start_run_with_options("do the thing", RunOptions::default())
            .expect("start");
        let run_id = started.run_id.clone();
        // Batch mode, deliberately, rather than typing `/quit` after: a
        // fixed script's lines are all read within microseconds of each
        // other, real turns take real time, so a scripted `/quit` cannot be
        // relied on to land after the attach settles — the "ask once, then
        // abandon" rule (§10.2) would either warn and then have nothing left
        // to supply the second request, or (with a second `/quit`) cancel
        // the still-live run before it got a chance to finish on its own,
        // testing a cancellation instead of a natural completion. Batch
        // mode's EOF is re-signalled on every otherwise-empty read (§12.3),
        // so `on_drained` keeps getting a chance to notice the attach has
        // settled and the queued second turn has run, and *then* exit —
        // deterministically, with no race against the run's own timing.
        let mut io = ScriptedIo::batch([format!("/attach {run_id}"), "second turn".to_string()]);
        let code = run(io.handle(), host, Start::fresh())
            .await
            .expect("chat runs");
        assert_eq!(code, 0, "the chat must not hang once the attach settles");
        let out = io.output();
        assert!(
            out.contains("slow turn done"),
            "the attached run's own answer streamed live:\n{out}"
        );
        assert!(
            out.matches("  = ").count() >= 2,
            "the queued second turn ran to its own footer too, proving the \
             chat kept accepting input once the attach settled:\n{out}"
        );
        let runs = service.list_runs().expect("list runs");
        let attached_run = runs
            .iter()
            .find(|r| r.run_id == run_id)
            .expect("the attached run is still listed");
        assert_eq!(
            attached_run.state.as_str(),
            "completed",
            "the run finished on its own, not via a forced cancellation: {attached_run:?}"
        );
    }
}
