//! The terminal seam, and the transcript's one line type.
//!
//! Nothing here touches a terminal: `Line`/`Style` are data, and `ChatIo`
//! is the trait `forge-cli` implements over `rustyline` (TTY), over plain
//! stdin (piped), and a test implements over a script.

use async_trait::async_trait;

/// What a transcript line *is*, not how it looks. Colour is the writer's
/// decision (design §12.2), so a render test asserts on text alone and a
/// palette change cannot break one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Style {
    /// The assistant's answer, and the user's own echoed line: no gutter.
    Plain,
    /// Routing, skills, session notes, turn footers.
    Meta,
    /// A tool call the model requested.
    Tool,
    /// A tool result that succeeded.
    Ok,
    /// A question the user must answer (an approval).
    Warn,
    /// An error, a denial, a cancellation.
    Bad,
    /// Out-of-band news about a background job.
    Notice,
}

/// One line of transcript: its semantic class and its already-gutter-ed
/// text.
///
/// The gutter is applied by the constructor, exactly once, so there is a
/// single place where the §4.1 visual grammar is spelled out and no caller
/// can invent a second one. ASCII only — no box drawing, no arrows, no
/// emoji — so a Windows console, a `TERM=dumb` session and a CI log render
/// identically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Line {
    pub style: Style,
    pub text: String,
}

impl Line {
    /// The assistant's answer, verbatim: the one class with no gutter,
    /// because it is the thing the user asked for.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            style: Style::Plain,
            text: text.into(),
        }
    }

    /// Meta: routing decisions, skill activations, session notes. `  - `
    pub fn meta(text: impl AsRef<str>) -> Self {
        Self::gutter(Style::Meta, "  - ", text)
    }

    /// A requested tool call. `  * `
    pub fn tool(text: impl AsRef<str>) -> Self {
        Self::gutter(Style::Tool, "  * ", text)
    }

    /// A tool result that succeeded, indented under its call. `    -> `
    pub fn ok(text: impl AsRef<str>) -> Self {
        Self::gutter(Style::Ok, "    -> ", text)
    }

    /// A failure: a tool result that did not succeed, an error, a
    /// cancellation. Shares the `  ! ` gutter with [`Line::warn`] and
    /// differs only in style, because to a reader they are the same
    /// interruption of the flow.
    pub fn bad(text: impl AsRef<str>) -> Self {
        Self::gutter(Style::Bad, "  ! ", text)
    }

    /// A question that stops the turn until it is answered. `  ! `
    pub fn warn(text: impl AsRef<str>) -> Self {
        Self::gutter(Style::Warn, "  ! ", text)
    }

    /// The one-line summary that closes a turn. `  = `
    pub fn footer(text: impl AsRef<str>) -> Self {
        Self::gutter(Style::Meta, "  = ", text)
    }

    /// Out-of-band news (a background job changed state), which may land
    /// while a prompt is on screen. `  # `
    pub fn notice(text: impl AsRef<str>) -> Self {
        Self::gutter(Style::Notice, "  # ", text)
    }

    fn gutter(style: Style, gutter: &str, text: impl AsRef<str>) -> Self {
        Self {
            style,
            text: format!("{gutter}{}", text.as_ref()),
        }
    }
}

/// Completion candidates as data (design §9.2).
///
/// The snapshot travels *inside* [`Prompt`], so the completer is a pure
/// function of it: it never calls back into the host and never touches the
/// filesystem from the editor thread. The commands themselves are compiled
/// in; everything here is what only the host (or the last `/jobs`
/// listing) can know.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionSnapshot {
    /// Discovered skills, offered as `/name`.
    pub skills: Vec<String>,
    /// User-visible model names — already filtered by
    /// [`crate::host::ChatHost::models`], which is the one place test-only
    /// entries are dropped.
    pub models: Vec<String>,
    /// Run ids from the last `/jobs` snapshot.
    pub jobs: Vec<String>,
    /// The project's recent session ids.
    pub sessions: Vec<String>,
}

/// What to draw, and everything the editor needs to be useful, as data.
#[derive(Clone, Debug, Default)]
pub struct Prompt {
    /// Always `"> "` today: the approval question is a transcript line,
    /// not a second prompt (§8), so nothing swaps this mid-turn.
    pub text: String,
    /// Candidates as data (commands, skills, model names, job ids,
    /// session ids), so the completer is a pure function of a snapshot and
    /// never calls back into the host from the editor thread.
    pub completions: CompletionSnapshot,
    /// Lines to seed the editor's history with, oldest first. Data for the
    /// same reason: the pure layer decides what is recallable, the
    /// terminal implementation only stores it.
    pub history: Vec<String>,
}

/// The outcome of asking for one line — a closed set, so the pure layer
/// never has to interpret a raw error string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadOutcome {
    Line(String),
    /// Ctrl-C at the prompt (rustyline `Interrupted`).
    Interrupt,
    /// Ctrl-D on an empty line, or stdin EOF.
    Eof,
    /// The editor itself failed; the message is already user-facing.
    Failed(String),
}

/// Can this io ask the user a follow-up question?
///
/// A terminal can, so an exit with a live background job is confirmed;
/// piped stdin cannot, so EOF drains the queue and leaves (§12.3). One
/// enum rather than an `is_tty` bool because the question the loop asks is
/// never "is this a terminal", it is "may I ask".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Interactivity {
    /// A person is at the other end and can answer.
    Interactive,
    /// Input is a script: no questions, and EOF is the end.
    Batch,
}

/// Everything the chat needs from the terminal. One implementation over
/// rustyline (TTY), one over plain stdin (piped), one scripted (tests).
#[async_trait]
pub trait ChatIo: Send {
    /// Ask for one line of input. Returns the typed outcome, never a raw
    /// error string.
    async fn read(&mut self, prompt: Prompt) -> ReadOutcome;
    /// A transcript line (stdout).
    fn write(&mut self, line: &Line);
    /// An out-of-band line (a background job's state change) that may
    /// arrive while a prompt is on screen.
    fn notify(&mut self, line: &Line);
    /// Resolves when the user interrupts *without* it arriving as a read
    /// outcome: `SIGINT` in piped mode, and a channel the test fires in
    /// tests. On a TTY a typed Ctrl-C normally arrives as
    /// `ReadOutcome::Interrupt` instead (§6.2); both funnel into one pure
    /// `Controller::on_signal`, which is what makes "Ctrl-C cancels the
    /// turn" testable with no terminal and no signal.
    async fn interrupted(&mut self);
    /// Can this io ask the user a follow-up question? A terminal can (so
    /// an exit with a live job is confirmed); piped stdin cannot (so EOF
    /// drains the queue and leaves). §12.3.
    fn interactivity(&self) -> Interactivity;
    /// Flush and restore the terminal. Called once, on the way out.
    fn shutdown(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Line` is semantic, not styled: the writer decides colour, so a
    /// renderer test asserts on text alone and a palette change cannot
    /// break one.
    #[test]
    fn a_line_carries_text_and_a_semantic_style() {
        let line = Line::meta("routing: needle -> qwen3-coder (conf 0.91)");
        assert_eq!(line.style, Style::Meta);
        assert_eq!(line.text, "  - routing: needle -> qwen3-coder (conf 0.91)");
        assert!(line.text.is_ascii(), "the transcript is ASCII only");
    }

    #[test]
    fn every_constructor_uses_its_documented_gutter() {
        assert!(Line::tool("read_file src/main.rs").text.starts_with("  * "));
        assert!(Line::ok("ok (12 ms)").text.starts_with("    -> "));
        assert!(Line::bad("error: boom").text.starts_with("  ! "));
        assert!(
            Line::warn("approval needed: x (risky)")
                .text
                .starts_with("  ! ")
        );
        assert!(
            Line::footer("2 turns, 3 tool calls, 4.2s")
                .text
                .starts_with("  = ")
        );
        assert!(Line::notice("job 01J finished").text.starts_with("  # "));
        // Assistant text is the one class with no gutter: it is the answer.
        assert_eq!(
            Line::plain("the parser is recursive-descent").text,
            "the parser is recursive-descent"
        );
    }
}
