use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use forge_core::ForgeError;
use serde::{Deserialize, Serialize};

use crate::{Checkpoint, InterruptionReason, TaskPlan, TaskState, TransitionRequest, Verification};

pub const TASK_EVENT_SCHEMA_VERSION: u32 = 1;

pub fn new_task_id() -> String {
    ulid::Ulid::new().to_string()
}

pub fn new_node_id() -> String {
    ulid::Ulid::new().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TaskEvent {
    v: u32,
    task_id: String,
    sequence: u64,
    ts: DateTime<Utc>,
    #[serde(flatten)]
    kind: TaskEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
enum TaskEventKind {
    PlanCreated {
        plan: TaskPlan,
        checkpoint: Checkpoint,
    },
    NodeTransitioned {
        node_id: String,
        from: TaskState,
        to: TaskState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interruption: Option<InterruptionReason>,
        #[serde(default)]
        verifications: Vec<Verification>,
        checkpoint: Checkpoint,
    },
}

impl TaskEvent {
    fn checkpoint(&self) -> &Checkpoint {
        match &self.kind {
            TaskEventKind::PlanCreated { checkpoint, .. }
            | TaskEventKind::NodeTransitioned { checkpoint, .. } => checkpoint,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedTask {
    pub plan: TaskPlan,
    pub checkpoint: Checkpoint,
    /// True when an incomplete or invalid final record was ignored. Valid
    /// records before that tail remain authoritative.
    pub recovered_tail: bool,
}

/// Append-only task journal. Call [`JsonlTaskStore::for_project`] for the
/// conventional `<project>/.forge/tasks` location.
pub struct JsonlTaskStore {
    root: PathBuf,
    mutation: Mutex<()>,
}

impl JsonlTaskStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            mutation: Mutex::new(()),
        }
    }

    pub fn for_project(project_root: impl AsRef<Path>) -> Self {
        Self::new(project_root.as_ref().join(".forge").join("tasks"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(&self, plan: TaskPlan) -> Result<LoadedTask, ForgeError> {
        plan.validate()?;
        let _guard = self
            .mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::fs::create_dir_all(&self.root).map_err(ForgeError::Io)?;
        let path = self.file_for(&plan.task_id)?;
        let checkpoint = plan.initial_checkpoint();
        let event = TaskEvent {
            v: TASK_EVENT_SCHEMA_VERSION,
            task_id: plan.task_id.clone(),
            sequence: 1,
            ts: Utc::now(),
            kind: TaskEventKind::PlanCreated {
                plan: plan.clone(),
                checkpoint: checkpoint.clone(),
            },
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    ForgeError::task(format!("task {} already exists", plan.task_id))
                } else {
                    ForgeError::Io(error)
                }
            })?;
        write_event(&mut file, &event)?;
        Ok(LoadedTask {
            plan,
            checkpoint,
            recovered_tail: false,
        })
    }

    pub fn load(&self, task_id: &str) -> Result<LoadedTask, ForgeError> {
        let path = self.file_for(task_id)?;
        Ok(load_file(&path, task_id)?.loaded)
    }

    pub fn transition(
        &self,
        task_id: &str,
        request: TransitionRequest,
    ) -> Result<LoadedTask, ForgeError> {
        let _guard = self
            .mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = self.file_for(task_id)?;
        let replay = load_file(&path, task_id)?;
        let current = replay.loaded;
        let from = current
            .checkpoint
            .state(&request.node_id)
            .ok_or_else(|| ForgeError::task(format!("unknown task node {}", request.node_id)))?;
        let checkpoint = current.checkpoint.apply(&current.plan, &request)?;
        let event = TaskEvent {
            v: TASK_EVENT_SCHEMA_VERSION,
            task_id: task_id.to_string(),
            sequence: checkpoint.sequence,
            ts: Utc::now(),
            kind: TaskEventKind::NodeTransitioned {
                node_id: request.node_id.clone(),
                from,
                to: request.to,
                interruption: request.interruption.clone(),
                verifications: request.verifications.clone(),
                checkpoint: checkpoint.clone(),
            },
        };
        if current.recovered_tail {
            let file = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(ForgeError::Io)?;
            file.set_len(replay.valid_bytes).map_err(ForgeError::Io)?;
            file.sync_data().map_err(ForgeError::Io)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(ForgeError::Io)?;
        write_event(&mut file, &event)?;
        Ok(LoadedTask {
            plan: current.plan,
            checkpoint,
            recovered_tail: false,
        })
    }

    fn file_for(&self, task_id: &str) -> Result<PathBuf, ForgeError> {
        validate_task_id(task_id)?;
        Ok(self.root.join(format!("{task_id}.jsonl")))
    }
}

fn validate_task_id(task_id: &str) -> Result<(), ForgeError> {
    if task_id.is_empty()
        || !task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ForgeError::task("invalid task id"));
    }
    Ok(())
}

fn write_event(file: &mut std::fs::File, event: &TaskEvent) -> Result<(), ForgeError> {
    let line = serde_json::to_string(event)
        .map_err(|error| ForgeError::task(format!("serializing task event: {error}")))?;
    file.write_all(line.as_bytes()).map_err(ForgeError::Io)?;
    file.write_all(b"\n").map_err(ForgeError::Io)?;
    // A checkpoint is only announced after the bytes needed to resume it
    // have reached the filesystem. A killed process may lose the final
    // record, but it cannot expose a checkpoint newer than its journal.
    file.sync_data().map_err(ForgeError::Io)
}

struct Replay {
    loaded: LoadedTask,
    valid_bytes: u64,
}

fn load_file(path: &Path, task_id: &str) -> Result<Replay, ForgeError> {
    let bytes = std::fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ForgeError::task(format!("unknown task {task_id}"))
        } else {
            ForgeError::Io(error)
        }
    })?;
    let mut plan: Option<TaskPlan> = None;
    let mut checkpoint: Option<Checkpoint> = None;
    let mut expected_sequence = 1_u64;
    let mut recovered_tail = false;
    let mut offset = 0_u64;
    let mut valid_bytes = 0_u64;
    let segments = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();

    for (index, segment) in segments.iter().enumerate() {
        let line_number = index + 1;
        offset = offset.saturating_add(segment.len() as u64);
        if segment.last() != Some(&b'\n') {
            recovered_tail = true;
            break;
        }
        let line = segment.strip_suffix(b"\n").unwrap_or(segment);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            valid_bytes = offset;
            continue;
        }
        let is_tail = segments[index + 1..]
            .iter()
            .all(|rest| rest.iter().all(u8::is_ascii_whitespace));
        let event: TaskEvent = match serde_json::from_slice(line) {
            Ok(event) => event,
            Err(_) if is_tail => {
                recovered_tail = true;
                break;
            }
            Err(error) => {
                return Err(ForgeError::task(format!(
                    "corrupt task event at {}:{line_number}: {error}",
                    path.display()
                )));
            }
        };
        if event.v != TASK_EVENT_SCHEMA_VERSION {
            return Err(ForgeError::task(format!(
                "unsupported task event version {} at {}:{line_number}",
                event.v,
                path.display()
            )));
        }
        if event.task_id != task_id || event.sequence != expected_sequence {
            return Err(ForgeError::task(format!(
                "invalid task event identity or sequence at {}:{line_number}",
                path.display()
            )));
        }
        if event.checkpoint().task_id != task_id || event.checkpoint().sequence != event.sequence {
            return Err(ForgeError::task(format!(
                "invalid task checkpoint at {}:{line_number}",
                path.display()
            )));
        }
        match &event.kind {
            TaskEventKind::PlanCreated {
                plan: event_plan,
                checkpoint: event_checkpoint,
            } if event.sequence == 1 => {
                event_plan.validate()?;
                if event_plan.task_id != task_id {
                    return Err(ForgeError::task("task plan id does not match journal"));
                }
                if event_checkpoint != &event_plan.initial_checkpoint() {
                    return Err(ForgeError::task(
                        "initial task checkpoint does not match plan",
                    ));
                }
                plan = Some(event_plan.clone());
            }
            TaskEventKind::PlanCreated { .. } => {
                return Err(ForgeError::task("task plan may only be created once"));
            }
            TaskEventKind::NodeTransitioned {
                from,
                to,
                node_id,
                interruption,
                verifications,
                checkpoint: event_checkpoint,
            } => {
                let Some(previous) = &checkpoint else {
                    return Err(ForgeError::task("task journal does not start with a plan"));
                };
                if previous.state(node_id) != Some(*from) {
                    return Err(ForgeError::task(format!(
                        "task transition for {node_id} does not match prior checkpoint"
                    )));
                }
                let event_plan = plan
                    .as_ref()
                    .ok_or_else(|| ForgeError::task("task journal does not start with a plan"))?;
                let expected = previous.apply(
                    event_plan,
                    &TransitionRequest {
                        node_id: node_id.clone(),
                        to: *to,
                        interruption: interruption.clone(),
                        verifications: verifications.clone(),
                    },
                )?;
                if &expected != event_checkpoint {
                    return Err(ForgeError::task(format!(
                        "task checkpoint does not match transition at {}:{line_number}",
                        path.display()
                    )));
                }
            }
        }
        checkpoint = Some(event.checkpoint().clone());
        expected_sequence = expected_sequence.saturating_add(1);
        valid_bytes = offset;
    }

    Ok(Replay {
        loaded: LoadedTask {
            plan: plan
                .ok_or_else(|| ForgeError::task("task journal has no valid plan checkpoint"))?,
            checkpoint: checkpoint
                .ok_or_else(|| ForgeError::task("task journal has no valid checkpoint"))?,
            recovered_tail,
        },
        valid_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InterruptionReason, TaskNode, Verification, VerificationStatus};

    fn plan() -> TaskPlan {
        TaskPlan::builder("inspect, edit, and verify")
            .with_id("task-1")
            .add_node(TaskNode::new("inspect", "Inspect"))
            .add_node(TaskNode::new("edit", "Edit").depends_on("inspect"))
            .build()
            .unwrap()
    }

    #[test]
    fn append_only_journal_replays_attempts_interruptions_and_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlTaskStore::new(tmp.path());
        store.create(plan()).unwrap();
        store
            .transition(
                "task-1",
                TransitionRequest::new("inspect", TaskState::Running),
            )
            .unwrap();
        store
            .transition(
                "task-1",
                TransitionRequest::new("inspect", TaskState::Interrupted)
                    .interrupted_by(InterruptionReason::ProcessInterrupted),
            )
            .unwrap();
        store
            .transition(
                "task-1",
                TransitionRequest::new("inspect", TaskState::Running),
            )
            .unwrap();
        let loaded = store
            .transition(
                "task-1",
                TransitionRequest::new("inspect", TaskState::Succeeded)
                    .with_verification(Verification::new("cargo test", VerificationStatus::Passed)),
            )
            .unwrap();

        assert_eq!(loaded.checkpoint.nodes["inspect"].attempts, 2);
        assert_eq!(loaded.checkpoint.nodes["inspect"].verifications.len(), 1);
        assert_eq!(loaded.checkpoint.ready_nodes(), vec!["edit"]);
        let lines = std::fs::read_to_string(tmp.path().join("task-1.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, 5);
    }

    #[test]
    fn persisted_contracts_are_versioned_and_use_closed_snake_case_values() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlTaskStore::new(tmp.path());
        store.create(plan()).unwrap();
        let first = std::fs::read_to_string(tmp.path().join("task-1.jsonl")).unwrap();
        let value: serde_json::Value = serde_json::from_str(first.trim()).unwrap();
        assert_eq!(value["v"], TASK_EVENT_SCHEMA_VERSION);
        assert_eq!(
            value["plan"]["version"],
            crate::plan::TASK_PLAN_SCHEMA_VERSION
        );
        assert_eq!(value["type"], "plan_created");
        assert_eq!(value["checkpoint"]["nodes"]["inspect"]["state"], "ready");
        assert_eq!(value["checkpoint"]["nodes"]["edit"]["state"], "pending");
    }

    #[test]
    fn corrupt_or_truncated_tail_recovers_the_last_valid_checkpoint() {
        for tail in [b"{broken}\n".as_slice(), b"{\"v\":1".as_slice()] {
            let tmp = tempfile::tempdir().unwrap();
            let store = JsonlTaskStore::new(tmp.path());
            store.create(plan()).unwrap();
            store
                .transition(
                    "task-1",
                    TransitionRequest::new("inspect", TaskState::Running),
                )
                .unwrap();
            let mut file = OpenOptions::new()
                .append(true)
                .open(tmp.path().join("task-1.jsonl"))
                .unwrap();
            file.write_all(tail).unwrap();

            let recovered = store.load("task-1").unwrap();
            assert!(recovered.recovered_tail);
            assert_eq!(
                recovered.checkpoint.state("inspect"),
                Some(TaskState::Running)
            );
            assert_eq!(recovered.checkpoint.sequence, 2);
            store
                .transition(
                    "task-1",
                    TransitionRequest::new("inspect", TaskState::Interrupted)
                        .interrupted_by(InterruptionReason::ProcessInterrupted),
                )
                .unwrap();
            let resumed = store.load("task-1").unwrap();
            assert!(!resumed.recovered_tail);
            assert_eq!(resumed.checkpoint.sequence, 3);
        }
    }

    #[test]
    fn corrupt_interior_record_is_never_silently_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlTaskStore::new(tmp.path());
        store.create(plan()).unwrap();
        let path = tmp.path().join("task-1.jsonl");
        let valid = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{valid}{{broken}}\n{valid}")).unwrap();
        assert!(
            store
                .load("task-1")
                .unwrap_err()
                .to_string()
                .contains("corrupt")
        );
    }

    #[test]
    fn illegal_transition_does_not_append() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlTaskStore::new(tmp.path());
        store.create(plan()).unwrap();
        assert!(
            store
                .transition("task-1", TransitionRequest::new("edit", TaskState::Running))
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("task-1.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn replay_rejects_a_valid_json_event_that_lies_about_its_transition() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlTaskStore::new(tmp.path());
        store.create(plan()).unwrap();
        store
            .transition(
                "task-1",
                TransitionRequest::new("inspect", TaskState::Running),
            )
            .unwrap();
        let path = tmp.path().join("task-1.jsonl");
        let journal = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            journal.replace("\"from\":\"ready\"", "\"from\":\"pending\""),
        )
        .unwrap();
        assert!(
            store
                .load("task-1")
                .unwrap_err()
                .to_string()
                .contains("prior checkpoint")
        );
    }
}
