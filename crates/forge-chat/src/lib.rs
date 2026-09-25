//! Forge's interactive chat, as a **pure** crate: no terminal, no
//! `rustyline`, no I/O syscalls.
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
//!
//! Design: `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md`.

pub mod host;
pub mod io;

pub use host::{
    ChatHost, ConfigLine, ContextLine, Environment, HostChange, ModelChoice, NeedleState,
    SkillChoice,
};
pub use io::{ChatIo, CompletionSnapshot, Interactivity, Line, Prompt, ReadOutcome, Style};
