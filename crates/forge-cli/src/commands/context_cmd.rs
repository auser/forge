//! Shared CLI/chat bridge. Never build an AgentService or start observer jobs
//! merely to inspect context or change session consent.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_core::ForgeError;
use forge_runtime::inspection::{ContextMemoryService, ObserverReadiness};
use forge_session::JsonlSessionStore;
use serde_json::Value;

use super::Context;
use crate::cli::{ContextCommand, MemoryCommand};

#[derive(Clone, Copy)]
pub(crate) enum Request {
    ContextStatus,
    MemoryStatus,
    SetMemory(bool),
    Show(usize),
    Sources(usize),
}

fn readiness(config: &forge_config::Config, root: &Path) -> ObserverReadiness {
    let reason = if !config.observer.enabled {
        Some("project observer is disabled")
    } else if config.observer.model.is_none() {
        Some("an explicit observer model is required")
    } else if forge_providers::observer_prices_from_config(config).is_none() {
        Some("complete observer prices are required")
    } else if forge_providers::observer_model_from_config(config, root).is_err() {
        Some("observer model unavailable under configured policy")
    } else {
        None
    };
    match reason {
        Some(reason) => ObserverReadiness::Unavailable {
            reason: reason.into(),
        },
        None => ObserverReadiness::Ready,
    }
}

pub(crate) fn query(
    config: Arc<forge_config::Config>,
    sessions: Arc<JsonlSessionStore>,
    root: PathBuf,
    session: &str,
    request: Request,
    require_existing: bool,
) -> Result<Value, ForgeError> {
    if session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(ForgeError::session("invalid session identifier"));
    }
    if require_existing
        && sessions
            .events_for(session)
            .map_err(|_| ForgeError::session("session inspection unavailable"))?
            .is_empty()
    {
        return Err(ForgeError::session("unknown session"));
    }
    let readiness = readiness(&config, &root);
    let service = ContextMemoryService::new(
        config,
        sessions.clone(),
        root.join(".forge/context"),
        readiness,
    );
    let encode = |result: Result<Value, serde_json::Error>| {
        result.map_err(|_| ForgeError::session("context status serialization unavailable"))
    };
    let mut value = match request {
        Request::ContextStatus => encode(serde_json::to_value(service.context_status(session)?))?,
        Request::MemoryStatus => encode(serde_json::to_value(service.memory_status(session)?))?,
        Request::SetMemory(enabled) => encode(serde_json::to_value(
            service.set_memory_enabled(session, enabled)?,
        ))?,
        Request::Show(offset) => {
            encode(serde_json::to_value(service.memory_show(session, offset)?))?
        }
        Request::Sources(offset) => encode(serde_json::to_value(
            service.memory_sources(session, offset)?,
        ))?,
    };
    // Preserve the machine-readable envelope while filtering every text value.
    sessions.redactor().redact_value(&mut value);
    Ok(value)
}

fn run(ctx: &Context, session: String, request: Request) -> Result<(), ForgeError> {
    let root = ctx.project_root()?;
    let config = Arc::new(ctx.resolve_config()?.config);
    let sessions = Arc::new(JsonlSessionStore::new(root.join(".forge/sessions")));
    let report = query(config, sessions, root, &session, request, true)?;
    if ctx.global.json {
        println!("{report}");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|_| ForgeError::session("context status serialization unavailable"))?
        );
    }
    Ok(())
}

pub fn context(ctx: &Context, command: ContextCommand) -> Result<(), ForgeError> {
    match command {
        ContextCommand::Status { session } => run(ctx, session, Request::ContextStatus),
    }
}

pub fn memory(ctx: &Context, command: MemoryCommand) -> Result<(), ForgeError> {
    match command {
        MemoryCommand::Status { session } => run(ctx, session, Request::MemoryStatus),
        MemoryCommand::On { session } => run(ctx, session, Request::SetMemory(true)),
        MemoryCommand::Off { session } => run(ctx, session, Request::SetMemory(false)),
        MemoryCommand::Show { session, offset } => run(ctx, session, Request::Show(offset)),
        MemoryCommand::Sources { session, offset } => run(ctx, session, Request::Sources(offset)),
    }
}
