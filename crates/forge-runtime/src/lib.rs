//! `AgentService`: the single transport-neutral entry point shared by the
//! CLI and the server. A run is a multi-turn agent loop (model → tool
//! calls → results → model) emitting append-only events into the session
//! store and onto a per-run broadcast channel — `subscribe` is the seam
//! the server's SSE transport consumes.

pub mod replay;
mod service;
mod tools;

pub use replay::{Replay, conversation_from_events, fit_to_budget, history_budget_chars};
pub use service::{
    AgentService, Attachment, ForkOutcome, NullSkillRegistry, RunOptions, RunOutcome, RunSummary,
    StartedRun,
};
pub use tools::{ToolDispatcher, tool_definitions};
