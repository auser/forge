//! The input/signal state machine: `(line | signal) -> Vec<Action>`.
//!
//! This is where the §6.3 Ctrl-C table, the §6.5 during-a-turn table and the
//! FIFO prompt queue live, as a pure function of state — no terminal, no
//! signal handler, no clock the caller cannot age. That is the whole point:
//! "Ctrl-C cancels the turn and does not quit" is the single most irritating
//! thing a REPL can get wrong, and here it is a unit test rather than
//! something you can only find out by using the thing.
//!
//! The controller decides *what may happen now*; [`crate::command`] decides
//! what was said. Neither performs an effect — the driver (`app.rs`) executes
//! the returned [`Action`]s in order.
//!
//! The chat keeps no conversation state: the session log is the memory (§5).
//! What this holds is the shape of the moment — is a run attached, is it
//! parked on an approval, what has the user typed that has not run yet, and
//! whether an exit has already been requested once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Re-exported, not redeclared: [`crate::io::Interactivity`] is the same
/// question the `ChatIo` seam answers ("may I ask the user something?"), and
/// two definitions of it would be two places to get piped mode wrong.
pub use crate::io::Interactivity;

use crate::command::{APPROVAL_MODES, Command, Parsed};
use crate::host::HostChange;
use crate::io::{CompletionSnapshot, Line};

/// How long a first `Ctrl-C` at an idle prompt stays armed (§6.3).
const ARM_WINDOW: Duration = Duration::from_secs(2);

/// `128 + SIGINT`, the shell's convention for "killed by an interrupt".
const INTERRUPT_EXIT_CODE: i32 = 130;

/// The hint a first idle `Ctrl-C` prints. Every way out is named, so the
/// second press is a choice and not a discovery.
const EXIT_HINT: &str = "(press Ctrl-C again, or Ctrl-D, or /quit, to exit)";

/// §9.1, verbatim: `/model` and `/approval` rebuild the runtime, which would
/// orphan a live run's input channel and cancellation token.
const REBUILD_REFUSAL: &str =
    "finish or cancel the running turn first (/jobs, /attach <id>, Ctrl-C)";

/// §11, verbatim: the live run belongs to the source session, so its
/// remaining events would land in a log the chat had stopped following.
const FORK_REFUSAL: &str =
    "a turn is still running - /bg detaches it and continues in a fork, or Ctrl-C cancels it";

/// Where the conversation is. Derived from one field, so there is no way for
/// the reported state and the behaviour to disagree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatState {
    /// Nothing is running and nothing is detached.
    Idle,
    /// A run is attached: its transcript is on screen.
    Running,
    /// A run is attached and parked on an approval question.
    AwaitingApproval,
    /// Nothing is attached, but at least one detached job is still live.
    Detached,
}

/// An interrupt or an end of input, whichever way it was delivered — a typed
/// `Ctrl-C` on a TTY, a `SIGINT` in piped mode, or a test firing a channel.
/// One enum, so those paths cannot drift (§6.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    Interrupt,
    /// `Ctrl-D` on an empty line, or stdin EOF. In [`Interactivity::Batch`]
    /// that means "no more input", not "stop now" (§12.3).
    Eof,
}

/// What the driver must do, in the order given.
///
/// One variant per §9.1 effect plus the signal outcomes, so the driver's job
/// is a `match` with no policy in it: every decision about *whether*
/// something is allowed has already been taken here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Start a turn. Only ever one of these is live at a time.
    Prompt(String),
    /// Answer the parked run's approval question.
    Approve(bool),
    /// Cancel the attached run and await its handle (§5).
    CancelRun,
    /// Detach the attached run and continue in a fork (§10.1.1).
    Background,
    /// Rebuild the runtime with one setting overridden.
    Host(HostChange),
    /// Print a transcript line.
    Write(Line),
    Help,
    ListModels,
    ShowApproval,
    /// `/config` (`None`) or `/config <key>` (`Some`).
    ShowConfig(Option<String>),
    ListSkills,
    Graph(String),
    ShowSession,
    NewSession,
    SwitchSession(String),
    /// `/fork`, optionally `--at <pos|run-id>`.
    Fork(Option<String>),
    ListJobs,
    Attach(String),
    /// Cancel every live run, recording each cancellation (§10.2). Emitted
    /// only on the way out, and harmless when nothing is live.
    CancelAllJobs,
    /// Leave with this exit code: 0 for `/quit`/`Ctrl-D`, 130 for the second
    /// `Ctrl-C`.
    Quit(i32),
}

/// What is attached to the foreground of the conversation.
#[derive(Clone, Debug)]
enum Attached {
    /// A run is attached and working.
    Working,
    /// A run is attached and parked on an approval question, which is kept
    /// so a driver that printed something else in between can restate it.
    Approval(String),
}

/// The input/signal state machine.
pub struct Controller {
    /// What `Tab` offers, and what makes a discovered skill a command. The
    /// host refreshes it (jobs and sessions change); the controller only
    /// reads it.
    snapshot: CompletionSnapshot,
    /// May the user be asked a follow-up question? Piped stdin may not, and
    /// that changes what EOF means (§12.3).
    interactivity: Interactivity,
    attached: Option<Attached>,
    /// Detached jobs still live in this process. A count, not ids: the chat
    /// needs to know *whether* work would die with it, and `/jobs` asks the
    /// runtime for the rest.
    detached: usize,
    /// Prompts typed while a run was attached. FIFO and unbounded: what the
    /// user typed is never dropped (§6.5), and running them one at a time is
    /// what keeps the chat from ever provoking the runtime's
    /// one-live-run-per-session refusal (§10.1.1).
    queue: VecDeque<String>,
    /// When the last idle `Ctrl-C` arrived, if it is still armed.
    armed_at: Option<Instant>,
    /// An exit has been requested once while work was live (§10.2). Not
    /// reset by other input: "exiting is one action, however it was
    /// requested", so `/quit` then `Ctrl-D` is ask-then-leave.
    exit_requested: bool,
    /// Batch mode saw EOF: finish the in-flight turn, run the queue, leave.
    draining: bool,
}

impl Controller {
    pub fn new(snapshot: CompletionSnapshot, interactivity: Interactivity) -> Self {
        Self {
            snapshot,
            interactivity,
            attached: None,
            detached: 0,
            queue: VecDeque::new(),
            armed_at: None,
            exit_requested: false,
            draining: false,
        }
    }

    /// How long a first idle `Ctrl-C` stays armed (§6.3). Public so a test
    /// can reason about the window without sleeping through it.
    pub const fn arm_window() -> Duration {
        ARM_WINDOW
    }

    pub fn state(&self) -> ChatState {
        match self.attached {
            Some(Attached::Working) => ChatState::Running,
            Some(Attached::Approval(_)) => ChatState::AwaitingApproval,
            None if self.detached > 0 => ChatState::Detached,
            None => ChatState::Idle,
        }
    }

    /// Replace the completion snapshot: the host learns new skills, `/jobs`
    /// learns new run ids, a new session appears.
    pub fn set_completions(&mut self, snapshot: CompletionSnapshot) {
        self.snapshot = snapshot;
    }

    /// The question the attached run is parked on, if it is parked.
    pub fn pending_approval(&self) -> Option<&str> {
        match &self.attached {
            Some(Attached::Approval(question)) => Some(question),
            _ => None,
        }
    }

    /// One submitted line. The §6.5 table.
    pub fn on_line(&mut self, line: &str) -> Vec<Action> {
        // Submitting anything at all disarms the double-`Ctrl-C` exit (§6.3),
        // so "clear a line, then interrupt once" can never quit.
        self.armed_at = None;

        // A parked run owns the next bare line: that is the rule `forge run`
        // established for piped stdin, and the chat generalises rather than
        // contradicts it. A slash line is still a command, or a mistyped one
        // would silently deny (Review Focus 2's neighbour).
        if self.pending_approval().is_some() && !Command::looks_like_a_command(line) {
            self.attached = Some(Attached::Working);
            return vec![Action::Approve(is_affirmative(line))];
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        match Command::parse(trimmed, &self.snapshot) {
            Parsed::Prompt(text) => self.on_prompt(text),
            Parsed::Unknown(name) => vec![Action::Write(Line::bad(format!(
                "unknown command /{name} - /help lists them"
            )))],
            Parsed::Usage(usage) => vec![Action::Write(Line::bad(format!("usage: {usage}")))],
            Parsed::Help => vec![Action::Help],
            Parsed::Quit => self.request_exit(0),
            Parsed::Model(None) => vec![Action::ListModels],
            Parsed::Model(Some(name)) => self.rebuild(HostChange::Model(name)),
            Parsed::Approval(None) => vec![Action::ShowApproval],
            Parsed::Approval(Some(mode)) => {
                if !APPROVAL_MODES.contains(&mode.as_str()) {
                    return vec![Action::Write(Line::bad(format!(
                        "unknown approval mode {mode} - one of {}",
                        APPROVAL_MODES.join(", ")
                    )))];
                }
                self.rebuild(HostChange::Approval(mode))
            }
            Parsed::Config(key) => vec![Action::ShowConfig(key)],
            Parsed::Skills => vec![Action::ListSkills],
            Parsed::Graph(query) => vec![Action::Graph(query)],
            Parsed::Session => vec![Action::ShowSession],
            Parsed::SessionNew => vec![Action::NewSession],
            Parsed::SessionSwitch(id) => vec![Action::SwitchSession(id)],
            Parsed::Fork(at) => {
                // Refused only while *attached* (§11): a detached job is in
                // another session already, so forking cannot strand it.
                if self.attached.is_some() {
                    return vec![Action::Write(Line::bad(FORK_REFUSAL))];
                }
                vec![Action::Fork(at)]
            }
            Parsed::Background => self.on_background(),
            Parsed::Jobs => vec![Action::ListJobs],
            Parsed::Attach(run_id) => {
                // The attached run becomes the foreground one (§10), so a
                // prompt typed while it finishes queues behind it. `detached`
                // is deliberately *not* decremented: an attach that fails
                // leaves the job running, and over-counting live work only
                // costs one extra confirmation on the way out, where
                // under-counting would abandon a job without asking.
                self.attached = Some(Attached::Working);
                vec![Action::Attach(run_id)]
            }
        }
    }

    /// An interrupt or an end of input. The §6.3 table.
    pub fn on_signal(&mut self, signal: Signal) -> Vec<Action> {
        match signal {
            Signal::Interrupt => self.on_interrupt(),
            Signal::Eof => self.on_eof(),
        }
    }

    /// The attached run parked on an approval question.
    ///
    /// Only meaningful while a run is attached: a *detached* job's approval
    /// is a `  # ` notice telling you to `/attach` (§10.2), never a question
    /// the foreground prompt answers.
    pub fn on_approval_requested(&mut self, question: &str) {
        if self.attached.is_some() {
            self.attached = Some(Attached::Approval(question.to_string()));
        }
    }

    /// The attached run reached a terminal state, however it ended.
    pub fn on_run_settled(&mut self) {
        self.attached = None;
    }

    /// A detached job reached a terminal state (its watcher saw the event, or
    /// the driver did after re-attaching to it).
    pub fn on_job_settled(&mut self) {
        self.detached = self.detached.saturating_sub(1);
    }

    /// The driver has nothing left to run: no attached run, no queue.
    ///
    /// In [`Interactivity::Batch`] after EOF that is the end of the script,
    /// and there is nobody to confirm anything with, so any still-detached
    /// job is cancelled and the chat leaves 0 (§12.3). Otherwise it is just
    /// the prompt coming back.
    pub fn on_drained(&mut self) -> Vec<Action> {
        if !self.draining || self.attached.is_some() || !self.queue.is_empty() {
            return Vec::new();
        }
        self.draining = false;
        vec![Action::CancelAllJobs, Action::Quit(0)]
    }

    /// The next queued prompt, which becomes the attached run.
    ///
    /// Popping registers the turn, so a driver cannot take two prompts and
    /// run them at once — the queue is what means the chat never asks the
    /// runtime for a second concurrent run on one session (§10.1.1).
    pub fn take_queued(&mut self) -> Option<String> {
        let next = self.queue.pop_front()?;
        self.attached = Some(Attached::Working);
        Some(next)
    }

    fn on_prompt(&mut self, text: String) -> Vec<Action> {
        if self.attached.is_some() {
            // §6.5: queued as the next turn, FIFO, never dropped.
            self.queue.push_back(text);
            return Vec::new();
        }
        self.attached = Some(Attached::Working);
        vec![Action::Prompt(text)]
    }

    fn on_background(&mut self) -> Vec<Action> {
        if self.attached.is_none() {
            return vec![Action::Write(Line::meta(
                "nothing is running - /bg detaches a turn in progress",
            ))];
        }
        self.attached = None;
        self.detached += 1;
        vec![Action::Background]
    }

    /// `/model <name>` and `/approval <mode>`: refused while *any* run is
    /// live in this process, attached or not (§9.1). Rebuilding the runtime
    /// would orphan a detached run's input channel and cancellation token,
    /// and a `/bg` job you can no longer answer or cancel is worse than a
    /// command you have to retype.
    fn rebuild(&mut self, change: HostChange) -> Vec<Action> {
        if self.live_work() > 0 {
            return vec![Action::Write(Line::bad(REBUILD_REFUSAL))];
        }
        vec![Action::Host(change)]
    }

    fn on_interrupt(&mut self) -> Vec<Action> {
        match self.attached {
            // Cancel the turn. Never quit: that is the row REPLs get
            // backwards, and the one a user meets most often.
            Some(Attached::Working) => {
                self.armed_at = None;
                vec![Action::CancelRun]
            }
            // Cancel, and answer nothing. `await_approval` already selects
            // on the cancellation token, so cancelling alone unblocks it
            // with `Cancelled` and the tool is never dispatched. Sending a
            // denial as well would leave an unordered answer in a channel
            // that a *later* approval in the same run could consume if
            // cancellation lost the race (§6.3). The run is no longer parked
            // either, so the next line the user types is not read as an
            // answer to a question that no longer exists.
            Some(Attached::Approval(_)) => {
                self.armed_at = None;
                self.attached = Some(Attached::Working);
                vec![
                    Action::CancelRun,
                    Action::Write(Line::bad("the operation was not run")),
                ]
            }
            // Idle, or only detached jobs live: there is nothing attached to
            // cancel, so this is the exit gesture. Once to hint, twice
            // within the window to leave.
            None => {
                if self.armed_at.is_some_and(|at| at.elapsed() < ARM_WINDOW) {
                    self.armed_at = None;
                    return self.request_exit(INTERRUPT_EXIT_CODE);
                }
                self.armed_at = Some(Instant::now());
                vec![Action::Write(Line::meta(EXIT_HINT))]
            }
        }
    }

    fn on_eof(&mut self) -> Vec<Action> {
        match self.interactivity {
            // `Ctrl-D` requests an exit in every state, including during a
            // turn, because a prompt is up during a turn too (§6.2).
            Interactivity::Interactive => self.request_exit(0),
            Interactivity::Batch => {
                self.draining = true;
                // A parked run cannot be answered by anybody now, and
                // blocking for ever is the one outcome that is certainly
                // wrong: take the safe stated action instead, exactly as
                // `NativeExecution` does when it cannot ask (§12.3).
                if self.pending_approval().is_some() {
                    self.attached = Some(Attached::Working);
                    return vec![
                        Action::Write(Line::bad("input ended with an approval pending - denied")),
                        Action::Approve(false),
                    ];
                }
                self.on_drained()
            }
        }
    }

    /// Exiting is one action however it was requested (§6.3), so `/quit`,
    /// `Ctrl-D` and the second `Ctrl-C` all come through here and all get
    /// §10.2's one confirmation when work would die with the process.
    fn request_exit(&mut self, code: i32) -> Vec<Action> {
        let live = self.live_work();
        if live > 0 && self.interactivity == Interactivity::Interactive && !self.exit_requested {
            self.exit_requested = true;
            let (plural, them) = if live == 1 { ("", "it") } else { ("s", "them") };
            return vec![Action::Write(Line::bad(format!(
                "{live} job{plural} still running - ask again to abandon {them}"
            )))];
        }
        let mut actions = Vec::new();
        if live > 0 {
            actions.push(Action::CancelAllJobs);
        }
        actions.push(Action::Quit(code));
        actions
    }

    /// Runs that would die with this process: the attached one, if any, plus
    /// every detached job.
    fn live_work(&self) -> usize {
        self.detached + usize::from(self.attached.is_some())
    }

    /// Test-only: move the arming into the past instead of sleeping through
    /// the window.
    #[cfg(test)]
    fn age_arming(&mut self, by: Duration) {
        self.armed_at = self.armed_at.and_then(|at| at.checked_sub(by));
    }
}

/// `y`/`yes` approves; everything else denies, including an empty line (§8).
/// Denial is the safe default, so an ambiguous answer is never consent.
fn is_affirmative(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> Controller {
        Controller::new(CompletionSnapshot::default(), Interactivity::Interactive)
    }

    fn batch() -> Controller {
        Controller::new(CompletionSnapshot::default(), Interactivity::Batch)
    }

    #[test]
    fn a_prompt_starts_a_turn() {
        let mut c = idle();
        assert_eq!(
            c.on_line("explain the parser"),
            vec![Action::Prompt("explain the parser".into())]
        );
        assert_eq!(c.state(), ChatState::Running);
    }

    #[test]
    fn an_empty_line_does_nothing() {
        let mut c = idle();
        assert_eq!(c.on_line("   "), vec![]);
        assert_eq!(c.state(), ChatState::Idle);
    }

    // --- Review Focus 1: the Ctrl-C table ------------------------------

    #[test]
    fn interrupt_while_running_cancels_the_turn_and_never_quits() {
        let mut c = idle();
        c.on_line("do something slow");
        let actions = c.on_signal(Signal::Interrupt);
        assert!(actions.contains(&Action::CancelRun), "{actions:?}");
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Quit(_))),
            "{actions:?}"
        );
    }

    #[test]
    fn interrupt_while_awaiting_approval_cancels_and_does_not_answer() {
        let mut c = idle();
        c.on_line("delete the logs");
        c.on_approval_requested("rm -rf logs (destructive)");
        assert_eq!(c.state(), ChatState::AwaitingApproval);
        let actions = c.on_signal(Signal::Interrupt);
        assert!(actions.contains(&Action::CancelRun), "{actions:?}");
        // Review Focus 2: no denial may be queued as well — an unconsumed
        // "n" could be swallowed by a later approval in the same run.
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Approve(_))),
            "{actions:?}"
        );
    }

    /// The other half of Review Focus 2: after the cancel, the run is
    /// unwinding and is no longer parked, so the *next* line the user types
    /// must not be read as the answer to a question that no longer exists.
    #[test]
    fn a_line_typed_after_a_cancelled_approval_is_not_an_answer() {
        let mut c = idle();
        c.on_line("delete the logs");
        c.on_approval_requested("rm -rf logs (destructive)");
        c.on_signal(Signal::Interrupt);
        assert_ne!(c.state(), ChatState::AwaitingApproval);
        let actions = c.on_line("y");
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Approve(_))),
            "{actions:?}"
        );
    }

    #[test]
    fn one_interrupt_at_an_idle_prompt_only_hints() {
        let mut c = idle();
        let actions = c.on_signal(Signal::Interrupt);
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Quit(_))),
            "{actions:?}"
        );
        let hint = actions
            .iter()
            .find_map(|a| match a {
                Action::Write(line) => Some(line.text.clone()),
                _ => None,
            })
            .expect("a hint is printed");
        assert!(hint.contains("Ctrl-D"), "{hint}");
        assert!(hint.contains("/quit"), "{hint}");
    }

    #[test]
    fn a_second_interrupt_at_an_idle_prompt_quits_with_130() {
        let mut c = idle();
        c.on_signal(Signal::Interrupt);
        let actions = c.on_signal(Signal::Interrupt);
        assert!(actions.contains(&Action::Quit(130)), "{actions:?}");
    }

    /// Typing between the two interrupts means the user is still working.
    #[test]
    fn a_line_between_two_interrupts_resets_the_quit_arming() {
        let mut c = idle();
        c.on_signal(Signal::Interrupt);
        c.on_line("still here");
        c.on_run_settled();
        let actions = c.on_signal(Signal::Interrupt);
        assert!(!actions.contains(&Action::Quit(130)), "{actions:?}");
    }

    /// The arming is a window, not a latch. Aged rather than slept, which is
    /// what `arm_window()` is exposed for.
    #[test]
    fn an_interrupt_older_than_the_arm_window_does_not_quit() {
        let mut c = idle();
        c.on_signal(Signal::Interrupt);
        c.age_arming(Controller::arm_window() + Duration::from_millis(1));
        let actions = c.on_signal(Signal::Interrupt);
        assert!(!actions.contains(&Action::Quit(130)), "{actions:?}");
        // and it re-arms, so a prompt Ctrl-C Ctrl-C still exits
        assert!(
            c.on_signal(Signal::Interrupt).contains(&Action::Quit(130)),
            "the window re-arms"
        );
    }

    /// The §6.3 row that is easiest to get backwards: an interrupt during a
    /// turn cancels *only*, and leaves no arming behind, so the next one at
    /// the returned prompt is a first press.
    #[test]
    fn an_interrupt_that_cancelled_a_turn_does_not_arm_the_exit() {
        let mut c = idle();
        c.on_line("do something slow");
        c.on_signal(Signal::Interrupt);
        c.on_run_settled();
        let actions = c.on_signal(Signal::Interrupt);
        assert!(!actions.contains(&Action::Quit(130)), "{actions:?}");
    }

    #[test]
    fn eof_quits_zero() {
        let mut c = idle();
        assert_eq!(c.on_signal(Signal::Eof), vec![Action::Quit(0)]);
    }

    /// Piped stdin's EOF means "no more input", not "stop now": the queue
    /// still runs, and there is nobody to ask for the live-job
    /// confirmation. `printf 'a\nb\n' | forge` must run both turns.
    #[test]
    fn in_batch_mode_eof_drains_the_queue_before_quitting() {
        let mut c = batch();
        c.on_line("first");
        c.on_line("second");
        let actions = c.on_signal(Signal::Eof);
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Quit(_))),
            "the queued prompt has not run yet: {actions:?}"
        );
        c.on_run_settled();
        assert_eq!(c.take_queued(), Some("second".to_string()));
        c.on_run_settled();
        assert_eq!(c.take_queued(), None);
        assert_eq!(c.on_drained(), vec![Action::CancelAllJobs, Action::Quit(0)]);
    }

    /// Until the EOF arrives there is nothing to drain: `on_drained` is only
    /// an exit in batch mode after input ended.
    #[test]
    fn on_drained_is_not_an_exit_on_its_own() {
        let mut c = batch();
        c.on_line("first");
        c.on_run_settled();
        assert_eq!(c.on_drained(), vec![]);
        let mut interactive = idle();
        interactive.on_line("first");
        interactive.on_run_settled();
        assert_eq!(interactive.on_drained(), vec![]);
    }

    /// Input ended with a run parked on an approval: nobody can answer it,
    /// so take the safe stated action rather than blocking for ever (§12.3,
    /// the same principle as `NativeExecution`'s non-interactive approval).
    #[test]
    fn in_batch_mode_eof_denies_a_pending_approval_rather_than_hanging() {
        let mut c = batch();
        c.on_line("delete the logs");
        c.on_approval_requested("rm -rf logs (destructive)");
        let actions = c.on_signal(Signal::Eof);
        assert!(actions.contains(&Action::Approve(false)), "{actions:?}");
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Quit(_))),
            "the turn still has to unwind: {actions:?}"
        );
        assert_ne!(c.state(), ChatState::AwaitingApproval);
    }

    // --- approvals ------------------------------------------------------

    #[test]
    fn approval_answers_map_y_to_approve_and_everything_else_to_deny() {
        for (answer, approved) in [
            ("y", true),
            ("Y", true),
            ("yes", true),
            ("n", false),
            ("", false),
            ("maybe", false),
        ] {
            let mut c = idle();
            c.on_line("delete the logs");
            c.on_approval_requested("rm -rf logs (destructive)");
            assert_eq!(
                c.on_line(answer),
                vec![Action::Approve(approved)],
                "answer {answer:?}"
            );
            assert_eq!(c.state(), ChatState::Running, "the turn continues");
        }
    }

    /// The question is kept, so a driver that had to print something else in
    /// between can put it back on screen.
    #[test]
    fn the_pending_question_is_remembered_while_it_is_pending() {
        let mut c = idle();
        c.on_line("delete the logs");
        assert_eq!(c.pending_approval(), None);
        c.on_approval_requested("rm -rf logs (destructive)");
        assert_eq!(c.pending_approval(), Some("rm -rf logs (destructive)"));
        c.on_line("n");
        assert_eq!(c.pending_approval(), None);
    }

    // --- refusals and quit with work in flight --------------------------

    #[test]
    fn switching_the_model_is_refused_while_a_run_is_live() {
        let mut c = idle();
        c.on_line("something long");
        let actions = c.on_line("/model deepseek-chat");
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Host(_))),
            "{actions:?}"
        );
        let msg = actions
            .iter()
            .find_map(|a| match a {
                Action::Write(line) => Some(line.text.clone()),
                _ => None,
            })
            .expect("an explanation is printed");
        assert!(msg.contains("/jobs"), "{msg}");
    }

    /// Rebuilding the runtime would orphan a *detached* run's input channel
    /// and cancellation token too, so `/model` is refused while any run is
    /// live in this process (§9.1) — not merely while one is attached.
    #[test]
    fn switching_the_model_is_refused_while_only_a_detached_job_is_live() {
        let mut c = idle();
        c.on_line("long job");
        c.on_line("/bg");
        assert_eq!(c.state(), ChatState::Detached);
        let actions = c.on_line("/approval deny");
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Host(_))),
            "{actions:?}"
        );
    }

    #[test]
    fn switching_the_model_works_when_nothing_is_live() {
        let mut c = idle();
        assert_eq!(
            c.on_line("/model deepseek-chat"),
            vec![Action::Host(HostChange::Model("deepseek-chat".into()))]
        );
        assert_eq!(
            c.on_line("/approval prompt-dangerous"),
            vec![Action::Host(HostChange::Approval(
                "prompt-dangerous".into()
            ))]
        );
    }

    /// An unknown mode is not passed to the host to fail there: the four
    /// policies are the completion list, so they are also the check.
    #[test]
    fn an_unknown_approval_mode_is_refused_before_the_host_sees_it() {
        let mut c = idle();
        let actions = c.on_line("/approval sometimes");
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Host(_))),
            "{actions:?}"
        );
    }

    /// `/fork` is refused only while a turn is *attached* (§11): the live run
    /// belongs to the source session. Detached jobs are in other sessions
    /// already, so forking is fine.
    #[test]
    fn forking_is_refused_while_a_turn_is_attached_and_allowed_when_detached() {
        let mut c = idle();
        c.on_line("something long");
        let refused = c.on_line("/fork");
        assert!(
            !refused.iter().any(|a| matches!(a, Action::Fork(_))),
            "{refused:?}"
        );
        let msg = refused
            .iter()
            .find_map(|a| match a {
                Action::Write(line) => Some(line.text.clone()),
                _ => None,
            })
            .expect("an explanation is printed");
        assert!(msg.contains("/bg"), "{msg}");
        c.on_line("/bg");
        assert_eq!(c.on_line("/fork"), vec![Action::Fork(None)]);
    }

    #[test]
    fn quitting_with_a_live_job_asks_once_then_abandons() {
        let mut c = idle();
        c.on_line("long job");
        c.on_line("/bg");
        let first = c.on_line("/quit");
        assert!(
            !first.iter().any(|a| matches!(a, Action::Quit(_))),
            "{first:?}"
        );
        let second = c.on_line("/quit");
        assert!(second.contains(&Action::Quit(0)), "{second:?}");
        assert!(second.contains(&Action::CancelAllJobs), "{second:?}");
    }

    /// "Exiting is one action, however it was requested" (§6.3): the
    /// confirmation is armed by any exit request and consumed by any other.
    #[test]
    fn the_abandon_warning_is_shared_by_every_way_of_exiting() {
        let mut c = idle();
        c.on_line("long job");
        c.on_line("/bg");
        let asked = c.on_signal(Signal::Interrupt);
        assert!(
            !asked.iter().any(|a| matches!(a, Action::Quit(_))),
            "one interrupt at a prompt only hints: {asked:?}"
        );
        let armed = c.on_signal(Signal::Interrupt);
        assert!(
            !armed.iter().any(|a| matches!(a, Action::Quit(_))),
            "the second interrupt is an exit request, which asks once: {armed:?}"
        );
        let out = c.on_signal(Signal::Eof);
        assert!(out.contains(&Action::Quit(0)), "{out:?}");
        assert!(out.contains(&Action::CancelAllJobs), "{out:?}");
    }

    /// A job that finished is not a reason to confirm anything.
    #[test]
    fn a_detached_job_that_finished_stops_being_a_reason_to_confirm() {
        let mut c = idle();
        c.on_line("long job");
        c.on_line("/bg");
        c.on_job_settled();
        assert_eq!(c.state(), ChatState::Idle);
        assert_eq!(c.on_signal(Signal::Eof), vec![Action::Quit(0)]);
    }

    /// Quitting mid-turn is an exit request like any other, and the attached
    /// run is work that dies with the process, so it is confirmed once.
    #[test]
    fn quitting_during_a_turn_asks_once_too() {
        let mut c = idle();
        c.on_line("long job");
        let first = c.on_signal(Signal::Eof);
        assert!(
            !first.iter().any(|a| matches!(a, Action::Quit(_))),
            "{first:?}"
        );
        let second = c.on_signal(Signal::Eof);
        assert!(second.contains(&Action::Quit(0)), "{second:?}");
    }

    /// `/bg` with nothing running detaches nothing, and must not tell the
    /// driver to detach a run that does not exist.
    #[test]
    fn backgrounding_nothing_is_explained_not_obeyed() {
        let mut c = idle();
        let actions = c.on_line("/bg");
        assert!(!actions.contains(&Action::Background), "{actions:?}");
        assert_eq!(c.state(), ChatState::Idle);
    }

    /// A line typed during a turn becomes the next turn; it is never lost.
    /// (The driver keeps a read outstanding during a turn — spec §6.2 —
    /// which is what makes this reachable, and `/bg` reachable at all.)
    #[test]
    fn prompts_submitted_while_running_queue_in_order() {
        let mut c = idle();
        c.on_line("first");
        assert!(c.on_line("second").is_empty(), "nothing happens yet");
        assert!(c.on_line("third").is_empty());
        c.on_run_settled();
        assert_eq!(c.take_queued(), Some("second".to_string()));
        assert_eq!(
            c.take_queued(),
            Some("third".to_string()),
            "FIFO, nothing dropped"
        );
        assert_eq!(c.take_queued(), None);
    }

    /// Taking a queued prompt starts a turn, so the driver cannot pop two of
    /// them and run them at once — the queue is what keeps the chat from
    /// provoking the runtime's one-live-run-per-session refusal (§10.1.1).
    #[test]
    fn taking_a_queued_prompt_is_a_running_turn() {
        let mut c = idle();
        c.on_line("first");
        c.on_line("second");
        c.on_run_settled();
        assert_eq!(c.state(), ChatState::Idle);
        assert_eq!(c.take_queued(), Some("second".to_string()));
        assert_eq!(c.state(), ChatState::Running);
    }

    /// The commands whose whole purpose is a turn that is taking too long.
    #[test]
    fn during_turn_commands_act_immediately() {
        let mut c = idle();
        c.on_line("long job");
        assert_eq!(c.on_line("/bg"), vec![Action::Background]);
        c.on_line("another long job");
        assert_eq!(c.on_line("/jobs"), vec![Action::ListJobs]);
    }

    /// Review Focus 2's neighbour: a command typed while an approval is
    /// pending must not be read as "anything else", which would deny.
    #[test]
    fn a_command_while_an_approval_is_pending_does_not_answer_it() {
        let mut c = idle();
        c.on_line("delete the logs");
        c.on_approval_requested("rm -rf logs (destructive)");
        let actions = c.on_line("/jobs");
        assert_eq!(actions, vec![Action::ListJobs]);
        assert_eq!(
            c.state(),
            ChatState::AwaitingApproval,
            "still waiting for an answer"
        );
        assert_eq!(c.on_line("n"), vec![Action::Approve(false)]);
    }

    /// An unknown `/word` at an approval prompt is a typo, not a denial.
    #[test]
    fn an_unknown_command_while_an_approval_is_pending_does_not_answer_it() {
        let mut c = idle();
        c.on_line("delete the logs");
        c.on_approval_requested("rm -rf logs (destructive)");
        let actions = c.on_line("/wat");
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Approve(_))),
            "{actions:?}"
        );
        assert_eq!(c.state(), ChatState::AwaitingApproval);
    }

    #[test]
    fn an_unknown_command_names_itself_and_points_at_help() {
        let mut c = idle();
        let actions = c.on_line("/wat");
        let msg = actions
            .iter()
            .find_map(|a| match a {
                Action::Write(line) => Some(line.text.clone()),
                _ => None,
            })
            .expect("an error is printed");
        assert!(msg.contains("/wat"), "{msg}");
        assert!(msg.contains("/help"), "{msg}");
        assert_eq!(c.state(), ChatState::Idle, "a typo is not a turn");
    }

    /// The snapshot is what makes a discovered skill a command, and it
    /// changes as the host learns things.
    #[test]
    fn a_skill_from_the_snapshot_becomes_a_turn() {
        let mut c = idle();
        c.set_completions(CompletionSnapshot {
            skills: vec!["tdd".into()],
            ..CompletionSnapshot::default()
        });
        assert_eq!(
            c.on_line("/tdd write the failing test"),
            vec![Action::Prompt(
                "Use the tdd skill.\n\nwrite the failing test".into()
            )]
        );
    }

    /// Every line this module can emit is ASCII and not width-sensitive: no
    /// boxes, no tables, no rules (§12.1).
    #[test]
    fn nothing_this_module_writes_is_width_sensitive() {
        let mut lines = Vec::new();
        let mut c = idle();
        lines.extend(written(c.on_signal(Signal::Interrupt)));
        lines.extend(written(c.on_line("/wat")));
        lines.extend(written(c.on_line("/attach")));
        lines.extend(written(c.on_line("/approval sometimes")));
        lines.extend(written(c.on_line("/bg")));
        c.on_line("long job");
        lines.extend(written(c.on_line("/model x")));
        lines.extend(written(c.on_line("/fork")));
        c.on_approval_requested("rm -rf logs (destructive)");
        lines.extend(written(c.on_signal(Signal::Interrupt)));
        c.on_run_settled();
        c.on_line("job");
        c.on_line("/bg");
        lines.extend(written(c.on_line("/quit")));
        assert!(!lines.is_empty(), "the sweep must actually collect lines");
        for line in lines {
            assert!(line.is_ascii(), "{line}");
            assert!(!line.contains("---"), "no rules: {line}");
            assert!(!line.contains('|'), "no tables: {line}");
        }
    }

    fn written(actions: Vec<Action>) -> Vec<String> {
        actions
            .into_iter()
            .filter_map(|a| match a {
                Action::Write(line) => Some(line.text),
                _ => None,
            })
            .collect()
    }
}
