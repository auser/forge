use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ForgeError;

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
    pub required_capabilities: Vec<Capability>,
    #[serde(default)]
    pub candidates: Vec<String>,
}

impl RoutingRequest {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            required_capabilities: Vec::new(),
            candidates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub selected_model: String,
    pub confidence: f32,
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
