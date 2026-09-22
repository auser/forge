use forge_core::{Event, EventKind, ForgeError};
use forge_session::JsonlSessionStore;

use crate::commands::Context;
use crate::commands::service::build_service;

fn store(ctx: &Context) -> Result<JsonlSessionStore, ForgeError> {
    Ok(JsonlSessionStore::new(
        ctx.project_root()?.join(".forge").join("sessions"),
    ))
}

/// One-line human rendering of an event.
fn format_event(event: &Event) -> String {
    let ts = event.ts.format("%Y-%m-%dT%H:%M:%SZ");
    let detail = match &event.kind {
        EventKind::RunStarted { provider, model } => {
            format!("run_started provider={provider} model={model}")
        }
        EventKind::RoutingDecisionMade {
            router,
            selected_model,
            confidence,
            fallback_used,
        } => format!(
            "routing_decision router={router} model={selected_model} confidence={confidence:.2} fallback={fallback_used}"
        ),
        EventKind::SkillActivated { name, path } => {
            format!("skill_activated name={name} path={}", path.display())
        }
        EventKind::ToolStarted { name } => format!("tool_started name={name}"),
        EventKind::ToolCompleted { name, success } => {
            format!("tool_completed name={name} success={success}")
        }
        EventKind::FileChanged { path } => format!("file_changed path={}", path.display()),
        EventKind::Note { message } => format!("note message={message}"),
        EventKind::Error { message } => format!("error message={message}"),
        EventKind::Cancelled { reason } => format!("cancelled reason={reason}"),
        EventKind::Completed { summary } => format!("completed summary={summary}"),
    };
    format!("{ts} [{}] {detail}", event.run_id)
}

fn print_events(ctx: &Context, events: &[Event]) -> Result<(), ForgeError> {
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(events)
                .map_err(|e| ForgeError::session(format!("serializing events: {e}")))?
        );
    } else {
        for event in events {
            println!("{}", format_event(event));
        }
    }
    Ok(())
}

/// `forge resume <id>` — load and report the event history of a session or
/// run. Real re-execution arrives in a later phase.
pub fn resume(ctx: &Context, id: &str) -> Result<(), ForgeError> {
    let service = build_service(ctx)?;
    let events = service.resume(id)?;
    if !ctx.global.json {
        println!("session history for {id} ({} events)", events.len());
    }
    print_events(ctx, &events)
}

/// `forge cancel <id>` — record a cancellation event for a run or session.
pub fn cancel(ctx: &Context, id: &str) -> Result<(), ForgeError> {
    let service = build_service(ctx)?;
    service.cancel(id)?;
    if ctx.global.json {
        println!("{}", serde_json::json!({ "cancelled": id }));
    } else {
        println!("cancelled {id}");
    }
    Ok(())
}

/// `forge session list`
pub fn list(ctx: &Context) -> Result<(), ForgeError> {
    let sessions = store(ctx)?.list_sessions()?;
    if ctx.global.json {
        let out: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "session_id": s.session_id,
                    "event_count": s.event_count,
                    "path": s.path,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::session(format!("serializing sessions: {e}")))?
        );
    } else if sessions.is_empty() {
        println!("no sessions yet");
    } else {
        for s in &sessions {
            println!("{} ({} events)", s.session_id, s.event_count);
        }
    }
    Ok(())
}

/// `forge session show <id>`
pub fn show(ctx: &Context, id: &str) -> Result<(), ForgeError> {
    let events = store(ctx)?.events_for(id)?;
    if events.is_empty() {
        return Err(ForgeError::session(format!("unknown session: {id}")));
    }
    print_events(ctx, &events)
}
