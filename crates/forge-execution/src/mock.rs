use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_core::{ExecRequest, ExecResult, ExecutionProvider, FileOp, FileOpResult, ForgeError};

/// Records every command and file op, returning canned results. Used by
/// unit tests and offline BDD scenarios.
#[derive(Clone)]
pub struct MockExecution {
    project_root: PathBuf,
    recorded: Arc<Mutex<Vec<ExecRequest>>>,
    recorded_file_ops: Arc<Mutex<Vec<FileOp>>>,
    result: ExecResult,
    read_content: Option<String>,
}

impl MockExecution {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            recorded: Arc::new(Mutex::new(Vec::new())),
            recorded_file_ops: Arc::new(Mutex::new(Vec::new())),
            result: ExecResult {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            read_content: None,
        }
    }

    pub fn with_result(project_root: impl Into<PathBuf>, result: ExecResult) -> Self {
        Self {
            result,
            ..Self::new(project_root)
        }
    }

    /// Canned content returned for `FileOp::Read`.
    pub fn with_read_content(mut self, content: impl Into<String>) -> Self {
        self.read_content = Some(content.into());
        self
    }

    pub fn project_root(&self) -> &std::path::Path {
        &self.project_root
    }

    pub fn recorded(&self) -> Vec<ExecRequest> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn recorded_file_ops(&self) -> Vec<FileOp> {
        self.recorded_file_ops
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

    async fn file_op(&self, op: FileOp) -> Result<FileOpResult, ForgeError> {
        let changed = !matches!(op, FileOp::Read { .. });
        let content = if matches!(op, FileOp::Read { .. }) {
            Some(self.read_content.clone().unwrap_or_default())
        } else {
            None
        };
        self.recorded_file_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(op);
        Ok(FileOpResult { content, changed })
    }
}

#[cfg(test)]
mod tests {
    use forge_core::RiskLevel;

    use super::*;

    #[tokio::test]
    async fn mock_records_requests_and_returns_canned_result() {
        let mock = MockExecution::with_result(
            std::env::temp_dir(),
            ExecResult {
                exit_code: 7,
                stdout: "out".to_string(),
                stderr: "err".to_string(),
            },
        );

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

    #[tokio::test]
    async fn mock_records_inherit_stdio_flag() {
        let mock = MockExecution::new(std::env::temp_dir());
        mock.execute(ExecRequest::new("python3", RiskLevel::Safe).inheriting_stdio())
            .await
            .expect("ok");
        assert!(mock.recorded()[0].inherit_stdio);
        assert!(!mock.recorded().is_empty());
    }

    #[test]
    fn exec_request_inherit_stdio_defaults_to_false() {
        let request: ExecRequest =
            serde_json::from_str(r#"{"command": "ls", "risk": "safe"}"#).expect("parses");
        assert!(!request.inherit_stdio);
        let with_flag: ExecRequest =
            serde_json::from_str(r#"{"command": "ls", "risk": "safe", "inherit_stdio": true}"#)
                .expect("parses");
        assert!(with_flag.inherit_stdio);
    }

    #[tokio::test]
    async fn mock_records_file_ops_and_serves_canned_reads() {
        let mock =
            MockExecution::new(std::env::temp_dir()).with_read_content("canned file content");

        let read = mock
            .file_op(FileOp::Read {
                path: PathBuf::from("src/main.rs"),
            })
            .await
            .expect("read");
        assert_eq!(read.content.as_deref(), Some("canned file content"));
        assert!(!read.changed);

        let write = mock
            .file_op(FileOp::Write {
                path: PathBuf::from("src/new.rs"),
                content: "fn new() {}".to_string(),
            })
            .await
            .expect("write");
        assert!(write.changed);
        assert_eq!(write.content, None);

        let ops = mock.recorded_file_ops();
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], FileOp::Read { .. }));
        assert!(matches!(ops[1], FileOp::Write { .. }));
    }
}
