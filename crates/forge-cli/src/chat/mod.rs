//! `forge-cli`'s half of the interactive chat split: the two impure seams
//! (`ChatIo`) `forge-chat` declares and cannot implement itself, because
//! `forge-chat` stays pure — no terminal, no `rustyline`, no I/O syscalls
//! (`crates/forge-chat/src/lib.rs`).
//!
//! * [`terminal_io`] — `ChatIo` over `rustyline`, for a real TTY.
//! * [`piped_io`] — `ChatIo` over plain stdin, for `forge chat < script`.
//! * [`palette`] — the `NO_COLOR`/dumb-terminal decision both of the above
//!   share, so a piped run and an interactive one agree on when colour is
//!   off.
//! * [`host`] — `CliHost`, the `ChatHost` seam: config resolution, provider
//!   construction and credential detection, none of which `forge-chat` is
//!   allowed to see.
//!
//! Design: `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md`.

pub mod host;
pub mod palette;
pub mod piped_io;
pub mod terminal_io;
