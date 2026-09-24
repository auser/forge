use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::{ForgeError, RunningProcess};

use crate::commands::Context;
use crate::commands::router_cmd::spawn_adapter;
use crate::commands::service::{build_execution, build_run_service};

/// How long to wait for the adapter to preload its model and come up
/// (Laya's checkpoint load is slow on busy machines).
const ADAPTER_READY_BUDGET: Duration = Duration::from_secs(300);

/// `forge serve [--host --port]` — REST/SSE server over the same
/// AgentService the CLI uses. Binds loopback by default. When
/// `router = "laya"` and the router endpoint is unreachable, the Laya
/// adapter is auto-started as a managed child first (see
/// [`maybe_autostart_laya`]).
pub async fn run(ctx: &Context, host: Option<String>, port: Option<u16>) -> Result<(), ForgeError> {
    let service = Arc::new(build_run_service(ctx).await?);
    let resolved = ctx.resolve_config()?;
    let host = host.unwrap_or_else(|| resolved.config.server_host.clone());
    let port = port.unwrap_or(resolved.config.server_port);

    let root = ctx.project_root()?;
    let graph = forge_graph::LocalGraph::open(&root).ok().map(Arc::new);
    let skills = service.skills().clone();
    let router = forge_server::build_router(service, skills, graph, resolved.config.clone());

    // Bring up the Laya adapter before the listening line, so everything
    // is ready when the user sees the URL.
    let mut adapter = maybe_autostart_laya(&resolved.config, &root).await;
    let adapter_url = adapter.as_ref().map(|(_, url)| url.clone());

    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({
                "listening": format!("{host}:{port}"),
                "laya_adapter": adapter_url,
            })
        );
    } else {
        if let Some(url) = &adapter_url {
            println!("laya adapter ready at {url}");
        }
        println!("listening on http://{host}:{port}");
    }

    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        // Kill the managed adapter first; never leave an orphan.
        if let Some((child, _)) = adapter.as_mut() {
            let _ = child.kill().await;
        }
    };
    forge_server::serve_with_shutdown(router, &host, port, shutdown).await
}

/// When `router = "laya"` and the router endpoint is unreachable, start
/// the embedded adapter as a managed child and wait for readiness.
/// Failures warn and degrade to static fallback routing — `forge serve`
/// never fails because the router is missing. (`forge run` deliberately
/// does not autostart: FallbackRouter already covers a down router for
/// one-shot commands.)
async fn maybe_autostart_laya(
    config: &forge_config::Config,
    root: &std::path::Path,
) -> Option<(Box<dyn RunningProcess>, String)> {
    if config.router != "laya" {
        return None;
    }
    let url = config
        .router_url
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:8788/decide".to_string());
    let (host, port) = host_port_of(&url);
    let base = format!("http://{host}:{port}");

    if !config.router_autostart {
        if !probe_http(&base, Duration::from_millis(500)).await {
            tracing::warn!(
                "laya router endpoint {base} unreachable and router_autostart is off; \
                 static fallback routing applies"
            );
        }
        return None;
    }

    if probe_http(&base, Duration::from_millis(500)).await {
        tracing::debug!(url = %base, "laya router already reachable; not autostarting");
        return None;
    }

    let exec = build_execution(config, root).ok()?;
    let mut child = match spawn_adapter(&exec, &host, port).await {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!("laya adapter not started: {e}; static fallback routing applies");
            return None;
        }
    };

    if wait_until_ready(&base, ADAPTER_READY_BUDGET).await {
        Some((child, base))
    } else {
        let _ = child.kill().await;
        tracing::warn!(
            "laya adapter did not become ready within {}s; static fallback routing applies",
            ADAPTER_READY_BUDGET.as_secs()
        );
        None
    }
}

/// Parse `http(s)://host[:port]/...` (default port 8788, matching the
/// adapter and the laya router_url default).
fn host_port_of(url: &str) -> (String, u16) {
    let without_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or("");
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().unwrap_or(8788);
            (host.to_string(), port)
        }
        None => (authority.to_string(), 8788),
    }
}

/// Any HTTP response counts as reachable (the adapter serves liveness on
/// every GET); connection/timeout errors mean "down".
async fn probe_http(base: &str, timeout: Duration) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
        return false;
    };
    client.get(format!("{base}/")).send().await.is_ok()
}

async fn wait_until_ready(base: &str, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if probe_http(base, Duration::from_secs(2)).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_parsing() {
        assert_eq!(
            host_port_of("http://127.0.0.1:8788/decide"),
            ("127.0.0.1".to_string(), 8788)
        );
        assert_eq!(
            host_port_of("http://localhost:9000/"),
            ("localhost".to_string(), 9000)
        );
        assert_eq!(
            host_port_of("https://router.internal/decide"),
            ("router.internal".to_string(), 8788)
        );
        assert_eq!(
            host_port_of("http://[::1]:8788/"),
            ("[::1]".to_string(), 8788)
        );
    }
}
