//! `AgentService`: the single transport-neutral entry point shared by the
//! CLI and the server. A run is a multi-turn agent loop (model → tool
//! calls → results → model) emitting append-only events into the session
//! store and onto a per-run broadcast channel — `subscribe` is the seam
//! the server's SSE transport consumes.

pub mod availability;
pub mod budget;
pub mod inspection;
pub mod observer;
pub mod replay;
mod service;
mod tasks;
mod tools;
mod workflow;

pub use availability::{AvailabilitySnapshot, AvailabilityState, ProviderAvailability};
pub use budget::{BudgetTrip, SpendTracker, completion_cost};

pub use replay::{Replay, conversation_from_events, fit_to_budget, history_budget_chars};
pub use service::{
    AgentService, Attachment, EffectJournal, ForkOutcome, NullSkillRegistry, RunOptions,
    RunOutcome, RunSummary, StartedRun,
};
pub use tasks::{DurableTaskState, TaskInspector, TaskRoute, TaskSpend, TaskView};
pub use tools::{ToolDispatcher, tool_definitions};
pub use workflow::{
    DevelopmentWorkflowOutcome, resume_development_workflow, run_development_workflow,
};
