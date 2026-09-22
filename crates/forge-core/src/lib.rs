//! Forge core: pluggable runtime traits, shared types, versioned event
//! protocol, and typed errors shared by every adapter (CLI, server, TUI).

pub mod error;
pub mod events;
pub mod execution;
pub mod graph;
pub mod model;
pub mod project;
pub mod router;
pub mod session;
pub mod skill;

pub use error::ForgeError;
pub use events::{EVENT_PROTOCOL_VERSION, Event, EventKind};
pub use execution::{ApprovalPolicy, ExecRequest, ExecResult, ExecutionProvider, RiskLevel};
pub use graph::{GraphStats, GrepMatch, ProjectGraph, SymbolInfo};
pub use model::{
    CompletionRequest, CompletionResponse, Message, ModelCapabilities, ModelProvider, Role, Usage,
};
pub use project::find_project_root;
pub use router::{Capability, DecisionRouter, RoutingDecision, RoutingRequest};
pub use session::SessionStore;
pub use skill::{Skill, SkillMeta, SkillRegistry};
