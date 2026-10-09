//! ChatGPT-subscription Codex provider.
//!
//! This is deliberately separate from the OpenAI API provider.  Subscription
//! OAuth is sent to ChatGPT's Codex Responses backend, whose wire format is the
//! Responses API rather than chat completions.

use std::time::Duration;

use async_trait::async_trait;
use forge_core::{
    CompletionRequest, CompletionResponse, ForgeError, ModelCapabilities, ModelProvider, Role,
    ToolCall, Usage,
};
use serde_json::{Value, json};

use crate::local_only::{EgressPolicy, error_detail};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

pub struct CodexModel {
    client: reqwest::Client,
    response_max_bytes: Option<usize>,
    base_url: String,
    model: String,
    access_token: String,
    account_id: String,
}

impl CodexModel {
    /// Construct a subscription-backed provider. Credentials are supplied by
    /// the caller; this provider never discovers or reads credential files.
    pub fn new(
        base_url: Option<String>,
        model: impl Into<String>,
        access_token: impl Into<String>,
        account_id: impl Into<String>,
        timeout: Duration,
        egress: EgressPolicy,
    ) -> Result<Self, ForgeError> {
        let client = egress
            .client(timeout)
            .map_err(|e| ForgeError::provider(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            response_max_bytes: None,
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_owned(),
            model: model.into(),
            access_token: access_token.into(),
            account_id: account_id.into(),
        })
    }

    pub(crate) fn with_response_max_bytes(mut self, max_bytes: Option<usize>) -> Self {
        self.response_max_bytes = max_bytes;
        self
    }

    fn body(&self, request: &CompletionRequest) -> Value {
        let mut input = Vec::new();
        for message in &request.messages {
            match message.role {
                Role::Tool => input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message.content,
                })),
                Role::Assistant => {
                    if !message.content.is_empty() {
                        input.push(json!({"role": "assistant", "content": [{
                            "type": "output_text", "text": message.content
                        }]}));
                    }
                    for call in &message.tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.arguments.to_string(),
                        }));
                    }
                }
                Role::System | Role::User => input.push(json!({
                    "role": match message.role { Role::System => "system", _ => "user" },
                    "content": [{"type": "input_text", "text": message.content}],
                })),
            }
        }
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "strict": false,
                })
            })
            .collect();
        let mut body = json!({
            "model": self.model,
            "input": input,
            "tools": tools,
            // The ChatGPT subscription backend requires SSE even when the
            // caller wants a whole response. `complete` consumes the stream
            // and returns the terminal Responses object.
            "stream": true,
            "store": false,
        });
        if let Some(max) = request.max_tokens {
            body["max_output_tokens"] = json!(max);
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        body
    }

    fn parse_response(&self, body: Value) -> Result<CompletionResponse, ForgeError> {
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for item in body["output"].as_array().into_iter().flatten() {
            match item["type"].as_str() {
                Some("message") => {
                    for part in item["content"].as_array().into_iter().flatten() {
                        if part["type"] == "output_text"
                            && let Some(text) = part["text"].as_str()
                        {
                            content.push_str(text);
                        }
                    }
                }
                Some("function_call") => {
                    let arguments = item["arguments"]
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_else(|| item["arguments"].clone());
                    tool_calls.push(ToolCall::new(
                        item["call_id"].as_str().unwrap_or_default(),
                        item["name"].as_str().unwrap_or_default(),
                        arguments,
                    ));
                }
                _ => {}
            }
        }
        let usage = body.get("usage").map(|usage| Usage {
            prompt_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
            total_tokens: usage["total_tokens"].as_u64().unwrap_or(0) as u32,
        });
        Ok(CompletionResponse {
            model: body["model"].as_str().unwrap_or(&self.model).to_owned(),
            content,
            tool_calls,
            finish_reason: body["status"].as_str().map(str::to_owned),
            usage,
        })
    }

    fn parse_wire_response(&self, text: &str) -> Result<CompletionResponse, ForgeError> {
        if let Ok(body) = serde_json::from_str::<Value>(text) {
            return self.parse_response(body);
        }
        let mut completed = None;
        let mut output_items = Vec::new();
        let mut text_deltas = String::new();
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(data)
                .map_err(|e| ForgeError::provider(format!("invalid Codex SSE event: {e}")))?;
            match event["type"].as_str() {
                Some("response.completed") => completed = event.get("response").cloned(),
                Some("response.output_item.done") => {
                    if let Some(item) = event.get("item") {
                        output_items.push(item.clone());
                    }
                }
                Some("response.output_text.delta") => {
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        text_deltas.push_str(delta);
                    }
                }
                Some("response.failed") => {
                    return Err(ForgeError::provider(format!(
                        "Codex response failed: {}",
                        event.get("response").unwrap_or(&event)
                    )));
                }
                _ => {}
            }
        }
        let mut response = completed.ok_or_else(|| {
            ForgeError::provider("Codex stream ended without a response.completed event")
        })?;
        let terminal_has_output = response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|output| !output.is_empty());
        if !terminal_has_output && !output_items.is_empty() {
            response["output"] = Value::Array(output_items);
        } else if !terminal_has_output && !text_deltas.is_empty() {
            response["output"] = json!([{
                "type": "message",
                "content": [{"type": "output_text", "text": text_deltas}]
            }]);
        }
        self.parse_response(response)
    }
}

#[async_trait]
impl ModelProvider for CodexModel {
    fn name(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: false,
            tools: true,
            structured_output: false,
            vision: false,
            max_context: 400_000,
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.access_token)
            .header("chatgpt-account-id", &self.account_id)
            .header("OpenAI-Beta", "responses=v1")
            .header("originator", "forge")
            .json(&self.body(&request))
            .send()
            .await
            .map_err(|e| {
                if e.is_redirect() {
                    ForgeError::provider(format!(
                        "Codex request was not completed: {}",
                        error_detail(&e)
                    ))
                } else {
                    ForgeError::provider_failure(
                        &self.model,
                        forge_core::ProviderFailureKind::Transient,
                        None,
                        format!("Codex request failed: {}", error_detail(&e)),
                    )
                }
            })?;
        let status = response.status();
        if let Some(error) = crate::response::rate_limit_error(&self.model, &response) {
            return Err(error);
        }
        if let Some(max_bytes) = self.response_max_bytes {
            // Codex complete is SSE on the wire: bound the whole event
            // envelope, not just the final extracted answer.
            let bytes = crate::response::bounded_bytes(response, max_bytes).await?;
            if !status.is_success() {
                return Err(crate::response::http_status_error(
                    &self.model,
                    status,
                    format!("Codex returned HTTP {status}"),
                ));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| ForgeError::provider("invalid bounded Codex response UTF-8"))?;
            return self
                .parse_wire_response(text)
                .map_err(|_| ForgeError::provider("invalid bounded Codex response"));
        }
        let text = response
            .text()
            .await
            .map_err(|e| ForgeError::provider(format!("reading Codex response: {e}")))?;
        if !status.is_success() {
            return Err(crate::response::http_status_error(
                &self.model,
                status,
                format!("Codex returned HTTP {status}: {text}"),
            ));
        }
        self.parse_wire_response(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::{Message, ToolDefinition};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    #[tokio::test]
    async fn rate_limit_preserves_retry_after_without_reading_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "23")
                    .set_body_string("private account detail"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let model = CodexModel::new(
            Some(server.uri()),
            "gpt-test",
            "oauth-token",
            "acct-1",
            Duration::from_secs(5),
            EgressPolicy::default(),
        )
        .expect("model");

        let error = model
            .complete(CompletionRequest::new(
                "gpt-test",
                vec![Message::user("hello")],
            ))
            .await
            .expect_err("rate limited");
        assert!(matches!(
            error,
            ForgeError::ProviderRateLimited {
                provider,
                retry_after_seconds: Some(23),
            } if provider == "gpt-test"
        ));
    }

    #[tokio::test]
    async fn serializes_auth_conversation_and_tools_and_parses_response() {
        let server = MockServer::start().await;
        let expected = json!({
            "model": "gpt-test", "stream": true, "store": false,
            "input": [
                {"role":"system","content":[{"type":"input_text","text":"be terse"}]},
                {"role":"user","content":[{"type":"input_text","text":"inspect"}]}
            ],
            "tools": [{"type":"function","name":"read","description":"Read a file",
                "parameters":{"type":"object"},"strict":false}]
        });
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer oauth-token"))
            .and(header("chatgpt-account-id", "acct-1"))
            .and(body_json(expected))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model":"gpt-test","status":"completed",
                "output":[
                    {"type":"message","content":[{"type":"output_text","text":"Calling read."}]},
                    {"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"path\":\"a\"}"}
                ],
                "usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}
            })))
            .mount(&server).await;
        let model = CodexModel::new(
            Some(server.uri()),
            "gpt-test",
            "oauth-token",
            "acct-1",
            Duration::from_secs(2),
            EgressPolicy::Unrestricted,
        )
        .unwrap();
        let response = model
            .complete(
                CompletionRequest::new(
                    "ignored",
                    vec![Message::system("be terse"), Message::user("inspect")],
                )
                .with_tools(vec![ToolDefinition::new(
                    "read",
                    "Read a file",
                    json!({"type":"object"}),
                )]),
            )
            .await
            .unwrap();
        assert_eq!(response.content, "Calling read.");
        assert_eq!(
            response.tool_calls[0],
            ToolCall::new("call-1", "read", json!({"path":"a"}))
        );
        assert_eq!(response.usage.unwrap().total_tokens, 14);
    }

    #[tokio::test]
    async fn consumes_the_required_sse_transport_into_one_response() {
        let server = MockServer::start().await;
        let terminal = json!({
            "type":"response.completed",
            "response":{
                "model":"gpt-test",
                "status":"completed",
                "output":[{"type":"message","content":[
                    {"type":"output_text","text":"pong"}
                ]}],
                "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}
            }
        });
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!(
                        "event: response.completed\ndata: {terminal}\n\ndata: [DONE]\n\n"
                    )),
            )
            .mount(&server)
            .await;
        let model = CodexModel::new(
            Some(server.uri()),
            "gpt-test",
            "oauth-token",
            "acct-1",
            Duration::from_secs(2),
            EgressPolicy::Unrestricted,
        )
        .unwrap();
        let response = model
            .complete(CompletionRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
            ))
            .await
            .unwrap();
        assert_eq!(response.content, "pong");
        assert_eq!(response.usage.unwrap().total_tokens, 3);
    }

    #[tokio::test]
    async fn keeps_output_items_when_terminal_event_contains_only_metadata() {
        let server = MockServer::start().await;
        let message = json!({
            "type":"response.output_item.done",
            "item":{"type":"message","content":[
                {"type":"output_text","text":"I will read it."}
            ]}
        });
        let call = json!({
            "type":"response.output_item.done",
            "item":{"type":"function_call","call_id":"call-1","name":"read_file",
                    "arguments":"{\"path\":\"src/lib.rs\"}"}
        });
        let terminal = json!({
            "type":"response.completed",
            "response":{"model":"gpt-test","status":"completed",
                        "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}
        });
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!(
                        "data: {message}\n\ndata: {call}\n\ndata: {terminal}\n\ndata: [DONE]\n\n"
                    )),
            )
            .mount(&server)
            .await;
        let model = CodexModel::new(
            Some(server.uri()),
            "gpt-test",
            "oauth-token",
            "acct-1",
            Duration::from_secs(2),
            EgressPolicy::Unrestricted,
        )
        .unwrap();
        let response = model
            .complete(CompletionRequest::new(
                "gpt-test",
                vec![Message::user("inspect")],
            ))
            .await
            .unwrap();
        assert_eq!(response.content, "I will read it.");
        assert_eq!(
            response.tool_calls,
            vec![ToolCall::new(
                "call-1",
                "read_file",
                json!({"path":"src/lib.rs"})
            )]
        );
    }

    #[test]
    fn advertises_non_streaming_tool_support() {
        let model = CodexModel::new(
            Some("http://localhost".into()),
            "gpt",
            "token",
            "account",
            Duration::from_secs(1),
            EgressPolicy::Unrestricted,
        )
        .unwrap();
        assert!(model.capabilities().tools);
        assert!(!model.capabilities().streaming);
    }
}
