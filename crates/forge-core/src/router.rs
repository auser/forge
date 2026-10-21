use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ForgeError;

/// The development phase whose work is being routed.
///
/// Routers may spend more on planning or review while preferring an
/// economical capable model for execution. `General` preserves the behavior
/// of callers that do not run Forge's development workflow.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPhase {
    #[default]
    General,
    Plan,
    Execute,
    Review,
}

/// A capability a task requires from the selected model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Streaming,
    Tools,
    StructuredOutput,
    Vision,
    MinContext(usize),
}

impl Capability {
    pub fn satisfied_by(&self, caps: &crate::model::ModelCapabilities) -> bool {
        match self {
            Self::Streaming => caps.streaming,
            Self::Tools => caps.tools,
            Self::StructuredOutput => caps.structured_output,
            Self::Vision => caps.vision,
            Self::MinContext(min) => caps.max_context >= *min,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRequest {
    pub task: String,
    #[serde(default)]
    pub phase: RoutingPhase,
    #[serde(default)]
    pub required_capabilities: Vec<Capability>,
    #[serde(default)]
    pub candidates: Vec<String>,
}

impl RoutingRequest {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            phase: RoutingPhase::General,
            required_capabilities: Vec::new(),
            candidates: Vec::new(),
        }
    }

    pub fn for_phase(task: impl Into<String>, phase: RoutingPhase) -> Self {
        Self {
            phase,
            ..Self::new(task)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub selected_model: String,
    pub confidence: f64,
    pub router_name: String,
    pub fallback_used: bool,
    pub reason: String,
}

/// Chooses a model (and later workflows, skills, and budgets) for a task.
/// Implemented by static rules, mocks, and System One-compatible HTTP
/// services such as TypeSafe Jev, Kev, or local variants.
#[async_trait]
pub trait DecisionRouter: Send + Sync {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError>;
}
