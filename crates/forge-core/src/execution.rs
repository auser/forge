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
    /// Label used when forwarding a spawned process's output to tracing
    /// (e.g. "laya"); defaults to the command name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_label: Option<String>,
}

impl ExecRequest {
    pub fn new(command: impl Into<String>, risk: RiskLevel) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            risk,
            inherit_stdio: false,
            log_label: None,
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

    /// Risk classification: Read inside the project is Safe; Write/Edit
    /// inside the project are Risky; Delete, or any path escaping the
    /// project root (including a Read), is Destructive.
    ///
    /// The escape check runs for every variant, `Read` included: `Safe` is
    /// the one risk level every `ApprovalPolicy` — even `Deny` — lets
    /// through unconditionally (see `NativeExecution::check_approval`), so
    /// exempting `Read` from the escape check would let a model read
    /// arbitrary files outside the project (`../../secret`, `/etc/passwd`,
    /// ...) under any policy. An escaping read is classified `Destructive`
    /// rather than `Risky` because it can exfiltrate secrets outside the
    /// project in one shot, the same severity as an escaping write.
    pub fn risk(&self, project_root: &Path) -> RiskLevel {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_root_read_is_safe() {
        let root = Path::new("/project");
        let read = FileOp::Read {
            path: PathBuf::from("src/lib.rs"),
        };
        assert_eq!(read.risk(root), RiskLevel::Safe);
    }

    #[test]
    fn relative_escaping_read_is_gated() {
        let root = Path::new("/project");
        let read = FileOp::Read {
            path: PathBuf::from("../outside.txt"),
        };
        // Doc comment: "any path escaping the project root is Destructive" —
        // a `Read` must not be exempted from that rule, or a model could
        // read arbitrary files (`../../secret`, `/etc/passwd`, ...) through
        // a risk level that every `ApprovalPolicy` (including `Deny`) lets
        // through unconditionally.
        assert_eq!(read.risk(root), RiskLevel::Destructive);
    }

    #[test]
    fn absolute_escaping_read_is_gated() {
        let root = Path::new("/project");
        let read = FileOp::Read {
            path: PathBuf::from("/etc/passwd"),
        };
        assert_eq!(read.risk(root), RiskLevel::Destructive);
    }

    #[test]
    fn absolute_in_root_read_is_safe() {
        let root = Path::new("/project");
        let read = FileOp::Read {
            path: PathBuf::from("/project/src/lib.rs"),
        };
        assert_eq!(read.risk(root), RiskLevel::Safe);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOpResult {
    /// File contents for `Read`; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Whether the filesystem changed.
    pub changed: bool,
}

/// A long-running managed child process (e.g. the Laya adapter), spawned
/// without waiting. Output is forwarded to tracing by the implementation
/// (see `ExecRequest::log_label`).
#[async_trait]
pub trait RunningProcess: Send {
    /// Terminate the process (SIGKILL-equivalent).
    async fn kill(&mut self) -> Result<(), ForgeError>;

    /// Wait for exit and return the exit code (-1 when unknown/signaled).
    async fn wait(&mut self) -> Result<i32, ForgeError>;
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

    /// Spawn a long-running managed child without waiting for it. Used
    /// for services Forge manages (e.g. the Laya adapter).
    async fn spawn(&self, request: ExecRequest) -> Result<Box<dyn RunningProcess>, ForgeError>;

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
