//! Tool-call protocol types: OpenAI-tools-compatible but provider-neutral.

use serde::{Deserialize, Serialize};

/// A tool the model may call, described with a JSON Schema for its
/// parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the arguments.
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

/// The outcome of a tool invocation, fed back to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(call_id: impl Into<String>, name: impl Into<String>, content: String) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            content,
            is_error: false,
        }
    }

    pub fn error(call_id: impl Into<String>, name: impl Into<String>, message: String) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            content: message,
            is_error: true,
        }
    }
}
