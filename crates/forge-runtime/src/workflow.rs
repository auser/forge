use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use forge_core::{EventKind, ForgeError, ToolCall};
use forge_task::{
    CapabilityNeed, EffectClass, EffectRequest, InterruptionReason, JsonlTaskStore, TaskNode,
    TaskNodeKind, TaskPlan, TaskState, TransitionRequest, Verification, VerificationStatus,
    WorkspaceCheckpoint, new_task_id,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{AgentService, EffectJournal, RunOptions, RunOutcome};

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
    task_store: Arc<JsonlTaskStore>,
    project_root: &Path,
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
    let run_id = options
        .run_id
        .take()
        .unwrap_or_else(forge_session::new_run_id);
    let session_id = options
        .session_id
        .take()
        .unwrap_or_else(forge_session::new_session_id);
    task_store.bind_run(
        &task_id,
        run_id.clone(),
        session_id.clone(),
        capture_workspace(project_root)?,
    )?;
    options.run_id = Some(run_id);
    options.session_id = Some(session_id);
    options.development_workflow = true;
    options.effect_journal = Some(Arc::new(WorkflowEffectJournal::new(
        Arc::clone(&task_store),
        task_id.clone(),
        project_root.to_path_buf(),
    )));

    let run = match service.run_with_options(request, options).await {
        Ok(run) => run,
        Err(error) => {
            record_failure(task_store.as_ref(), &task_id, &error)?;
            return Err(error);
        }
    };
    let evidence = WorkflowEvidence::from_run(&run);
    complete_plan(task_store.as_ref(), &task_id, &evidence)?;

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

pub async fn resume_development_workflow(
    service: &AgentService,
    task_store: Arc<JsonlTaskStore>,
    project_root: &Path,
    task_id: &str,
    mut options: RunOptions,
) -> Result<DevelopmentWorkflowOutcome, ForgeError> {
    let loaded = task_store.load(task_id)?;
    let prior_run = loaded
        .checkpoint
        .run_id
        .clone()
        .ok_or_else(|| ForgeError::task(format!("task {task_id} has no recorded run")))?;
    let session_id = loaded
        .checkpoint
        .session_id
        .clone()
        .ok_or_else(|| ForgeError::task(format!("task {task_id} has no recorded session")))?;
    if let Some(effect) = loaded.checkpoint.ambiguous_effects().next() {
        park_task(
            task_store.as_ref(),
            task_id,
            InterruptionReason::AmbiguousEffect {
                effect_id: effect.effect_id.clone(),
                tool: effect.tool.clone(),
            },
        )?;
        return Err(ForgeError::task(format!(
            "task {task_id} has a possibly-executed {} effect {}; inspect it before retrying",
            effect.tool, effect.effect_id
        )));
    }
    let actual = capture_workspace(project_root)?;
    if let Some(expected) = &loaded.checkpoint.workspace
        && expected.fingerprint != actual.fingerprint
    {
        park_task(
            task_store.as_ref(),
            task_id,
            InterruptionReason::TreeConflict {
                expected: expected.fingerprint.clone(),
                actual: actual.fingerprint.clone(),
            },
        )?;
        return Err(ForgeError::task(format!(
            "task {task_id} working tree changed since its checkpoint; inspect the tree before resuming"
        )));
    }
    resume_active_node(task_store.as_ref(), task_id)?;
    let run_id = options
        .run_id
        .take()
        .unwrap_or_else(forge_session::new_run_id);
    task_store.bind_run(task_id, run_id.clone(), session_id.clone(), actual)?;
    options.run_id = Some(run_id);
    options.session_id = Some(session_id);
    options.development_workflow = true;
    options.effect_journal = Some(Arc::new(WorkflowEffectJournal::new(
        Arc::clone(&task_store),
        task_id.to_string(),
        project_root.to_path_buf(),
    )));

    let run = match service.resume_interrupted(&prior_run, options).await {
        Ok(run) => run,
        Err(error) => {
            record_failure(task_store.as_ref(), task_id, &error)?;
            return Err(error);
        }
    };
    let evidence = WorkflowEvidence::from_run(&run);
    complete_plan(task_store.as_ref(), task_id, &evidence)?;
    Ok(DevelopmentWorkflowOutcome {
        task_id: task_id.to_string(),
        plan: render_plan(&loaded.plan),
        changed_paths: evidence.changed_paths.into_iter().collect(),
        checks: evidence.checks,
        review: bounded(&run.text, REVIEW_LIMIT),
        diff: bounded(&evidence.diff.unwrap_or_default(), DIFF_LIMIT),
        run,
    })
}

fn resume_active_node(store: &JsonlTaskStore, task_id: &str) -> Result<(), ForgeError> {
    let loaded = store.load(task_id)?;
    let node = loaded
        .checkpoint
        .active_node()
        .or_else(|| {
            loaded
                .plan
                .nodes
                .iter()
                .find(|node| {
                    matches!(
                        loaded.checkpoint.state(&node.id),
                        Some(TaskState::Interrupted | TaskState::Ready)
                    )
                })
                .map(|node| node.id.as_str())
        })
        .ok_or_else(|| ForgeError::task(format!("task {task_id} has no resumable node")))?;
    match loaded.checkpoint.state(node) {
        Some(TaskState::Running) => {
            store.transition(
                task_id,
                TransitionRequest::new(node, TaskState::Interrupted)
                    .interrupted_by(InterruptionReason::ProcessInterrupted),
            )?;
            store.transition(task_id, TransitionRequest::new(node, TaskState::Running))?;
        }
        Some(TaskState::WaitingForApproval) => {
            store.transition(
                task_id,
                TransitionRequest::new(node, TaskState::Interrupted)
                    .interrupted_by(InterruptionReason::ApprovalRequired),
            )?;
            store.transition(task_id, TransitionRequest::new(node, TaskState::Running))?;
        }
        Some(TaskState::Interrupted | TaskState::Ready) => {
            store.transition(task_id, TransitionRequest::new(node, TaskState::Running))?;
        }
        _ => {}
    }
    Ok(())
}

fn park_task(
    store: &JsonlTaskStore,
    task_id: &str,
    reason: InterruptionReason,
) -> Result<(), ForgeError> {
    let loaded = store.load(task_id)?;
    let node = loaded
        .checkpoint
        .active_node()
        .or_else(|| {
            loaded
                .plan
                .nodes
                .iter()
                .find(|node| loaded.checkpoint.state(&node.id) == Some(TaskState::Interrupted))
                .map(|node| node.id.as_str())
        })
        .ok_or_else(|| ForgeError::task(format!("task {task_id} has no active node")))?;
    if matches!(
        loaded.checkpoint.state(node),
        Some(TaskState::Interrupted | TaskState::WaitingForApproval)
    ) {
        store.transition(task_id, TransitionRequest::new(node, TaskState::Running))?;
    }
    store.transition(
        task_id,
        TransitionRequest::new(node, TaskState::Interrupted).interrupted_by(reason),
    )?;
    Ok(())
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

#[derive(Debug)]
struct WorkflowEffectJournal {
    store: Arc<JsonlTaskStore>,
    task_id: String,
    project_root: PathBuf,
    active_effects: Mutex<HashMap<String, String>>,
}

impl WorkflowEffectJournal {
    fn new(store: Arc<JsonlTaskStore>, task_id: String, project_root: PathBuf) -> Self {
        Self {
            store,
            task_id,
            project_root,
            active_effects: Mutex::new(HashMap::new()),
        }
    }

    fn node_for(call: &ToolCall) -> &'static str {
        match call.name.as_str() {
            "write_file" | "edit_file" | "delete_file" => "edit",
            "run_command"
                if call.arguments["command"].as_str() == Some("git")
                    && call.arguments["args"]
                        .as_array()
                        .and_then(|args| args.first())
                        .and_then(serde_json::Value::as_str)
                        == Some("diff") =>
            {
                "review"
            }
            "run_command" => "check",
            _ => "inspect",
        }
    }

    fn class_for(call: &ToolCall) -> EffectClass {
        match call.name.as_str() {
            "run_command" => EffectClass::NonIdempotent,
            "write_file" | "edit_file" | "delete_file" => EffectClass::Idempotent,
            _ => EffectClass::ReadOnly,
        }
    }

    fn effect_id(&self, call: &ToolCall) -> String {
        format!("{}-{}", call.id, forge_task::new_node_id())
    }
}

impl EffectJournal for WorkflowEffectJournal {
    fn before_effect(&self, call: &ToolCall) -> Result<(), ForgeError> {
        let node = Self::node_for(call);
        advance_to_node(self.store.as_ref(), &self.task_id, node)?;
        let effect_id = self.effect_id(call);
        let arguments_hash = hex_digest(&serde_json::to_vec(&call.arguments).map_err(|error| {
            ForgeError::task(format!(
                "serializing tool arguments for checkpoint: {error}"
            ))
        })?);
        self.store.register_effect(
            &self.task_id,
            EffectRequest::new(
                &effect_id,
                node,
                &call.name,
                arguments_hash,
                Self::class_for(call),
            ),
        )?;
        self.store.begin_effect(&self.task_id, &effect_id)?;
        self.active_effects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(call.id.clone(), effect_id);
        Ok(())
    }

    fn approval_required(&self, call: &ToolCall) -> Result<(), ForgeError> {
        let node = Self::node_for(call);
        advance_to_node(self.store.as_ref(), &self.task_id, node)?;
        let loaded = self.store.load(&self.task_id)?;
        if loaded.checkpoint.state(node) == Some(TaskState::Running) {
            self.store.transition(
                &self.task_id,
                TransitionRequest::new(node, TaskState::WaitingForApproval),
            )?;
        }
        Ok(())
    }

    fn after_effect(
        &self,
        call: &ToolCall,
        success: bool,
        changed_path: Option<&Path>,
    ) -> Result<(), ForgeError> {
        let effect_id = self
            .active_effects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&call.id)
            .ok_or_else(|| ForgeError::task(format!("missing active effect for {}", call.id)))?;
        self.store.complete_effect(
            &self.task_id,
            effect_id,
            success,
            changed_path.map(|path| path.to_string_lossy().replace('\\', "/")),
            capture_workspace(&self.project_root)?,
        )?;
        Ok(())
    }
}

fn advance_to_node(store: &JsonlTaskStore, task_id: &str, target: &str) -> Result<(), ForgeError> {
    const ORDER: [&str; 4] = ["inspect", "edit", "check", "review"];
    let target_index = ORDER
        .iter()
        .position(|node| *node == target)
        .ok_or_else(|| ForgeError::task(format!("unknown workflow node {target}")))?;
    loop {
        let loaded = store.load(task_id)?;
        if loaded.checkpoint.state(target) == Some(TaskState::Running) {
            return Ok(());
        }
        if loaded.checkpoint.state(target) == Some(TaskState::WaitingForApproval) {
            store.transition(task_id, TransitionRequest::new(target, TaskState::Running))?;
            return Ok(());
        }
        if let Some(active) = loaded.checkpoint.active_node() {
            let active_index = ORDER
                .iter()
                .position(|node| *node == active)
                .ok_or_else(|| {
                    ForgeError::task(format!("unknown active workflow node {active}"))
                })?;
            if active_index >= target_index {
                return Err(ForgeError::task(format!(
                    "workflow cannot move backward from {active} to {target}"
                )));
            }
            store.transition(task_id, completion_request(&loaded, active))?;
            continue;
        }
        match loaded.checkpoint.state(target) {
            Some(TaskState::Ready) => {
                store.transition(task_id, TransitionRequest::new(target, TaskState::Running))?;
                return Ok(());
            }
            Some(TaskState::Succeeded) => return Ok(()),
            state => {
                let next = ORDER[..=target_index]
                    .iter()
                    .find(|node| loaded.checkpoint.state(node) == Some(TaskState::Ready));
                let Some(next) = next else {
                    return Err(ForgeError::task(format!(
                        "workflow node {target} is not resumable from {state:?}"
                    )));
                };
                store.transition(task_id, TransitionRequest::new(*next, TaskState::Running))?;
            }
        }
    }
}

fn completion_request(loaded: &forge_task::LoadedTask, node: &str) -> TransitionRequest {
    let mut request = TransitionRequest::new(node, TaskState::Succeeded);
    for effect in loaded.checkpoint.effects.values().filter(|effect| {
        effect.node_id == node && effect.state == forge_task::EffectState::Completed
    }) {
        request = request.with_verification(
            Verification::new(
                effect.tool.clone(),
                if effect.success == Some(true) {
                    VerificationStatus::Passed
                } else {
                    VerificationStatus::Failed
                },
            )
            .with_detail(format!("effect {}", effect.effect_id)),
        );
    }
    request
}

fn capture_workspace(project_root: &Path) -> Result<WorkspaceCheckpoint, ForgeError> {
    let head = git_output(project_root, &["rev-parse", "HEAD"])
        .map(|output| output.trim().to_string())
        .unwrap_or_else(|| "unborn-or-not-git".to_string());
    let status = git_output_bytes(
        project_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    );
    let mut changed_paths = BTreeSet::new();
    for args in [
        &["diff", "--name-only", "-z", "HEAD", "--"][..],
        &["diff", "--cached", "--name-only", "-z", "--"][..],
        &["ls-files", "--others", "--exclude-standard", "-z"][..],
    ] {
        if let Some(output) = git_output_bytes(project_root, args) {
            for path in output
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
            {
                changed_paths.insert(String::from_utf8_lossy(path).into_owned());
            }
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(head.as_bytes());
    if let Some(status) = status {
        hasher.update(status);
        for path in &changed_paths {
            hasher.update(path.as_bytes());
            let full = project_root.join(path);
            if full.is_dir() {
                hasher.update(b"[directory]");
                continue;
            }
            match std::fs::read(full) {
                Ok(bytes) => hasher.update(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    hasher.update(b"[deleted]")
                }
                Err(error) => return Err(ForgeError::Io(error)),
            }
        }
    } else {
        hash_directory(project_root, project_root, &mut hasher, &mut changed_paths)?;
    }
    Ok(WorkspaceCheckpoint {
        head,
        fingerprint: format!("{:x}", hasher.finalize()),
        changed_paths: changed_paths.into_iter().collect(),
    })
}

fn git_output(project_root: &Path, args: &[&str]) -> Option<String> {
    git_output_bytes(project_root, args).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn git_output_bytes(project_root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn hash_directory(
    root: &Path,
    directory: &Path,
    hasher: &mut Sha256,
    paths: &mut BTreeSet<String>,
) -> Result<(), ForgeError> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(ForgeError::Io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(ForgeError::Io)?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if relative.components().next().is_some_and(|component| {
            matches!(
                component.as_os_str().to_str(),
                Some(".git" | ".forge" | "target" | "node_modules")
            )
        }) {
            continue;
        }
        if path.is_dir() {
            hash_directory(root, &path, hasher, paths)?;
        } else if path.is_file() {
            let relative = relative.to_string_lossy().replace('\\', "/");
            paths.insert(relative.clone());
            hasher.update(relative.as_bytes());
            hasher.update(std::fs::read(path).map_err(ForgeError::Io)?);
        }
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn complete_plan(
    store: &JsonlTaskStore,
    task_id: &str,
    evidence: &WorkflowEvidence,
) -> Result<(), ForgeError> {
    if let Some(failed) = evidence
        .checks
        .iter()
        .find(|check| check.status == VerificationStatus::Failed)
    {
        advance_to_node(store, task_id, "check")?;
        store.transition(
            task_id,
            TransitionRequest::new("check", TaskState::Interrupted)
                .interrupted_by(InterruptionReason::VerificationFailed {
                    check: failed.name.clone(),
                })
                .with_verification(failed.clone()),
        )?;
        return Err(ForgeError::task(format!(
            "verification failed; task {task_id} is parked at check: {}",
            failed.name
        )));
    }

    advance_to_node(store, task_id, "review")?;
    let loaded = store.load(task_id)?;
    if loaded.checkpoint.state("review") == Some(TaskState::Running) {
        let mut review =
            completion_request(&loaded, "review").with_verification(Verification::new(
                "working tree diff",
                if evidence.diff.is_some() {
                    VerificationStatus::Passed
                } else {
                    VerificationStatus::Skipped
                },
            ));
        if evidence.checks.is_empty() {
            review = review.with_verification(
                Verification::new("repository focused checks", VerificationStatus::Skipped)
                    .with_detail("provider completed without invoking a focused check"),
            );
        }
        store.transition(task_id, review)?;
    }
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
    let loaded = store.load(task_id)?;
    let node = loaded.checkpoint.active_node().unwrap_or("inspect");
    if loaded.checkpoint.state(node) == Some(TaskState::WaitingForApproval)
        && !matches!(reason, Some(InterruptionReason::ApprovalRequired))
    {
        store.transition(task_id, TransitionRequest::new(node, TaskState::Running))?;
    }
    if let Some(reason) = reason {
        store.transition(
            task_id,
            TransitionRequest::new(node, TaskState::Interrupted).interrupted_by(reason),
        )?;
    } else {
        store.transition(task_id, TransitionRequest::new(node, TaskState::Failed))?;
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
    use forge_config::Config;
    use forge_core::{
        ApprovalPolicy, CompletionRequest, CompletionResponse, ModelCapabilities, ModelProvider,
    };
    use forge_execution::{ApprovalChannel, MockExecution, NativeExecution};
    use forge_providers::{MockRouter, ScriptedMockModel, ScriptedReply};
    use forge_session::JsonlSessionStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct OnceLimitedModel {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelProvider for OnceLimitedModel {
        fn name(&self) -> &str {
            "once-limited"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, ForgeError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ForgeError::provider_rate_limited("once-limited", Some(1)))
            } else {
                Ok(CompletionResponse {
                    model: self.name().into(),
                    content: "resumed safely".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    fn task_id(store: &JsonlTaskStore) -> String {
        std::fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .find_map(|entry| {
                (entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                    .then(|| entry.path().file_stem()?.to_str().map(str::to_string))
                    .flatten()
            })
            .expect("one task journal")
    }

    fn service_with_model(root: &Path, model: Arc<dyn ModelProvider>) -> AgentService {
        AgentService::new(
            model,
            Arc::new(MockRouter::selecting("once-limited")),
            Arc::new(MockExecution::new(root)),
            Arc::new(crate::NullSkillRegistry),
            Arc::new(JsonlSessionStore::new(root.join(".forge/sessions"))),
            Config::default(),
        )
    }

    #[tokio::test]
    async fn provider_limit_parks_and_resumes_the_same_durable_task() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
        let model = Arc::new(OnceLimitedModel {
            calls: AtomicUsize::new(0),
        });
        let service = service_with_model(tmp.path(), model);
        let store = Arc::new(JsonlTaskStore::for_project(tmp.path()));
        let error = run_development_workflow(
            &service,
            Arc::clone(&store),
            tmp.path(),
            "explain the project",
            RunOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ForgeError::ProviderRateLimited { .. }));
        let id = task_id(&store);
        assert!(matches!(
            store.load(&id).unwrap().checkpoint.nodes["inspect"].interruption,
            Some(InterruptionReason::ProviderRateLimited { .. })
        ));

        let outcome = resume_development_workflow(
            &service,
            Arc::clone(&store),
            tmp.path(),
            &id,
            RunOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.review, "resumed safely");
        assert_eq!(
            store.load(&id).unwrap().checkpoint.state("review"),
            Some(TaskState::Succeeded)
        );
    }

    #[tokio::test]
    async fn resume_refuses_tree_conflicts_before_calling_the_provider_again() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "before\n").unwrap();
        let model = Arc::new(OnceLimitedModel {
            calls: AtomicUsize::new(0),
        });
        let service = service_with_model(tmp.path(), model.clone());
        let store = Arc::new(JsonlTaskStore::for_project(tmp.path()));
        let _ = run_development_workflow(
            &service,
            Arc::clone(&store),
            tmp.path(),
            "change it",
            RunOptions::default(),
        )
        .await;
        let id = task_id(&store);
        std::fs::write(tmp.path().join("main.rs"), "changed outside forge\n").unwrap();
        let error = resume_development_workflow(
            &service,
            Arc::clone(&store),
            tmp.path(),
            &id,
            RunOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("working tree changed"));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            store.load(&id).unwrap().checkpoint.nodes["inspect"].interruption,
            Some(InterruptionReason::TreeConflict { .. })
        ));
    }

    #[tokio::test]
    async fn resume_never_repeats_a_possibly_executed_command() {
        let tmp = tempfile::tempdir().unwrap();
        let model = Arc::new(OnceLimitedModel {
            calls: AtomicUsize::new(0),
        });
        let service = service_with_model(tmp.path(), model.clone());
        let store = Arc::new(JsonlTaskStore::for_project(tmp.path()));
        let _ = run_development_workflow(
            &service,
            Arc::clone(&store),
            tmp.path(),
            "run a command",
            RunOptions::default(),
        )
        .await;
        let id = task_id(&store);
        store
            .register_effect(
                &id,
                EffectRequest::new(
                    "ambiguous",
                    "inspect",
                    "run_command",
                    "hash",
                    EffectClass::NonIdempotent,
                ),
            )
            .unwrap();
        store.begin_effect(&id, "ambiguous").unwrap();
        let error = resume_development_workflow(
            &service,
            Arc::clone(&store),
            tmp.path(),
            &id,
            RunOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("possibly-executed"));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            store.load(&id).unwrap().checkpoint.nodes["inspect"].interruption,
            Some(InterruptionReason::AmbiguousEffect { .. })
        ));
    }

    #[tokio::test]
    async fn approval_and_verification_failures_park_typed_nodes() {
        let approval_tmp = tempfile::tempdir().unwrap();
        let approval_model = Arc::new(ScriptedMockModel::new(vec![ScriptedReply {
            text: None,
            tool_calls: vec![ToolCall::new(
                "write",
                "write_file",
                serde_json::json!({"path":"note.txt", "content":"x"}),
            )],
        }]));
        let approval_service = AgentService::new(
            approval_model,
            Arc::new(MockRouter::selecting("scripted-mock")),
            Arc::new(NativeExecution::with_channel(
                ApprovalPolicy::Prompt,
                approval_tmp.path(),
                ApprovalChannel::Parked,
            )),
            Arc::new(crate::NullSkillRegistry),
            Arc::new(JsonlSessionStore::new(
                approval_tmp.path().join(".forge/sessions"),
            )),
            Config::default(),
        );
        approval_service.close_input("approval-run");
        let approval_store = Arc::new(JsonlTaskStore::for_project(approval_tmp.path()));
        let error = run_development_workflow(
            &approval_service,
            Arc::clone(&approval_store),
            approval_tmp.path(),
            "write a note",
            RunOptions {
                run_id: Some("approval-run".into()),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ForgeError::ApprovalRequired { .. }));
        let approval_id = task_id(&approval_store);
        assert!(matches!(
            approval_store.load(&approval_id).unwrap().checkpoint.nodes["edit"].interruption,
            Some(InterruptionReason::ApprovalRequired)
        ));

        let check_tmp = tempfile::tempdir().unwrap();
        let check_model = Arc::new(ScriptedMockModel::new(vec![
            ScriptedReply {
                text: None,
                tool_calls: vec![ToolCall::new(
                    "check",
                    "run_command",
                    serde_json::json!({"command":"sh", "args":["-c", "exit 1"]}),
                )],
            },
            ScriptedReply {
                text: Some("check failed".into()),
                tool_calls: Vec::new(),
            },
        ]));
        let check_service = AgentService::new(
            check_model,
            Arc::new(MockRouter::selecting("scripted-mock")),
            Arc::new(NativeExecution::new(ApprovalPolicy::Auto, check_tmp.path())),
            Arc::new(crate::NullSkillRegistry),
            Arc::new(JsonlSessionStore::new(
                check_tmp.path().join(".forge/sessions"),
            )),
            Config::default(),
        );
        let check_store = Arc::new(JsonlTaskStore::for_project(check_tmp.path()));
        let error = run_development_workflow(
            &check_service,
            Arc::clone(&check_store),
            check_tmp.path(),
            "run the check",
            RunOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("verification failed"));
        let check_id = task_id(&check_store);
        assert!(matches!(
            check_store.load(&check_id).unwrap().checkpoint.nodes["check"].interruption,
            Some(InterruptionReason::VerificationFailed { .. })
        ));
    }
}
