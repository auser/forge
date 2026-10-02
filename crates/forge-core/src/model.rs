use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ForgeError;
use crate::tool::{ToolCall, ToolDefinition};

/// Explicit capability advertisement for a model backend. Routers filter
/// candidates against these flags instead of assuming features exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub structured_output: bool,
    pub vision: bool,
    pub max_context: usize,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            streaming: false,
            tools: false,
            structured_output: false,
            vision: false,
            max_context: 8_192,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Carries a tool result; `tool_call_id` identifies the call.
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls the assistant requested (empty for other roles).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `Role::Tool` messages: which call this answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Assistant message that requests tool calls instead of (or in
    /// addition to) text.
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Tool result message answering `tool_call_id`.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// Tools the model may invoke. Providers without the `tools`
    /// capability must reject a non-empty list with a typed error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl CompletionRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub model: String,
    pub content: String,
    /// Tool calls requested by the model (empty for a plain text reply).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// A language-model backend (hosted, local, or mock).
///
/// Capability contract: a provider whose `capabilities().tools` is false
/// MUST reject a `CompletionRequest` with a non-empty `tools` list by
/// returning `ForgeError::Provider`, never silently drop the tools.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;

    fn capabilities(&self) -> ModelCapabilities;

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError>;

    /// Complete one request, invoking `on_delta` with each text fragment as
    /// it becomes available, then returning the assembled response — exactly
    /// what `complete` would have returned, tool calls included.
    ///
    /// Contract: the fragments passed to `on_delta`, concatenated, equal the
    /// returned response's `content`. A provider that cannot honor that must
    /// not call `on_delta` at all. Tool-call deltas are reassembled inside
    /// the provider and surface whole in the response; only text streams.
    ///
    /// The default is the graceful fallback: answer with `complete` and emit
    /// no deltas, so every provider written before streaming — including one
    /// advertising `streaming: true` — behaves byte-identically to before.
    async fn stream_complete(
        &self,
        request: CompletionRequest,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<CompletionResponse, ForgeError> {
        let _ = on_delta;
        self.complete(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that implements only `complete` — every pre-streaming
    /// provider is this. The default `stream_complete` must answer with the
    /// whole response and call the delta callback *never*.
    struct WholeOnly;

    #[async_trait]
    impl ModelProvider for WholeOnly {
        fn name(&self) -> &str {
            "whole-only"
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: true,
                ..ModelCapabilities::default()
            }
        }
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, ForgeError> {
            Ok(CompletionResponse {
                model: "whole-only".into(),
                content: format!("answer to {}", request.messages.len()),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn the_default_stream_is_a_silent_whole_response_fallback() {
        let provider = WholeOnly;
        let mut deltas: Vec<String> = Vec::new();
        let response = provider
            .stream_complete(
                CompletionRequest::new("whole-only", vec![Message::user("hi")]),
                &mut |d: &str| deltas.push(d.to_string()),
            )
            .await
            .expect("stream falls back to complete");
        assert_eq!(response.content, "answer to 1");
        assert!(
            deltas.is_empty(),
            "the fallback emits no deltas: {deltas:?}"
        );
    }

    /// An overriding provider receives the fragments it hands out.
    #[tokio::test]
    async fn an_override_delivers_fragments_then_the_assembled_response() {
        struct Chunky;
        #[async_trait]
        impl ModelProvider for Chunky {
            fn name(&self) -> &str {
                "chunky"
            }
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities::default()
            }
            async fn complete(
                &self,
                _: CompletionRequest,
            ) -> Result<CompletionResponse, ForgeError> {
                unreachable!("streaming providers are called through stream_complete")
            }
            async fn stream_complete(
                &self,
                _: CompletionRequest,
                on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
            ) -> Result<CompletionResponse, ForgeError> {
                on_delta("he");
                on_delta("llo");
                Ok(CompletionResponse {
                    model: "chunky".into(),
                    content: "hello".into(),
                    tool_calls: Vec::new(),
                    finish_reason: None,
                    usage: None,
                })
            }
        }
        let mut got = String::new();
        let response = Chunky
            .stream_complete(
                CompletionRequest::new("chunky", vec![Message::user("hi")]),
                &mut |d: &str| got.push_str(d),
            )
            .await
            .expect("streamed");
        assert_eq!(got, "hello");
        assert_eq!(
            response.content, "hello",
            "fragments concatenate to the response"
        );
    }
}
