use std::sync::Arc;

use forge_core::ForgeError;
use forge_runtime::{TaskInspector, TaskView};
use forge_session::JsonlSessionStore;
use forge_task::JsonlTaskStore;

use super::Context;

fn inspector(ctx: &Context) -> Result<TaskInspector, ForgeError> {
    let root = ctx.project_root()?;
    Ok(TaskInspector::new(
        Arc::new(JsonlTaskStore::for_project(&root)),
        Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions"))),
    ))
}

pub fn list(ctx: &Context) -> Result<(), ForgeError> {
    let tasks = inspector(ctx)?.list()?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&tasks)
                .map_err(|error| ForgeError::task(format!("serializing tasks: {error}")))?
        );
    } else if tasks.is_empty() {
        println!("no tasks yet");
    } else {
        for task in tasks {
            let node = task.current_node.as_deref().unwrap_or("-");
            println!("{} {} {}", task.task_id, task.state.as_str(), node);
        }
    }
    Ok(())
}

pub fn show(ctx: &Context, task_id: &str) -> Result<(), ForgeError> {
    let task = inspector(ctx)?.show(task_id)?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&task)
                .map_err(|error| ForgeError::task(format!("serializing task: {error}")))?
        );
    } else {
        print_human(&task);
    }
    Ok(())
}

fn print_human(task: &TaskView) {
    println!("task: {}", task.task_id);
    println!("state: {}", task.state.as_str());
    println!("request: {}", task.request);
    println!("node: {}", task.current_node.as_deref().unwrap_or("-"));
    if let Some(route) = &task.route {
        println!(
            "route: {} -> {} ({:.0}% confidence{})",
            route.router,
            route.model,
            route.confidence * 100.0,
            if route.fallback_used {
                ", fallback"
            } else {
                ""
            }
        );
        if !route.reason.is_empty() {
            println!("route reason: {}", route.reason);
        }
    } else {
        println!("route: -");
    }
    let cost = task
        .spend
        .cost_usd
        .map(|value| format!("${value:.6}"))
        .unwrap_or_else(|| "unknown".to_string());
    println!("spend: {} tokens, {cost}", task.spend.total_tokens);
    if let Some(reason) = &task.parked_reason {
        println!("parked: {reason:?}");
    }
    if task.changed_files.is_empty() {
        println!("changed files: none recorded");
    } else {
        println!("changed files:");
        for path in &task.changed_files {
            println!("  {path}");
        }
    }
    if task.checks.is_empty() {
        println!("checks: none recorded");
    } else {
        println!("checks:");
        for check in &task.checks {
            println!("  {}: {:?}", check.name, check.status);
        }
    }
    if let Some(result) = &task.terminal_result {
        println!("result: {result}");
    }
}
