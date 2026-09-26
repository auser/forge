//! `ChatIo` over plain stdin, for `forge chat < script.txt` and the like
//! (design §12.3).
//!
//! No `rustyline` here: a non-TTY stdin gets no editing, no history and no
//! completion, because there is no cursor to move and no human to
//! recall for. What it must still do is make the transcript legible: each
//! line read is echoed back with the same `"> "` gutter a real prompt
//! would show, so a piped run's stdout reads the same as an interactive
//! one even though nothing was ever drawn to a screen.
//!
//! # Why a persistent reader thread, not one `spawn_blocking` per call
//!
//! `App::drive`'s main loop (`forge-chat`) has an *unconditional* `io.read`
//! arm in its `select!`, evaluated on every iteration regardless of which
//! branch actually wins (§6.2: an outstanding read must stay reachable
//! even mid-turn, so `/bg` works). When some other branch — an event from
//! an attached run, most commonly an `ApprovalRequested` — wins instead,
//! `select!` drops every losing branch's future, `read`'s included.
//!
//! A `read` built from a fresh `tokio::task::spawn_blocking(read_one_line)`
//! every call is not safe to drop like that: dropping the `JoinHandle`
//! future does not cancel the spawned blocking task (blocking tasks are
//! real OS threads, potentially parked in a real `read(2)` syscall, and
//! cannot be force-cancelled). That orphaned task keeps running,
//! un-awaited, and will still be the one to actually consume the *next*
//! bytes written to stdin — silently, since nothing is listening for its
//! result. The line the user typed to answer the question they can see on
//! screen (e.g. approving a pending operation) is the one that vanishes;
//! the *following* line is what the next fresh `read()` call sees instead.
//! This reproduced deterministically over a real pipe (see
//! `forge-cli/tests/chat.rs`'s `an_approval_is_answered_from_the_conversation`)
//! even though every in-process test of the same driver
//! (`forge-chat::app`'s `ScriptedIo`) stayed green — `ScriptedIo::read`
//! pops its queue and returns within one poll with no intervening await,
//! so it is never caught mid-flight the way a real blocking read is.
//!
//! The fix: read stdin on one dedicated `std::thread` for the whole chat,
//! pushed into a channel `read()` merely receives from.
//! `mpsc::UnboundedReceiver::recv` is documented cancel-safe — dropped
//! mid-`select!`, no message is lost; it is simply still there for the
//! next `recv().await`. The underlying blocking read only ever happens on
//! the dedicated thread, never inside a future `select!` can drop.
//!
//! # `SIGINT` mid-turn: why it is handled inside `read`, not `interrupted`
//!
//! The obvious place to check for `SIGINT` looks like [`ChatIo::interrupted`]
//! — that is what it is for. But `App::drive` only calls it in a momentary,
//! non-blocking peek taken right *after* `select!` resolves
//! (`App::interrupted_now`), never as a live arm of the `select!` itself
//! (the module doc on `forge_chat::app` explains why: `read` and
//! `interrupted` both take `&mut self`, so they cannot be two concurrent
//! arms of one `select!` — confirmed against the compiler in Task 7).
//! That peek only runs as a side effect of *something else* making
//! `select!` resolve. For the whole span a real `select!` branch is
//! genuinely pending on its own — a tool call actually running, most
//! commonly, since no event streams out of one mid-flight — nothing calls
//! `interrupted()` at all, checked or not: the code path is simply never
//! reached until that branch resolves on its own.
//!
//! Two distinct failures follow from that, both confirmed against the real
//! binary (`forge-cli/tests/chat.rs`'s
//! `sigint_cancels_the_turn_without_killing_the_chat`) before this fix,
//! neither reachable from the in-process `forge-chat::app` tests (whose
//! `ScriptedIo::interrupted` is a plain `Notify`, torn down by nothing):
//!
//! 1. A one-shot `tokio::signal::ctrl_c()`, freshly created for one peek and
//!    dropped right after (fired or not), leaves **no listener registered
//!    at all** for as long as nothing else happens. Tokio's own signal
//!    registry (`tokio::signal::registry`) marks an incoming signal
//!    `pending` on the *kind*, then immediately broadcasts and clears that
//!    flag to whatever listeners exist *right then*; a signal delivered
//!    while zero listeners exist is swapped back to `false` with nothing
//!    to tell, and is gone — not queued for whichever listener registers
//!    next. The `sleep 1` in the test above simply ran to completion as if
//!    nothing had been pressed, however many times it was retried.
//! 2. Fixing *that* (one persistent `tokio::signal::unix::Signal`, checked
//!    from the same instance every time — the same shape as the read-side
//!    fix, since a `Signal`'s `recv()` is a thin poll over an internal
//!    `watch::Receiver` that tracks "changed since I last looked," safe to
//!    drop mid-peek without losing the registration itself) closes gap 1
//!    but not gap 2: the peek still only runs once `select!` resolves for
//!    some *other* reason, so a `SIGINT` sent while a tool call is
//!    genuinely the only thing pending is now correctly *recorded*, but
//!    not *noticed*, until that call finishes on its own — at which point
//!    it is racing the run's own next step (the top-of-loop
//!    `cancel_requested` check in `forge-runtime::service`) rather than
//!    reliably winning. Measured: with only fix 1, this test's `sleep 1`
//!    passed about 3 times in 5 and hung out to its own 10-second timeout
//!    the other 2 — a coin flip between "the driver notices before the run
//!    races on to its own completion" and "it does not," not the
//!    unconditional guarantee "`Ctrl-C` cancels the turn" is supposed to
//!    be.
//!
//! The actual fix folds the interrupt into [`ChatIo::read`] itself, which
//! *is* a live, unconditionally-reconstructed arm of `App::drive`'s
//! `select!` every iteration — the same trick [`super::terminal_io`]
//! already uses for a typed Ctrl-C on a real terminal (see its module doc,
//! "Ctrl-C arrives through `read`, not `interrupted`"). `read()`'s body
//! now races the stdin-line channel against the persistent `sigint`
//! listener with `tokio::select!`; if the signal wins, it returns
//! [`ReadOutcome::Interrupt`], which `App::on_read` already handles
//! (`Signal::Interrupt`, the very code path `interrupted_now` would
//! otherwise have reached, just reliably rather than only sometimes).
//! `interrupted()` itself is kept as a second, harmless line of defence
//! for the narrow window between one `read()` call resolving and the next
//! one being constructed — not load-bearing any more, but not wrong to
//! keep either: the shared `Signal`'s own "changed since last seen"
//! bookkeeping means the two call sites can never double-fire on the same
//! delivery.
use std::io::BufRead as _;

use async_trait::async_trait;
use forge_chat::{ChatIo, Interactivity, Line, Prompt, ReadOutcome};
use forge_core::ForgeError;
use tokio::sync::mpsc;

use super::palette::Palette;

/// Plain stdin, read line-by-line on a dedicated thread and delivered
/// through a cancel-safe channel; `SIGINT` is a persistent listener raced
/// against that channel inside `read()` itself (see the module doc for why
/// both of those are load-bearing, not incidental).
pub struct PipedIo {
    palette: Palette,
    lines: mpsc::UnboundedReceiver<ReadOutcome>,
    /// One persistent `SIGINT` listener for this `PipedIo`'s whole life —
    /// see the module doc for why a fresh one per check silently drops
    /// signals that arrive while nothing is registered.
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
}

impl PipedIo {
    pub fn new(palette: Palette) -> Result<Self, ForgeError> {
        // Unbounded, but not actually unbounded in practice: the producer
        // thread below blocks inside `read_one_line` (a synchronous,
        // line-buffered stdin read) between every `send`, so it can never
        // race ahead and pile up more than the one line it just read
        // while waiting for `read()` to drain the previous one. What makes
        // `recv` cancel-safe to drop mid-`select!` (the module doc above)
        // is a property of the channel type regardless of bound; the
        // choice of `unbounded` over `channel(1)` here is just this
        // natural one-line-at-a-time backpressure, not a claim that
        // arbitrarily much stdin can queue up unread.
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                let outcome = read_one_line();
                // EOF/error are terminal for real stdin (it does not come
                // back once exhausted or broken): report it exactly once,
                // then stop — the channel closing after that is what makes
                // `read()` see `ReadOutcome::Eof` on every call from then
                // on (§12.3's "batch mode re-signals EOF on every otherwise
                // empty read"), with no thread left spinning on a stream
                // that will never produce anything else.
                let done = !matches!(outcome, ReadOutcome::Line(_));
                if tx.send(outcome).is_err() || done {
                    break;
                }
            }
        });
        #[cfg(unix)]
        let sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|e| ForgeError::config(format!("failed to install a SIGINT handler: {e}")))?;
        Ok(Self {
            palette,
            lines: rx,
            #[cfg(unix)]
            sigint,
        })
    }
}

#[async_trait]
impl ChatIo for PipedIo {
    #[cfg(unix)]
    async fn read(&mut self, prompt: Prompt) -> ReadOutcome {
        // `biased`, signal first: a `SIGINT` that arrives concurrently with
        // an already-queued line must cancel rather than let that line be
        // misread as an answer to whatever the signal was meant to
        // interrupt (mirrors the controller's own rule for an interrupt
        // during an approval — no denial is queued alongside the
        // cancellation either, `forge-chat::controller`'s Review Focus 2).
        let outcome = tokio::select! {
            biased;
            _ = self.sigint.recv() => ReadOutcome::Interrupt,
            line = self.lines.recv() => line.unwrap_or(ReadOutcome::Eof),
        };
        self.echo(&prompt, &outcome);
        outcome
    }

    #[cfg(not(unix))]
    async fn read(&mut self, prompt: Prompt) -> ReadOutcome {
        let outcome = self.lines.recv().await.unwrap_or(ReadOutcome::Eof);
        self.echo(&prompt, &outcome);
        outcome
    }

    fn write(&mut self, line: &Line) {
        println!("{}", self.palette.paint(line.style, &line.text));
    }

    fn notify(&mut self, line: &Line) {
        println!("{}", self.palette.paint(line.style, &line.text));
    }

    /// A second, harmless line of defence for the narrow window between
    /// one `read()` call resolving and the next being constructed — the
    /// real fix for a `SIGINT` mid-turn is folded into `read()` itself; see
    /// the module doc.
    #[cfg(unix)]
    async fn interrupted(&mut self) {
        let _ = self.sigint.recv().await;
    }

    #[cfg(not(unix))]
    async fn interrupted(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }

    fn interactivity(&self) -> Interactivity {
        Interactivity::Batch
    }

    fn shutdown(&mut self) {}
}

impl PipedIo {
    /// Echo what was "typed" under the same gutter a real prompt shows, so
    /// the transcript is legible even though stdin was never a screen.
    /// `prompt.text` is always `"> "` (§4.1); a piped run has no
    /// completions or history to lose by ignoring the rest of `prompt`.
    fn echo(&self, prompt: &Prompt, outcome: &ReadOutcome) {
        if let ReadOutcome::Line(text) = outcome {
            println!("{}{text}", prompt.text);
        }
    }
}

/// One blocking read of a line from stdin, off the async runtime (called
/// only from the dedicated thread `PipedIo::new` starts, never inline in a
/// future — see the module doc).
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
