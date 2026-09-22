use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_core::{ExecRequest, ExecResult, ExecutionProvider, ForgeError};

/// Records every request and returns a canned result. Used by unit tests
/// and offline BDD scenarios.
#[derive(Clone)]
pub struct MockExecution {
    recorded: Arc<Mutex<Vec<ExecRequest>>>,
    result: ExecResult,
}

impl Default for MockExecution {
    fn default() -> Self {
        Self::new()
    }
}

impl MockExecution {
    pub fn new() -> Self {
        Self::with_result(ExecResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    pub fn with_result(result: ExecResult) -> Self {
        Self {
            recorded: Arc::new(Mutex::new(Vec::new())),
            result,
        }
    }

    pub fn recorded(&self) -> Vec<ExecRequest> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl ExecutionProvider for MockExecution {
    fn name(&self) -> &str {
        "mock"
    }

    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, ForgeError> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
        Ok(self.result.clone())
    }
}

#[cfg(test)]
mod tests {
    use forge_core::RiskLevel;

    use super::*;

    #[tokio::test]
    async fn mock_records_requests_and_returns_canned_result() {
        let mock = MockExecution::with_result(ExecResult {
            exit_code: 7,
            stdout: "out".to_string(),
            stderr: "err".to_string(),
        });

        let first = mock
            .execute(ExecRequest::new("ls", RiskLevel::Safe))
            .await
            .expect("mock succeeds");
        let second = mock
            .execute(ExecRequest::new("rm", RiskLevel::Destructive))
            .await
            .expect("mock succeeds");

        assert_eq!(first.exit_code, 7);
        assert_eq!(second.stdout, "out");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].command, "ls");
        assert_eq!(recorded[1].command, "rm");
        assert_eq!(recorded[1].risk, RiskLevel::Destructive);
    }
}
