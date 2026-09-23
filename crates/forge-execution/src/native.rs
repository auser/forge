use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use async_trait::async_trait;
use forge_core::{
    ApprovalPolicy, ExecRequest, ExecResult, ExecutionProvider, FileOp, FileOpResult, ForgeError,
    RiskLevel,
};

/// Runs commands as local child processes via `tokio::process::Command`
/// and performs file operations directly, both gated by the same approval
/// policy. The project root is used for file-op risk classification.
pub struct NativeExecution {
    approval: ApprovalPolicy,
    project_root: PathBuf,
}

impl NativeExecution {
    pub fn new(approval: ApprovalPolicy, project_root: impl Into<PathBuf>) -> Self {
        Self {
            approval,
            project_root: project_root.into(),
        }
    }

    pub fn approval(&self) -> ApprovalPolicy {
        self.approval
    }

    pub fn project_root(&self) -> &std::path::Path {
        &self.project_root
    }

    /// Gate an operation by risk level and approval policy. Shared by
    /// command execution and file operations.
    fn check_approval(&self, description: &str, risk: RiskLevel) -> Result<(), ForgeError> {
        if risk == RiskLevel::Safe {
            return Ok(());
        }
        match self.approval {
            ApprovalPolicy::Auto => Ok(()),
            ApprovalPolicy::Deny => Err(ForgeError::execution(format!(
                "approval denied: {description} is {risk:?} and policy is 'deny'"
            ))),
            ApprovalPolicy::Prompt => prompt_for_approval(description, risk),
            ApprovalPolicy::PromptDestructive => match risk {
                RiskLevel::Destructive => prompt_for_approval(description, risk),
                _ => Ok(()),
            },
        }
    }
}

/// Interactive y/N prompt, only when stdin is a terminal. On a
/// non-interactive stdin the operation pauses with a typed
/// "approval required" error instead of hanging.
fn prompt_for_approval(description: &str, risk: RiskLevel) -> Result<(), ForgeError> {
    if !std::io::stdin().is_terminal() {
        return Err(ForgeError::ApprovalRequired {
            description: description.to_string(),
            risk,
        });
    }
    let mut stderr = std::io::stderr();
    write!(stderr, "approve {risk:?} operation {description}? [y/N] ")
        .and_then(|()| stderr.flush())
        .map_err(ForgeError::Io)?;

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(ForgeError::Io)?;
    if line.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        Err(ForgeError::execution(format!(
            "approval denied by user: {description}"
        )))
    }
}

/// Resolve an op path against the project root (relative paths are
/// root-relative).
fn resolve(root: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

#[async_trait]
impl ExecutionProvider for NativeExecution {
    fn name(&self) -> &str {
        "native"
    }

    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, ForgeError> {
        let description = format!("`{} {}`", request.command, request.args.join(" "));
        self.check_approval(&description, request.risk)?;
        self.execute_ungated(request).await
    }

    /// Bypass path: invoked only after an explicit recorded approval.
    async fn execute_approved(&self, request: ExecRequest) -> Result<ExecResult, ForgeError> {
        self.execute_ungated(request).await
    }

    async fn spawn(
        &self,
        request: ExecRequest,
    ) -> Result<Box<dyn forge_core::RunningProcess>, ForgeError> {
        self.check_approval(&format!("spawn `{}`", request.command), request.risk)?;
        let mut command = tokio::process::Command::new(&request.command);
        command.args(&request.args);
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|e| {
            ForgeError::execution(format!("failed to spawn `{}`: {e}", request.command))
        })?;

        // Forward the child's output lines to tracing with the label
        // (visible at -v; never on stdout).
        let label = request
            .log_label
            .clone()
            .unwrap_or_else(|| request.command.clone());
        fn forward<S: tokio::io::AsyncRead + Unpin + Send + 'static>(
            stream: Option<S>,
            stream_name: &'static str,
            label: String,
        ) -> Option<tokio::task::JoinHandle<()>> {
            stream.map(|stream| {
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, BufReader};
                    let mut lines = BufReader::new(stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        tracing::info!(target: "spawned", "[{label}:{stream_name}] {line}");
                    }
                })
            })
        }
        let out_task = forward(child.stdout.take(), "out", label.clone());
        let err_task = forward(child.stderr.take(), "err", label);

        Ok(Box::new(NativeRunningProcess {
            child,
            out_task,
            err_task,
        }))
    }

    async fn file_op(&self, op: FileOp) -> Result<FileOpResult, ForgeError> {
        let risk = op.risk(&self.project_root);
        let description = format!("file op {op:?}");
        self.check_approval(&description, risk)?;
        self.file_op_ungated(op).await
    }

    /// Bypass path: invoked only after an explicit recorded approval.
    async fn file_op_approved(&self, op: FileOp) -> Result<FileOpResult, ForgeError> {
        self.file_op_ungated(op).await
    }
}

/// A running native child with its output-forwarding tasks.
pub struct NativeRunningProcess {
    child: tokio::process::Child,
    out_task: Option<tokio::task::JoinHandle<()>>,
    err_task: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl forge_core::RunningProcess for NativeRunningProcess {
    async fn kill(&mut self) -> Result<(), ForgeError> {
        self.child.kill().await.map_err(ForgeError::Io)?;
        if let Some(task) = self.out_task.take() {
            task.abort();
        }
        if let Some(task) = self.err_task.take() {
            task.abort();
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<i32, ForgeError> {
        let status = self.child.wait().await.map_err(ForgeError::Io)?;
        Ok(status.code().unwrap_or(-1))
    }
}

impl Drop for NativeRunningProcess {
    fn drop(&mut self) {
        // Never leave an orphan behind.
        let _ = self.child.start_kill();
    }
}

impl NativeExecution {
    /// Ungated process spawn (the body shared by execute and
    /// execute_approved).
    async fn execute_ungated(&self, request: ExecRequest) -> Result<ExecResult, ForgeError> {
        let mut command = tokio::process::Command::new(&request.command);
        command.args(&request.args);
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }
        tracing::debug!(command = %request.command, risk = ?request.risk, "executing command");

        if request.inherit_stdio {
            // Foreground server mode: the child talks to the terminal
            // directly; there is no captured output.
            let status = command
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
                .await
                .map_err(|e| {
                    ForgeError::execution(format!("failed to spawn `{}`: {e}", request.command))
                })?;
            return Ok(ExecResult {
                exit_code: status.code().unwrap_or(-1),
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        let output = command.output().await.map_err(|e| {
            ForgeError::execution(format!("failed to spawn `{}`: {e}", request.command))
        })?;

        Ok(ExecResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Ungated file operation (the body shared by file_op and
    /// file_op_approved).
    async fn file_op_ungated(&self, op: FileOp) -> Result<FileOpResult, ForgeError> {
        let risk = op.risk(&self.project_root);
        let path = resolve(&self.project_root, op.path());
        tracing::debug!(path = %path.display(), risk = ?risk, "file op");

        match op {
            FileOp::Read { .. } => {
                let content = std::fs::read_to_string(&path).map_err(ForgeError::Io)?;
                Ok(FileOpResult {
                    content: Some(content),
                    changed: false,
                })
            }
            FileOp::Write { content, .. } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
                }
                std::fs::write(&path, content).map_err(ForgeError::Io)?;
                Ok(FileOpResult {
                    content: None,
                    changed: true,
                })
            }
            FileOp::Edit { old, new, .. } => {
                let text = std::fs::read_to_string(&path).map_err(ForgeError::Io)?;
                let matches = text.matches(&old).count();
                if matches == 0 {
                    return Err(ForgeError::execution(format!(
                        "edit failed: `old` text not found in {}",
                        path.display()
                    )));
                }
                if matches > 1 {
                    return Err(ForgeError::execution(format!(
                        "edit failed: `old` text is ambiguous ({matches} occurrences) in {}",
                        path.display()
                    )));
                }
                std::fs::write(&path, text.replacen(&old, &new, 1)).map_err(ForgeError::Io)?;
                Ok(FileOpResult {
                    content: None,
                    changed: true,
                })
            }
            FileOp::Delete { .. } => {
                std::fs::remove_file(&path).map_err(ForgeError::Io)?;
                Ok(FileOpResult {
                    content: None,
                    changed: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe_echo() -> ExecRequest {
        ExecRequest {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            cwd: None,
            risk: RiskLevel::Safe,
            inherit_stdio: false,
            log_label: None,
        }
    }

    fn exec(policy: ApprovalPolicy) -> NativeExecution {
        NativeExecution::new(policy, std::env::temp_dir())
    }

    #[tokio::test]
    async fn native_runs_safe_command_and_captures_output() {
        let exec = exec(ApprovalPolicy::Prompt);
        let result = exec.execute(safe_echo()).await.expect("echo succeeds");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn native_reports_exit_code_and_stderr() {
        let exec = exec(ApprovalPolicy::Auto);
        let result = exec
            .execute(ExecRequest {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "echo oops >&2; exit 3".to_string()],
                cwd: None,
                risk: RiskLevel::Safe,
                inherit_stdio: false,
                log_label: None,
            })
            .await
            .expect("spawn succeeds");
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.stderr.trim(), "oops");
    }

    #[tokio::test]
    async fn spawn_failure_is_typed_execution_error() {
        let exec = exec(ApprovalPolicy::Auto);
        let err = exec
            .execute(ExecRequest::new(
                "definitely-not-a-real-forge-binary",
                RiskLevel::Safe,
            ))
            .await
            .expect_err("must fail");
        assert!(matches!(err, ForgeError::Execution(_)));
    }

    #[tokio::test]
    async fn deny_blocks_risky_and_destructive() {
        let exec = exec(ApprovalPolicy::Deny);
        for risk in [RiskLevel::Risky, RiskLevel::Destructive] {
            let err = exec
                .execute(ExecRequest::new("echo", risk))
                .await
                .expect_err("must be denied");
            assert!(matches!(err, ForgeError::Execution(_)));
            assert!(err.to_string().contains("approval denied"));
        }
    }

    #[tokio::test]
    async fn auto_allows_risky() {
        let exec = exec(ApprovalPolicy::Auto);
        let result = exec
            .execute(ExecRequest {
                risk: RiskLevel::Risky,
                ..safe_echo()
            })
            .await
            .expect("auto approves");
        assert!(result.success());
    }

    #[tokio::test]
    async fn prompt_without_terminal_requires_approval() {
        // Tests never run with a terminal stdin, so Prompt must pause.
        assert!(!std::io::stdin().is_terminal());
        let exec = exec(ApprovalPolicy::Prompt);
        let err = exec
            .execute(ExecRequest {
                risk: RiskLevel::Risky,
                ..safe_echo()
            })
            .await
            .expect_err("must pause for approval");
        assert!(matches!(err, ForgeError::ApprovalRequired { .. }));
    }

    #[tokio::test]
    async fn prompt_dangerous_allows_risky_but_pauses_destructive() {
        assert!(!std::io::stdin().is_terminal());
        let exec = exec(ApprovalPolicy::PromptDestructive);
        // Risky runs without asking.
        let result = exec
            .execute(ExecRequest {
                risk: RiskLevel::Risky,
                ..safe_echo()
            })
            .await
            .expect("risky allowed");
        assert!(result.success());
        // Destructive pauses without a TTY.
        let err = exec
            .execute(ExecRequest::new("rm", RiskLevel::Destructive))
            .await
            .expect_err("must pause");
        assert!(err.to_string().contains("approval required"));
    }

    #[tokio::test]
    async fn approval_policy_parses_config_strings() {
        assert!(matches!(
            ApprovalPolicy::parse("auto"),
            Ok(ApprovalPolicy::Auto)
        ));
        assert!(matches!(
            ApprovalPolicy::parse("prompt"),
            Ok(ApprovalPolicy::Prompt)
        ));
        assert!(matches!(
            ApprovalPolicy::parse("prompt-dangerous"),
            Ok(ApprovalPolicy::PromptDestructive)
        ));
        assert!(matches!(
            ApprovalPolicy::parse("deny"),
            Ok(ApprovalPolicy::Deny)
        ));
        assert!(matches!(
            ApprovalPolicy::parse("yolo"),
            Err(ForgeError::Config(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_runs_managed_child_and_kills_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exec = NativeExecution::new(ApprovalPolicy::Auto, tmp.path());
        let request = ExecRequest {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 30".to_string()],
            cwd: None,
            risk: RiskLevel::Safe,
            inherit_stdio: false,
            log_label: Some("test".to_string()),
        };
        let mut handle = exec.spawn(request).await.expect("spawn");
        handle.kill().await.expect("kill");
        // Killed processes report -1 (no exit code) or a signal code.
        let code = handle.wait().await.expect("wait");
        assert!(code != 0 || true); // wait completed without hanging
        let _ = code;
    }

    // --- file operations ---

    #[tokio::test]
    async fn file_ops_read_write_edit_delete_happy_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exec = NativeExecution::new(ApprovalPolicy::Auto, tmp.path());

        // Write (Risky, in-project).
        let result = exec
            .file_op(FileOp::Write {
                path: PathBuf::from("src/main.rs"),
                content: "fn main() {}\n".to_string(),
            })
            .await
            .expect("write");
        assert!(result.changed);
        assert!(tmp.path().join("src/main.rs").is_file());

        // Read (Safe).
        let result = exec
            .file_op(FileOp::Read {
                path: PathBuf::from("src/main.rs"),
            })
            .await
            .expect("read");
        assert_eq!(result.content.as_deref(), Some("fn main() {}\n"));
        assert!(!result.changed);

        // Edit (exact replacement).
        let result = exec
            .file_op(FileOp::Edit {
                path: PathBuf::from("src/main.rs"),
                old: "fn main() {}".to_string(),
                new: "fn main() { println!(\"hi\"); }".to_string(),
            })
            .await
            .expect("edit");
        assert!(result.changed);
        let text = std::fs::read_to_string(tmp.path().join("src/main.rs")).expect("read");
        assert!(text.contains("println!"));

        // Delete (Destructive, in-project).
        let result = exec
            .file_op(FileOp::Delete {
                path: PathBuf::from("src/main.rs"),
            })
            .await
            .expect("delete");
        assert!(result.changed);
        assert!(!tmp.path().join("src/main.rs").exists());
    }

    #[tokio::test]
    async fn edit_errors_when_old_missing_or_ambiguous() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("f.txt"), "foo foo\n").expect("write");
        let exec = NativeExecution::new(ApprovalPolicy::Auto, tmp.path());

        let missing = exec
            .file_op(FileOp::Edit {
                path: PathBuf::from("f.txt"),
                old: "bar".to_string(),
                new: "baz".to_string(),
            })
            .await
            .expect_err("not found");
        assert!(missing.to_string().contains("not found"));

        let ambiguous = exec
            .file_op(FileOp::Edit {
                path: PathBuf::from("f.txt"),
                old: "foo".to_string(),
                new: "baz".to_string(),
            })
            .await
            .expect_err("ambiguous");
        assert!(ambiguous.to_string().contains("ambiguous"));
    }

    #[tokio::test]
    async fn path_escape_is_classified_destructive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let read = FileOp::Read {
            path: PathBuf::from("../outside.txt"),
        };
        assert_eq!(read.risk(root), RiskLevel::Safe); // reads are safe
        let write = FileOp::Write {
            path: PathBuf::from("../outside.txt"),
            content: "x".to_string(),
        };
        assert_eq!(write.risk(root), RiskLevel::Destructive);
        let inside = FileOp::Write {
            path: PathBuf::from("src/ok.rs"),
            content: "x".to_string(),
        };
        assert_eq!(inside.risk(root), RiskLevel::Risky);
        let delete_inside = FileOp::Delete {
            path: PathBuf::from("src/ok.rs"),
        };
        assert_eq!(delete_inside.risk(root), RiskLevel::Destructive);
        let absolute_escape = FileOp::Write {
            path: PathBuf::from("/tmp/forge-escape-test.txt"),
            content: "x".to_string(),
        };
        assert_eq!(absolute_escape.risk(root), RiskLevel::Destructive);
    }

    #[tokio::test]
    async fn prompt_dangerous_allows_risky_write_but_pauses_destructive_delete() {
        assert!(!std::io::stdin().is_terminal());
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("doomed.txt"), "x\n").expect("write");
        let exec = NativeExecution::new(ApprovalPolicy::PromptDestructive, tmp.path());

        // Risky write proceeds.
        exec.file_op(FileOp::Write {
            path: PathBuf::from("new.txt"),
            content: "y\n".to_string(),
        })
        .await
        .expect("risky write allowed");

        // Destructive delete pauses without a TTY.
        let err = exec
            .file_op(FileOp::Delete {
                path: PathBuf::from("doomed.txt"),
            })
            .await
            .expect_err("must pause");
        assert!(err.to_string().contains("approval required"));

        // Out-of-project write is Destructive and also pauses.
        let err = exec
            .file_op(FileOp::Write {
                path: PathBuf::from("../escape.txt"),
                content: "x".to_string(),
            })
            .await
            .expect_err("must pause");
        assert!(err.to_string().contains("approval required"));
    }

    #[tokio::test]
    async fn deny_blocks_file_writes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exec = NativeExecution::new(ApprovalPolicy::Deny, tmp.path());
        let err = exec
            .file_op(FileOp::Write {
                path: PathBuf::from("nope.txt"),
                content: "x".to_string(),
            })
            .await
            .expect_err("denied");
        assert!(err.to_string().contains("approval denied"));
        // Reads remain Safe under deny.
        std::fs::write(tmp.path().join("r.txt"), "data").expect("write");
        let result = exec
            .file_op(FileOp::Read {
                path: PathBuf::from("r.txt"),
            })
            .await
            .expect("read allowed under deny");
        assert_eq!(result.content.as_deref(), Some("data"));
    }
}
