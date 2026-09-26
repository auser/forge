//! Forge's interactive chat, with the terminal held at arm's length: no
//! terminal, no `rustyline`, no TTY, and nothing here that can only be
//! exercised by a person typing.
//!
//! **What this crate does *not* claim.** It is not free of I/O. Everything
//! outside [`app`] is a pure function, but `app` drives a real
//! `AgentService` handed to it by [`ChatHost::service`], and several of
//! that service's methods are synchronous filesystem reads taken straight
//! on the executor: [`app`]'s `refresh_completions` (`list_runs()` —
//! which re-reads and re-parses every session's whole JSONL log — plus
//! `list_sessions()`), `events_for`, and `fork_session`. The multi-thread
//! runtime `forge-cli` builds keeps that off the critical path, so it is
//! latency rather than a stall, and §14 has the bounded-listing follow-up.
//! Stated plainly here because the earlier "no I/O syscalls" wording was
//! not true and would mislead whoever next adds a `ChatHost` method: the
//! rule this crate actually keeps is **no terminal**, and filesystem
//! access **only** through `AgentService`.
//!
//! This is `forge-acp`'s split applied again — there, `dispatch` is pure
//! and `server` owns the I/O, which is why the whole forge→ACP mapping is
//! unit-testable without a process. Here the impure half lives *outside*
//! the crate entirely: `forge-cli` implements the two seams declared here,
//! so `forge-chat` cannot accidentally grow a terminal dependency and
//! `cargo test -p forge-chat` can never need a TTY.
//!
//! * [`io`] — the [`ChatIo`] seam (the terminal) plus [`Line`]/[`Style`],
//!   the transcript's only output type.
//! * [`host`] — the [`ChatHost`] seam: everything config-shaped, as plain
//!   owned data.
//! * [`render`] — the `Event` → [`Line`] mapping, as a pure function of a
//!   [`TranscriptState`].
//! * [`command`] — slash parsing and Tab completion: what a submitted line
//!   *says*.
//! * [`controller`] — the input/signal state machine: what may happen now.
//!   The §6.3 Ctrl-C table lives there, which is why "Ctrl-C cancels the
//!   turn and does not quit" is a unit test and not a hope.
//! * [`app`] — the async driver: the only module that actually runs
//!   anything, by executing the `Controller`'s `Action`s against a real
//!   `AgentService` and a `ChatIo`. Tested in-process against
//!   [`testing::ScriptedIo`] and [`testing::FakeHost`] — no terminal, no
//!   process.
//!
//! Design: `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md`.

pub mod app;
pub mod command;
pub mod controller;
pub mod host;
pub mod io;
pub mod render;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use app::{SessionStart, Start, run};
pub use command::{APPROVAL_MODES, COMMANDS, Command, Parsed, help_lines};
pub use controller::{Action, ChatState, Controller, Signal};
pub use host::{
    ChatHost, ConfigLine, ContextLine, Environment, HostChange, ModelChoice, NeedleState,
    SkillChoice,
};
pub use io::{ChatIo, CompletionSnapshot, Interactivity, Line, Prompt, ReadOutcome, Style};
pub use render::{TranscriptState, summarize_call};
