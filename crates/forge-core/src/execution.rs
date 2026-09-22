use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ForgeError;

/// How dangerous a command is; drives approval gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Safe,
    Risky,
    Destructive,
}

/// Policy for gating `Risky`/`Destructive` commands. `Safe` commands always
/// run regardless of policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Run without asking.
    Auto,
    /// Ask on an interactive terminal; without one, pause with an
    /// "approval required" error.
    Prompt,
    /// Always refuse.
    Deny,
}

impl ApprovalPolicy {
    /// Parse the `approval` configuration string (`auto`|`prompt`|`deny`).
    pub fn parse(value: &str) -> Result<Self, ForgeError> {
        match value {
            "auto" => Ok(Self::Auto),
            "prompt" => Ok(Self::Prompt),
            "deny" => Ok(Self::Deny),
            other => Err(ForgeError::config(format!(
                "invalid approval mode {other:?} (expected auto, prompt, or deny)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub risk: RiskLevel,
}

impl ExecRequest {
    pub fn new(command: impl Into<String>, risk: RiskLevel) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            risk,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecResult {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Runs commands and mutations. The runtime never invokes processes outside
/// this trait; implementations include native, mock, container, MVM, remote.
#[async_trait]
pub trait ExecutionProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, ForgeError>;
}
