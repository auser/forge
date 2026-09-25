//! Forge core: pluggable runtime traits, shared types, versioned event
//! protocol, and typed errors shared by every adapter (CLI, server, TUI).

pub mod embed;
pub mod error;
pub mod events;
pub mod execution;
pub mod graph;
pub mod model;
pub mod project;
pub mod router;
pub mod run;
pub mod session;
pub mod skill;
pub mod tool;

pub use error::ForgeError;
pub use events::{EVENT_SCHEMA_VERSION, Event, EventKind, MAX_TOOL_OUTPUT_BYTES, cap_tool_output};
pub use execution::{
    ApprovalPolicy, ExecRequest, ExecResult, ExecutionProvider, FileOp, FileOpResult, RiskLevel,
    RunningProcess, path_escapes_root,
};
pub use graph::{ContextHit, GraphStats, GrepMatch, ProjectGraph, SymbolInfo};
pub use model::{
    CompletionRequest, CompletionResponse, Message, ModelCapabilities, ModelProvider, Role, Usage,
};
pub use project::find_project_root;
pub use router::{Capability, DecisionRouter, RoutingDecision, RoutingRequest};
pub use run::RunState;
pub use session::SessionStore;
pub use skill::{Skill, SkillMeta, SkillRegistry};
pub use tool::{ToolCall, ToolDefinition, ToolResult};
