//! `ChatIo` over `rustyline`, on a dedicated editor thread.
//!
//! `rustyline::Editor::readline` blocks the calling thread until a line is
//! typed, so it cannot live on a tokio worker (it would starve every other
//! task on that worker for the whole prompt). Instead one `std::thread`
//! owns the `Editor` for the life of the chat and receives
//! `(Prompt, oneshot::Sender<ReadOutcome>)` jobs over an `mpsc`, exactly the
//! pattern `NeedleEngine::spawn` uses for `libneedle`
//! (`crates/forge-needle/src/engine.rs`): a blocking, non-`Send`-friendly
//! resource lives behind a channel rather than trying to make it `Send` or
//! `async`.
//!
//! # Ctrl-C arrives through `read`, not `interrupted`
//!
//! `ChatIo::read` and `ChatIo::interrupted` both take `&mut self`, so the
//! async driver cannot run them as two concurrent `select!` arms (Task 7
//! confirmed this against the compiler — `E0499`). On a TTY that is fine:
//! a typed Ctrl-C never reaches this thread as `SIGINT` at all (raw mode
//! clears `ISIG`, so the terminal driver does not generate one — see the
//! termios facts below), and its only path out is the `EventHandler`
//! bound to Ctrl-C below, which resolves the very `readline` call that is
//! blocked, via `Cmd::Interrupt` -> `ReadlineError::Interrupted` ->
//! [`ReadOutcome::Interrupt`]. `interrupted()` here is `tokio::signal::
//! ctrl_c()`, the documented secondary path for a `SIGINT` that reaches the
//! process some other way (`kill -INT`, a shell that has not handed us a
//! real TTY).
//!
//! # Two verified termios facts this design rests on
//!
//! * Raw mode is entered with `TCSADRAIN`, not `TCSAFLUSH`
//!   (`tty/unix.rs:1646` in `rustyline` 18.0.1), so type-ahead queued while
//!   a turn is running is preserved rather than discarded.
//! * `Ctrl-D` is the terminal's own `VEOF` character (`tty/unix.rs:1641`),
//!   so it honours whatever the user's `stty` says, and `Cmd::EndOfFile`
//!   only becomes `ReadlineError::Eof` on an *empty* line — a half-typed
//!   line is never silently dropped.

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;

use async_trait::async_trait;
use forge_chat::{ChatIo, Command, CompletionSnapshot, Interactivity, Line, Prompt, ReadOutcome};
use forge_core::ForgeError;
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Context, Editor, Event, EventContext,
    EventHandler, ExternalPrinter, Helper, KeyEvent, Movement, RepeatCount,
};
use tokio::sync::{mpsc, oneshot};

use super::palette::Palette;

/// One request to the editor thread: ask for a line under this prompt, and
/// where to send the answer.
///
/// # A separate, real, not-fixed-here hazard: this `read` is not
/// cancel-safe the way `App::drive`'s `select!` needs it to be
///
/// `App::drive` (`forge-chat::app`) reconstructs `self.io.read(prompt)`
/// fresh every loop iteration and drops whichever branch does not win
/// (`forge-chat`'s module doc; `PipedIo`'s own module doc walks through why
/// that is safe for a `read` backed by a channel it only *receives* from).
/// `TerminalIo::read` is not that: its first poll *sends* a new `Job::Read`
/// to this thread before awaiting the reply, and `mpsc::Sender::send`, once
/// it has enqueued the message, cannot be un-sent by dropping the future
/// that called it. While a turn is streaming multiple events — normal,
/// even for a one-line answer (a routing line, the answer, a footer are
/// three) — the read arm can lose several iterations in a row, each one
/// constructing, sending, and then abandoning its own `Job::Read` with a
/// now-dropped `oneshot::Sender` on this end. This thread has no way to
/// tell an abandoned job from a live one: it serves whichever one it
/// dequeues next, `resp.send` silently failing for a dropped receiver, and
/// the caller that is actually still awaiting a reply is left holding a
/// *different* `Job`'s receiver that will now never fire — genuinely stuck.
/// (`TerminalIo::shutdown` used to compound this into a hang of the whole
/// process by joining a thread parked inside an orphaned `readline()`; it
/// detaches the thread instead — see its doc. The stuck *caller* above is
/// still open.) Reproduced against a real pty: a turn with several events,
/// followed by a further typed line, occasionally has that line vanish
/// into an abandoned job instead of reaching `App::on_read` at all.
///
/// `PipedIo`'s fix (one persistent producer thread, `read()` only ever
/// receiving) does not carry over directly, because unlike `PipedIo`,
/// *what* to read next genuinely depends on a per-call `Prompt` (fresh
/// completions) — this thread cannot free-run a queue of results the way
/// `PipedIo`'s stdin thread does. The right shape is likely a persistent
/// loop here that always reads the *latest* known prompt from a
/// non-blocking side channel (e.g. `watch`) rather than one popped per
/// call, feeding an unbounded outcomes channel `read()` only receives
/// from — real, separate work, out of scope for the printer/`select`
/// defect this module's other doc comments describe.
enum Job {
    Read(Prompt, oneshot::Sender<ReadOutcome>),
}

/// What the editor thread reports back once it has (or has not) built the
/// `Editor`. `TerminalIo::new` blocks on this exactly once, at startup —
/// after that every exchange is the async `Job`/`ReadOutcome` channel.
enum StartupResult {
    Ready(Box<dyn ExternalPrinter + Send>),
    Failed(String),
}

/// The real terminal: `rustyline` on a dedicated thread, plus the palette
/// that decides whether anything it prints gets colour.
pub struct TerminalIo {
    palette: Palette,
    /// `None` after [`TerminalIo::shutdown`]: dropping the sender is how the
    /// editor thread is told to stop (see `shutdown`'s doc).
    jobs_tx: Option<mpsc::Sender<Job>>,
    printer: Box<dyn ExternalPrinter + Send>,
    /// Dropped, never joined, on the way out — see [`TerminalIo::shutdown`].
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TerminalIo {
    /// Start the dedicated editor thread and block briefly (microseconds:
    /// termios queries, no I/O) for it to report whether the `Editor` came
    /// up. `history_path` is where `rustyline`'s own history file lives —
    /// distinct from [`Prompt::history`], which is data the pure layer
    /// supplies to seed recall with, not a file this type owns.
    pub fn new(palette: Palette, history_path: PathBuf) -> Result<Self, ForgeError> {
        let (ready_tx, ready_rx) = std_mpsc::sync_channel::<StartupResult>(1);
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>(1);
        let color_mode = palette.color_mode();

        let thread = std::thread::Builder::new()
            .name("chat-editor".to_string())
            .spawn(move || editor_thread_main(color_mode, history_path, ready_tx, jobs_rx))
            .map_err(|e| {
                ForgeError::config(format!("failed to start the chat editor thread: {e}"))
            })?;

        match ready_rx.recv() {
            Ok(StartupResult::Ready(printer)) => Ok(Self {
                palette,
                jobs_tx: Some(jobs_tx),
                printer,
                thread: Some(thread),
            }),
            Ok(StartupResult::Failed(message)) => {
                let _ = thread.join();
                Err(ForgeError::config(format!(
                    "failed to start the terminal line editor: {message}"
                )))
            }
            Err(_) => {
                let _ = thread.join();
                Err(ForgeError::config(
                    "the chat editor thread exited before it was ready".to_string(),
                ))
            }
        }
    }
}

#[async_trait]
impl ChatIo for TerminalIo {
    async fn read(&mut self, prompt: Prompt) -> ReadOutcome {
        let Some(tx) = self.jobs_tx.as_ref() else {
            return ReadOutcome::Failed("the terminal editor has been shut down".to_string());
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        if tx.send(Job::Read(prompt, resp_tx)).await.is_err() {
            return ReadOutcome::Failed("the terminal editor thread is gone".to_string());
        }
        resp_rx.await.unwrap_or_else(|_| {
            ReadOutcome::Failed("the terminal editor dropped the reply".to_string())
        })
    }

    fn write(&mut self, line: &Line) {
        println!("{}", self.palette.paint(line.style, &line.text));
    }

    fn notify(&mut self, line: &Line) {
        let text = self.palette.paint(line.style, &line.text);
        if let Err(e) = self.printer.print(format!("{text}\n")) {
            tracing::debug!(error = %e, "notice print failed; it may be delayed or lost");
        }
    }

    /// The secondary path (see the module doc): on a TTY a typed Ctrl-C
    /// resolves the outstanding `read` instead, because raw mode leaves no
    /// `SIGINT` for this to catch. This exists for a `SIGINT` that reaches
    /// the process some other way.
    async fn interrupted(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }

    fn interactivity(&self) -> Interactivity {
        Interactivity::Interactive
    }

    fn shutdown(&mut self) {
        // Dropping the only sender closes the channel; the editor thread's
        // `blocking_recv` then returns `None` on its own, so it falls
        // through to `save_history` and exits without needing a dedicated
        // shutdown message.
        self.jobs_tx = None;
        // Deliberately **not** joined. That only ever completes if the
        // thread is back at `blocking_recv` when this runs; if it is
        // instead inside `readline()` serving an orphaned `Job` (the `Job`
        // doc's separate, still-open cancel-safety hazard), `readline`
        // returns only when a key is pressed — so a join here is the
        // process hanging after `/quit` with no prompt on screen and
        // nothing saying why. Reachable without the `Job` hazard being
        // fixed: a turn running with an orphaned job outstanding, then
        // enough `kill -INT`s to drive the `Controller` to `Quit(130)`.
        //
        // Nothing is lost by detaching: `append_history` already ran for
        // every accepted line (see `editor_thread_main`), so the
        // thread-exit `save_history` is a belt-and-braces flush, not the
        // durability mechanism. A detached thread at process exit is
        // strictly better than a hang.
        drop(self.thread.take());
    }
}

/// The editor thread's body: build the `Editor` (reporting success or
/// failure over `ready_tx` exactly once), then serve `Job::Read` requests
/// until the channel closes, then persist history on the way out.
fn editor_thread_main(
    color_mode: rustyline::ColorMode,
    history_path: PathBuf,
    ready_tx: std_mpsc::SyncSender<StartupResult>,
    mut jobs_rx: mpsc::Receiver<Job>,
) {
    let mut editor = match build_editor(color_mode) {
        Ok(editor) => editor,
        Err(e) => {
            let _ = ready_tx.send(StartupResult::Failed(e.to_string()));
            return;
        }
    };

    // A missing or corrupt history file must never block the chat from
    // starting: this is a convenience, not a dependency.
    if let Err(e) = editor.load_history(&history_path) {
        tracing::debug!(
            error = %e,
            path = %history_path.display(),
            "no prior chat history loaded"
        );
    }

    // Deliberately *never* `editor.create_external_printer()`: doing so
    // switches every keystroke's wait, for the rest of this `Editor`'s life,
    // from `PosixRawReader::next_key` (which drains its own `BufReader`
    // before ever asking the OS for more) onto `PosixRawReader::select`
    // (rustyline 18.0.1, `tty/unix.rs`), which does not — `select`'s sibling
    // `poll` guards its blocking OS call with `if self.tty_in.buffer().len()
    // > 0 { return Ok(true) }`; `select` has no such guard before calling
    // `select::select(..)`. Whenever a single kernel-level read hands
    // rustyline more than one byte at once — a paste, or exactly the
    // type-ahead this app means to preserve across a turn (see this
    // module's doc) — only the first byte is consumed; the rest sit
    // forever in that private buffer, and the *next* wait for input blocks
    // in `select` for a new byte that will never arrive, because the OS
    // already delivered everything it had. `crates/forge-cli/tests/
    // chat.rs`'s pty tests reproduced this with no Ctrl-C involved at all
    // (a single `write_all` of a whole line, first read of the session),
    // and also confirmed a typed Ctrl-C followed by a line typed one byte
    // at a time never wedges — Ctrl-C itself adds nothing; it is just one
    // easy way to end up typing the next line quickly enough to burst.
    // `notify()` therefore always uses [`StdoutPrinter`]: plain, uncoloured,
    // and unsynchronized with an in-progress prompt, but never able to wedge
    // the next `readline()`. A notice printed awkwardly beats a chat that
    // stops accepting input.
    let printer: Box<dyn ExternalPrinter + Send> = Box::new(StdoutPrinter);
    if ready_tx.send(StartupResult::Ready(printer)).is_err() {
        return; // the caller gave up before we were ready
    }

    // `Prompt::history` is a one-time seed (recall from before this process
    // started); re-adding it on every turn would otherwise re-insert the
    // same lines throughout the session, since `history_ignore_dups` only
    // catches *consecutive* duplicates.
    let mut seeded = false;

    while let Some(Job::Read(prompt, resp)) = jobs_rx.blocking_recv() {
        if !seeded {
            for line in &prompt.history {
                let _ = editor.add_history_entry(line.as_str());
            }
            seeded = true;
        }
        editor.set_helper(Some(ChatHelper {
            completions: prompt.completions.clone(),
        }));

        let outcome = match editor.readline(prompt.text.as_str()) {
            Ok(line) => {
                // `auto_add_history` already added it in memory; this is
                // what makes it survive the process.
                if let Err(e) = editor.append_history(&history_path) {
                    tracing::debug!(error = %e, "failed to append chat history");
                }
                ReadOutcome::Line(line)
            }
            Err(ReadlineError::Interrupted) => ReadOutcome::Interrupt,
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(e) => ReadOutcome::Failed(e.to_string()),
        };
        // `resp.send` failing (the receiver already dropped) is not
        // reported anywhere here — see the `Job` doc for why that is a
        // real, separate, not-fixed-here hazard rather than a harmless
        // no-op: this line was already consumed either way, and it is not
        // recoverable from this side.
        let _ = resp.send(outcome);
    }

    let _ = editor.save_history(&history_path);
}

fn build_editor(
    color_mode: rustyline::ColorMode,
) -> rustyline::Result<Editor<ChatHelper, DefaultHistory>> {
    let config = Config::builder()
        .max_history_size(1000)?
        .history_ignore_dups(true)?
        .completion_type(CompletionType::List)
        .auto_add_history(true)
        .color_mode(color_mode)
        .build();
    let mut editor = Editor::<ChatHelper, DefaultHistory>::with_config(config)?;
    editor.bind_sequence(KeyEvent::alt('\r'), Cmd::Newline);
    editor.bind_sequence(
        KeyEvent::ctrl('C'),
        EventHandler::Conditional(Box::new(CtrlCHandler)),
    );
    Ok(editor)
}

/// The only [`ExternalPrinter`] this module uses (see `editor_thread_main`'s
/// comment on why `create_external_printer` is never called): plain stdout,
/// unsynchronized with an in-progress prompt. Not wired through [`Palette`]
/// itself because [`TerminalIo::notify`] already paints the line before
/// handing it to `print`; this just writes bytes.
struct StdoutPrinter;

impl ExternalPrinter for StdoutPrinter {
    fn print(&mut self, msg: String) -> rustyline::Result<()> {
        use std::io::Write as _;
        print!("{msg}");
        std::io::stdout().flush().map_err(ReadlineError::Io)
    }
}

/// The `rustyline::Helper`: Tab completion delegates to
/// [`forge_chat::Command::complete`] over the snapshot carried in the
/// current [`Prompt`] (never the filesystem, never the host — the editor
/// thread only ever sees data); multi-line validation is [`is_complete`];
/// hinting and highlighting are the crate's defaults (none).
struct ChatHelper {
    completions: CompletionSnapshot,
}

impl Completer for ChatHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(Command::complete(line, pos, &self.completions))
    }
}

impl Hinter for ChatHelper {
    type Hint = String;
}

impl Highlighter for ChatHelper {}

impl Validator for ChatHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        if is_complete(ctx.input()) {
            Ok(ValidationResult::Valid(None))
        } else {
            Ok(ValidationResult::Incomplete)
        }
    }
}

impl Helper for ChatHelper {}

/// Is `input` a submittable line, or should Enter insert a newline instead
/// (`Cmd::AcceptOrInsertLine`'s job, driven by this `Validator`)?
///
/// Two continuations, both reversible by the user just backspacing and
/// retyping: an odd number of trailing backslashes (a line-continuation
/// backslash, not an escaped one), or an odd number of ``` fences opened
/// so far (Review Focus 4: a pasted fenced code block is one input, not
/// five turns racing the model with each intermediate line).
fn is_complete(input: &str) -> bool {
    !ends_with_unescaped_backslash(input) && !is_inside_open_fence(input)
}

fn ends_with_unescaped_backslash(input: &str) -> bool {
    let trailing = input.chars().rev().take_while(|&c| c == '\\').count();
    trailing % 2 == 1
}

fn is_inside_open_fence(input: &str) -> bool {
    let mut open = false;
    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            open = !open;
        }
    }
    open
}

/// What Ctrl-C should do, decided from the line rustyline is currently
/// editing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CtrlC {
    /// A typed line: clear it, same as a shell. Two Ctrl-Cs that each clear
    /// a typed line must never look like a request to exit.
    ClearLine,
    /// Nothing typed: this Ctrl-C means it, so it becomes
    /// [`ReadOutcome::Interrupt`].
    Interrupt,
}

fn ctrl_c_command(line: &str) -> CtrlC {
    if line.is_empty() {
        CtrlC::Interrupt
    } else {
        CtrlC::ClearLine
    }
}

/// Bound to Ctrl-C (`custom-bindings`, see the design's §2.3 table): reads
/// the in-progress buffer from [`EventContext::line`] because
/// `ReadlineError::Interrupted` itself carries none, which is the only way
/// to tell "clear this line" from "I mean it" apart at the point rustyline
/// asks.
struct CtrlCHandler;

impl ConditionalEventHandler for CtrlCHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        Some(match ctrl_c_command(ctx.line()) {
            CtrlC::ClearLine => Cmd::Kill(Movement::WholeBuffer),
            CtrlC::Interrupt => Cmd::Interrupt,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review Focus 4: a pasted fenced block is one input, not five turns.
    #[test]
    fn an_unterminated_fence_is_incomplete_input() {
        assert!(!is_complete("fix this:\n```rust\nfn main() {}"));
        assert!(is_complete("fix this:\n```rust\nfn main() {}\n```"));
    }

    #[test]
    fn a_trailing_backslash_continues_the_line() {
        assert!(!is_complete("first line \\"));
        assert!(is_complete("first line"));
        // An escaped backslash is not a continuation.
        assert!(is_complete("a path c:\\\\tmp\\\\"));
    }

    #[test]
    fn an_ordinary_line_is_complete() {
        assert!(is_complete("explain the parser"));
        assert!(is_complete(""));
    }

    /// `ReadlineError::Interrupted` carries no buffer, so the distinction
    /// the spec's Ctrl-C table needs is made here: a typed line is cleared
    /// inside the editor and the chat is never told, while an empty line
    /// interrupts. Without this, two Ctrl-Cs used to clear two typed lines
    /// would look like a request to exit.
    #[test]
    fn ctrl_c_clears_a_typed_line_and_interrupts_an_empty_one() {
        assert_eq!(ctrl_c_command("half a thought"), CtrlC::ClearLine);
        assert_eq!(ctrl_c_command(""), CtrlC::Interrupt);
        assert_eq!(
            ctrl_c_command("   "),
            CtrlC::ClearLine,
            "whitespace is typed text"
        );
    }
}
