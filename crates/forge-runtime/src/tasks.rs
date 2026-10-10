use std::collections::BTreeSet;
use std::sync::Arc;

use forge_core::{Event, EventKind, ForgeError};
use forge_session::JsonlSessionStore;
use forge_task::{InterruptionReason, JsonlTaskStore, LoadedTask, TaskState, Verification};
use serde::Serialize;

/// Stable, transport-neutral lifecycle shown by every Forge front end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableTaskState {
    Queued,
    Running,
    Parked,
    Failed,
    Succeeded,
}

impl DurableTaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Parked => "parked",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskRoute {
    pub router: String,
    pub model: String,
    pub confidence: f64,
    pub fallback_used: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TaskSpend {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// `None` means at least one completion was unpriced or no priced
    /// completion exists; unknown cost is never presented as free.
    pub cost_usd: Option<f64>,
}

/// One projection of the durable task journal plus its bound run events.
/// Adapters serialize or render this type instead of rebuilding task state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskView {
    pub task_id: String,
    pub request: String,
    pub state: DurableTaskState,
    pub current_node: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub route: Option<TaskRoute>,
    pub spend: TaskSpend,
    pub checks: Vec<Verification>,
    pub changed_files: Vec<String>,
    pub parked_reason: Option<InterruptionReason>,
    pub terminal_result: Option<String>,
    pub sequence: u64,
    pub recovered_tail: bool,
}

#[derive(Clone)]
pub struct TaskInspector {
    tasks: Arc<JsonlTaskStore>,
    sessions: Arc<JsonlSessionStore>,
}

impl TaskInspector {
    pub fn new(tasks: Arc<JsonlTaskStore>, sessions: Arc<JsonlSessionStore>) -> Self {
        Self { tasks, sessions }
    }

    pub fn list(&self) -> Result<Vec<TaskView>, ForgeError> {
        self.tasks
            .list()?
            .into_iter()
            .map(|task| self.view(task))
            .collect()
    }

    pub fn show(&self, task_id: &str) -> Result<TaskView, ForgeError> {
        self.view(self.tasks.load(task_id)?)
    }

    fn view(&self, task: LoadedTask) -> Result<TaskView, ForgeError> {
        let events = match &task.checkpoint.session_id {
            Some(session_id) => self
                .sessions
                .events_for(session_id)?
                .into_iter()
                .filter(|event| task.checkpoint.run_id.as_deref() == Some(event.run_id.as_str()))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        Ok(project(task, &events))
    }
}

fn project(task: LoadedTask, events: &[Event]) -> TaskView {
    let mut current_node = None;
    let mut parked_reason = None;
    let mut checks = Vec::new();
    let mut any_running = false;
    let mut any_parked = false;
    let mut any_failed = false;
    let mut all_succeeded = true;
    for node in &task.plan.nodes {
        let checkpoint = &task.checkpoint.nodes[&node.id];
        checks.extend(checkpoint.verifications.clone());
        all_succeeded &= checkpoint.state == TaskState::Succeeded;
        match checkpoint.state {
            TaskState::Running => {
                any_running = true;
                current_node.get_or_insert_with(|| node.id.clone());
            }
            TaskState::WaitingForApproval | TaskState::Interrupted => {
                any_parked = true;
                current_node.get_or_insert_with(|| node.id.clone());
                if parked_reason.is_none() {
                    parked_reason = checkpoint.interruption.clone();
                }
            }
            TaskState::Failed => {
                any_failed = true;
                current_node.get_or_insert_with(|| node.id.clone());
            }
            TaskState::Ready if current_node.is_none() => current_node = Some(node.id.clone()),
            TaskState::Pending | TaskState::Ready | TaskState::Succeeded => {}
        }
    }
    let state = if all_succeeded {
        DurableTaskState::Succeeded
    } else if any_failed {
        DurableTaskState::Failed
    } else if any_parked {
        DurableTaskState::Parked
    } else if any_running {
        DurableTaskState::Running
    } else {
        DurableTaskState::Queued
    };

    let route = events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            EventKind::RoutingDecisionMade {
                router,
                selected_model,
                confidence,
                fallback_used,
                reason,
            } => Some(TaskRoute {
                router: router.clone(),
                model: selected_model.clone(),
                confidence: *confidence,
                fallback_used: *fallback_used,
                reason: reason.clone(),
            }),
            _ => None,
        })
        .or_else(|| {
            events.iter().find_map(|event| match &event.kind {
                EventKind::RunStarted {
                    provider, model, ..
                } => Some(TaskRoute {
                    router: "direct".into(),
                    model: model.clone(),
                    confidence: 1.0,
                    fallback_used: false,
                    reason: format!("direct dispatch through {provider}"),
                }),
                _ => None,
            })
        });
    let mut spend = TaskSpend::default();
    let mut priced = 0_u64;
    let mut completions = 0_u64;
    for event in events {
        if let EventKind::UsageRecorded {
            usage, cost_usd, ..
        } = &event.kind
        {
            completions += 1;
            if let Some(usage) = usage {
                spend.input_tokens += u64::from(usage.prompt_tokens);
                spend.output_tokens += u64::from(usage.completion_tokens);
                spend.total_tokens += u64::from(usage.total_tokens);
            }
            if let Some(cost) = cost_usd {
                priced += 1;
                spend.cost_usd = Some(spend.cost_usd.unwrap_or_default() + cost);
            }
        }
    }
    if completions == 0 || priced != completions {
        spend.cost_usd = None;
    }

    let mut changed_files = task
        .checkpoint
        .effects
        .values()
        .filter_map(|effect| effect.changed_path.clone())
        .collect::<BTreeSet<_>>();
    for event in events {
        if let EventKind::FileChanged { path } = &event.kind {
            changed_files.insert(path.to_string_lossy().replace('\\', "/"));
        }
    }
    let terminal_result = events.iter().rev().find_map(|event| match &event.kind {
        EventKind::AssistantMessage { text, tool_calls } if tool_calls.is_empty() => {
            Some(text.clone())
        }
        EventKind::Completed { summary } => Some(summary.clone()),
        EventKind::Error { message } => Some(message.clone()),
        EventKind::Cancelled { reason } => Some(reason.clone()),
        _ => None,
    });

    TaskView {
        task_id: task.plan.task_id,
        request: task.plan.request,
        state,
        current_node,
        run_id: task.checkpoint.run_id,
        session_id: task.checkpoint.session_id,
        route,
        spend,
        checks,
        changed_files: changed_files.into_iter().collect(),
        parked_reason,
        terminal_result,
        sequence: task.checkpoint.sequence,
        recovered_tail: task.recovered_tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::{EventKind, SessionStore, Usage};
    use forge_task::{
        TaskNode, TaskNodeKind, TaskPlan, TaskState, TransitionRequest, VerificationStatus,
        WorkspaceCheckpoint,
    };

    #[test]
    fn one_projection_carries_route_spend_checks_files_and_result() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tasks = Arc::new(JsonlTaskStore::for_project(tmp.path()));
        let sessions = Arc::new(JsonlSessionStore::new(
            tmp.path().join(".forge").join("sessions"),
        ));
        let plan = TaskPlan::builder("change the value")
            .with_id("task-view")
            .add_node(TaskNode::new("inspect", "Inspect", TaskNodeKind::Inspect))
            .build()
            .expect("plan");
        tasks.create(plan).expect("create");
        tasks
            .transition(
                "task-view",
                TransitionRequest::new("inspect", TaskState::Running),
            )
            .expect("running");
        tasks
            .bind_run(
                "task-view",
                "run-view",
                "session-view",
                WorkspaceCheckpoint {
                    fingerprint: "before".into(),
                    head: String::new(),
                    changed_paths: vec![],
                },
            )
            .expect("bind");
        tasks
            .transition(
                "task-view",
                TransitionRequest::new("inspect", TaskState::Succeeded).with_verification(
                    Verification::new("cargo check", VerificationStatus::Passed),
                ),
            )
            .expect("complete");
        for kind in [
            EventKind::RunStarted {
                provider: "provider".into(),
                model: "model-a".into(),
                prompt: "change the value".into(),
            },
            EventKind::RoutingDecisionMade {
                router: "jev".into(),
                selected_model: "model-a".into(),
                confidence: 0.9,
                fallback_used: false,
                reason: "capability fit".into(),
            },
            EventKind::UsageRecorded {
                model: "model-a".into(),
                usage: Some(Usage {
                    prompt_tokens: 100,
                    completion_tokens: 20,
                    total_tokens: 120,
                }),
                cost_usd: Some(0.002),
            },
            EventKind::FileChanged {
                path: "src/lib.rs".into(),
            },
            EventKind::AssistantMessage {
                text: "done".into(),
                tool_calls: vec![],
            },
            EventKind::Completed {
                summary: "done".into(),
            },
        ] {
            sessions
                .append(Event::new("run-view", "session-view", kind))
                .expect("event");
        }

        let view = TaskInspector::new(tasks, sessions)
            .show("task-view")
            .expect("view");
        assert_eq!(view.state, DurableTaskState::Succeeded);
        assert_eq!(
            view.route.as_ref().map(|route| route.router.as_str()),
            Some("jev")
        );
        assert_eq!(view.spend.total_tokens, 120);
        assert_eq!(view.spend.cost_usd, Some(0.002));
        assert_eq!(view.changed_files, ["src/lib.rs"]);
        assert_eq!(view.checks[0].status, VerificationStatus::Passed);
        assert_eq!(view.terminal_result.as_deref(), Some("done"));
    }
}
