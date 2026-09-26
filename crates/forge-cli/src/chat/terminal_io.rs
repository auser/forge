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
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
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

    let printer: Box<dyn ExternalPrinter + Send> = match editor.create_external_printer() {
        Ok(printer) => Box::new(printer),
        Err(e) => {
            // A dumb or unsupported terminal, most likely. A notice printed
            // slightly awkwardly (no coordination with an in-progress
            // prompt) beats a notice lost.
            tracing::debug!(error = %e, "external printer unavailable; notices fall back to stdout");
            Box::new(StdoutPrinter)
        }
    };
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

/// Fallback used when `create_external_printer` fails: plain, unstyled
/// stdout. Not wired through [`Palette`] because by the time this is
/// chosen the terminal has already told rustyline it cannot do the things
/// colour depends on.
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
