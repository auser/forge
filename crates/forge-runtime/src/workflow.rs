use std::collections::{BTreeMap, BTreeSet};

use forge_core::{EventKind, ForgeError, ToolCall};
use forge_task::{
    CapabilityNeed, InterruptionReason, JsonlTaskStore, TaskNode, TaskNodeKind, TaskPlan,
    TaskState, TransitionRequest, Verification, VerificationStatus, new_task_id,
};
use serde::Serialize;

use crate::{AgentService, RunOptions, RunOutcome};

const REVIEW_LIMIT: usize = 16 * 1024;
const DIFF_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct DevelopmentWorkflowOutcome {
    pub task_id: String,
    pub plan: String,
    pub changed_paths: Vec<String>,
    pub checks: Vec<Verification>,
    pub review: String,
    pub diff: String,
    #[serde(flatten)]
    pub run: RunOutcome,
}

pub async fn run_development_workflow(
    service: &AgentService,
    task_store: &JsonlTaskStore,
    request: &str,
    mut options: RunOptions,
) -> Result<DevelopmentWorkflowOutcome, ForgeError> {
    let task_id = new_task_id();
    let plan = workflow_plan(&task_id, request)?;
    let human_plan = render_plan(&plan);
    task_store.create(plan)?;
    task_store.transition(
        &task_id,
        TransitionRequest::new("inspect", TaskState::Running),
    )?;
    options.development_workflow = true;

    let run = match service.run_with_options(request, options).await {
        Ok(run) => run,
        Err(error) => {
            record_failure(task_store, &task_id, &error)?;
            return Err(error);
        }
    };
    let evidence = WorkflowEvidence::from_run(&run);
    complete_plan(task_store, &task_id, &evidence)?;

    Ok(DevelopmentWorkflowOutcome {
        task_id,
        plan: human_plan,
        changed_paths: evidence.changed_paths.into_iter().collect(),
        checks: evidence.checks,
        review: bounded(&run.text, REVIEW_LIMIT),
        diff: bounded(&evidence.diff.unwrap_or_default(), DIFF_LIMIT),
        run,
    })
}

fn workflow_plan(task_id: &str, request: &str) -> Result<TaskPlan, ForgeError> {
    TaskPlan::builder(request)
        .with_id(task_id)
        .add_node(
            TaskNode::new(
                "inspect",
                "Inspect relevant project context",
                TaskNodeKind::Inspect,
            )
            .requires(CapabilityNeed::Tools),
        )
        .add_node(
            TaskNode::new("edit", "Apply the requested changes", TaskNodeKind::Edit)
                .depends_on("inspect")
                .requires(CapabilityNeed::Tools),
        )
        .add_node(
            TaskNode::new(
                "check",
                "Run focused repository checks",
                TaskNodeKind::Check,
            )
            .depends_on("edit")
            .requires(CapabilityNeed::Tools),
        )
        .add_node(
            TaskNode::new(
                "review",
                "Review the diff and check results",
                TaskNodeKind::Review,
            )
            .depends_on("check")
            .requires(CapabilityNeed::Tools),
        )
        .build()
}

fn render_plan(plan: &TaskPlan) -> String {
    plan.nodes
        .iter()
        .enumerate()
        .map(|(index, node)| format!("{}. {}", index + 1, node.title))
        .collect::<Vec<_>>()
        .join("\n")
}

fn complete_plan(
    store: &JsonlTaskStore,
    task_id: &str,
    evidence: &WorkflowEvidence,
) -> Result<(), ForgeError> {
    store.transition(
        task_id,
        TransitionRequest::new("inspect", TaskState::Succeeded),
    )?;
    store.transition(task_id, TransitionRequest::new("edit", TaskState::Running))?;
    store.transition(
        task_id,
        TransitionRequest::new("edit", TaskState::Succeeded).with_verification(
            Verification::new(
                "working tree changes",
                if evidence.changed_paths.is_empty() {
                    VerificationStatus::Skipped
                } else {
                    VerificationStatus::Passed
                },
            )
            .with_detail(format!("{} changed path(s)", evidence.changed_paths.len())),
        ),
    )?;
    store.transition(task_id, TransitionRequest::new("check", TaskState::Running))?;
    let mut check = TransitionRequest::new("check", TaskState::Succeeded);
    if evidence.checks.is_empty() {
        check = check.with_verification(
            Verification::new("repository focused checks", VerificationStatus::Skipped)
                .with_detail("provider completed without invoking run_command"),
        );
    } else {
        for verification in &evidence.checks {
            check = check.with_verification(verification.clone());
        }
    }
    store.transition(task_id, check)?;
    store.transition(
        task_id,
        TransitionRequest::new("review", TaskState::Running),
    )?;
    store.transition(
        task_id,
        TransitionRequest::new("review", TaskState::Succeeded).with_verification(
            Verification::new(
                "working tree diff",
                if evidence.diff.is_some() {
                    VerificationStatus::Passed
                } else {
                    VerificationStatus::Skipped
                },
            ),
        ),
    )?;
    Ok(())
}

fn record_failure(
    store: &JsonlTaskStore,
    task_id: &str,
    error: &ForgeError,
) -> Result<(), ForgeError> {
    let reason = match error {
        ForgeError::ProviderRateLimited {
            retry_after_seconds,
            ..
        } => Some(InterruptionReason::ProviderRateLimited {
            retry_after_seconds: *retry_after_seconds,
        }),
        ForgeError::ProviderFailure { kind, .. } => {
            Some(InterruptionReason::ProviderUnavailable { failure: *kind })
        }
        ForgeError::ApprovalRequired { .. } => Some(InterruptionReason::ApprovalRequired),
        ForgeError::Cancelled(_) => Some(InterruptionReason::Cancelled),
        _ => None,
    };
    if let Some(reason) = reason {
        store.transition(
            task_id,
            TransitionRequest::new("inspect", TaskState::Interrupted).interrupted_by(reason),
        )?;
    } else {
        store.transition(
            task_id,
            TransitionRequest::new("inspect", TaskState::Failed),
        )?;
    }
    Ok(())
}

struct WorkflowEvidence {
    changed_paths: BTreeSet<String>,
    checks: Vec<Verification>,
    diff: Option<String>,
}

impl WorkflowEvidence {
    fn from_run(run: &RunOutcome) -> Self {
        let mut calls: BTreeMap<String, ToolCall> = BTreeMap::new();
        let mut changed_paths = BTreeSet::new();
        let mut checks = Vec::new();
        let mut diff = None;
        for event in &run.events {
            match &event.kind {
                EventKind::AssistantMessage { tool_calls, .. } => {
                    for call in tool_calls {
                        calls.insert(call.id.clone(), call.clone());
                    }
                }
                EventKind::FileChanged { path } => {
                    changed_paths.insert(path.to_string_lossy().replace('\\', "/"));
                }
                EventKind::ToolResult {
                    call_id,
                    tool,
                    output,
                    is_error,
                } if tool == "run_command" => {
                    let Some(call) = calls.get(call_id) else {
                        continue;
                    };
                    let command = call.arguments["command"].as_str().unwrap_or("command");
                    let args = call.arguments["args"]
                        .as_array()
                        .map(|args| {
                            args.iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let is_diff = command == "git" && args.first() == Some(&"diff");
                    if is_diff && !*is_error {
                        diff = Some(command_stdout(output));
                    } else if !is_diff {
                        checks.push(
                            Verification::new(
                                format!("{command} {}", args.join(" ")).trim().to_string(),
                                if *is_error {
                                    VerificationStatus::Failed
                                } else {
                                    VerificationStatus::Passed
                                },
                            )
                            .with_detail(bounded(output, 1_024)),
                        );
                    }
                }
                _ => {}
            }
        }
        Self {
            changed_paths,
            checks,
            diff,
        }
    }
}

fn command_stdout(output: &str) -> String {
    output
        .split_once("stdout:\n")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("\nstderr:\n").map(|(stdout, _)| stdout))
        .unwrap_or(output)
        .to_string()
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated by Forge]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_bounded_to_the_four_initial_node_kinds() {
        let plan = workflow_plan("task", "fix it").unwrap();
        assert_eq!(
            plan.nodes.iter().map(|node| node.kind).collect::<Vec<_>>(),
            vec![
                TaskNodeKind::Inspect,
                TaskNodeKind::Edit,
                TaskNodeKind::Check,
                TaskNodeKind::Review,
            ]
        );
        assert!(render_plan(&plan).len() < 512);
    }

    #[test]
    fn bounded_text_keeps_utf8_valid() {
        assert_eq!(bounded("ééé", 3), "é\n[truncated by Forge]");
    }
}
