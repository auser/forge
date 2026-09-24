//! The ACP v1 wire subset, as serde types, plus the JSON-RPC 2.0
//! envelope.
//!
//! Every field name and enum spelling here was transcribed from the
//! authoritative schema source (`agent-client-protocol-schema` 1.9.1,
//! `src/v1/`) — see the crate docs for why we carry these types instead
//! of depending on the SDK. Two conventions apply throughout and are the
//! only things you need to remember when extending this file:
//!
//! * structs are `rename_all = "camelCase"`;
//! * the two update/outcome enums are *internally tagged* with a
//!   protocol-specific tag name (`sessionUpdate`, `outcome`) and
//!   `rename_all = "snake_case"` variants.
//!
//! Optional fields are `skip_serializing_if` so our notifications stay
//! close to the examples in the spec: clients have to tolerate absent
//! optionals, but a wire log full of explicit nulls is needlessly hard to
//! read.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version this adapter speaks.
///
/// ACP versions are a single integer, "only bumped for breaking changes"
/// (schema `ProtocolVersion`); `V1` is `LATEST` in 1.9.1, and `V2` exists
/// only behind the SDK's `unstable_protocol_v2` feature as a draft. So 1
/// is both what we implement and what we answer with.
pub const PROTOCOL_VERSION: u16 = 1;

// --- JSON-RPC envelope ---------------------------------------------------

/// An incoming line. ACP is JSON-RPC 2.0, so exactly three shapes can
/// arrive, and they are distinguished by which fields are present:
/// requests have `id` + `method`, notifications have `method` only, and
/// responses to requests *we* issued have `id` + `result`/`error`.
///
/// This is deliberately one lenient struct rather than an untagged enum:
/// untagged deserialization reports "data did not match any variant",
/// which is useless for telling a client which of its fields was wrong.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

impl Incoming {
    /// True when this is a response to a request we sent (an `id` with no
    /// `method`), which the reader routes to the waiting caller instead of
    /// dispatching.
    pub fn is_response(&self) -> bool {
        self.method.is_none()
            && self.id.is_some()
            && (self.result.is_some() || self.error.is_some())
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Invalid JSON was received (-32700).
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(-32700, message)
    }

    /// The JSON sent is not a valid Request object (-32600).
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }

    /// The method does not exist (-32601).
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("unknown method: {method}"))
    }

    /// Invalid method parameters (-32602). Everything the client can get
    /// wrong about a session/prompt or session/new lands here.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    /// Internal error (-32603): forge itself failed.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

/// An outgoing line: a response to a client request, a notification, or a
/// request we are making of the client.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Outgoing {
    Response {
        jsonrpc: &'static str,
        id: Value,
        result: Value,
    },
    Error {
        jsonrpc: &'static str,
        id: Value,
        error: RpcError,
    },
    Notification {
        jsonrpc: &'static str,
        method: &'static str,
        params: Value,
    },
    Request {
        jsonrpc: &'static str,
        id: u64,
        method: &'static str,
        params: Value,
    },
}

impl Outgoing {
    pub fn response(id: Value, result: Value) -> Self {
        Self::Response {
            jsonrpc: "2.0",
            id,
            result,
        }
    }

    pub fn error(id: Value, error: RpcError) -> Self {
        Self::Error {
            jsonrpc: "2.0",
            id,
            error,
        }
    }

    pub fn notification(method: &'static str, params: Value) -> Self {
        Self::Notification {
            jsonrpc: "2.0",
            method,
            params,
        }
    }

    pub fn request(id: u64, method: &'static str, params: Value) -> Self {
        Self::Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

// --- method names -------------------------------------------------------

/// Agent-side methods we implement (schema `AGENT_METHOD_NAMES`).
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_CANCEL: &str = "session/cancel";
}

/// Client-side methods we call (schema `CLIENT_METHOD_NAMES`).
pub mod client_method {
    pub const SESSION_UPDATE: &str = "session/update";
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
}

// --- initialize ---------------------------------------------------------

/// `initialize` params. Every field is optional-with-default on purpose:
/// a client that omits its capabilities or identity is still a client we
/// can serve, and the spec notes `clientInfo` only becomes required in a
/// future version.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    #[serde(default)]
    pub protocol_version: u16,
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    #[serde(default)]
    pub client_info: Option<Implementation>,
}

/// What the client can do for us. v1 of this adapter reads these purely to
/// log them: forge runs tools through its own `ExecutionProvider` in the
/// project root, so it never calls `fs/*` or `terminal/*`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub fs: FsCapabilities,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    #[serde(default)]
    pub read_text_file: bool,
    #[serde(default)]
    pub write_text_file: bool,
}

/// Name/version of one side of the connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u16,
    pub agent_capabilities: AgentCapabilities,
    pub auth_methods: Vec<Value>,
    pub agent_info: Implementation,
}

/// What we tell the client we can do — honestly. Everything here is
/// `false`/empty and that is the point: no `session/load`, and text-only
/// prompts.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: PromptCapabilities,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

// --- session/new --------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
}

// --- session/prompt -----------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub prompt: Vec<ContentBlock>,
}

/// A prompt content block. `text` and `resource_link` are the two all
/// agents MUST support; the rest are capability-gated and we advertise
/// none of them, so they are represented only so the dispatcher can
/// refuse them by name instead of failing to parse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Resource {
        resource: Value,
    },
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    Audio {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// The spelling of this block's `type` tag, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::ResourceLink { .. } => "resource_link",
            Self::Resource { .. } => "resource",
            Self::Image { .. } => "image",
            Self::Audio { .. } => "audio",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
}

/// Why a turn ended. We can produce three of the five: `MaxTokens` and
/// `MaxTurnRequests` describe budget exhaustion the agent loop reports as
/// a completed run, and `Refusal` would need a model that tells us it
/// refused.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    Cancelled,
    Refusal,
}

// --- session/cancel -----------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    #[serde(default)]
    pub session_id: Option<String>,
}

// --- session/update -----------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

/// The update variants we emit. Internally tagged with `sessionUpdate`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk { content: ContentBlock },
    AgentThoughtChunk { content: ContentBlock },
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
}

/// A new tool call. `kind` and `status` are skipped when they hold the
/// schema's default (`other`/`pending`), matching how the SDK serializes
/// them.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolCallLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
}

/// A change to an existing tool call: only the fields being updated are
/// sent, which is why every one of them is optional here.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolCallLocation>,
}

impl ToolCallUpdate {
    pub fn new(tool_call_id: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            ..Self::default()
        }
    }

    pub fn status(mut self, status: ToolCallStatus) -> Self {
        self.status = Some(status);
        self
    }
}

/// Tool categories, for the client's icon and UI treatment.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// A file this tool call touched. Populating it is what enables Zed's
/// "follow the agent" — the editor opens the file as forge works on it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallLocation {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl ToolCallLocation {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            line: None,
        }
    }
}

// --- session/request_permission -----------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// The client's answer. Internally tagged with `outcome`; `cancelled` is
/// what a client sends when it cancels the turn instead of answering, and
/// the spec requires it to answer *every* pending permission request that
/// way.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RequestPermissionOutcome {
    Cancelled,
    #[serde(rename_all = "camelCase")]
    Selected {
        option_id: String,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
}
