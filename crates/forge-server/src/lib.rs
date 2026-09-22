//! REST/SSE adapter over [`AgentService`]. The server only translates
//! HTTP ↔ service calls; all behavior lives in the transport-neutral
//! runtime. Endpoints (v0.1):
//!
//! ```text
//! GET  /health
//! GET  /v1/capabilities
//! GET  /v1/models
//! POST /v1/runs                  202 {"run_id","session_id"} (run is async)
//! GET  /v1/runs/{id}             status + events so far
//! POST /v1/runs/{id}/input       records a run-scoped `note` event
//! POST /v1/runs/{id}/cancel      aborts in-flight runs, records `cancelled`
//! GET  /v1/runs/{id}/events      SSE: stored events replayed, then live
//! GET  /v1/skills
//! GET  /v1/project/graph         stats + freshness (never builds)
//! POST /v1/project/context       graph-aware context selection
//! ```

mod handlers;
mod state;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use forge_config::Config;
use forge_core::{ForgeError, SkillRegistry};
use forge_graph::LocalGraph;
use forge_runtime::AgentService;

pub use state::{AppState, RunStatus};

/// Build the axum router. Takes the shared service plus the skill registry
/// and project graph handles explicitly so tests can compose them freely.
pub fn build_router(
    service: Arc<AgentService>,
    skills: Arc<dyn SkillRegistry>,
    graph: Option<Arc<LocalGraph>>,
    config: Config,
) -> Router {
    let state = AppState::new(service, skills, graph, config);
    Router::new()
        .route("/health", get(handlers::health))
        .route("/v1/capabilities", get(handlers::capabilities))
        .route("/v1/models", get(handlers::models))
        .route("/v1/runs", post(handlers::create_run))
        .route("/v1/runs/{id}", get(handlers::get_run))
        .route("/v1/runs/{id}/input", post(handlers::run_input))
        .route("/v1/runs/{id}/cancel", post(handlers::cancel_run))
        .route("/v1/runs/{id}/events", get(handlers::run_events))
        .route("/v1/skills", get(handlers::list_skills))
        .route("/v1/project/graph", get(handlers::graph_status))
        .route("/v1/project/context", post(handlers::project_context))
        .with_state(state)
}

/// Bind a TCP listener and return the actual address plus the server
/// future. Tests use this with port 0 for ephemeral serving.
pub async fn bind(
    router: Router,
    host: &str,
    port: u16,
) -> Result<
    (
        SocketAddr,
        impl std::future::Future<Output = Result<(), ForgeError>>,
    ),
    ForgeError,
> {
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(ForgeError::Io)?;
    let addr = listener.local_addr().map_err(ForgeError::Io)?;
    Ok((addr, async move {
        axum::serve(listener, router).await.map_err(ForgeError::Io)
    }))
}

/// Serve with graceful shutdown on ctrl-c.
pub async fn serve(router: Router, host: &str, port: u16) -> Result<(), ForgeError> {
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(ForgeError::Io)?;
    let addr = listener.local_addr().map_err(ForgeError::Io)?;
    tracing::info!(%addr, "forge server listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(ForgeError::Io)
}
