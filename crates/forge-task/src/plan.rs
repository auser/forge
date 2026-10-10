use std::collections::{BTreeMap, BTreeSet};

use forge_core::{ForgeError, ProviderFailureKind};
use serde::{Deserialize, Serialize};

pub const TASK_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityNeed {
    Streaming,
    Tools,
    StructuredOutput,
    Vision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Ready,
    Running,
    WaitingForApproval,
    Interrupted,
    Succeeded,
    Failed,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InterruptionReason {
    ProcessInterrupted,
    ProviderRateLimited {
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_after_seconds: Option<u64>,
    },
    ProviderUnavailable {
        failure: ProviderFailureKind,
    },
    ApprovalRequired,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub name: String,
    pub status: VerificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Verification {
    pub fn new(name: impl Into<String>, status: VerificationStatus) -> Self {
        Self {
            name: name.into(),
            status,
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capability_needs: Vec<CapabilityNeed>,
}

impl TaskNode {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            dependencies: Vec::new(),
            capability_needs: Vec::new(),
        }
    }

    pub fn depends_on(mut self, node_id: impl Into<String>) -> Self {
        self.dependencies.push(node_id.into());
        self
    }

    pub fn requires(mut self, capability: CapabilityNeed) -> Self {
        self.capability_needs.push(capability);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlan {
    pub version: u32,
    pub task_id: String,
    pub request: String,
    pub nodes: Vec<TaskNode>,
}

impl TaskPlan {
    pub fn builder(request: impl Into<String>) -> TaskPlanBuilder {
        TaskPlanBuilder {
            task_id: ulid::Ulid::new().to_string(),
            request: request.into(),
            nodes: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ForgeError> {
        if self.version != TASK_PLAN_SCHEMA_VERSION {
            return Err(ForgeError::task(format!(
                "unsupported task plan version {}",
                self.version
            )));
        }
        validate_id("task", &self.task_id)?;
        if self.request.trim().is_empty() {
            return Err(ForgeError::task("task request cannot be empty"));
        }
        if self.nodes.is_empty() {
            return Err(ForgeError::task("task plan must contain at least one node"));
        }

        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            validate_id("node", &node.id)?;
            if node.title.trim().is_empty() {
                return Err(ForgeError::task(format!(
                    "task node {} has an empty title",
                    node.id
                )));
            }
            if !ids.insert(node.id.as_str()) {
                return Err(ForgeError::task(format!(
                    "duplicate task node id {}",
                    node.id
                )));
            }
        }
        for node in &self.nodes {
            let mut dependencies = BTreeSet::new();
            for dependency in &node.dependencies {
                if !ids.contains(dependency.as_str()) {
                    return Err(ForgeError::task(format!(
                        "task node {} depends on missing node {dependency}",
                        node.id
                    )));
                }
                if dependency == &node.id || !dependencies.insert(dependency) {
                    return Err(ForgeError::task(format!(
                        "task node {} has an invalid dependency {dependency}",
                        node.id
                    )));
                }
            }
        }

        let dependencies = self
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node.dependencies.as_slice()))
            .collect::<BTreeMap<_, _>>();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for id in dependencies.keys() {
            visit(id, &dependencies, &mut visiting, &mut visited)?;
        }
        Ok(())
    }

    pub(crate) fn initial_checkpoint(&self) -> Checkpoint {
        let nodes = self
            .nodes
            .iter()
            .map(|node| {
                let state = if node.dependencies.is_empty() {
                    TaskState::Ready
                } else {
                    TaskState::Pending
                };
                (node.id.clone(), NodeCheckpoint::new(state))
            })
            .collect();
        Checkpoint {
            task_id: self.task_id.clone(),
            sequence: 1,
            nodes,
        }
    }
}

pub struct TaskPlanBuilder {
    task_id: String,
    request: String,
    nodes: Vec<TaskNode>,
}

impl TaskPlanBuilder {
    pub fn with_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = task_id.into();
        self
    }

    pub fn add_node(mut self, node: TaskNode) -> Self {
        self.nodes.push(node);
        self
    }

    pub fn build(self) -> Result<TaskPlan, ForgeError> {
        let plan = TaskPlan {
            version: TASK_PLAN_SCHEMA_VERSION,
            task_id: self.task_id,
            request: self.request,
            nodes: self.nodes,
        };
        plan.validate()?;
        Ok(plan)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCheckpoint {
    pub state: TaskState,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<InterruptionReason>,
    #[serde(default)]
    pub verifications: Vec<Verification>,
}

impl NodeCheckpoint {
    fn new(state: TaskState) -> Self {
        Self {
            state,
            attempts: 0,
            interruption: None,
            verifications: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub task_id: String,
    pub sequence: u64,
    pub nodes: BTreeMap<String, NodeCheckpoint>,
}

impl Checkpoint {
    pub fn ready_nodes(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.state == TaskState::Ready)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    pub fn state(&self, node_id: &str) -> Option<TaskState> {
        self.nodes.get(node_id).map(|node| node.state)
    }

    pub(crate) fn apply(
        &self,
        plan: &TaskPlan,
        request: &TransitionRequest,
    ) -> Result<Self, ForgeError> {
        let current = self
            .nodes
            .get(&request.node_id)
            .ok_or_else(|| ForgeError::task(format!("unknown task node {}", request.node_id)))?;
        validate_transition(current.state, request.to)?;
        if request.to == TaskState::Interrupted && request.interruption.is_none() {
            return Err(ForgeError::task(
                "an interrupted node requires an interruption reason",
            ));
        }
        if request.to != TaskState::Interrupted && request.interruption.is_some() {
            return Err(ForgeError::task(
                "an interruption reason is only valid for interrupted state",
            ));
        }

        let mut next = self.clone();
        next.sequence = next.sequence.saturating_add(1);
        let node = next.nodes.get_mut(&request.node_id).expect("checked above");
        if request.to == TaskState::Running
            && matches!(current.state, TaskState::Ready | TaskState::Interrupted)
        {
            node.attempts = node.attempts.saturating_add(1);
        }
        node.state = request.to;
        node.interruption = request.interruption.clone();
        node.verifications.extend(request.verifications.clone());

        if request.to == TaskState::Succeeded {
            for candidate in &plan.nodes {
                let candidate_state = next.nodes[&candidate.id].state;
                if candidate_state == TaskState::Pending
                    && candidate
                        .dependencies
                        .iter()
                        .all(|dependency| next.nodes[dependency].state == TaskState::Succeeded)
                {
                    next.nodes.get_mut(&candidate.id).expect("plan node").state = TaskState::Ready;
                }
            }
        }
        Ok(next)
    }
}

pub struct TransitionRequest {
    pub(crate) node_id: String,
    pub(crate) to: TaskState,
    pub(crate) interruption: Option<InterruptionReason>,
    pub(crate) verifications: Vec<Verification>,
}

impl TransitionRequest {
    pub fn new(node_id: impl Into<String>, to: TaskState) -> Self {
        Self {
            node_id: node_id.into(),
            to,
            interruption: None,
            verifications: Vec::new(),
        }
    }

    pub fn interrupted_by(mut self, reason: InterruptionReason) -> Self {
        self.interruption = Some(reason);
        self
    }

    pub fn with_verification(mut self, verification: Verification) -> Self {
        self.verifications.push(verification);
        self
    }
}

fn validate_id(kind: &str, id: &str) -> Result<(), ForgeError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ForgeError::task(format!(
            "{kind} id must contain only ASCII letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

fn visit<'a>(
    id: &'a str,
    dependencies: &BTreeMap<&'a str, &'a [String]>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<(), ForgeError> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(ForgeError::task(format!(
            "task dependency cycle includes node {id}"
        )));
    }
    for dependency in dependencies[id] {
        visit(dependency, dependencies, visiting, visited)?;
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

fn validate_transition(from: TaskState, to: TaskState) -> Result<(), ForgeError> {
    let allowed = matches!(
        (from, to),
        (TaskState::Ready, TaskState::Running)
            | (TaskState::Running, TaskState::WaitingForApproval)
            | (TaskState::Running, TaskState::Interrupted)
            | (TaskState::Running, TaskState::Succeeded)
            | (TaskState::Running, TaskState::Failed)
            | (TaskState::WaitingForApproval, TaskState::Running)
            | (TaskState::WaitingForApproval, TaskState::Interrupted)
            | (TaskState::WaitingForApproval, TaskState::Failed)
            | (TaskState::Interrupted, TaskState::Running)
            | (TaskState::Interrupted, TaskState::Failed)
    );
    if allowed {
        Ok(())
    } else {
        Err(ForgeError::task(format!(
            "illegal task state transition {from:?} -> {to:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_reject_missing_dependencies_and_cycles() {
        let missing = TaskPlan::builder("work")
            .with_id("task")
            .add_node(TaskNode::new("a", "A").depends_on("missing"))
            .build()
            .expect_err("missing dependency");
        assert!(missing.to_string().contains("missing node"));

        let cycle = TaskPlan::builder("work")
            .with_id("task")
            .add_node(TaskNode::new("a", "A").depends_on("b"))
            .add_node(TaskNode::new("b", "B").depends_on("a"))
            .build()
            .expect_err("cycle");
        assert!(cycle.to_string().contains("cycle"));
    }

    #[test]
    fn transitions_promote_dependencies_and_never_requeue_success() {
        let plan = TaskPlan::builder("work")
            .with_id("task")
            .add_node(TaskNode::new("inspect", "Inspect"))
            .add_node(TaskNode::new("edit", "Edit").depends_on("inspect"))
            .build()
            .unwrap();
        let initial = plan.initial_checkpoint();
        assert_eq!(initial.ready_nodes(), vec!["inspect"]);
        let running = initial
            .apply(
                &plan,
                &TransitionRequest::new("inspect", TaskState::Running),
            )
            .unwrap();
        let done = running
            .apply(
                &plan,
                &TransitionRequest::new("inspect", TaskState::Succeeded),
            )
            .unwrap();
        assert_eq!(done.ready_nodes(), vec!["edit"]);
        assert_eq!(done.nodes["inspect"].attempts, 1);
        assert!(
            done.apply(
                &plan,
                &TransitionRequest::new("inspect", TaskState::Running)
            )
            .is_err()
        );
    }
}
