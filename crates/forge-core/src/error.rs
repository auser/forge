use thiserror::Error;

/// Typed error shared by every Forge crate and adapter.
#[derive(Debug, Error)]
pub enum ForgeError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("model provider error: {0}")]
    Provider(String),

    #[error("router error: {0}")]
    Router(String),

    #[error("execution error: {0}")]
    Execution(String),

    #[error("skill error: {0}")]
    Skill(String),

    #[error("project graph error: {0}")]
    Graph(String),

    #[error("session error: {0}")]
    Session(String),

    #[error("server error: {0}")]
    Server(String),

    /// A risky/destructive operation needs approval and no interactive
    /// terminal is available; the agent loop pauses for input on this.
    #[error("approval required: {description} (risk: {risk:?})")]
    ApprovalRequired {
        description: String,
        risk: crate::execution::RiskLevel,
    },

    /// Agent-loop-level failure (budget exhaustion, cancellation).
    #[error("agent error: {0}")]
    Agent(String),

    /// Returned by commands or backends that exist in the interface but are
    /// scheduled for a later phase.
    #[error("not yet implemented: {0}")]
    NotImplemented(String),
}

impl ForgeError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn provider(message: impl Into<String>) -> Self {
        Self::Provider(message.into())
    }

    pub fn router(message: impl Into<String>) -> Self {
        Self::Router(message.into())
    }

    pub fn execution(message: impl Into<String>) -> Self {
        Self::Execution(message.into())
    }

    pub fn skill(message: impl Into<String>) -> Self {
        Self::Skill(message.into())
    }

    pub fn graph(message: impl Into<String>) -> Self {
        Self::Graph(message.into())
    }

    pub fn session(message: impl Into<String>) -> Self {
        Self::Session(message.into())
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::Server(message.into())
    }

    pub fn agent(message: impl Into<String>) -> Self {
        Self::Agent(message.into())
    }

    pub fn not_implemented(what: impl Into<String>) -> Self {
        Self::NotImplemented(what.into())
    }
}
