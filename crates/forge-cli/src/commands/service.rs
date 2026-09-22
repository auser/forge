use std::sync::Arc;

use forge_core::{ApprovalPolicy, ExecutionProvider, ForgeError};
use forge_execution::{MockExecution, NativeExecution};
use forge_runtime::AgentService;
use forge_session::JsonlSessionStore;
use forge_skills::FsSkillRegistry;

use crate::commands::Context;

/// Build the execution provider from configuration.
pub fn build_execution(
    config: &forge_config::Config,
) -> Result<Arc<dyn ExecutionProvider>, ForgeError> {
    match config.execution.as_str() {
        "native" => Ok(Arc::new(NativeExecution::new(ApprovalPolicy::parse(
            &config.approval,
        )?))),
        "mock" => Ok(Arc::new(MockExecution::new())),
        other => Err(ForgeError::execution(format!(
            "unknown execution provider {other:?} (expected native or mock)"
        ))),
    }
}

/// Build the transport-neutral agent runtime from the resolved
/// configuration: model provider, decision router (with fallback),
/// execution provider, filesystem skill registry, and the JSONL session
/// store.
pub fn build_service(ctx: &Context) -> Result<AgentService, ForgeError> {
    let resolved = ctx.resolve_config()?;
    let root = ctx.project_root()?;

    let model = forge_providers::model_from_config(&resolved.config)?;
    let registry = vec![(model.name().to_string(), model.capabilities())];
    let router = forge_providers::router_from_config(&resolved.config, &registry)?;

    let execution = build_execution(&resolved.config)?;
    let skills = Arc::new(FsSkillRegistry::new(&root, Some(execution.clone())));

    let sessions = Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions")));

    Ok(AgentService::new(
        model,
        router,
        execution,
        skills,
        sessions,
        resolved.config,
    ))
}
