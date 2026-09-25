use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::{ForgeError, RunningProcess};
use forge_providers::EgressPolicy;

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
    // Resolved by `forge-providers` rather than re-derived here, so the
    // adapter is probed at the URL the router would actually dial.
    let url = forge_providers::router_endpoint("laya", config)?;
    if local_only_blocks_adapter(config, &url) {
        tracing::warn!(
            "router = \"laya\" endpoint {url} is not local; --local-only pruned it from the \
             router stack, so no adapter is started and static routing applies"
        );
        return None;
    }
    let (host, port) = host_port_of(&url);
    let base = format!("http://{host}:{port}");
    let egress = EgressPolicy::from_config(config);

    if !config.router_autostart {
        if !probe_http(&base, Duration::from_millis(500), egress).await {
            tracing::warn!(
                "laya router endpoint {base} unreachable and router_autostart is off; \
                 static fallback routing applies"
            );
        }
        return None;
    }

    if probe_http(&base, Duration::from_millis(500), egress).await {
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

    if wait_until_ready(&base, ADAPTER_READY_BUDGET, egress).await {
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

/// Whether `local_only` rules out autostarting an adapter for this endpoint.
///
/// `router_from_config` has already pruned an off-device laya router from the
/// stack, so there is nothing to start — and probing the address anyway would
/// be forge originating a request to exactly the host the setting forbids,
/// the same reason `forge doctor` skips its probe. Worse, `spawn_adapter`
/// would then try to bind a local adapter to a remote hostname.
fn local_only_blocks_adapter(config: &forge_config::Config, url: &str) -> bool {
    config.local_only && !forge_providers::endpoint_is_local(url)
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
///
/// `egress` keeps even this probe inside `local_only`'s promise: the caller
/// has already established the base is local, and a redirect must not take
/// the probe anywhere else.
async fn probe_http(base: &str, timeout: Duration, egress: EgressPolicy) -> bool {
    let Ok(client) = egress.client(timeout) else {
        return false;
    };
    client.get(format!("{base}/")).send().await.is_ok()
}

async fn wait_until_ready(base: &str, budget: Duration, egress: EgressPolicy) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if probe_http(base, Duration::from_secs(2), egress).await {
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

    /// `forge serve` must not probe — or try to bind an adapter to — a host
    /// `local_only` forbids. The loopback default keeps working.
    #[test]
    fn local_only_blocks_autostart_only_for_an_off_device_endpoint() {
        let on = forge_config::Config {
            local_only: true,
            ..forge_config::Config::default()
        };
        assert!(local_only_blocks_adapter(
            &on,
            "https://laya.example.com/decide"
        ));
        assert!(local_only_blocks_adapter(
            &on,
            "http://192.168.1.9:8788/decide"
        ));
        assert!(!local_only_blocks_adapter(
            &on,
            "http://127.0.0.1:8788/decide"
        ));

        // Off, nothing is blocked.
        let off = forge_config::Config::default();
        assert!(!local_only_blocks_adapter(
            &off,
            "https://laya.example.com/decide"
        ));
    }
}
