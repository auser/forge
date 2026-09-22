use std::sync::Arc;

use forge_core::ForgeError;

use crate::commands::Context;
use crate::commands::service::build_service;

/// `forge serve [--host --port]` — REST/SSE server over the same
/// AgentService the CLI uses. Binds loopback by default.
pub async fn run(ctx: &Context, host: Option<String>, port: Option<u16>) -> Result<(), ForgeError> {
    let service = Arc::new(build_service(ctx)?);
    let resolved = ctx.resolve_config()?;
    let host = host.unwrap_or_else(|| resolved.config.server_host.clone());
    let port = port.unwrap_or(resolved.config.server_port);

    let root = ctx.project_root()?;
    let graph = forge_graph::LocalGraph::open(&root).ok().map(Arc::new);
    let skills = service.skills().clone();
    let router = forge_server::build_router(service, skills, graph, resolved.config.clone());

    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({ "listening": format!("{host}:{port}") })
        );
    } else {
        println!("listening on http://{host}:{port}");
    }
    forge_server::serve(router, &host, port).await
}
