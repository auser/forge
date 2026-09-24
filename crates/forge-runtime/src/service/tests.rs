use std::sync::Arc;

use forge_config::Config;
use forge_core::ToolCall;
use forge_core::{DecisionRouter, EventKind, ForgeError, RiskLevel, RoutingRequest};
use forge_execution::{MockExecution, NativeExecution};
use forge_providers::{MockModel, MockRouter, ScriptedMockModel, ScriptedReply};
use forge_session::JsonlSessionStore;

use super::*;

fn test_service(root: &std::path::Path) -> AgentService {
    AgentService::new(
        Arc::new(MockModel::new()),
        Arc::new(MockRouter::selecting("mock-local")),
        Arc::new(MockExecution::new(root)),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions"))),
        Config::default(),
    )
}

fn scripted_service(
    root: &std::path::Path,
    replies: Vec<ScriptedReply>,
    approval: forge_core::ApprovalPolicy,
) -> AgentService {
    AgentService::new(
        Arc::new(ScriptedMockModel::new(replies)),
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(NativeExecution::new(approval, root)),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions"))),
        Config::default(),
    )
}

/// A service wired to the deterministic `HashBackend` needle engine. The
/// fast-path tests drive it with exact `"<tool>: <json-object>"` prompts —
/// the one shape `HashBackend::tool_call` answers (everything else it
/// declines, which is the fall-through path).
fn needle_service(
    root: &std::path::Path,
    model: Arc<dyn forge_core::ModelProvider>,
    execution: Arc<dyn forge_core::ExecutionProvider>,
) -> AgentService {
    AgentService::new(
        model,
        Arc::new(MockRouter::selecting("scripted-mock")),
        execution,
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions"))),
        Config::default(),
    )
    .with_needle(Some(Arc::new(forge_needle::NeedleEngine::spawn(
        forge_needle::HashBackend::new(),
    ))))
}

fn text_reply(text: &str) -> ScriptedReply {
    ScriptedReply {
        text: Some(text.to_string()),
        tool_calls: Vec::new(),
    }
}

fn tool_reply(name: &str, arguments: serde_json::Value) -> ScriptedReply {
    ScriptedReply {
        text: None,
        tool_calls: vec![ToolCall::new("call_1", name, arguments)],
    }
}

fn event_kinds(outcome: &RunOutcome) -> Vec<&str> {
    outcome
        .events
        .iter()
        .map(|e| match &e.kind {
            EventKind::RunStarted { .. } => "run_started",
            EventKind::RoutingDecisionMade { .. } => "routing_decision_made",
            EventKind::SkillActivated { .. } => "skill_activated",
            EventKind::ToolCallRequested { .. } => "tool_call_requested",
            EventKind::ToolStarted { .. } => "tool_started",
            EventKind::ToolCompleted { .. } => "tool_completed",
            EventKind::FileChanged { .. } => "file_changed",
            EventKind::ApprovalRequested { .. } => "approval_requested",
            EventKind::ApprovalDecided { .. } => "approval_decided",
            EventKind::TurnCompleted { .. } => "turn_completed",
            EventKind::InputReceived { .. } => "input_received",
            EventKind::Error { .. } => "error",
            EventKind::Cancelled { .. } => "cancelled",
            EventKind::Completed { .. } => "completed",
            _ => "other",
        })
        .collect()
}

#[tokio::test]
async fn full_run_emits_ordered_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = test_service(tmp.path());

    let outcome = service.run("hello there").await.expect("run succeeds");

    assert_eq!(outcome.text, "mock response to: hello there");
    assert_eq!(outcome.turns, 1);
    assert_eq!(
        event_kinds(&outcome),
        ["run_started", "routing_decision_made", "completed"]
    );

    // Everything was persisted, with monotonic sequence numbers.
    let persisted = service
        .sessions()
        .events_for(&outcome.session_id)
        .expect("read");
    assert_eq!(persisted.len(), 3);
    assert!(persisted.iter().all(|e| e.run_id == outcome.run_id));
    let seqs: Vec<u64> = persisted.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3]);
}

struct FailingRouter;

#[async_trait::async_trait]
impl DecisionRouter for FailingRouter {
    async fn route(&self, _: &RoutingRequest) -> Result<forge_core::RoutingDecision, ForgeError> {
        Err(ForgeError::router("router exploded"))
    }
}

#[tokio::test]
async fn run_failure_appends_error_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = AgentService::new(
        Arc::new(MockModel::new()),
        Arc::new(FailingRouter),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
        Config::default(),
    );

    let err = service.run("doomed").await.expect_err("routing fails");
    assert!(matches!(err, ForgeError::Router(_)));

    let all = service.sessions().events().expect("read all");
    assert_eq!(all.len(), 2);
    assert!(matches!(all[0].kind, EventKind::RunStarted { .. }));
    match &all[1].kind {
        EventKind::Error { message } => assert!(message.contains("router exploded")),
        other => panic!("expected error event, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_unknown_run_is_typed_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = test_service(tmp.path());
    let err = service
        .cancel("definitely-unknown-run")
        .expect_err("unknown");
    assert!(matches!(err, ForgeError::Session(_)));
}

#[tokio::test]
async fn subscribers_receive_live_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = test_service(tmp.path());

    let outcome = service.run("stream me").await.expect("run");
    let mut rx = service.subscribe(&outcome.run_id);
    service.cancel(&outcome.run_id).expect("cancel");
    let event = rx.try_recv().expect("broadcast delivered");
    assert!(matches!(event.kind, EventKind::Cancelled { .. }));
}

// --- agent loop with the scripted mock ---

#[tokio::test]
async fn scripted_single_turn_text_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("the answer")],
        forge_core::ApprovalPolicy::Deny,
    );
    let outcome = service.run("question").await.expect("run");
    assert_eq!(outcome.text, "the answer");
    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.tool_calls, 0);
}

#[tokio::test]
async fn scripted_two_turn_run_writes_file_and_emits_full_trail() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![
            tool_reply(
                "write_file",
                serde_json::json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
            ),
            text_reply("created main.rs"),
        ],
        forge_core::ApprovalPolicy::Auto,
    );

    let outcome = service.run("add main.rs").await.expect("run");
    assert_eq!(outcome.text, "created main.rs");
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.tool_calls, 1);

    // The file was actually written via native execution.
    let written = std::fs::read_to_string(tmp.path().join("src/main.rs")).expect("file");
    assert_eq!(written, "fn main() {}\n");

    assert_eq!(
        event_kinds(&outcome),
        [
            "run_started",
            "routing_decision_made",
            "tool_call_requested",
            "tool_started",
            "file_changed",
            "tool_completed",
            "turn_completed",
            "completed"
        ]
    );
    // Sequence numbers are monotonic.
    let seqs: Vec<u64> = outcome.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=8).collect::<Vec<_>>());
}

#[tokio::test]
async fn tool_errors_go_back_to_the_model_without_aborting() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![
            tool_reply("read_file", serde_json::json!({"path": "missing.txt"})),
            text_reply("recovered"),
        ],
        forge_core::ApprovalPolicy::Auto,
    );
    let outcome = service.run("read it").await.expect("run completes");
    assert_eq!(outcome.text, "recovered");
    let tool_completed = outcome
        .events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ToolCompleted { success, .. } if !success));
    assert!(tool_completed, "tool failure recorded as unsuccessful");
}

#[tokio::test]
async fn approval_pause_without_input_fails_cleanly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![
            tool_reply(
                "write_file",
                serde_json::json!({"path": "out.txt", "content": "x"}),
            ),
            text_reply("never"),
        ],
        forge_core::ApprovalPolicy::Prompt,
    );
    // Simulate stdin EOF: close the channel before the run starts.
    let run_id = "approval-run".to_string();
    service.close_input(&run_id);

    let err = service
        .run_with_options(
            "write something",
            RunOptions {
                run_id: Some(run_id),
                ..RunOptions::default()
            },
        )
        .await
        .expect_err("approval cannot be satisfied");
    assert!(matches!(err, ForgeError::ApprovalRequired { .. }));

    let events = service.sessions().events().expect("events");
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.kind {
            EventKind::ApprovalRequested { .. } => "approval_requested",
            EventKind::ApprovalDecided { approved, .. } => {
                assert!(!approved);
                "approval_decided"
            }
            EventKind::Error { .. } => "error",
            _ => "other",
        })
        .collect();
    assert!(kinds.contains(&"approval_requested"), "{kinds:?}");
    assert!(kinds.contains(&"approval_decided"), "{kinds:?}");
    assert!(kinds.contains(&"error"), "{kinds:?}");
    assert!(!tmp.path().join("out.txt").exists(), "no write happened");
}

#[tokio::test]
async fn approval_granted_via_input_retries_and_writes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![
            tool_reply(
                "write_file",
                serde_json::json!({"path": "approved.txt", "content": "yes"}),
            ),
            text_reply("written"),
        ],
        forge_core::ApprovalPolicy::Prompt,
    );
    let run_id = "approved-run".to_string();
    // Pre-queue the approval (channel buffers it).
    service.send_input(&run_id, "y").expect("queue input");

    let outcome = service
        .run_with_options(
            "write it",
            RunOptions {
                run_id: Some(run_id),
                ..RunOptions::default()
            },
        )
        .await
        .expect("run completes after approval");
    assert_eq!(outcome.text, "written");
    assert!(tmp.path().join("approved.txt").is_file());
    assert!(
        outcome
            .events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::ApprovalDecided { approved: true, .. }))
    );
}

#[tokio::test]
async fn approval_denied_via_input_continues_the_loop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![
            tool_reply(
                "write_file",
                serde_json::json!({"path": "denied.txt", "content": "x"}),
            ),
            text_reply("ok, skipped"),
        ],
        forge_core::ApprovalPolicy::Prompt,
    );
    let run_id = "denied-run".to_string();
    service.send_input(&run_id, "n").expect("queue input");

    let outcome = service
        .run_with_options(
            "write it",
            RunOptions {
                run_id: Some(run_id),
                ..RunOptions::default()
            },
        )
        .await
        .expect("run continues after denial");
    assert_eq!(outcome.text, "ok, skipped");
    assert!(!tmp.path().join("denied.txt").exists());
    assert!(outcome.events.iter().any(|e| matches!(
        &e.kind,
        EventKind::ApprovalDecided {
            approved: false,
            ..
        }
    )));
}

#[tokio::test]
async fn prompt_dangerous_flows_risky_but_pauses_destructive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("victim.txt"), "x").expect("write");
    // Risky write flows without approval under prompt-dangerous.
    let service = scripted_service(
        tmp.path(),
        vec![
            tool_reply(
                "write_file",
                serde_json::json!({"path": "risky.txt", "content": "r"}),
            ),
            text_reply("done"),
        ],
        forge_core::ApprovalPolicy::PromptDestructive,
    );
    let outcome = service.run("write risky").await.expect("runs");
    assert_eq!(outcome.text, "done");
    assert!(tmp.path().join("risky.txt").is_file());

    // Destructive delete pauses; with a closed input channel the run
    // fails cleanly.
    let service = scripted_service(
        tmp.path(),
        vec![
            tool_reply("delete_file", serde_json::json!({"path": "victim.txt"})),
            text_reply("never"),
        ],
        forge_core::ApprovalPolicy::PromptDestructive,
    );
    let run_id = "delete-run".to_string();
    service.close_input(&run_id);
    let err = service
        .run_with_options(
            "delete it",
            RunOptions {
                run_id: Some(run_id),
                ..RunOptions::default()
            },
        )
        .await
        .expect_err("destructive pauses");
    assert!(matches!(err, ForgeError::ApprovalRequired { .. }));
    assert!(tmp.path().join("victim.txt").exists());
}

#[tokio::test]
async fn max_turns_exhaustion_fails_with_error_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Every reply requests another tool call: never terminates on its own.
    let replies = (0..10)
        .map(|_| tool_reply("read_file", serde_json::json!({"path": "x.txt"})))
        .collect();
    let service = scripted_service(tmp.path(), replies, forge_core::ApprovalPolicy::Auto);

    let err = service
        .run_with_options(
            "loop forever",
            RunOptions {
                max_turns: Some(3),
                ..RunOptions::default()
            },
        )
        .await
        .expect_err("budget exhausts");
    assert!(err.to_string().contains("max turns (3) exhausted"));

    let events = service.sessions().events().expect("events");
    assert!(
        events.iter().any(
            |e| matches!(&e.kind, EventKind::Error { message } if message.contains("max turns"))
        )
    );
    let turns_completed = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::TurnCompleted { .. }))
        .count();
    assert_eq!(turns_completed, 3);
}

// Cancellation is tested against a run parked in an approval wait —
// the only deterministic "mid-loop" point with an instant mock model.
async fn parked_service(
    dir: &std::path::Path,
) -> (
    Arc<AgentService>,
    String,
    tokio::task::JoinHandle<Result<RunOutcome, ForgeError>>,
) {
    let service = Arc::new(scripted_service(
        dir,
        vec![
            tool_reply(
                "write_file",
                serde_json::json!({"path": "parked.txt", "content": "x"}),
            ),
            text_reply("never reached"),
        ],
        forge_core::ApprovalPolicy::Prompt,
    ));
    let (run_id, _session, handle) = service.start_run("park me", None);
    // Wait until the loop is actually parked in the approval wait.
    let mut rx = service.subscribe(&run_id);
    loop {
        match rx.recv().await {
            Ok(event) => {
                if matches!(event.kind, EventKind::ApprovalRequested { .. }) {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(e) => panic!("broadcast: {e}"),
        }
    }
    (service, run_id, handle)
}

#[tokio::test]
async fn cancel_stops_the_loop_mid_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, run_id, handle) = parked_service(tmp.path()).await;

    service.cancel(&run_id).expect("cancel");

    let err = handle.await.expect("join").expect_err("run aborted");
    assert!(err.to_string().contains("cancelled"), "got: {err}");
    assert!(!tmp.path().join("parked.txt").exists());

    let events = service.events(&run_id).expect("events");
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Cancelled { .. }))
    );
}

#[tokio::test]
async fn cancel_marker_file_stops_a_loop_without_token() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_service, run_id, handle) = parked_service(tmp.path()).await;

    // Cross-process simulation: only write the marker file, no token.
    let marker_dir = tmp.path().join(".forge").join("runs");
    std::fs::create_dir_all(&marker_dir).expect("mkdir");
    std::fs::write(marker_dir.join(format!("{run_id}.cancel")), "cancelled\n").expect("marker");

    let err = handle.await.expect("join").expect_err("run aborted");
    assert!(err.to_string().contains("cancelled"), "got: {err}");
    assert!(!tmp.path().join("parked.txt").exists());
}

#[tokio::test]
async fn resume_continues_in_same_session_with_prior_outcome() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Script: first run answers; the resumed run answers again (script
    // continues across runs because the model is shared).
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("first answer"), text_reply("second answer")],
        forge_core::ApprovalPolicy::Auto,
    );

    let first = service.run("original prompt").await.expect("first run");
    let resumed = service.resume(&first.run_id).await.expect("resume");

    assert_eq!(resumed.text, "second answer");
    assert_eq!(resumed.session_id, first.session_id, "same session");
    assert_ne!(resumed.run_id, first.run_id, "new run");
    // The resume marker links the runs.
    assert!(
        resumed.events.iter().any(|e| matches!(
            &e.kind,
            EventKind::InputReceived { message } if message.contains(&first.run_id)
        )),
        "resume marker missing"
    );
}

#[tokio::test]
async fn resume_rejects_v1_runs_without_prompt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store_dir = tmp.path().join(".forge").join("sessions");
    std::fs::create_dir_all(&store_dir).expect("mkdir");
    // A v1-style run: RunStarted has no prompt field.
    std::fs::write(
        store_dir.join("s1.jsonl"),
        "{\"v\":1,\"ts\":\"2026-09-22T20:01:39.172579Z\",\"run_id\":\"old-run\",\"session_id\":\"s1\",\"type\":\"run_started\",\"provider\":\"mock-local\",\"model\":\"mock-local\"}\n{\"v\":1,\"ts\":\"2026-09-22T20:01:40.172579Z\",\"run_id\":\"old-run\",\"session_id\":\"s1\",\"type\":\"completed\",\"summary\":\"done\"}\n",
    )
    .expect("write v1 log");

    let service = test_service(tmp.path());
    let err = service
        .resume("old-run")
        .await
        .expect_err("cannot resume v1");
    assert!(err.to_string().contains("schema v2"), "got: {err}");
}

#[tokio::test]
async fn run_seeds_graph_context_when_available() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
    std::fs::write(
        tmp.path().join("src/util.rs"),
        "pub fn helper_function() {}\n",
    )
    .expect("write");
    let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
    forge_core::ProjectGraph::build(&mut graph).expect("build");

    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("with context")]));
    let recorded = model.clone();
    let service = AgentService::new(
        model,
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(
            tmp.path().join(".forge").join("sessions"),
        )),
        Config::default(),
    )
    .with_graph(Some(Arc::new(graph)));

    let outcome = service.run("ask about helper_function").await.expect("run");
    assert_eq!(outcome.text, "with context");
    let requests = recorded.recorded();
    let system = requests[0]
        .messages
        .iter()
        .filter(|m| m.role == forge_core::Role::System)
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system.contains("src/util.rs"), "system context: {system}");
}

#[tokio::test]
async fn graph_tools_work_and_report_unavailable() {
    // With a graph.
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
    std::fs::write(tmp.path().join("src/a.rs"), "fn alpha() {}\n").expect("write");
    let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
    forge_core::ProjectGraph::build(&mut graph).expect("build");

    let model = Arc::new(ScriptedMockModel::new(vec![
        tool_reply("graph_grep", serde_json::json!({"pattern": "alpha"})),
        text_reply("found it"),
    ]));
    let recorded = model.clone();
    let service = AgentService::new(
        model,
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(
            tmp.path().join(".forge").join("sessions"),
        )),
        Config::default(),
    )
    .with_graph(Some(Arc::new(graph)));

    let outcome = service.run("find alpha").await.expect("run");
    assert_eq!(outcome.text, "found it");
    let requests = recorded.recorded();
    let tool_msg = requests[1]
        .messages
        .iter()
        .find(|m| m.role == forge_core::Role::Tool)
        .expect("tool result message");
    assert!(
        tool_msg.content.contains("alpha"),
        "got: {}",
        tool_msg.content
    );

    // Without a graph: error tool result, loop continues.
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp2.path(),
        vec![
            tool_reply("graph_context", serde_json::json!({"query": "anything"})),
            text_reply("no graph, fine"),
        ],
        forge_core::ApprovalPolicy::Auto,
    );
    let outcome = service.run("query").await.expect("run");
    assert_eq!(outcome.text, "no graph, fine");
}

// --- needle direct-dispatch fast path ---

fn has_needle_dispatch(outcome: &RunOutcome) -> bool {
    outcome.events.iter().any(|e| {
        matches!(&e.kind, EventKind::RoutingDecisionMade { router, .. } if router == "needle-dispatch")
    })
}

#[tokio::test]
async fn needle_fast_path_dispatches_exact_tool_prompt_without_the_model() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply(
        "the model must never be called",
    )]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("fn main() {}\n"));
    let service = needle_service(tmp.path(), model.clone(), exec.clone());

    let outcome = service
        .run("read_file: {\"path\": \"Cargo.toml\"}")
        .await
        .expect("run");

    // The tool output is the run's answer, produced without a model call.
    assert_eq!(outcome.text, "fn main() {}\n");
    assert_eq!(outcome.turns, 0, "no model turns");
    assert_eq!(outcome.tool_calls, 1);
    assert!(model.recorded().is_empty(), "model loop never ran");
    assert_eq!(exec.recorded_file_ops().len(), 1, "the tool really ran");

    assert!(has_needle_dispatch(&outcome), "{:?}", event_kinds(&outcome));
    assert_eq!(
        event_kinds(&outcome),
        [
            "run_started",
            "routing_decision_made", // model routing
            "routing_decision_made", // needle-dispatch
            "tool_call_requested",
            "tool_started",
            "tool_completed",
            "completed",
        ]
    );
    assert!(
        !event_kinds(&outcome).contains(&"turn_completed"),
        "the model loop must not run"
    );
}

#[tokio::test]
async fn needle_fast_path_refuses_destructive_calls_and_falls_through() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("I will not")]));
    let exec = Arc::new(MockExecution::new(tmp.path()));
    let service = needle_service(tmp.path(), model.clone(), exec.clone());

    let outcome = service
        .run("run_command: {\"command\": \"rm\", \"args\": [\"-rf\", \"logs\"]}")
        .await
        .expect("run completes through the normal loop");

    assert!(!has_needle_dispatch(&outcome), "guardrail must refuse");
    assert_eq!(outcome.text, "I will not");
    assert_eq!(model.recorded().len(), 1, "the full loop ran");
    // No partial execution: the refused call never reached the provider.
    assert!(exec.recorded().is_empty(), "no command executed");
    assert!(exec.recorded_file_ops().is_empty(), "no file op");
    assert_eq!(outcome.tool_calls, 0);
}

#[tokio::test]
async fn needle_fast_path_absent_engine_changes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("plain loop")],
        forge_core::ApprovalPolicy::Auto,
    );
    let outcome = service
        .run("read_file: {\"path\": \"Cargo.toml\"}")
        .await
        .expect("run");

    assert_eq!(outcome.text, "plain loop");
    assert_eq!(outcome.turns, 1);
    assert_eq!(
        event_kinds(&outcome),
        ["run_started", "routing_decision_made", "completed"]
    );
}

#[tokio::test]
async fn needle_fast_path_never_attempts_a_non_read_only_operation() {
    // A write is at least `Risky`, so it could pause for approval — the
    // fast path must therefore refuse it *before* the execution provider is
    // touched at all. `MockExecution` never gates anything, so a recorded
    // file op here would mean the fast path had attempted (and completed) a
    // write whose approval the loop is supposed to own.
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("loop handled it")]));
    let exec = Arc::new(MockExecution::new(tmp.path()));
    let service = needle_service(tmp.path(), model.clone(), exec.clone());

    let outcome = service
        .run("write_file: {\"path\": \"out.txt\", \"content\": \"x\"}")
        .await
        .expect("run completes through the normal loop");

    assert!(!has_needle_dispatch(&outcome));
    assert_eq!(outcome.text, "loop handled it");
    assert!(
        exec.recorded_file_ops().is_empty(),
        "no execution attempt may be made for a gateable operation"
    );

    // Same prompt against a provider that does gate: still no write, and
    // the loop — not the fast path — owns the approval decision.
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let native = Arc::new(NativeExecution::new(
        forge_core::ApprovalPolicy::Prompt,
        tmp2.path(),
    ));
    let service = needle_service(
        tmp2.path(),
        Arc::new(ScriptedMockModel::new(vec![text_reply("loop handled it")])),
        native,
    );
    let outcome = service
        .run("write_file: {\"path\": \"out.txt\", \"content\": \"x\"}")
        .await
        .expect("run completes through the normal loop");
    assert!(!has_needle_dispatch(&outcome));
    assert!(!tmp2.path().join("out.txt").exists(), "no write happened");
}

#[tokio::test]
async fn needle_fast_path_dispatches_safe_reads_under_prompt_approval() {
    // The strictest interactive policy still fast-paths a read: `Safe`
    // operations return from `check_approval` before the policy is
    // consulted, so no prompt can originate here. (Tests have no terminal
    // stdin, so an approval attempt would fail the run outright — passing
    // proves none was made.)
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("notes.txt"), "read me\n").expect("write");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("never used")]));
    let exec = Arc::new(NativeExecution::new(
        forge_core::ApprovalPolicy::Prompt,
        tmp.path(),
    ));
    let service = needle_service(tmp.path(), model.clone(), exec);

    let outcome = service
        .run("read_file: {\"path\": \"notes.txt\"}")
        .await
        .expect("read dispatches without approval");

    assert!(has_needle_dispatch(&outcome));
    assert_eq!(outcome.text, "read me\n");
    assert!(model.recorded().is_empty(), "model never called");
    assert!(
        !event_kinds(&outcome).contains(&"approval_requested"),
        "the fast path must never ask for approval"
    );
}

#[tokio::test]
async fn needle_fast_path_requires_a_tool_capable_model() {
    // A chat-only provider's run is a plain completion with no tools at
    // all; the fast path must not turn it into tool execution.
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(
        MockModel::new().with_capabilities(forge_core::ModelCapabilities {
            streaming: true,
            tools: false,
            structured_output: false,
            vision: false,
            max_context: 8_192,
        }),
    );
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("secret"));
    let service = needle_service(tmp.path(), model, exec.clone());

    let outcome = service
        .run("read_file: {\"path\": \"Cargo.toml\"}")
        .await
        .expect("run");

    assert!(!has_needle_dispatch(&outcome));
    assert!(
        exec.recorded_file_ops().is_empty(),
        "a chat-only model's run must execute no tools"
    );
    assert_eq!(outcome.turns, 1, "plain completion path");
}

#[tokio::test]
async fn needle_fast_path_guardrail_refuses_destructive_wording_on_a_safe_tool() {
    // The guardrail is a second, independent gate: this call is read-only
    // (so the risk gate admits it), but its arguments read destructively,
    // and the brain's verdict alone must stop it.
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("loop answered")]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("contents"));
    let service = needle_service(tmp.path(), model.clone(), exec.clone());

    let outcome = service
        .run("read_file: {\"path\": \"delete_me.txt\"}")
        .await
        .expect("run completes through the normal loop");

    assert!(!has_needle_dispatch(&outcome), "guardrail must refuse");
    assert!(exec.recorded_file_ops().is_empty(), "nothing was read");
    assert_eq!(outcome.text, "loop answered");
}

#[tokio::test]
async fn needle_fast_path_skips_resumed_runs() {
    // A resume re-uses the original prompt; re-dispatching the same tool
    // instead of continuing the conversation would be wrong.
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("resumed answer")]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("contents"));
    let service = needle_service(tmp.path(), model.clone(), exec.clone());

    let first = service
        .run("read_file: {\"path\": \"Cargo.toml\"}")
        .await
        .expect("first run");
    assert!(has_needle_dispatch(&first), "first run fast-paths");

    let resumed = service.resume(&first.run_id).await.expect("resume");
    assert!(
        !has_needle_dispatch(&resumed),
        "resume goes through the loop"
    );
    assert_eq!(resumed.text, "resumed answer");
    assert_eq!(exec.recorded_file_ops().len(), 1, "no second dispatch");
}

#[tokio::test]
async fn needle_fast_path_declines_prompts_it_cannot_fill() {
    // Prose (not "<tool>: <json>"): HashBackend declines, run is unchanged.
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("prose answer")]));
    let exec = Arc::new(MockExecution::new(tmp.path()));
    let service = needle_service(tmp.path(), model.clone(), exec);

    let outcome = service
        .run("please read Cargo.toml for me")
        .await
        .expect("run");
    assert!(!has_needle_dispatch(&outcome));
    assert_eq!(outcome.text, "prose answer");
    assert_eq!(outcome.turns, 1);
}

#[tokio::test]
async fn run_command_tool_executes_and_clamps_risk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // The model claims "safe"; the dispatcher must clamp to Risky.
    // Approval Deny would reject Risky — use Auto here and check the
    // recorded request's risk separately via a deny variant below.
    let exec = Arc::new(MockExecution::new(tmp.path()));
    let service = AgentService::new(
        Arc::new(ScriptedMockModel::new(vec![
            tool_reply(
                "run_command",
                serde_json::json!({"command": "rm", "args": ["-rf", "x"], "risk": "safe"}),
            ),
            text_reply("done"),
        ])),
        Arc::new(MockRouter::selecting("scripted-mock")),
        exec.clone(),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(
            tmp.path().join(".forge").join("sessions"),
        )),
        Config::default(),
    );
    let outcome = service.run("run it").await.expect("run");
    assert_eq!(outcome.text, "done");
    let recorded = exec.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].command, "rm");
    assert_eq!(
        recorded[0].risk,
        RiskLevel::Risky,
        "model hint 'safe' must be clamped up to risky"
    );
}
