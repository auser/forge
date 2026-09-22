//! `AgentService`: the single transport-neutral entry point shared by the
//! CLI and (in Phase D) the REST/SSE server. Every run emits append-only
//! events into the session store and onto a per-run broadcast channel —
//! `subscribe` is the seam the server's SSE transport will use.

mod service;

pub use service::{AgentService, NullSkillRegistry, RunOutcome};
