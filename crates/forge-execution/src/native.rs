use std::io::{BufRead, IsTerminal, Write};

use async_trait::async_trait;
use forge_core::{
    ApprovalPolicy, ExecRequest, ExecResult, ExecutionProvider, ForgeError, RiskLevel,
};

/// Runs commands as local child processes via `tokio::process::Command`.
pub struct NativeExecution {
    approval: ApprovalPolicy,
}

impl NativeExecution {
    pub fn new(approval: ApprovalPolicy) -> Self {
        Self { approval }
    }

    pub fn approval(&self) -> ApprovalPolicy {
        self.approval
    }

    /// Gate a request by risk level and approval policy.
    fn check_approval(&self, request: &ExecRequest) -> Result<(), ForgeError> {
        if request.risk == RiskLevel::Safe {
            return Ok(());
        }
        match self.approval {
            ApprovalPolicy::Auto => Ok(()),
            ApprovalPolicy::Deny => Err(ForgeError::execution(format!(
                "approval denied: `{}` is {:?} and policy is 'deny'",
                request.command, request.risk
            ))),
            ApprovalPolicy::Prompt => prompt_for_approval(request),
        }
    }
}

/// Interactive y/N prompt, only when stdin is a terminal. On a
/// non-interactive stdin the command pauses with a typed
/// "approval required" error instead of hanging.
fn prompt_for_approval(request: &ExecRequest) -> Result<(), ForgeError> {
    if !std::io::stdin().is_terminal() {
        return Err(ForgeError::execution(format!(
            "approval required: `{}` is {:?}; re-run interactively or with --approval auto",
            request.command, request.risk
        )));
    }
    let mut stderr = std::io::stderr();
    write!(
        stderr,
        "approve {:?} command `{} {}`? [y/N] ",
        request.risk,
        request.command,
        request.args.join(" ")
    )
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
            "approval denied by user: `{}`",
            request.command
        )))
    }
}

#[async_trait]
impl ExecutionProvider for NativeExecution {
    fn name(&self) -> &str {
        "native"
    }

    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, ForgeError> {
        self.check_approval(&request)?;

        let mut command = tokio::process::Command::new(&request.command);
        command.args(&request.args);
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }
        tracing::debug!(command = %request.command, risk = ?request.risk, "executing command");

        let output = command.output().await.map_err(|e| {
            ForgeError::execution(format!("failed to spawn `{}`: {e}", request.command))
        })?;

        Ok(ExecResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
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
        }
    }

    #[tokio::test]
    async fn native_runs_safe_command_and_captures_output() {
        let exec = NativeExecution::new(ApprovalPolicy::Prompt);
        let result = exec.execute(safe_echo()).await.expect("echo succeeds");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn native_reports_exit_code_and_stderr() {
        let exec = NativeExecution::new(ApprovalPolicy::Auto);
        let result = exec
            .execute(ExecRequest {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "echo oops >&2; exit 3".to_string()],
                cwd: None,
                risk: RiskLevel::Safe,
            })
            .await
            .expect("spawn succeeds");
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.stderr.trim(), "oops");
    }

    #[tokio::test]
    async fn spawn_failure_is_typed_execution_error() {
        let exec = NativeExecution::new(ApprovalPolicy::Auto);
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
        let exec = NativeExecution::new(ApprovalPolicy::Deny);
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
        let exec = NativeExecution::new(ApprovalPolicy::Auto);
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
        let exec = NativeExecution::new(ApprovalPolicy::Prompt);
        let err = exec
            .execute(ExecRequest {
                risk: RiskLevel::Risky,
                ..safe_echo()
            })
            .await
            .expect_err("must pause for approval");
        assert!(matches!(err, ForgeError::Execution(_)));
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
            ApprovalPolicy::parse("deny"),
            Ok(ApprovalPolicy::Deny)
        ));
        assert!(matches!(
            ApprovalPolicy::parse("yolo"),
            Err(ForgeError::Config(_))
        ));
    }
}
