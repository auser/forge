//! `AgentService`: the single transport-neutral entry point shared by the
//! CLI and the server. A run is a multi-turn agent loop (model → tool
//! calls → results → model) emitting append-only events into the session
//! store and onto a per-run broadcast channel — `subscribe` is the seam
//! the server's SSE transport consumes.

mod service;
mod tools;

pub use service::{AgentService, NullSkillRegistry, ResumeSeed, RunOptions, RunOutcome};
pub use tools::{ToolDispatcher, tool_definitions};
