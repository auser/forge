//! `ChatIo` over plain stdin, for `forge chat < script.txt` and the like
//! (design §12.3).
//!
//! No `rustyline` here: a non-TTY stdin gets no editing, no history and no
//! completion, because there is no cursor to move and no human to
//! recall for. What it must still do is make the transcript legible: each
//! line read is echoed back with the same `"> "` gutter a real prompt
//! would show, so a piped run's stdout reads the same as an interactive
//! one even though nothing was ever drawn to a screen.

use std::io::BufRead as _;

use async_trait::async_trait;
use forge_chat::{ChatIo, Interactivity, Line, Prompt, ReadOutcome};

use super::palette::Palette;

/// Plain stdin, read line-by-line on a blocking task (locking and reading
/// stdin is a blocking syscall, same reasoning as [`super::terminal_io`]
/// but cheap enough not to need a whole dedicated thread — one
/// `spawn_blocking` call per line is enough).
pub struct PipedIo {
    palette: Palette,
}

impl PipedIo {
    pub fn new(palette: Palette) -> Self {
        Self { palette }
    }
}

#[async_trait]
impl ChatIo for PipedIo {
    async fn read(&mut self, prompt: Prompt) -> ReadOutcome {
        let outcome = tokio::task::spawn_blocking(read_one_line)
            .await
            .unwrap_or_else(|e| ReadOutcome::Failed(format!("stdin reader panicked: {e}")));
        // Echo what was "typed" under the same gutter a real prompt shows,
        // so the transcript is legible even though stdin was never a
        // screen. `prompt.text` is always `"> "` (§4.1); a piped run has no
        // completions or history to lose by ignoring the rest of `prompt`.
        if let ReadOutcome::Line(ref text) = outcome {
            println!("{}{text}", prompt.text);
        }
        outcome
    }

    fn write(&mut self, line: &Line) {
        println!("{}", self.palette.paint(line.style, &line.text));
    }

    fn notify(&mut self, line: &Line) {
        println!("{}", self.palette.paint(line.style, &line.text));
    }

    /// The *only* path here (§12.3): there is no terminal to type a Ctrl-C
    /// into between lines, so this is not a fallback the way it is for
    /// [`super::terminal_io::TerminalIo`] — it is how Ctrl-C works in a
    /// pipe at all.
    async fn interrupted(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }

    fn interactivity(&self) -> Interactivity {
        Interactivity::Batch
    }

    fn shutdown(&mut self) {}
}

/// One blocking read of a line from stdin, off the async runtime.
fn read_one_line() -> ReadOutcome {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => ReadOutcome::Eof,
        Ok(_) => {
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            ReadOutcome::Line(line)
        }
        Err(e) => ReadOutcome::Failed(e.to_string()),
    }
}
