use std::io::{BufRead, IsTerminal};
use std::sync::Arc;

use forge_core::{EventKind, ForgeError, ProjectGraph};
use forge_runtime::RunOptions;

use crate::commands::Context;
use crate::commands::service::build_run_service;

/// `forge run <prompt...>` — run the agent loop, print the final text.
///
/// When stdin is not a terminal, a feeder task forwards stdin lines into
/// the run's input channel so approvals can be answered non-interactively
/// (`echo y | forge run ...`). At stdin EOF the channel is closed and a
/// pending approval fails the run cleanly instead of hanging.
///
/// `--skill <NAME>` asks for a skill explicitly; an unknown name is the
/// runtime's `ForgeError::Skill` on stderr with a non-zero exit — the
/// CLI's ordinary error path, nothing printed here.
pub async fn run(
    ctx: &Context,
    prompt: Vec<String>,
    max_turns: Option<u32>,
    skills: Vec<String>,
    plan: bool,
) -> Result<(), ForgeError> {
    let prompt = prompt.join(" ");
    let service = Arc::new(build_run_service(ctx).await?);

    let run_id = forge_session::new_run_id();
    let feeder = spawn_stdin_feeder(&service, &run_id);

    let project_root = ctx.project_root()?;
    let task_store = Arc::new(forge_task::JsonlTaskStore::for_project(&project_root));
    let result = forge_runtime::run_development_workflow(
        service.as_ref(),
        task_store,
        &project_root,
        &prompt,
        RunOptions {
            run_id: Some(run_id),
            max_turns,
            activate_skills: skills,
            plan_before_execute: plan,
            ..RunOptions::default()
        },
    )
    .await;

    if let Some(handle) = feeder {
        handle.abort();
    }
    let outcome = result?;
    let learning_candidates = super::learn_cmd::refresh_after_success(&project_root)?;

    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome)
                .map_err(|e| ForgeError::session(format!("serializing run outcome: {e}")))?
        );
    } else {
        if outcome.diff.is_empty() {
            println!("{}", outcome.review);
        } else {
            println!(
                "Plan:\n{}\n\nReview:\n{}\n\nDiff:\n{}",
                outcome.plan, outcome.review, outcome.diff
            );
        }
        print_status(
            &project_root,
            &prompt,
            &outcome.run.events,
            learning_candidates,
        );
    }
    Ok(())
}

/// One compact post-run receipt makes the default workflow inspectable without
/// asking people to learn several status commands. The underlying event log
/// remains the source of truth for full telemetry.
fn print_status(
    root: &std::path::Path,
    task: &str,
    events: &[forge_core::Event],
    learning_candidates: usize,
) {
    let route = events.iter().rev().find_map(|event| match &event.kind {
        EventKind::RoutingDecisionMade {
            router,
            selected_model,
            phase,
            fallback_used,
            ..
        } => Some(format!(
            "{selected_model} via {router} for {phase:?}{}",
            if *fallback_used { " (fallback)" } else { "" }
        )),
        _ => None,
    });
    let cost = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::UsageRecorded { cost_usd, .. } => *cost_usd,
            _ => None,
        })
        .sum::<f64>();
    let graph = forge_graph::LocalGraph::open(root)
        .ok()
        .filter(|graph| graph.graph_file().is_file())
        .map(|graph| if graph.is_fresh() { "fresh" } else { "stale" })
        .unwrap_or("unavailable");
    let graph_paths = forge_graph::LocalGraph::open(root)
        .ok()
        .filter(|graph| graph.graph_file().is_file())
        .map(|graph| {
            graph
                .context(task, 5)
                .into_iter()
                .map(|hit| hit.path)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let relevant = forge_context::relevant_project_learnings(
        &forge_context::load_project_learnings(root),
        task,
        &graph_paths,
    )
    .len();
    let route = route.unwrap_or_else(|| "Needle safe-tool fast path".to_string());
    eprintln!(
        "forge: {route} · cost ${cost:.4} · graph {graph} · {relevant} relevant learnings · {learning_candidates} learning candidates"
    );
}

/// Forward stdin lines to the run's input channel. Only spawned when
/// stdin is not a terminal (on a TTY the execution provider prompts
/// inline instead). Returns None for TTY stdin.
fn spawn_stdin_feeder(
    service: &Arc<forge_runtime::AgentService>,
    run_id: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let service = Arc::clone(service);
    let run_id = run_id.to_string();
    Some(tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match lock.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if service.send_input(&run_id, line.trim_end()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // stdin exhausted: no more approvals can arrive.
        service.close_input(&run_id);
    }))
}
