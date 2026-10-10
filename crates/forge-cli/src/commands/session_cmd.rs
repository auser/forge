use forge_core::{Event, EventKind, ForgeError};
use forge_session::JsonlSessionStore;
use std::sync::Arc;

use crate::commands::Context;
use crate::commands::service::{build_run_service, build_service};

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
        EventKind::UsageRecorded {
            model,
            usage,
            cost_usd,
        } => format!(
            "usage_recorded model={model} tokens={} cost_usd={}",
            usage
                .map(|value| value.total_tokens.to_string())
                .unwrap_or_else(|| "unknown".into()),
            cost_usd
                .map(|value| format!("{value:.6}"))
                .unwrap_or_else(|| "unknown".into())
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
        EventKind::ToolPolicyDecision {
            tool,
            risk,
            approval_policy,
            disposition,
            reason,
            policy_schema,
            forge_version,
        } => format!(
            "tool_policy_decision tool={tool} risk={risk:?} approval={approval_policy:?} \
             disposition={disposition:?} reason={reason:?} policy_schema={policy_schema} \
             forge_version={forge_version}"
        ),
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
        EventKind::ContextPlanRecorded {
            plan_id,
            request_ordinal,
            stable_prefix_hash,
            prefix_changed,
            total_estimated_input_tokens,
            reserved_output_tokens,
            plan_path,
        } => format!(
            "context_plan_recorded id={plan_id} request={request_ordinal} prefix={} changed={} input_tokens={} output_tokens={} path={plan_path}",
            &stable_prefix_hash[..stable_prefix_hash.len().min(12)],
            prefix_changed,
            total_estimated_input_tokens,
            reserved_output_tokens
                .map_or_else(|| "provider-default".to_string(), |value| value.to_string()),
        ),
        EventKind::ContextPlanUnavailable {
            request_ordinal,
            error_category,
            message,
        } => format!(
            "context_plan_unavailable request={request_ordinal} category={error_category} message={message}"
        ),
        EventKind::InputReceived { message } => format!("input_received message={message}"),
        EventKind::ToolOutputArtifact {
            handle, event_seq, ..
        } => {
            format!("tool_output_artifact handle={handle} source_seq={event_seq}")
        }
        EventKind::ToolOutputCompression {
            event_seq,
            version,
            kind,
            reason,
            baseline,
            view,
            omitted,
            ..
        } => format!(
            "tool_output_compression source_seq={event_seq} version={version} kind={kind:?} reason={reason:?} baseline_tokens={} view_tokens={} omitted={omitted}",
            baseline.estimated_tokens, view.estimated_tokens,
        ),
        EventKind::MemoryObservationChanged { enabled } => {
            format!("memory_observation_changed enabled={enabled}")
        }
        EventKind::Note { message } => format!("note message={message}"),
        EventKind::Error { message } => format!("error message={message}"),
        EventKind::Cancelled { reason } => format!("cancelled reason={reason}"),
        EventKind::AssistantDelta { text } => {
            let head: String = text.chars().take(40).collect();
            format!("assistant_delta text={head}")
        }
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
        // Numbered with the 1-based log position, which is what
        // `forge session fork --at <N>` takes — otherwise you would have to
        // re-run with `--json` and count lines to find a cut point.
        let width = events.len().to_string().len();
        for (position, event) in events.iter().enumerate() {
            println!("{:>width$}  {}", position + 1, format_event(event));
        }
    }
    Ok(())
}

/// `forge resume <id>` — continue a completed run: start a new run in the
/// same session whose model history is the session's conversation replayed
/// from the event log, and print the new run's output. (`forge session
/// show` for pure history.)
pub async fn resume(ctx: &Context, id: &str) -> Result<(), ForgeError> {
    let project_root = ctx.project_root()?;
    if id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        let task_store = Arc::new(forge_task::JsonlTaskStore::for_project(&project_root));
        if task_store.root().join(format!("{id}.jsonl")).is_file() {
            let service = build_run_service(ctx).await?;
            let outcome = forge_runtime::resume_development_workflow(
                &service,
                task_store,
                &project_root,
                id,
                forge_runtime::RunOptions::default(),
            )
            .await?;
            if ctx.global.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome).map_err(|error| {
                        ForgeError::session(format!("serializing task outcome: {error}"))
                    })?
                );
            } else if outcome.diff.is_empty() {
                println!("{}", outcome.review);
            } else {
                println!(
                    "Plan:\n{}\n\nReview:\n{}\n\nDiff:\n{}",
                    outcome.plan, outcome.review, outcome.diff
                );
            }
            return Ok(());
        }
    }
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

/// `forge session decisions` — summarise `.forge/sessions/*.decisions.jsonl`.
///
/// The decline rate is the number the next phase's design hangs on: whether
/// falling back to a second decider is worth its latency depends entirely on
/// how often the first one declines on real work. Published benchmarks do not
/// transfer; this does.
pub fn decisions(ctx: &Context) -> Result<(), ForgeError> {
    let root = ctx.project_root()?.join(".forge").join("sessions");
    let mut total = 0u64;
    let mut dispatched = 0u64;
    let mut declined = 0u64;
    let mut unavailable = 0u64;
    let mut elapsed_ms_total = 0u64;
    let mut routed = 0u64;
    // Per-model totals from Complete records, keyed by model name. BTreeMap
    // so the report's order is stable.
    let mut models: std::collections::BTreeMap<String, forge_session::SpendTotals> =
        std::collections::BTreeMap::new();

    if root.is_dir() {
        for entry in std::fs::read_dir(&root)
            .map_err(|e| ForgeError::session(format!("reading {}: {e}", root.display())))?
        {
            let entry = entry
                .map_err(|e| ForgeError::session(format!("reading {}: {e}", root.display())))?;
            let path = entry.path();
            if !path.to_string_lossy().ends_with(".decisions.jsonl") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| ForgeError::session(format!("reading {}: {e}", path.display())))?;
            for line in raw.lines() {
                // A truncated final line is normal for an append-only log that
                // was being written when the process died. Skip, never fail.
                let Ok(record) = serde_json::from_str::<forge_session::DecisionRecord>(line) else {
                    continue;
                };
                match record.stage {
                    forge_session::Stage::Decide => {
                        total += 1;
                        elapsed_ms_total += record.elapsed_ms;
                        match record.outcome {
                            forge_session::Outcome::Dispatched => dispatched += 1,
                            forge_session::Outcome::Declined => declined += 1,
                            forge_session::Outcome::Unavailable => unavailable += 1,
                            _ => {}
                        }
                    }
                    forge_session::Stage::Route => routed += 1,
                    forge_session::Stage::Complete => {
                        let totals = models.entry(record.choice.clone()).or_default();
                        totals.calls += 1;
                        if let Some(usage) = record.usage {
                            totals.input_tokens += u64::from(usage.prompt_tokens);
                            totals.output_tokens += u64::from(usage.completion_tokens);
                        }
                        if let Some(cost) = record.cost_usd {
                            totals.cost_usd += cost;
                        }
                    }
                }
            }
        }
    }

    let decline_rate = if total == 0 {
        0.0
    } else {
        declined as f64 / total as f64
    };
    let mean_ms = elapsed_ms_total.checked_div(total).unwrap_or(0);

    let report = serde_json::json!({
        "decide": {
            "total": total,
            "dispatched": dispatched,
            "declined": declined,
            "unavailable": unavailable,
            "decline_rate": decline_rate,
            "mean_elapsed_ms": mean_ms,
        },
        "route": { "total": routed },
        "models": models.iter().map(|(name, t)| (name.clone(), serde_json::json!({
            "calls": t.calls,
            "input_tokens": t.input_tokens,
            "output_tokens": t.output_tokens,
            "cost_usd": t.cost_usd,
        }))).collect::<serde_json::Map<String, serde_json::Value>>(),
    });

    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| ForgeError::session(format!("serializing decision summary: {e}")))?
        );
    } else {
        println!(
            "decide  {total} total — {dispatched} dispatched, {declined} declined, {unavailable} unavailable"
        );
        println!(
            "        decline rate {:.0}%, mean {mean_ms} ms",
            decline_rate * 100.0
        );
        println!("route   {routed} total");
        for (name, t) in &models {
            // Six decimals: per-call costs are often below a cent, and
            // "$0.0000" for real spend would read as "free".
            println!(
                "model   {name}: {} calls, {} in / {} out tokens, ${:.6}",
                t.calls, t.input_tokens, t.output_tokens, t.cost_usd
            );
        }
    }
    Ok(())
}
