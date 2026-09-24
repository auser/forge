//! The stdio server loop: the only module that speaks MCP.
//!
//! Everything protocol-shaped is delegated to `rmcp` (framing, JSON-RPC,
//! era/version negotiation, the `resultType` discriminator). What is left
//! here is the mapping between our [`ForgeTools`] registry and MCP's
//! `tools/list` / `tools/call`.

use std::borrow::Cow;
use std::sync::Arc;

use forge_core::ForgeError;
use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{Map, Value};

use crate::tools::{ForgeTools, ToolError, ToolOutcome, definitions};

/// Guidance shown to clients that surface server instructions.
const INSTRUCTIONS: &str = "forge exposes this project's structure (forge_graph_*), its skills \
(forge_skill_*) and its agent loop (forge_run*). Start with forge_graph_context to find the \
files that matter. A run that returns status \"waiting_for_approval\" is asking permission for \
a risky operation: answer it with forge_run_input (\"y\" approves).";

/// MCP server over forge's tool registry.
pub struct ForgeMcpServer {
    tools: Arc<ForgeTools>,
}

impl ForgeMcpServer {
    pub fn new(tools: Arc<ForgeTools>) -> Self {
        Self { tools }
    }
}

/// Convert a registry entry into an MCP `Tool`. The schema must be a JSON
/// object; a non-object would be a bug in `tools.rs`, and an empty object
/// is a safe, spec-valid fallback ("accepts any object").
fn tool_of(def: &crate::tools::ToolDef) -> Tool {
    let schema = match (def.input_schema)() {
        Value::Object(map) => map,
        other => {
            tracing::error!(tool = def.name, ?other, "input schema is not an object");
            let mut fallback = Map::new();
            fallback.insert("type".to_string(), Value::String("object".to_string()));
            fallback
        }
    };
    Tool::new(
        Cow::Borrowed(def.name),
        Cow::Borrowed(def.description),
        Arc::new(schema),
    )
    .with_title(def.title)
}

/// MCP tool results carry a `content` array; we send one text item holding
/// compact JSON and mirror the same value in `structuredContent`, which
/// the spec recommends for backwards compatibility with clients that only
/// read `content`.
fn result_of(outcome: ToolOutcome) -> CallToolResult {
    let text = serde_json::to_string(&outcome.value)
        .unwrap_or_else(|e| format!("{{\"error\":\"could not serialize tool result: {e}\"}}"));
    let content = vec![ContentBlock::text(text)];
    let mut result = if outcome.is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    result.structured_content = Some(outcome.value);
    result
}

impl ServerHandler for ForgeMcpServer {
    fn get_info(&self) -> ServerConfig {
        let mut config = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        config.server_info =
            Implementation::new("forge", env!("CARGO_PKG_VERSION")).with_title("Forge");
        config.instructions = Some(INSTRUCTIONS.to_string());
        config
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // The whole surface fits in one page, so no cursor is issued.
        Ok(ListToolsResult {
            tools: definitions().iter().map(tool_of).collect(),
            ..ListToolsResult::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Object(Map::new()));
        match self.tools.call(&request.name, &args).await {
            Ok(outcome) => Ok(CallToolResponse::Complete(result_of(outcome))),
            // Per the tools spec, an unknown tool is a protocol error.
            Err(ToolError::UnknownTool(name)) => Err(McpError::invalid_params(
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }
}

/// Serve MCP on stdin/stdout until the client closes stdin (EOF), which
/// the stdio binding names as the primary graceful-shutdown signal.
///
/// Nothing in this process may write to stdout afterwards: stdout is the
/// protocol channel.
pub async fn serve_stdio(tools: ForgeTools) -> Result<(), ForgeError> {
    let server = ForgeMcpServer::new(Arc::new(tools));
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| ForgeError::server(format!("MCP server could not start: {e}")))?;
    let reason = running
        .waiting()
        .await
        .map_err(|e| ForgeError::server(format!("MCP server stopped unexpectedly: {e}")))?;
    tracing::info!(?reason, "MCP server finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_entry_becomes_a_well_formed_mcp_tool() {
        for def in definitions() {
            let tool = tool_of(def);
            assert_eq!(tool.name, def.name);
            assert!(tool.description.is_some());
            assert_eq!(
                tool.input_schema.get("type").and_then(Value::as_str),
                Some("object"),
                "{}: inputSchema must be an object schema",
                def.name
            );
        }
    }

    #[test]
    fn a_tool_result_carries_compact_json_text_and_structured_content() {
        let outcome = ToolOutcome::ok(serde_json::json!({ "hits": [1, 2] }));
        let result = result_of(outcome.clone());

        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(outcome.value));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected a single text content item");
        };
        assert_eq!(text.text, "{\"hits\":[1,2]}");
    }

    #[test]
    fn a_tool_error_sets_is_error() {
        let result = result_of(ToolOutcome::error("unknown_run", "unknown run: r1"));
        assert_eq!(result.is_error, Some(true));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        assert!(text.text.contains("unknown run: r1"));
    }

    #[test]
    fn the_server_advertises_tools_and_names_itself_forge() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let service = std::sync::Arc::new(forge_runtime::AgentService::new(
            Arc::new(forge_providers::MockModel::new()),
            Arc::new(forge_providers::MockRouter::selecting("mock-local")),
            Arc::new(forge_execution::MockExecution::new(tmp.path())),
            Arc::new(forge_skills::FsSkillRegistry::with_roots(vec![], None)),
            Arc::new(forge_session::JsonlSessionStore::new(
                tmp.path().join("sessions"),
            )),
            forge_config::Config::default(),
        ));
        let server = ForgeMcpServer::new(Arc::new(ForgeTools::new(service, tmp.path())));

        let info = server.get_info();
        assert_eq!(info.server_info.name, "forge");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some());
        assert!(info.instructions.is_some());
    }
}
