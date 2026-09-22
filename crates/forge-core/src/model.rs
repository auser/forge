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
}
