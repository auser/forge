use std::path::Path;
use std::sync::Arc;

use forge_core::{ApprovalPolicy, ExecutionProvider, ForgeError};
use forge_execution::{MockExecution, NativeExecution};
use forge_runtime::AgentService;
use forge_session::JsonlSessionStore;
use forge_skills::FsSkillRegistry;

use crate::commands::Context;

/// Build the execution provider from configuration. The project root is
/// used for file-op risk classification.
pub fn build_execution(
    config: &forge_config::Config,
    project_root: &Path,
) -> Result<Arc<dyn ExecutionProvider>, ForgeError> {
    match config.execution.as_str() {
        "native" => Ok(Arc::new(NativeExecution::new(
            ApprovalPolicy::parse(&config.approval)?,
            project_root,
        ))),
        "mock" => Ok(Arc::new(MockExecution::new(project_root))),
        other => Err(ForgeError::execution(format!(
            "unknown execution provider {other:?} (expected native or mock)"
        ))),
    }
}

/// [`build_service`] plus the on-device Needle brain, which enables the
/// direct-dispatch fast path for fresh prompts (`AgentService::with_needle`).
///
/// The engine is attached only when it can actually answer — that check is
/// `forge_needle::engine_if_available`, the same seam `forge graph build`
/// uses, so env `FORGE_NEEDLE_BACKEND=hash` and the `[needle]` config are
/// honoured in exactly one place. Unavailable (no `ffi` feature, weights not
/// fetched) yields `None` and the plain agent loop.
///
/// Commands that never start a fresh prompt (`resume`, `cancel`, session
/// inspection) use plain [`build_service`]: the fast path cannot apply to
/// them, so probing the engine would only cost them latency.
pub async fn build_run_service(ctx: &Context) -> Result<AgentService, ForgeError> {
    let service = build_service(ctx)?;
    let engine = forge_needle::engine_if_available(service.config()).await;
    Ok(service.with_needle(engine))
}

/// Build the transport-neutral agent runtime from the resolved
/// configuration: model provider, decision router (with fallback),
/// execution provider, filesystem skill registry, and the JSONL session
/// store.
pub fn build_service(ctx: &Context) -> Result<AgentService, ForgeError> {
    let resolved = ctx.resolve_config()?;
    let root = ctx.project_root()?;

    let model = forge_providers::model_from_config(&resolved.config, &root)?;
    // Routing registry: `[models]` entries that declare capabilities are
    // known; the rest stay optimistic-unknown.
    let mut registry: Vec<(String, forge_core::ModelCapabilities)> = resolved
        .config
        .model_entries()
        .iter()
        .filter_map(|(name, entry)| entry.capabilities_if_known().map(|c| (name.clone(), c)))
        .collect();
    registry.push((model.name().to_string(), model.capabilities()));
    let router = forge_providers::router_from_config(&resolved.config, &registry)?;

    let execution = build_execution(&resolved.config, &root)?;
    let skills = Arc::new(FsSkillRegistry::new(&root, Some(execution.clone())));

    let sessions = Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions")));

    // Wire the project graph when one has been built; absence never
    // blocks a run.
    let graph = forge_graph::LocalGraph::open(&root)
        .ok()
        .filter(|g| g.graph_file().is_file())
        .map(|g| Arc::new(g) as Arc<dyn forge_core::ProjectGraph>);

    // Resolve a provider per routed model name: `[models]` entries supply
    // endpoint/key/capability overrides; anything else falls back to the
    // global model_* settings or the built-in mock.
    let cfg = resolved.config.clone();
    let root_for_factory = root.clone();
    let factory = move |name: &str| -> Result<Arc<dyn forge_core::ModelProvider>, ForgeError> {
        let mut cfg = cfg.clone();
        cfg.model = name.to_string();
        forge_providers::model_from_config(&cfg, &root_for_factory)
    };

    Ok(
        AgentService::new(model, router, execution, skills, sessions, resolved.config)
            .with_graph(graph)
            .with_model_factory(Arc::new(factory)),
    )
}
