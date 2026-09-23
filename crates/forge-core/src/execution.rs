use std::path::{Path, PathBuf};

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
    /// Ask for `Risky` and `Destructive` commands on an interactive terminal;
    /// without one, pause with an "approval required" error.
    Prompt,
    /// Ask only for `Destructive` commands; `Risky` commands run without
    /// asking. On a non-interactive stdin, destructive commands pause with an
    /// "approval required" error.
    PromptDestructive,
    /// Always refuse.
    Deny,
}

impl ApprovalPolicy {
    /// Parse the `approval` configuration string
    /// (`auto`|`prompt`|`prompt-dangerous`|`deny`).
    pub fn parse(value: &str) -> Result<Self, ForgeError> {
        match value {
            "auto" => Ok(Self::Auto),
            "prompt" => Ok(Self::Prompt),
            "prompt-dangerous" => Ok(Self::PromptDestructive),
            "deny" => Ok(Self::Deny),
            other => Err(ForgeError::config(format!(
                "invalid approval mode {other:?} (expected auto, prompt, prompt-dangerous, or deny)"
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
    /// Inherit the parent's stdio instead of capturing output (for
    /// long-running foreground servers); the result then carries empty
    /// stdout/stderr.
    #[serde(default)]
    pub inherit_stdio: bool,
}

impl ExecRequest {
    pub fn new(command: impl Into<String>, risk: RiskLevel) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            risk,
            inherit_stdio: false,
        }
    }

    pub fn inheriting_stdio(mut self) -> Self {
        self.inherit_stdio = true;
        self
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

/// A filesystem operation performed through the execution layer. File
/// reads/writes/edits never bypass `ExecutionProvider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileOp {
    Read {
        path: PathBuf,
    },
    Write {
        path: PathBuf,
        content: String,
    },
    /// Exact string replacement; fails when `old` is absent or ambiguous.
    Edit {
        path: PathBuf,
        old: String,
        new: String,
    },
    Delete {
        path: PathBuf,
    },
}

impl FileOp {
    pub fn path(&self) -> &Path {
        match self {
            Self::Read { path }
            | Self::Write { path, .. }
            | Self::Edit { path, .. }
            | Self::Delete { path } => path,
        }
    }

    /// Risk classification: Read is always Safe; Write/Edit inside the
    /// project are Risky; Delete, or any path escaping the project root,
    /// is Destructive.
    pub fn risk(&self, project_root: &Path) -> RiskLevel {
        if matches!(self, Self::Read { .. }) {
            return RiskLevel::Safe;
        }
        if path_escapes_root(self.path(), project_root) {
            return RiskLevel::Destructive;
        }
        match self {
            Self::Read { .. } => RiskLevel::Safe,
            Self::Write { .. } | Self::Edit { .. } => RiskLevel::Risky,
            Self::Delete { .. } => RiskLevel::Destructive,
        }
    }
}

/// Lexically check whether `path` (relative resolved against `root`, or
/// absolute) escapes `root` via `..` or absolute location.
pub fn path_escapes_root(path: &Path, root: &Path) -> bool {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    // Normalize dot segments lexically (no filesystem access needed).
    let mut parts: Vec<std::path::Component<'_>> = Vec::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                if parts
                    .last()
                    .is_some_and(|c| matches!(c, std::path::Component::Normal(_)))
                {
                    parts.pop();
                } else {
                    // `..` above the root prefix: escapes unless root is
                    // also reached via .. — treat conservatively as escape.
                    parts.push(component);
                }
            }
            other => parts.push(other),
        }
    }
    let normalized: PathBuf = parts.iter().collect();
    !normalized.starts_with(root)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOpResult {
    /// File contents for `Read`; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Whether the filesystem changed.
    pub changed: bool,
}

/// Runs commands and mutations. The runtime never invokes processes or
/// touches the filesystem outside this trait; implementations include
/// native, mock, container, MVM, remote.
#[async_trait]
pub trait ExecutionProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, ForgeError>;

    /// Perform a file operation. Implementations must classify risk via
    /// `FileOp::risk` and apply the same approval gating as `execute`.
    async fn file_op(&self, op: FileOp) -> Result<FileOpResult, ForgeError>;

    /// Execute bypassing approval gating. Invoked ONLY by the agent loop
    /// after an explicit, recorded user approval (an `ApprovalDecided`
    /// event). Default: delegates to `execute`.
    async fn execute_approved(&self, request: ExecRequest) -> Result<ExecResult, ForgeError> {
        self.execute(request).await
    }

    /// File-op counterpart of [`ExecutionProvider::execute_approved`].
    async fn file_op_approved(&self, op: FileOp) -> Result<FileOpResult, ForgeError> {
        self.file_op(op).await
    }
}
