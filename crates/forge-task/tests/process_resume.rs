use std::process::Command;

use forge_task::{
    InterruptionReason, JsonlTaskStore, TaskNode, TaskNodeKind, TaskPlan, TaskState,
    TransitionRequest,
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
fn task_boundary_child() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
        return;
    }
    let store = JsonlTaskStore::new(std::env::var(ROOT_ENV).unwrap());
    match std::env::var(STEP_ENV).unwrap().as_str() {
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
