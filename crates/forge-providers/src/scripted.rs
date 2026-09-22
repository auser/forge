use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_core::{
    CompletionRequest, CompletionResponse, ForgeError, ModelCapabilities, ModelProvider, ToolCall,
    Usage,
};
use serde::{Deserialize, Serialize};

/// One queued reply: text, tool calls, or both.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptedReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

/// Deterministic scripted model for offline tests/BDD of the agent loop.
/// Each `complete` pops the next queued reply; when the queue is empty a
/// fixed "script exhausted" text reply is returned. Full tool capability.
pub struct ScriptedMockModel {
    replies: Mutex<std::collections::VecDeque<ScriptedReply>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptedMockModel {
    pub fn new(replies: Vec<ScriptedReply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Parse a script: `[{"text": "..."}, {"tool_calls": [{"id": ...,
    /// "name": ..., "arguments": {...}}]}, ...]`.
    pub fn from_json(json: &str) -> Result<Self, ForgeError> {
        let replies: Vec<ScriptedReply> = serde_json::from_str(json)
            .map_err(|e| ForgeError::provider(format!("parsing mock script: {e}")))?;
        Ok(Self::new(replies))
    }

    pub fn from_path(path: &Path) -> Result<Self, ForgeError> {
        let text = std::fs::read_to_string(path).map_err(ForgeError::Io)?;
        Self::from_json(&text).map_err(|e| ForgeError::provider(format!("{}: {e}", path.display())))
    }

    /// Load from an optional path; `None` yields `None`.
    pub fn from_optional_path(path: Option<&Path>) -> Result<Option<Self>, ForgeError> {
        path.map(Self::from_path).transpose()
    }

    pub fn recorded(&self) -> Vec<CompletionRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl ModelProvider for ScriptedMockModel {
    fn name(&self) -> &str {
        "scripted-mock"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools: true,
            structured_output: true,
            vision: false,
            max_context: 32_768,
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.clone());

        let reply = self
            .replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| ScriptedReply {
                text: Some("script exhausted".to_string()),
                tool_calls: Vec::new(),
            });

        let content = reply.text.unwrap_or_default();
        Ok(CompletionResponse {
            model: "scripted-mock".to_string(),
            tool_calls: reply.tool_calls,
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: content.len() as u32,
                total_tokens: content.len() as u32,
            }),
            finish_reason: Some("stop".to_string()),
            content,
        })
    }
}

/// Helper for config plumbing: load a scripted mock from an optional
/// script path.
pub fn scripted_mock_from_path(
    path: Option<&Path>,
) -> Result<Option<Arc<dyn ModelProvider>>, ForgeError> {
    Ok(ScriptedMockModel::from_optional_path(path)?.map(|m| Arc::new(m) as Arc<dyn ModelProvider>))
}

#[cfg(test)]
mod tests {
    use forge_core::{Message, ToolDefinition};

    use super::*;

    #[tokio::test]
    async fn queue_pops_in_order_then_exhausts() {
        let model = ScriptedMockModel::new(vec![
            ScriptedReply {
                text: Some("first".to_string()),
                tool_calls: Vec::new(),
            },
            ScriptedReply {
                text: None,
                tool_calls: vec![ToolCall::new(
                    "call_1",
                    "write_file",
                    serde_json::json!({"path": "a.rs", "content": "fn a() {}"}),
                )],
            },
        ]);

        let req = || CompletionRequest::new("scripted-mock", vec![Message::user("x")]);
        let r1 = model.complete(req()).await.expect("first");
        assert_eq!(r1.content, "first");
        assert!(r1.tool_calls.is_empty());

        let r2 = model.complete(req()).await.expect("second");
        assert_eq!(r2.content, "");
        assert_eq!(r2.tool_calls.len(), 1);
        assert_eq!(r2.tool_calls[0].name, "write_file");

        let r3 = model.complete(req()).await.expect("exhausted");
        assert_eq!(r3.content, "script exhausted");

        // Deterministic: exhaustion keeps returning the same reply.
        let r4 = model.complete(req()).await.expect("still exhausted");
        assert_eq!(r4.content, "script exhausted");
        assert_eq!(model.recorded().len(), 4);
    }

    #[test]
    fn from_json_parses_text_and_tool_calls() {
        let model = ScriptedMockModel::from_json(
            r#"[
                {"text": "hello"},
                {"tool_calls": [{"id": "call_1", "name": "write_file", "arguments": {"path": "main.rs", "content": "fn main() {}"}}]},
                {"text": "done", "tool_calls": []}
            ]"#,
        )
        .expect("parse");
        assert_eq!(model.replies.lock().expect("lock").len(), 3);

        let err = match ScriptedMockModel::from_json("not json") {
            Err(e) => e,
            Ok(_) => panic!("must fail"),
        };
        assert!(matches!(err, ForgeError::Provider(_)));
    }

    #[tokio::test]
    async fn mock_model_without_tools_capability_rejects_tool_requests() {
        use crate::model::MockModel;
        let model = MockModel::new().with_capabilities(ModelCapabilities {
            tools: false,
            ..MockModel::new().capabilities()
        });
        let request =
            CompletionRequest::new("mock-local", vec![Message::user("x")]).with_tools(vec![
                ToolDefinition::new(
                    "write_file",
                    "write a file",
                    serde_json::json!({"type": "object"}),
                ),
            ]);
        let err = model.complete(request).await.expect_err("must reject");
        match err {
            ForgeError::Provider(msg) => assert!(msg.contains("tools"), "got: {msg}"),
            other => panic!("expected provider error, got {other:?}"),
        }
    }
}
