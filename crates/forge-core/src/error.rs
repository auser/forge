use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Closed failure classes that may affect whether a model is eligible for a
/// later routing decision. The message remains diagnostic; policy keys off
/// this enum rather than provider-specific prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureKind {
    Authentication,
    Transient,
    InvalidRequest,
    Entitlement,
}

impl fmt::Display for ProviderFailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => "authentication",
            Self::Transient => "transient",
            Self::InvalidRequest => "invalid_request",
            Self::Entitlement => "entitlement",
        })
    }
}

/// Typed error shared by every Forge crate and adapter.
#[derive(Debug, Error)]
pub enum ForgeError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("model provider error: {0}")]
    Provider(String),

    /// The provider explicitly refused a generation request because its
    /// current rate limit was exhausted. `retry_after_seconds` is present
    /// only when the provider supplied a valid `Retry-After` delta; Forge
    /// never invents provider capacity or a reset time.
    #[error(
        "model provider {provider} rate limited (HTTP 429){retry_after}",
        retry_after = retry_after_suffix(*retry_after_seconds)
    )]
    ProviderRateLimited {
        provider: String,
        retry_after_seconds: Option<u64>,
    },

    #[error("model provider {provider} failed ({kind}){status_suffix}: {message}",
        status_suffix = http_status_suffix(*status)
    )]
    ProviderFailure {
        provider: String,
        kind: ProviderFailureKind,
        status: Option<u16>,
        message: String,
    },

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

    /// A run was asked to start in a session that already has one in flight.
    ///
    /// Its own variant, not a [`Session`](Self::Session) string, because the
    /// refusal is a *conflict* rather than a missing or broken session: the
    /// REST adapter answers 409 on this and 404 on `Session`, and the choice
    /// has to come from the type. One live run per session is a correctness
    /// requirement, not a policy — two runs writing into one session log
    /// interleave their events, and the interleave corrupts the next replay
    /// of that session (see `forge_runtime::replay`).
    #[error(
        "session {session_id} already has a run in flight ({run_id}); wait for it or cancel it"
    )]
    SessionBusy { session_id: String, run_id: String },

    #[error("server error: {0}")]
    Server(String),

    /// A risky/destructive operation needs approval and no interactive
    /// terminal is available; the agent loop pauses for input on this.
    #[error("approval required: {description} (risk: {risk:?})")]
    ApprovalRequired {
        description: String,
        risk: crate::execution::RiskLevel,
    },

    /// Agent-loop-level failure (budget exhaustion, tool dispatch).
    #[error("agent error: {0}")]
    Agent(String),

    /// The run was cancelled — `forge cancel`, the in-process token, or the
    /// cross-process marker file. Its own variant so adapters can classify
    /// a cancellation from the type instead of the message (see
    /// [`crate::run::RunState::of_error`]); the Display text still says
    /// "cancelled" for the log and for callers that only have a string.
    #[error("run cancelled: {0}")]
    Cancelled(String),
}

impl ForgeError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn provider(message: impl Into<String>) -> Self {
        Self::Provider(message.into())
    }

    pub fn provider_rate_limited(
        provider: impl Into<String>,
        retry_after_seconds: Option<u64>,
    ) -> Self {
        Self::ProviderRateLimited {
            provider: provider.into(),
            retry_after_seconds,
        }
    }

    pub fn provider_failure(
        provider: impl Into<String>,
        kind: ProviderFailureKind,
        status: Option<u16>,
        message: impl Into<String>,
    ) -> Self {
        Self::ProviderFailure {
            provider: provider.into(),
            kind,
            status,
            message: message.into(),
        }
    }

    /// Classification available to routing health without parsing a display
    /// string. Generic provider errors deliberately remain unclassified.
    pub fn provider_failure_kind(&self) -> Option<ProviderFailureKind> {
        match self {
            Self::ProviderFailure { kind, .. } => Some(*kind),
            _ => None,
        }
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

    pub fn session_busy(session_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self::SessionBusy {
            session_id: session_id.into(),
            run_id: run_id.into(),
        }
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::Server(message.into())
    }

    pub fn agent(message: impl Into<String>) -> Self {
        Self::Agent(message.into())
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::Cancelled(message.into())
    }
}

fn retry_after_suffix(seconds: Option<u64>) -> String {
    seconds
        .map(|seconds| format!("; retry after {seconds} seconds"))
        .unwrap_or_default()
}

fn http_status_suffix(status: Option<u16>) -> String {
    status
        .map(|status| format!("; HTTP {status}"))
        .unwrap_or_default()
}
