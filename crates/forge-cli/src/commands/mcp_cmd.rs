//! `forge mcp` — the stdio MCP adapter.
//!
//! Sibling of `forge serve`: same `AgentService`, same construction path
//! (`service::build_run_service`, needle seam included), different
//! transport. The only thing assembled here is the tool host.

use std::sync::Arc;

use async_trait::async_trait;
use forge_core::ForgeError;
use forge_mcp::{Diagnostics, ForgeTools};

use crate::commands::{Context, doctor};

/// `forge mcp` — serve MCP on stdin/stdout until the client closes stdin.
///
/// Two things make this different from every other subcommand:
///
/// 1. **stdout is the protocol channel.** Nothing here may print. The
///    global `--json` flag is meaningless for this command and diagnostics
///    already go to stderr (`tracing_setup::init`), which the stdio
///    binding explicitly allows.
/// 2. **stdin is the protocol channel too**, so the run loop can never
///    prompt on it. It does not: `NativeExecution::prompt_for_approval`
///    checks `stdin().is_terminal()` first, and under an MCP client stdin
///    is a pipe — so a risky operation under `approval = "prompt"` pauses
///    the run with `ApprovalRequired` and waits for input delivered out of
///    band, which is what the `forge_run_input` tool does.
///
/// Unlike `forge serve`, this does not autostart the Laya router adapter:
/// a client expects the server to be answering requests immediately, and
/// `FallbackRouter` already covers an unreachable router.
pub async fn run(ctx: &Context) -> Result<(), ForgeError> {
    let service = Arc::new(crate::commands::service::build_run_service(ctx).await?);
    let root = ctx.project_root()?;

    tracing::info!(
        root = %root.display(),
        "serving MCP over stdio (stdout is the protocol channel)"
    );

    let tools = ForgeTools::new(service, root).with_diagnostics(Arc::new(CliDiagnostics {
        context: Context {
            global: ctx.global.clone(),
        },
    }));
    forge_mcp::serve_stdio(tools).await
}

/// Bridges the MCP `forge_doctor` tool to the CLI's own checks.
///
/// The checks probe providers, credentials, graph, skills and the needle
/// weights — a combination only this crate can see — so `forge-cli` keeps
/// them and the adapter renders them. Same code, same verdict, no
/// subprocess.
struct CliDiagnostics {
    context: Context,
}

#[async_trait]
impl Diagnostics for CliDiagnostics {
    async fn report(&self) -> Result<serde_json::Value, ForgeError> {
        let checks = doctor::collect_checks(&self.context).await?;
        Ok(doctor::report_json(&checks))
    }
}
