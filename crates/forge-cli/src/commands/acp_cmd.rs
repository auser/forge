//! `forge acp` — the stdio Agent Client Protocol adapter.
//!
//! Sibling of `forge mcp`: same `AgentService`, same construction path
//! (`service::build_run_service`, needle seam included), different
//! protocol. MCP hands an editor's *agent* a set of forge tools; ACP hands
//! the editor forge itself as the agent.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use forge_acp::ServiceFactory;
use forge_core::ForgeError;
use forge_runtime::AgentService;

use crate::cli::GlobalOpts;
use crate::commands::Context;

/// `forge acp` — serve ACP on stdin/stdout until the client closes stdin.
///
/// The same two rules as `forge mcp` apply, for the same reasons:
///
/// 1. **stdout is the protocol channel.** Nothing here may print. The
///    global `--json` flag is meaningless for this command, and diagnostics
///    already go to stderr (`tracing_setup::init`).
/// 2. **stdin is the protocol channel too**, so the run loop can never
///    prompt on it — and it does not: `NativeExecution::ask_approval` asks
///    inline only on the `ApprovalChannel::InlineTty` channel *and* a
///    terminal stdin, and under an editor stdin is a pipe. A risky
///    operation under `approval = "prompt"` therefore pauses
///    the run and waits for input delivered out of band, which here is the
///    answer to a `session/request_permission` request. That is how a
///    permission prompt ends up in the editor's own UI.
///
/// Unlike `forge serve`, this does not autostart the Laya router adapter: an
/// editor expects its agent to be answering immediately, and
/// `FallbackRouter` already covers an unreachable router.
pub async fn run(ctx: &Context) -> Result<(), ForgeError> {
    tracing::info!("serving ACP over stdio (stdout is the protocol channel)");
    let factory = Arc::new(CliServiceFactory {
        global: ctx.global.clone(),
    });
    forge_acp::serve_stdio(factory).await
}

/// Builds a session's runtime at the `cwd` the ACP client asked for.
///
/// An ACP client picks the project directory per session (`session/new`),
/// which is not necessarily the directory the editor happened to launch us
/// in — so the root cannot be resolved once at startup. Overriding
/// `--project` per session routes that choice through exactly the same
/// config discovery, provider resolution and needle probe as every other
/// subcommand, rather than growing a second way to build a service.
struct CliServiceFactory {
    global: GlobalOpts,
}

#[async_trait]
impl ServiceFactory for CliServiceFactory {
    async fn build(&self, root: &Path) -> Result<Arc<AgentService>, ForgeError> {
        let context = Context {
            global: GlobalOpts {
                project: Some(root.to_path_buf()),
                ..self.global.clone()
            },
        };
        Ok(Arc::new(
            crate::commands::service::build_run_service(&context).await?,
        ))
    }
}
