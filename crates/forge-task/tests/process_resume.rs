use std::process::Command;

use forge_task::{
    EffectClass, EffectRequest, EffectState, InterruptionReason, JsonlTaskStore, TaskNode,
    TaskNodeKind, TaskPlan, TaskState, TransitionRequest, WorkspaceCheckpoint,
};

const CHILD_ENV: &str = "FORGE_TASK_PROCESS_BOUNDARY_CHILD";
const ROOT_ENV: &str = "FORGE_TASK_PROCESS_BOUNDARY_ROOT";
const STEP_ENV: &str = "FORGE_TASK_PROCESS_BOUNDARY_STEP";

fn crash_at(root: &std::path::Path, step: &str) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "task_boundary_child", "--nocapture"])
        .env(CHILD_ENV, "1")
        .env(ROOT_ENV, root)
        .env(STEP_ENV, step)
        .status()
        .unwrap();
    assert!(!status.success(), "child must terminate abruptly at {step}");
}

#[test]
fn killed_process_resumes_at_every_state_boundary_without_repeating_success() {
    let tmp = tempfile::tempdir().unwrap();
    for (step, state, attempts) in [
        ("create", TaskState::Ready, 0),
        ("running", TaskState::Running, 1),
        ("interrupted", TaskState::Interrupted, 1),
        ("resumed", TaskState::Running, 2),
        ("succeeded", TaskState::Succeeded, 2),
    ] {
        crash_at(tmp.path(), step);

        let loaded = JsonlTaskStore::new(tmp.path()).load("kill-task").unwrap();
        assert_eq!(loaded.checkpoint.state("side-effect"), Some(state));
        assert_eq!(loaded.checkpoint.nodes["side-effect"].attempts, attempts);
        assert_eq!(
            loaded.checkpoint.state("review"),
            Some(if state == TaskState::Succeeded {
                TaskState::Ready
            } else {
                TaskState::Pending
            })
        );
    }

    let loaded = JsonlTaskStore::new(tmp.path()).load("kill-task").unwrap();
    assert_eq!(loaded.checkpoint.ready_nodes(), vec!["review"]);
    assert!(!loaded.checkpoint.ready_nodes().contains(&"side-effect"));
}

#[test]
fn killed_process_recovers_approval_and_failure_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    for (step, state, attempts) in [
        ("approval-create", TaskState::Ready, 0),
        ("approval-running", TaskState::Running, 1),
        ("approval-waiting", TaskState::WaitingForApproval, 1),
        ("approval-resumed", TaskState::Running, 1),
        ("approval-succeeded", TaskState::Succeeded, 1),
    ] {
        crash_at(tmp.path(), step);
        let loaded = JsonlTaskStore::new(tmp.path())
            .load("approval-task")
            .unwrap();
        assert_eq!(loaded.checkpoint.state("approval"), Some(state));
        assert_eq!(loaded.checkpoint.nodes["approval"].attempts, attempts);
    }

    for (step, state) in [
        ("failure-create", TaskState::Ready),
        ("failure-running", TaskState::Running),
        ("failure-failed", TaskState::Failed),
    ] {
        crash_at(tmp.path(), step);
        let loaded = JsonlTaskStore::new(tmp.path())
            .load("failure-task")
            .unwrap();
        assert_eq!(loaded.checkpoint.state("failure"), Some(state));
    }
}

#[test]
fn killed_process_preserves_every_workflow_node_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let nodes = ["inspect", "edit", "check", "review"];
    for (index, node) in nodes.iter().enumerate() {
        crash_at(tmp.path(), &format!("workflow-{node}"));
        let task_id = format!("workflow-{node}");
        let store = JsonlTaskStore::new(tmp.path());
        let loaded = store.load(&task_id).unwrap();
        for completed in &nodes[..index] {
            assert_eq!(
                loaded.checkpoint.state(completed),
                Some(TaskState::Succeeded)
            );
        }
        assert_eq!(loaded.checkpoint.state(node), Some(TaskState::Running));
        for pending in &nodes[index + 1..] {
            assert_eq!(loaded.checkpoint.state(pending), Some(TaskState::Pending));
        }
        store
            .transition(
                &task_id,
                TransitionRequest::new(*node, TaskState::Interrupted)
                    .interrupted_by(InterruptionReason::ProcessInterrupted),
            )
            .unwrap();
        store
            .transition(&task_id, TransitionRequest::new(*node, TaskState::Running))
            .unwrap();
        assert_eq!(
            store.load(&task_id).unwrap().checkpoint.nodes[*node].attempts,
            2
        );
    }
}

#[test]
fn killed_process_preserves_each_effect_commit_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    for (step, expected) in [
        ("effect-registered", EffectState::NotStarted),
        ("effect-started", EffectState::PossiblyExecuted),
        ("effect-completed", EffectState::Completed),
    ] {
        crash_at(tmp.path(), step);
        let loaded = JsonlTaskStore::new(tmp.path()).load(step).unwrap();
        assert_eq!(loaded.checkpoint.effects["effect"].state, expected);
    }
}

#[test]
fn task_boundary_child() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
        return;
    }
    let store = JsonlTaskStore::new(std::env::var(ROOT_ENV).unwrap());
    let step = std::env::var(STEP_ENV).unwrap();
    if let Some(node) = step.strip_prefix("workflow-") {
        let nodes = ["inspect", "edit", "check", "review"];
        let mut builder = TaskPlan::builder("four node workflow").with_id(&step);
        for (index, id) in nodes.iter().enumerate() {
            let kind = match *id {
                "inspect" => TaskNodeKind::Inspect,
                "edit" => TaskNodeKind::Edit,
                "check" => TaskNodeKind::Check,
                "review" => TaskNodeKind::Review,
                _ => unreachable!(),
            };
            let mut task_node = TaskNode::new(*id, *id, kind);
            if index > 0 {
                task_node = task_node.depends_on(nodes[index - 1]);
            }
            builder = builder.add_node(task_node);
        }
        store.create(builder.build().unwrap()).unwrap();
        for id in nodes {
            store
                .transition(&step, TransitionRequest::new(id, TaskState::Running))
                .unwrap();
            if id == node {
                std::process::abort();
            }
            store
                .transition(&step, TransitionRequest::new(id, TaskState::Succeeded))
                .unwrap();
        }
        unreachable!();
    }
    if step.starts_with("effect-") {
        let plan = TaskPlan::builder("effect boundary")
            .with_id(&step)
            .add_node(TaskNode::new("edit", "Edit", TaskNodeKind::Edit))
            .build()
            .unwrap();
        store.create(plan).unwrap();
        store
            .bind_run(
                &step,
                "run",
                "session",
                WorkspaceCheckpoint {
                    head: "head".into(),
                    fingerprint: "before".into(),
                    changed_paths: Vec::new(),
                },
            )
            .unwrap();
        store
            .register_effect(
                &step,
                EffectRequest::new(
                    "effect",
                    "edit",
                    "run_command",
                    "hash",
                    EffectClass::NonIdempotent,
                ),
            )
            .unwrap();
        if step == "effect-registered" {
            std::process::abort();
        }
        store.begin_effect(&step, "effect").unwrap();
        if step == "effect-started" {
            std::process::abort();
        }
        store
            .complete_effect(
                &step,
                "effect",
                true,
                None,
                WorkspaceCheckpoint {
                    head: "head".into(),
                    fingerprint: "after".into(),
                    changed_paths: Vec::new(),
                },
            )
            .unwrap();
        std::process::abort();
    }
    match step.as_str() {
        "create" => {
            let plan = TaskPlan::builder("one side effect")
                .with_id("kill-task")
                .add_node(TaskNode::new(
                    "side-effect",
                    "Perform side effect",
                    TaskNodeKind::Edit,
                ))
                .add_node(
                    TaskNode::new("review", "Review result", TaskNodeKind::Review)
                        .depends_on("side-effect"),
                )
                .build()
                .unwrap();
            store.create(plan).unwrap();
        }
        "running" => {
            store
                .transition(
                    "kill-task",
                    TransitionRequest::new("side-effect", TaskState::Running),
                )
                .unwrap();
        }
        "interrupted" => {
            store
                .transition(
                    "kill-task",
                    TransitionRequest::new("side-effect", TaskState::Interrupted)
                        .interrupted_by(InterruptionReason::ProcessInterrupted),
                )
                .unwrap();
        }
        "resumed" => {
            store
                .transition(
                    "kill-task",
                    TransitionRequest::new("side-effect", TaskState::Running),
                )
                .unwrap();
        }
        "succeeded" => {
            store
                .transition(
                    "kill-task",
                    TransitionRequest::new("side-effect", TaskState::Succeeded),
                )
                .unwrap();
        }
        "approval-create" => {
            let plan = TaskPlan::builder("approval flow")
                .with_id("approval-task")
                .add_node(TaskNode::new(
                    "approval",
                    "Approval boundary",
                    TaskNodeKind::Edit,
                ))
                .build()
                .unwrap();
            store.create(plan).unwrap();
        }
        "approval-running" => {
            store
                .transition(
                    "approval-task",
                    TransitionRequest::new("approval", TaskState::Running),
                )
                .unwrap();
        }
        "approval-waiting" => {
            store
                .transition(
                    "approval-task",
                    TransitionRequest::new("approval", TaskState::WaitingForApproval),
                )
                .unwrap();
        }
        "approval-resumed" => {
            store
                .transition(
                    "approval-task",
                    TransitionRequest::new("approval", TaskState::Running),
                )
                .unwrap();
        }
        "approval-succeeded" => {
            store
                .transition(
                    "approval-task",
                    TransitionRequest::new("approval", TaskState::Succeeded),
                )
                .unwrap();
        }
        "failure-create" => {
            let plan = TaskPlan::builder("failure flow")
                .with_id("failure-task")
                .add_node(TaskNode::new(
                    "failure",
                    "Failure boundary",
                    TaskNodeKind::Check,
                ))
                .build()
                .unwrap();
            store.create(plan).unwrap();
        }
        "failure-running" => {
            store
                .transition(
                    "failure-task",
                    TransitionRequest::new("failure", TaskState::Running),
                )
                .unwrap();
        }
        "failure-failed" => {
            store
                .transition(
                    "failure-task",
                    TransitionRequest::new("failure", TaskState::Failed),
                )
                .unwrap();
        }
        step => panic!("unknown step {step}"),
    }
    std::process::abort();
}
