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
        EventKind::RunStarted {
            provider, model, ..
        } => {
            format!("run_started provider={provider} model={model}")
        }
        EventKind::RoutingDecisionMade {
            router,
            selected_model,
            confidence,
            fallback_used,
            ..
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
        EventKind::ToolCallRequested { tool, args_summary } => {
            format!("tool_call_requested tool={tool} args={args_summary}")
        }
        EventKind::ApprovalRequested { command, risk } => {
            format!("approval_requested command={command} risk={risk:?}")
        }
        EventKind::ApprovalDecided { command, approved } => {
            format!("approval_decided command={command} approved={approved}")
        }
        EventKind::TurnCompleted { turn } => format!("turn_completed turn={turn}"),
        EventKind::AssistantMessage { text, tool_calls } => {
            let head: String = text.chars().take(80).collect();
            let calls: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
            if calls.is_empty() {
                format!("assistant_message text={head}")
            } else {
                format!(
                    "assistant_message text={head} tool_calls={}",
                    calls.join(",")
                )
            }
        }
        EventKind::ToolResult {
            call_id,
            tool,
            output,
            is_error,
        } => {
            let head: String = output.chars().take(80).collect();
            format!("tool_result call_id={call_id} tool={tool} error={is_error} output={head}")
        }
        EventKind::SessionForked {
            from_session,
            at_position,
        } => format!("session_forked from={from_session} at_position={at_position}"),
        EventKind::InputReceived { message } => format!("input_received message={message}"),
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

/// `forge resume <id>` — continue a completed run: start a new run in the
/// same session whose model history is the session's conversation replayed
/// from the event log, and print the new run's output. (`forge session
/// show` for pure history.)
pub async fn resume(ctx: &Context, id: &str) -> Result<(), ForgeError> {
    let service = build_service(ctx)?;
    let outcome = service.resume(id).await?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome)
                .map_err(|e| ForgeError::session(format!("serializing run outcome: {e}")))?
        );
    } else {
        println!("{}", outcome.text);
    }
    Ok(())
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

/// `forge session fork <id> [--at <position-or-run-id>]` — branch a session
/// into a new one whose log is a copy of the source's prefix. The source is
/// untouched; the fork is resumable like any other session.
pub fn fork(ctx: &Context, id: &str, at: Option<&str>) -> Result<(), ForgeError> {
    let service = build_service(ctx)?;
    let fork = service.fork_session(id, at)?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&fork)
                .map_err(|e| ForgeError::session(format!("serializing fork: {e}")))?
        );
    } else {
        println!(
            "forked {} at position {} (run {}) -> {} ({} events copied)",
            fork.source_session_id,
            fork.at_position,
            fork.at_run_id,
            fork.session_id,
            fork.events_copied
        );
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
