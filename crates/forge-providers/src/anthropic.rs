//! Anthropic Messages API provider (`POST {base}/v1/messages`), supporting
//! both API keys (`x-api-key`) and Claude subscription OAuth tokens
//! (`Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`).

use std::time::Duration;

use async_trait::async_trait;
use forge_core::{
    CompletionRequest, CompletionResponse, ForgeError, ModelCapabilities, ModelProvider, Role,
    Usage,
};
use serde::Serialize;

use crate::credentials::{CredentialKind, ResolvedCredential};
use crate::local_only::EgressPolicy;
use crate::model::reject_tools_without_capability;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

/// Anthropic Messages model. Capabilities are explicit; streaming is not
/// implemented (single response per request).
pub struct AnthropicModel {
    client: reqwest::Client,
    base_url: String,
    model: String,
    credential: ResolvedCredential,
    capabilities: ModelCapabilities,
    max_output_tokens: u32,
}

impl AnthropicModel {
    /// `egress` decides how far this client may travel, redirects included
    /// (see [`EgressPolicy`]).
    pub fn new(
        base_url: Option<String>,
        model: impl Into<String>,
        credential: ResolvedCredential,
        capabilities: ModelCapabilities,
        max_output_tokens: Option<u32>,
        timeout: Duration,
        egress: EgressPolicy,
    ) -> Result<Self, ForgeError> {
        let client = egress
            .client(timeout)
            .map_err(|e| ForgeError::provider(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            model: model.into(),
            credential,
            capabilities,
            max_output_tokens: max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        })
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<MessagesMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct MessagesMessage<'a> {
    role: &'a str,
    content: Vec<ContentBlock<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock<'a> {
    Text {
        text: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

#[async_trait]
impl ModelProvider for AnthropicModel {
    fn name(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        reject_tools_without_capability(&request, self.capabilities)?;

        let mut system: Vec<&str> = Vec::new();
        let mut messages: Vec<MessagesMessage> = Vec::new();
        for message in &request.messages {
            match message.role {
                Role::System => system.push(&message.content),
                Role::User => messages.push(MessagesMessage {
                    role: "user",
                    content: vec![ContentBlock::Text {
                        text: &message.content,
                    }],
                }),
                Role::Assistant => {
                    let mut content = Vec::new();
                    if !message.content.is_empty() {
                        content.push(ContentBlock::Text {
                            text: &message.content,
                        });
                    }
                    for call in &message.tool_calls {
                        content.push(ContentBlock::ToolUse {
                            id: &call.id,
                            name: &call.name,
                            input: &call.arguments,
                        });
                    }
                    messages.push(MessagesMessage {
                        role: "assistant",
                        content,
                    });
                }
                Role::Tool => {
                    let tool_use_id = message
                        .tool_call_id
                        .as_deref()
                        .ok_or_else(|| ForgeError::provider("tool message missing tool_call_id"))?;
                    messages.push(MessagesMessage {
                        role: "user",
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id,
                            content: &message.content,
                            is_error: false,
                        }],
                    });
                }
            }
        }

        let body = MessagesRequest {
            model: &self.model,
            max_tokens: request.max_tokens.unwrap_or(self.max_output_tokens),
            system: if system.is_empty() {
                None
            } else {
                Some(system.join("\n"))
            },
            messages,
            tools: request
                .tools
                .iter()
                .map(|t| AnthropicTool {
                    name: &t.name,
                    description: &t.description,
                    input_schema: &t.parameters,
                })
                .collect(),
            temperature: request.temperature,
        };

        let url = format!("{}/v1/messages", self.base_url);
        let mut http = self
            .client
            .post(&url)
            .json(&body)
            .header("anthropic-version", "2023-06-01");
        match self.credential.kind {
            CredentialKind::ApiKey => {
                http = http.header("x-api-key", &self.credential.secret);
            }
            CredentialKind::OAuthToken => {
                http = http
                    .bearer_auth(&self.credential.secret)
                    .header("anthropic-beta", "oauth-2025-04-20");
            }
        }

        let response = http.send().await.map_err(|e| {
            if e.is_timeout() {
                ForgeError::provider(format!("anthropic request to {url} timed out"))
            } else if e.is_connect() {
                ForgeError::provider(format!(
                    "cannot reach Anthropic endpoint at {url} (connection refused)"
                ))
            } else {
                // Source chain included: a `local_only` redirect refusal
                // explains itself here (see `local_only::error_detail`).
                ForgeError::provider(format!(
                    "anthropic request to {url} failed: {}",
                    crate::local_only::error_detail(&e)
                ))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            let text: String = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect();
            // The server error body never contains our credential.
            return Err(ForgeError::provider(format!(
                "anthropic endpoint {url} returned {status}: {text}"
            )));
        }
        let body: serde_json::Value = response.json().await.map_err(|e| {
            ForgeError::provider(format!("reading anthropic response from {url}: {e}"))
        })?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        if let Some(blocks) = body.get("content").and_then(serde_json::Value::as_array) {
            for block in blocks {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(serde_json::Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        let id = block.get("id").and_then(serde_json::Value::as_str);
                        let name = block.get("name").and_then(serde_json::Value::as_str);
                        if let (Some(id), Some(name)) = (id, name) {
                            tool_calls.push(forge_core::ToolCall::new(
                                id,
                                name,
                                block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(CompletionResponse {
            model: self.model.clone(),
            content: text,
            tool_calls,
            finish_reason: body
                .get("stop_reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            usage: body.get("usage").and_then(|u| {
                let input = u.get("input_tokens")?.as_u64()? as u32;
                let output = u.get("output_tokens")?.as_u64()? as u32;
                Some(Usage {
                    prompt_tokens: input,
                    completion_tokens: output,
                    total_tokens: input + output,
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use forge_core::{Message, ToolCall, ToolDefinition};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::credentials::CredentialSource;

    fn model(url: &str, kind: CredentialKind) -> AnthropicModel {
        let credential = match kind {
            CredentialKind::ApiKey => {
                ResolvedCredential::api_key("sk-ant-test-key", CredentialSource::EnvVar("X".into()))
            }
            CredentialKind::OAuthToken => ResolvedCredential::oauth_token(
                "sk-ant-oat01-test",
                CredentialSource::ClaudeCodeCredentials,
            ),
        };
        AnthropicModel::new(
            Some(url.to_string()),
            "claude-sonnet",
            credential,
            ModelCapabilities {
                tools: true,
                ..ModelCapabilities::default()
            },
            None,
            Duration::from_secs(5),
            EgressPolicy::default(),
        )
        .expect("construct")
    }

    #[tokio::test]
    async fn api_key_auth_and_system_extraction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "hello back"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 3}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let response = model
            .complete(CompletionRequest::new(
                "claude-sonnet",
                vec![Message::system("be brief"), Message::user("hello")],
            ))
            .await
            .expect("completes");
        assert_eq!(response.content, "hello back");
        assert_eq!(response.usage.map(|u| u.total_tokens), Some(13));

        let requests = server.received_requests().await.expect("requests");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request json");
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["max_tokens"], 8_192);
    }

    #[tokio::test]
    async fn oauth_token_uses_bearer_and_beta_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer sk-ant-oat01-test"))
            .and(header("anthropic-beta", "oauth-2025-04-20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "ok"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::OAuthToken);
        model
            .complete(CompletionRequest::new(
                "claude-sonnet",
                vec![Message::user("hi")],
            ))
            .await
            .expect("completes");
    }

    #[tokio::test]
    async fn tool_use_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [
                    {"type": "text", "text": "writing now"},
                    {"type": "tool_use", "id": "toolu_1", "name": "write_file",
                     "input": {"path": "a.rs", "content": "fn a() {}"}}
                ],
                "stop_reason": "tool_use"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let request = CompletionRequest::new(
            "claude-sonnet",
            vec![
                Message::user("write a file"),
                Message::assistant_tool_calls(vec![ToolCall::new(
                    "toolu_0",
                    "read_file",
                    serde_json::json!({"path": "b.rs"}),
                )]),
                Message::tool("toolu_0", "contents of b.rs"),
            ],
        )
        .with_tools(vec![ToolDefinition::new(
            "write_file",
            "write a file",
            serde_json::json!({"type": "object"}),
        )]);
        let response = model.complete(request).await.expect("completes");
        assert_eq!(response.content, "writing now");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "write_file");

        let requests = server.received_requests().await.expect("requests");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request json");
        // Tool schema mapped to Anthropic shape.
        assert_eq!(body["tools"][0]["name"], "write_file");
        assert!(body["tools"][0]["input_schema"].is_object());
        // Assistant tool_calls became tool_use blocks; tool result is a
        // user-role tool_result block.
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_0");
    }

    #[tokio::test]
    async fn error_maps_typed_and_never_echoes_secret() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"type": "authentication_error", "message": "invalid x-api-key"}
            })))
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let err = model
            .complete(CompletionRequest::new(
                "claude-sonnet",
                vec![Message::user("hi")],
            ))
            .await
            .expect_err("401");
        match err {
            ForgeError::Provider(msg) => {
                assert!(msg.contains("401"), "got: {msg}");
                assert!(!msg.contains("sk-ant-test-key"), "secret echoed: {msg}");
            }
            other => panic!("expected provider error, got {other:?}"),
        }
    }
}
