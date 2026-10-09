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

/// Anthropic Messages model. Capabilities are explicit; both whole-response
/// and SSE streaming (`stream: true`) requests are supported.
pub struct AnthropicModel {
    client: reqwest::Client,
    response_max_bytes: Option<usize>,
    /// The streaming twin of `client`: no total deadline (D7 — see
    /// [`EgressPolicy::streaming_client`]).
    stream_client: reqwest::Client,
    base_url: String,
    model: String,
    wire_model: String,
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
        let stream_client = egress
            .streaming_client(timeout)
            .map_err(|e| ForgeError::provider(format!("building streaming HTTP client: {e}")))?;
        let model = model.into();
        Ok(Self {
            client,
            response_max_bytes: None,
            stream_client,
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            wire_model: model.clone(),
            model,
            credential,
            capabilities,
            max_output_tokens: max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        })
    }

    /// Send a provider-specific model id while keeping Forge's stable
    /// configured alias in events, routing, and model selection.
    pub fn with_wire_model(mut self, wire_model: impl Into<String>) -> Self {
        self.wire_model = wire_model.into();
        self
    }

    pub(crate) fn with_response_max_bytes(mut self, max_bytes: Option<usize>) -> Self {
        self.response_max_bytes = max_bytes;
        self
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    /// The request wire body; `stream` only adds `stream: true` — with
    /// `stream: false` the body is byte-identical to the pre-streaming one.
    fn messages_body<'a>(
        &'a self,
        request: &'a CompletionRequest,
        stream: bool,
    ) -> Result<MessagesRequest<'a>, ForgeError> {
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

        Ok(MessagesRequest {
            model: &self.wire_model,
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
            stream,
        })
    }

    /// The headers both request shapes share: the API version, then the
    /// credential — API key or OAuth bearer + beta flag.
    fn authed(&self, http: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let http = http.header("anthropic-version", "2023-06-01");
        match self.credential.kind {
            CredentialKind::ApiKey => http.header("x-api-key", &self.credential.secret),
            CredentialKind::OAuthToken => http
                .bearer_auth(&self.credential.secret)
                .header("anthropic-beta", "oauth-2025-04-20"),
        }
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
    /// `stream: false` must serialize identically to a request without the
    /// key — the non-streaming wire body is byte-identical to before
    /// streaming landed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
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

/// The transport failure mapping shared by the buffered and streaming send
/// paths. Source chain included: a `local_only` redirect refusal explains
/// itself here (see `local_only::error_detail`).
fn send_error(url: &str, e: reqwest::Error) -> ForgeError {
    if e.is_timeout() {
        ForgeError::provider(format!("anthropic request to {url} timed out"))
    } else if e.is_connect() {
        ForgeError::provider(format!(
            "cannot reach Anthropic endpoint at {url} (connection refused)"
        ))
    } else {
        ForgeError::provider(format!(
            "anthropic request to {url} failed: {}",
            crate::local_only::error_detail(&e)
        ))
    }
}

/// A non-2xx answer becomes a typed provider error; the server error body
/// never contains our credential.
async fn status_error(
    url: &str,
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> ForgeError {
    let text: String = response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(500)
        .collect();
    ForgeError::provider(format!(
        "anthropic endpoint {url} returned {status}: {text}"
    ))
}

/// The whole-body response shape `complete()` parses — also the shape a
/// server that ignores `stream: true` answers a streaming request with, so
/// both paths share the one parser and cannot drift. Multiple text blocks
/// join with `\n`; unknown block types are skipped.
fn parse_message_body(model: &str, body: &serde_json::Value) -> CompletionResponse {
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

    CompletionResponse {
        model: model.to_string(),
        content: text,
        tool_calls,
        finish_reason: body
            .get("stop_reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        usage: body.get("usage").and_then(parse_usage),
    }
}

/// The whole-body usage shape: both token counts or no `Usage` at all.
fn parse_usage(value: &serde_json::Value) -> Option<Usage> {
    let input = value.get("input_tokens")?.as_u64()? as u32;
    let output = value.get("output_tokens")?.as_u64()? as u32;
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
    })
}

/// One in-flight content block of an Anthropic stream.
enum AnthropicBlock {
    Text,
    /// A `tool_use` block: its input arrives as `input_json_delta`
    /// partial-JSON strings and is parsed once at block end (D6).
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// An unknown block type — its deltas are skipped (Anthropic's
    /// versioning policy explicitly adds event types).
    Other,
}

/// Reassembly of one Anthropic event stream, keyed on each payload's
/// `"type"` (robust against a proxy dropping the `event:` names). Text
/// fragments are handed to `on_delta` verbatim — including the `\n`
/// separator between text blocks, so concatenated deltas equal the content
/// exactly as `complete()` would join it. Tool input never streams.
#[derive(Default)]
struct AnthropicStream {
    content: String,
    blocks: std::collections::BTreeMap<u64, AnthropicBlock>,
    tool_calls: Vec<forge_core::ToolCall>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: Option<String>,
    saw_stop: bool,
}

impl AnthropicStream {
    fn apply(
        &mut self,
        event: &crate::sse::SseEvent,
        url: &str,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<(), ForgeError> {
        let payload: serde_json::Value = serde_json::from_str(&event.data).map_err(|e| {
            ForgeError::provider(format!("malformed SSE data chunk from {url}: {e}"))
        })?;
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                self.input_tokens = payload
                    .pointer("/message/usage/input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .map(|n| n as u32);
            }
            Some("content_block_start") => {
                let index = payload
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let block = payload.get("content_block").cloned().unwrap_or_default();
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        // The `\n` join between text blocks, emitted as a
                        // delta so fragments concatenate to the content.
                        if !self.content.is_empty() {
                            self.content.push('\n');
                            on_delta("\n");
                        }
                        self.blocks.insert(index, AnthropicBlock::Text);
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.blocks.insert(
                            index,
                            AnthropicBlock::ToolUse {
                                id,
                                name,
                                json: String::new(),
                            },
                        );
                    }
                    _ => {
                        self.blocks.insert(index, AnthropicBlock::Other);
                    }
                }
            }
            Some("content_block_delta") => {
                let index = payload
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let Some(block) = self.blocks.get_mut(&index) else {
                    // A delta for a block we never opened: skip, never assume.
                    return Ok(());
                };
                let delta = payload.get("delta").cloned().unwrap_or_default();
                match delta.get("type").and_then(serde_json::Value::as_str) {
                    Some("text_delta") => {
                        if let (AnthropicBlock::Text, Some(text)) = (
                            &block,
                            delta.get("text").and_then(serde_json::Value::as_str),
                        ) && !text.is_empty()
                        {
                            self.content.push_str(text);
                            on_delta(text);
                        }
                    }
                    Some("input_json_delta") => {
                        if let AnthropicBlock::ToolUse { json, .. } = block
                            && let Some(part) = delta
                                .get("partial_json")
                                .and_then(serde_json::Value::as_str)
                        {
                            json.push_str(part);
                        }
                    }
                    // thinking_delta, signature_delta, unknown: skipped.
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = payload
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                if let Some(AnthropicBlock::ToolUse { id, name, json }) = self.blocks.remove(&index)
                {
                    // An empty accumulation is `{}` — a tool may legitimately
                    // take no arguments; malformed JSON stays a string value.
                    let input = if json.is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::from_str(&json).unwrap_or(serde_json::Value::String(json))
                    };
                    self.tool_calls
                        .push(forge_core::ToolCall::new(id, name, input));
                }
            }
            Some("message_delta") => {
                if let Some(reason) = payload
                    .pointer("/delta/stop_reason")
                    .and_then(serde_json::Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                // `output_tokens` here is *cumulative*: replace, don't add.
                if let Some(output) = payload
                    .pointer("/usage/output_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    self.output_tokens = Some(output as u32);
                }
            }
            Some("message_stop") => {
                self.saw_stop = true;
            }
            Some("error") => {
                // A mid-stream error payload (D5): the partial text is
                // discarded with the response.
                let error = payload.get("error").cloned().unwrap_or_default();
                let kind = error
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown_error");
                let message = error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error");
                return Err(ForgeError::provider(format!(
                    "anthropic stream from {url} failed after {} answer bytes: \
                     {message} (type {kind})",
                    self.content.len()
                )));
            }
            // ping, unknown event types: skipped.
            _ => {}
        }
        Ok(())
    }

    /// Usage is assembled only when both token counts are known — never
    /// fabricated (D9).
    fn into_response(self, model: &str) -> CompletionResponse {
        let usage = match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => Some(Usage {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: input + output,
            }),
            _ => None,
        };
        CompletionResponse {
            model: model.to_string(),
            content: self.content,
            tool_calls: self.tool_calls,
            finish_reason: self.stop_reason,
            usage,
        }
    }
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
        let url = self.messages_url();
        let response = self
            .authed(
                self.client
                    .post(&url)
                    .json(&self.messages_body(&request, false)?),
            )
            .send()
            .await
            .map_err(|e| send_error(&url, e))?;

        let status = response.status();
        if let Some(error) = crate::response::rate_limit_error(&self.model, &response) {
            return Err(error);
        }
        if let Some(max_bytes) = self.response_max_bytes {
            let bytes = crate::response::bounded_bytes(response, max_bytes).await?;
            if !status.is_success() {
                return Err(ForgeError::provider(format!(
                    "anthropic endpoint returned {status}"
                )));
            }
            let body = serde_json::from_slice(&bytes)
                .map_err(|_| ForgeError::provider("invalid bounded anthropic response JSON"))?;
            return Ok(parse_message_body(&self.model, &body));
        }
        if !status.is_success() {
            return Err(status_error(&url, status, response).await);
        }
        let body: serde_json::Value = response.json().await.map_err(|e| {
            ForgeError::provider(format!("reading anthropic response from {url}: {e}"))
        })?;
        Ok(parse_message_body(&self.model, &body))
    }

    async fn stream_complete(
        &self,
        request: CompletionRequest,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<CompletionResponse, ForgeError> {
        if self.response_max_bytes.is_some() {
            return Err(crate::response::streaming_unsupported());
        }
        reject_tools_without_capability(&request, self.capabilities)?;
        let url = self.messages_url();
        // Scoped so the body's borrow of `request` ends here: the fallback
        // below moves `request` into `complete`.
        let response = {
            let body = self.messages_body(&request, true)?;
            self.authed(self.stream_client.post(&url).json(&body))
                .send()
                .await
                .map_err(|e| send_error(&url, e))?
        };

        let status = response.status();
        if let Some(error) = crate::response::rate_limit_error(&self.model, &response) {
            return Err(error);
        }
        if !status.is_success() {
            // Nothing was shown yet, so exactly one non-streaming retry is
            // safe and cheap.
            let error = status_error(&url, status, response).await;
            tracing::warn!(
                model = %self.model,
                %status,
                "streaming request rejected ({error}); answering whole instead — \
                 set streaming = false in [models.{}] to skip the extra round-trip",
                self.model
            );
            return self.complete(request).await;
        }

        // A server that ignored `stream: true` answers a whole JSON body:
        // parse it exactly as `complete` would, and emit no deltas — the
        // contract forbids deltas that don't concatenate to the response.
        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.trim()
                    .to_ascii_lowercase()
                    .starts_with("text/event-stream")
            });
        if !is_sse {
            let body: serde_json::Value = response.json().await.map_err(|e| {
                ForgeError::provider(format!("reading anthropic response from {url}: {e}"))
            })?;
            return Ok(parse_message_body(&self.model, &body));
        }

        let mut parser = crate::sse::SseParser::new();
        let mut stream = AnthropicStream::default();
        let mut response = response;
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    for event in parser.feed(&bytes) {
                        stream.apply(&event, &url, on_delta)?;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(ForgeError::provider(format!(
                        "anthropic stream from {url} failed after {} answer bytes: {}",
                        stream.content.len(),
                        crate::local_only::error_detail(&e)
                    )));
                }
            }
        }
        for event in parser.finish() {
            stream.apply(&event, &url, on_delta)?;
        }
        // EOF with no terminal signal at all is truncation, never a silent
        // partial answer; EOF after a stop_reason without `message_stop` is
        // accepted — real servers omit the final event.
        if !stream.saw_stop && stream.stop_reason.is_none() {
            return Err(ForgeError::provider(format!(
                "anthropic stream from {url} ended early: EOF after {} answer bytes with no \
                 stop_reason and no message_stop",
                stream.content.len()
            )));
        }
        if !stream.saw_stop {
            tracing::debug!(
                model = %self.model,
                "stream ended after stop_reason without message_stop; accepting"
            );
        }
        Ok(stream.into_response(&self.model))
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

    // --- SSE streaming (TICKET-2) ------------------------------------------

    fn sse_server(body: &'static str) -> ResponseTemplate {
        // `set_body_raw` carries the content type: `set_body_string` would
        // stamp text/plain over any inserted header (wiremock 0.6).
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    async fn stream_with(
        model: &AnthropicModel,
    ) -> (Vec<String>, Result<CompletionResponse, ForgeError>) {
        let mut deltas: Vec<String> = Vec::new();
        let response = model
            .stream_complete(
                CompletionRequest::new("claude-sonnet", vec![Message::user("hi")]),
                &mut |d: &str| deltas.push(d.to_string()),
            )
            .await;
        (deltas, response)
    }

    /// Recorded shape, Anthropic messages streaming (the docs' full HTTP
    /// stream): message_start → ping → text block → message_delta →
    /// message_stop.
    const SSE_TEXT_AND_USAGE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
        "event: ping\n",
        "data: {\"type\":\"ping\"}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"The answer\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" is\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" ready\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":15}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    #[tokio::test]
    async fn streaming_emits_text_deltas_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(sse_server(SSE_TEXT_AND_USAGE))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (deltas, response) = stream_with(&model).await;
        let response = response.expect("streams");

        assert_eq!(deltas, vec!["The answer", " is", " ready"]);
        assert_eq!(deltas.concat(), response.content);
        assert_eq!(response.content, "The answer is ready");
        assert_eq!(response.finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(
            response.usage,
            Some(Usage {
                prompt_tokens: 25,
                completion_tokens: 15,
                total_tokens: 40,
            }),
            "input from message_start, cumulative output from message_delta"
        );

        let requests = server.received_requests().await.expect("requests");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request json");
        assert_eq!(body["stream"], true);
    }

    /// Recorded shape: a text block, then a `tool_use` block whose input
    /// arrives as five `input_json_delta` partial-JSON fragments (D6).
    const SSE_TOOL_USE_FRAGMENTS: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Writing the file.\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"a.rs\\\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\", \\\"content\\\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\": \\\"fn a() {}\\\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":30}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    #[tokio::test]
    async fn streaming_reassembles_tool_use_from_partial_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sse_server(SSE_TOOL_USE_FRAGMENTS))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (deltas, response) = stream_with(&model).await;
        let response = response.expect("streams");

        assert_eq!(deltas, vec!["Writing the file."], "no tool JSON streams");
        assert_eq!(response.content, "Writing the file.");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_1");
        assert_eq!(response.tool_calls[0].name, "write_file");
        assert_eq!(
            response.tool_calls[0].arguments,
            serde_json::json!({"path": "a.rs", "content": "fn a() {}"})
        );
    }

    #[tokio::test]
    async fn an_error_event_mid_stream_is_a_typed_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sse_server(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                "event: error\n",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (_deltas, response) = stream_with(&model).await;
        let err = response.expect_err("a mid-stream error must fail the call");
        let ForgeError::Provider(message) = err else {
            panic!("expected a provider error");
        };
        assert!(message.contains("overloaded_error"), "{message}");
        assert!(message.contains("Overloaded"), "{message}");
    }

    #[tokio::test]
    async fn a_stream_without_message_stop_and_no_stop_reason_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sse_server(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (_deltas, response) = stream_with(&model).await;
        let err = response.expect_err("truncation must fail");
        let ForgeError::Provider(message) = err else {
            panic!("expected a provider error");
        };
        assert!(message.contains("ended early"), "{message}");
        assert!(
            message.contains("7 answer bytes"),
            "names how much had arrived: {message}"
        );
    }

    /// D5 tolerance: `message_delta` (the terminal signal) seen, EOF before
    /// `message_stop` — real servers omit the final event; accept.
    #[tokio::test]
    async fn stop_reason_without_message_stop_is_accepted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sse_server(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"whole\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":9}}\n\n",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (deltas, response) = stream_with(&model).await;
        let response = response.expect("stop_reason seen; message_stop is optional");
        assert_eq!(deltas.concat(), response.content);
        assert_eq!(response.content, "whole");
        assert_eq!(response.finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(response.usage.map(|u| u.total_tokens), Some(34));
    }

    /// Anthropic's versioning policy explicitly adds event types: unknown
    /// events, blocks and delta types are skipped, not errors (D6).
    #[tokio::test]
    async fn unknown_events_and_delta_types_are_skipped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sse_server(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                "event: citation\n",
                "data: {\"type\":\"citation\",\"citation\":{\"title\":\"a doc\"}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"real\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (deltas, response) = stream_with(&model).await;
        let response = response.expect("streams");
        assert_eq!(deltas, vec!["real"], "only text_delta streams");
        assert_eq!(response.content, "real");
    }

    /// Two text blocks join with `\n` exactly as `complete()` does — and the
    /// separator is emitted *as a delta* so concatenated deltas still equal
    /// the content (D6).
    #[tokio::test]
    async fn two_text_blocks_join_with_a_newline_delta() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sse_server(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"first\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"second\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (deltas, response) = stream_with(&model).await;
        let response = response.expect("streams");
        assert_eq!(deltas, vec!["first", "\n", "second"]);
        assert_eq!(deltas.concat(), response.content);
        assert_eq!(response.content, "first\nsecond");
    }

    /// D4b for this family: a non-2xx answer to the streaming request gets
    /// exactly one non-streaming fallback.
    #[tokio::test]
    async fn a_non_2xx_streaming_request_falls_back_to_complete_once() {
        let server = MockServer::start().await;
        // The rejecting shape first (priority: lower numbers match first).
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_string_contains("\"stream\":true"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"type": "invalid_request_error", "message": "unknown field: stream"}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "answered whole"}],
                "stop_reason": "end_turn"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (deltas, response) = stream_with(&model).await;
        let response = response.expect("the fallback delivers");
        assert!(deltas.is_empty());
        assert_eq!(response.content, "answered whole");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 2, "exactly one fallback request");
        let first: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("first body");
        let second: serde_json::Value =
            serde_json::from_slice(&requests[1].body).expect("second body");
        assert_eq!(first["stream"], true, "the rejected request streamed");
        assert!(
            second.get("stream").is_none(),
            "the fallback must not retry the stream: {second}"
        );
    }

    #[tokio::test]
    async fn a_streaming_rate_limit_is_typed_and_never_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "19")
                    .set_body_string("rate limited"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::ApiKey);
        let (deltas, response) = stream_with(&model).await;
        assert!(deltas.is_empty());
        assert!(matches!(
            response,
            Err(ForgeError::ProviderRateLimited {
                provider,
                retry_after_seconds: Some(19),
            }) if provider == "claude-sonnet"
        ));
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    /// The OAuth arm on the streaming path: bearer token plus the beta
    /// header, exactly like `complete()`.
    #[tokio::test]
    async fn an_oauth_streaming_request_carries_bearer_and_beta_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer sk-ant-oat01-test"))
            .and(header("anthropic-beta", "oauth-2025-04-20"))
            .respond_with(sse_server(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let model = model(&server.uri(), CredentialKind::OAuthToken);
        let (deltas, response) = stream_with(&model).await;
        let response = response.expect("streams");
        assert_eq!(deltas, vec!["ok"]);
        assert_eq!(response.content, "ok");
    }
}
