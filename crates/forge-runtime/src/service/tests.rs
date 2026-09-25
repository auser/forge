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
            EventKind::AssistantMessage { .. } => "assistant_message",
            EventKind::ToolResult { .. } => "tool_result",
            EventKind::SessionForked { .. } => "session_forked",
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
        [
            "run_started",
            "routing_decision_made",
            // v3: the model's answer, verbatim, for replay
            "assistant_message",
            "completed"
        ]
    );

    // Everything was persisted, with monotonic sequence numbers.
    let persisted = service
        .sessions()
        .events_for(&outcome.session_id)
        .expect("read");
    assert_eq!(persisted.len(), 4);
    assert!(persisted.iter().all(|e| e.run_id == outcome.run_id));
    let seqs: Vec<u64> = persisted.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
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
    // A live (parked) run: subscribing mid-run streams its events, and a
    // cancel reaches the subscriber.
    let (service, run_id, handle) = parked_service(tmp.path()).await;
    let mut rx = service.subscribe(&run_id);
    service.cancel(&run_id).expect("cancel");
    let event = rx.try_recv().expect("broadcast delivered");
    assert!(matches!(event.kind, EventKind::Cancelled { .. }));
    let _ = handle.await;
}

#[tokio::test]
async fn subscribing_after_a_run_finished_yields_a_closed_stream() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = test_service(tmp.path());
    let outcome = service.run("stream me").await.expect("run");

    // The run is over: there is nothing live to join, and registering a
    // broadcaster for it would be the leak this replaces. `attach` (or the
    // session store) is how a finished run is read.
    let mut rx = service.subscribe(&outcome.run_id);
    service
        .cancel(&outcome.run_id)
        .expect("cancel still records");
    assert!(rx.try_recv().is_err(), "no live stream for a finished run");
    assert!(
        service
            .events(&outcome.run_id)
            .expect("events")
            .iter()
            .any(|e| matches!(e.kind, EventKind::Cancelled { .. })),
        "the cancellation is still recorded"
    );
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
            // v3 replay record of the model's tool-call turn
            "assistant_message",
            "tool_call_requested",
            "tool_started",
            "file_changed",
            "tool_completed",
            // v3 replay record of the tool's output
            "tool_result",
            "turn_completed",
            // v3 replay record of the final answer
            "assistant_message",
            "completed"
        ]
    );
    // Sequence numbers are monotonic.
    let seqs: Vec<u64> = outcome.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=11).collect::<Vec<_>>());
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

/// The service under test plus the scripted model, so a test can inspect
/// the requests the loop actually sent.
fn recording_service(
    root: &std::path::Path,
    replies: Vec<ScriptedReply>,
) -> (AgentService, Arc<ScriptedMockModel>) {
    let model = Arc::new(ScriptedMockModel::new(replies));
    let service = AgentService::new(
        model.clone(),
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(NativeExecution::new(forge_core::ApprovalPolicy::Auto, root)),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions"))),
        Config::default(),
    );
    (service, model)
}

#[tokio::test]
async fn resume_replays_the_whole_conversation_to_the_model() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, model) = recording_service(
        tmp.path(),
        vec![
            // Run 1: a tool call, then an answer.
            tool_reply("read_file", serde_json::json!({"path": "notes.txt"})),
            text_reply("notes.txt says hello"),
            // Run 2 (the resume): one answer.
            text_reply("and it still does"),
        ],
    );
    std::fs::write(tmp.path().join("notes.txt"), "hello from disk").expect("write");

    let first = service
        .run("what is in notes.txt")
        .await
        .expect("first run");
    assert_eq!(first.text, "notes.txt says hello");

    let requests_before = model.recorded().len();
    let resumed = service.resume(&first.run_id).await.expect("resume");
    assert_eq!(resumed.text, "and it still does");
    assert_eq!(resumed.session_id, first.session_id);

    // The resumed run's request carries the FIRST run's conversation.
    let resumed_request = model
        .recorded()
        .into_iter()
        .nth(requests_before)
        .expect("the resumed run called the model");
    let shape: Vec<(forge_core::Role, String)> = resumed_request
        .messages
        .iter()
        .map(|m| (m.role, m.content.clone()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (forge_core::Role::User, "what is in notes.txt".to_string()),
            (forge_core::Role::Assistant, String::new()),
            (forge_core::Role::Tool, "hello from disk".to_string()),
            (
                forge_core::Role::Assistant,
                "notes.txt says hello".to_string()
            ),
            (
                forge_core::Role::User,
                "Continue the work in the conversation above.".to_string()
            ),
        ],
        "the resumed run must see the full prior conversation"
    );
    // The tool call itself is replayed, not just its text.
    assert_eq!(resumed_request.messages[1].tool_calls.len(), 1);
    assert_eq!(resumed_request.messages[1].tool_calls[0].name, "read_file");
    assert_eq!(
        resumed_request.messages[2].tool_call_id.as_deref(),
        Some(resumed_request.messages[1].tool_calls[0].id.as_str()),
        "the replayed tool result must answer the replayed call"
    );
}

#[tokio::test]
async fn a_third_run_replays_both_earlier_runs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, model) = recording_service(
        tmp.path(),
        vec![
            text_reply("answer one"),
            text_reply("answer two"),
            text_reply("answer three"),
        ],
    );

    let first = service.run("the original ask").await.expect("run 1");
    let second = service.resume(&first.run_id).await.expect("run 2");
    let before = model.recorded().len();
    service.resume(&second.run_id).await.expect("run 3");

    let third = model
        .recorded()
        .into_iter()
        .nth(before)
        .expect("run 3 called the model");
    let contents: Vec<&str> = third.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec![
            "the original ask",
            "answer one",
            "Continue the work in the conversation above.",
            "answer two",
            "Continue the work in the conversation above.",
        ],
        "every prior run replays, in order"
    );
}

#[tokio::test]
async fn resume_of_a_pre_v3_log_degrades_to_the_recorded_summary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store_dir = tmp.path().join(".forge").join("sessions");
    std::fs::create_dir_all(&store_dir).expect("mkdir");
    // A v2 log: a prompt and a truncated completion summary, no v3 replay
    // events at all.
    std::fs::write(
        store_dir.join("s1.jsonl"),
        "{\"v\":2,\"seq\":1,\"ts\":\"2026-09-22T20:01:39.172579Z\",\"run_id\":\"old-run\",\"session_id\":\"s1\",\"type\":\"run_started\",\"provider\":\"scripted-mock\",\"model\":\"scripted-mock\",\"prompt\":\"the old ask\"}\n{\"v\":2,\"seq\":2,\"ts\":\"2026-09-22T20:01:40.172579Z\",\"run_id\":\"old-run\",\"session_id\":\"s1\",\"type\":\"completed\",\"summary\":\"the truncated old answer\"}\n",
    )
    .expect("write v2 log");

    let (service, model) = recording_service(tmp.path(), vec![text_reply("carrying on")]);
    let resumed = service.resume("old-run").await.expect("resume an old run");
    assert_eq!(resumed.text, "carrying on");
    assert_eq!(resumed.session_id, "s1");

    let request = model
        .recorded()
        .into_iter()
        .next()
        .expect("the model was called");
    let contents: Vec<&str> = request
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        contents,
        vec![
            "the old ask",
            "the truncated old answer",
            "Continue the work in the conversation above.",
        ],
        "an old log replays as well as its data allows"
    );
}

// --- attach / list_runs / pruning ---------------------------------------

/// Sizes of the three per-run tracking maps.
fn map_sizes(service: &AgentService) -> (usize, usize, usize) {
    fn len<T>(map: &Mutex<HashMap<String, T>>) -> usize {
        map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    (
        len(&service.inputs),
        len(&service.broadcasters),
        len(&service.cancel_tokens),
    )
}

#[tokio::test]
async fn tracking_maps_do_not_grow_across_runs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("a"), text_reply("b"), text_reply("c")],
        forge_core::ApprovalPolicy::Auto,
    );
    for _ in 0..3 {
        service.run("ask").await.expect("run");
    }
    assert_eq!(
        map_sizes(&service),
        (0, 0, 0),
        "a finished run must leave no tracking entries behind"
    );
}

#[tokio::test]
async fn a_failed_run_is_pruned_too() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = AgentService::new(
        Arc::new(MockModel::new()),
        Arc::new(FailingRouter),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
        Config::default(),
    );
    service.run("doomed").await.expect_err("routing fails");
    assert_eq!(map_sizes(&service), (0, 0, 0));
}

#[tokio::test]
async fn send_input_to_a_finished_run_is_a_typed_error_not_a_resurrection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("done")],
        forge_core::ApprovalPolicy::Auto,
    );
    let run = service.run("ask").await.expect("run");

    let err = service
        .send_input(&run.run_id, "y")
        .expect_err("a finished run takes no input");
    assert!(matches!(err, ForgeError::Session(_)), "got: {err}");
    assert!(err.to_string().contains("completed"), "got: {err}");
    assert_eq!(
        map_sizes(&service),
        (0, 0, 0),
        "the refusal must not recreate the run's channels"
    );
}

#[tokio::test]
async fn subscribing_to_a_finished_run_does_not_grow_the_maps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("done")],
        forge_core::ApprovalPolicy::Auto,
    );
    let run = service.run("ask").await.expect("run");
    for _ in 0..5 {
        let mut rx = service.subscribe(&run.run_id);
        assert!(rx.try_recv().is_err(), "a finished run has no live events");
    }
    assert_eq!(map_sizes(&service), (0, 0, 0));
}

#[tokio::test]
async fn attach_serves_a_terminal_runs_full_backlog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("done")],
        forge_core::ApprovalPolicy::Auto,
    );
    let run = service.run("ask").await.expect("run");

    let mut attachment = service.attach(&run.run_id).expect("attach");
    assert_eq!(attachment.state, forge_core::RunState::Completed);
    assert_eq!(attachment.session_id.as_deref(), Some(&*run.session_id));
    assert_eq!(
        attachment.backlog.len(),
        run.events.len(),
        "the backlog is the whole run"
    );
    assert!(!attachment.is_live());
    assert!(attachment.recv().await.is_none());
    assert_eq!(map_sizes(&service), (0, 0, 0), "attach must not leak");
}

#[tokio::test]
async fn attach_on_an_unknown_run_is_a_typed_error_and_leaks_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = test_service(tmp.path());
    let err = service.attach("no-such-run").expect_err("unknown");
    assert!(matches!(err, ForgeError::Session(_)), "got: {err}");
    assert_eq!(map_sizes(&service), (0, 0, 0));
}

#[tokio::test]
async fn a_late_attach_gets_the_backlog_and_the_live_tail_without_gap_or_duplicate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A run that parks on an approval: it is live and has a backlog, which
    // is exactly the state a UI attaches to.
    let (service, run_id, handle) = parked_service(tmp.path()).await;

    let mut attachment = service.attach(&run_id).expect("attach mid-run");
    assert!(attachment.is_live(), "a live run must stream");
    assert_eq!(
        attachment.state,
        forge_core::RunState::WaitingForApproval,
        "a run parked on an approval is waiting, not running"
    );
    assert!(!attachment.backlog.is_empty());
    let backlog_seqs: Vec<u64> = attachment.backlog.iter().map(|e| e.seq).collect();
    assert_eq!(
        backlog_seqs,
        (1..=backlog_seqs.len() as u64).collect::<Vec<_>>(),
        "the backlog itself is a gapless prefix"
    );

    // Unblock the run and drain the live tail.
    service.send_input(&run_id, "y").expect("approve");
    let mut live_seqs = Vec::new();
    while let Some(event) = attachment.recv().await {
        live_seqs.push(event.seq);
        if event.kind.is_terminal() {
            break;
        }
    }
    handle.await.expect("join").expect("run completes");

    // Backlog then live = every seq exactly once, in order.
    let mut all = backlog_seqs.clone();
    all.extend(&live_seqs);
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "backlog {backlog_seqs:?} + live {live_seqs:?} must be gapless and duplicate-free"
    );
    // And it matches what the store holds.
    let stored: Vec<u64> = service
        .events(&run_id)
        .expect("events")
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(all, stored);
}

#[tokio::test]
async fn list_runs_reports_live_runs_first_and_bounds_terminal_ones() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        (0..LISTED_TERMINAL_RUNS + 5)
            .map(|i| text_reply(&format!("answer {i}")))
            .collect(),
        forge_core::ApprovalPolicy::Auto,
    );
    let mut finished = Vec::new();
    for i in 0..LISTED_TERMINAL_RUNS + 5 {
        finished.push(service.run(&format!("ask {i}")).await.expect("run").run_id);
    }

    let listed = service.list_runs().expect("list");
    assert_eq!(
        listed.len(),
        LISTED_TERMINAL_RUNS,
        "terminal runs are bounded"
    );
    assert!(
        listed
            .iter()
            .all(|s| s.state == forge_core::RunState::Completed),
        "{listed:?}"
    );
    // The most recent ones survived.
    let newest = finished.last().expect("a run");
    assert!(listed.iter().any(|s| &s.run_id == newest), "{listed:?}");
    let oldest = finished.first().expect("a run");
    assert!(!listed.iter().any(|s| &s.run_id == oldest), "{listed:?}");
    // Shape.
    let summary = listed.first().expect("a summary");
    assert!(summary.session_id.is_some());
    assert!(summary.started_at.is_some());
    assert!(summary.last_seq > 0);
}

#[tokio::test]
async fn list_runs_never_hides_a_live_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, run_id, handle) = parked_service(tmp.path()).await;

    let listed = service.list_runs().expect("list");
    let parked = listed
        .iter()
        .find(|s| s.run_id == run_id)
        .unwrap_or_else(|| panic!("the parked run must be listed: {listed:?}"));
    assert_eq!(parked.state, forge_core::RunState::WaitingForApproval);

    service.send_input(&run_id, "n").expect("deny");
    handle.await.expect("join").expect("run completes");
    let listed = service.list_runs().expect("list again");
    assert_eq!(
        listed.iter().find(|s| s.run_id == run_id).map(|s| s.state),
        Some(forge_core::RunState::Completed)
    );
}

#[tokio::test]
async fn list_runs_ignores_a_fork_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("done")],
        forge_core::ApprovalPolicy::Auto,
    );
    let run = service.run("ask").await.expect("run");
    service.fork_session(&run.session_id, None).expect("fork");

    let listed = service.list_runs().expect("list");
    assert_eq!(
        listed.len(),
        2,
        "the original run and its copy in the fork — not the marker: {listed:?}"
    );
    assert!(listed.iter().all(|s| s.last_seq > 0), "{listed:?}");
}

// --- fork ---------------------------------------------------------------

/// Bytes of a session's log file, for "the source was not touched" checks.
fn session_bytes(service: &AgentService, session_id: &str) -> Vec<u8> {
    std::fs::read(
        service
            .sessions()
            .root()
            .join(format!("{session_id}.jsonl")),
    )
    .expect("session file")
}

#[tokio::test]
async fn fork_copies_the_whole_log_marks_provenance_and_leaves_the_source_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("the answer"), text_reply("forked answer")],
        forge_core::ApprovalPolicy::Auto,
    );
    let first = service.run("the ask").await.expect("run");
    let source_before = session_bytes(&service, &first.session_id);
    let source_events = service
        .sessions()
        .events_for(&first.session_id)
        .expect("read source");

    let fork = service
        .fork_session(&first.session_id, None)
        .expect("fork the whole log");

    assert_ne!(fork.session_id, first.session_id);
    assert_eq!(fork.source_session_id, first.session_id);
    assert_eq!(fork.events_copied, source_events.len());
    assert_eq!(fork.at_position, source_events.len() as u64);
    assert_eq!(fork.at_run_id, first.run_id);

    // The source is byte-identical.
    assert_eq!(
        session_bytes(&service, &first.session_id),
        source_before,
        "forking must never touch the source"
    );

    // The fork is the prefix plus one marker event.
    let forked = service
        .sessions()
        .events_for(&fork.session_id)
        .expect("read fork");
    assert_eq!(forked.len(), source_events.len() + 1);
    match &forked.last().expect("marker").kind {
        EventKind::SessionForked {
            from_session,
            at_position,
        } => {
            assert_eq!(from_session, &first.session_id);
            assert_eq!(*at_position, source_events.len() as u64);
        }
        other => panic!("expected a fork marker, got {other:?}"),
    }
    // Copied lines keep their original run id and seq.
    assert_eq!(
        forked[..source_events.len()]
            .iter()
            .map(|e| (e.run_id.as_str(), e.seq))
            .collect::<Vec<_>>(),
        source_events
            .iter()
            .map(|e| (e.run_id.as_str(), e.seq))
            .collect::<Vec<_>>()
    );
    // The marker gets its own run so it cannot disturb a copied run's seq.
    assert!(
        source_events
            .iter()
            .all(|e| e.run_id != forked.last().expect("marker").run_id)
    );
}

#[tokio::test]
async fn a_forked_session_is_resumable_and_continues_the_copied_history() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, model) = recording_service(
        tmp.path(),
        vec![text_reply("original answer"), text_reply("fork continues")],
    );
    let first = service.run("the shared ask").await.expect("run");
    let fork = service.fork_session(&first.session_id, None).expect("fork");

    let before = model.recorded().len();
    let resumed = service.resume(&fork.session_id).await.expect("resume fork");
    assert_eq!(resumed.session_id, fork.session_id);
    assert_eq!(resumed.text, "fork continues");

    let request = model
        .recorded()
        .into_iter()
        .nth(before)
        .expect("the fork's run called the model");
    let contents: Vec<&str> = request
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        contents,
        vec![
            "the shared ask",
            "original answer",
            "Continue the work in the conversation above.",
        ],
        "a fork replays the copied history, and the marker is not a turn"
    );

    // The source session gained nothing from the fork's run.
    let source = service
        .sessions()
        .events_for(&first.session_id)
        .expect("read source");
    assert!(source.iter().all(|e| e.run_id == first.run_id));
}

#[tokio::test]
async fn forking_at_a_mid_run_position_snaps_forward_to_the_run_boundary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("one"), text_reply("two")],
        forge_core::ApprovalPolicy::Auto,
    );
    let first = service.run("ask one").await.expect("run 1");
    let second = service.resume(&first.run_id).await.expect("run 2");
    let all = service
        .sessions()
        .events_for(&first.session_id)
        .expect("read");
    let first_run_len = all.iter().filter(|e| e.run_id == first.run_id).count();

    // Position 2 is inside run 1 → snap to the end of run 1.
    let fork = service
        .fork_session(&first.session_id, Some("2"))
        .expect("fork mid-run");
    assert_eq!(fork.at_run_id, first.run_id);
    assert_eq!(fork.events_copied, first_run_len);
    let forked = service
        .sessions()
        .events_for(&fork.session_id)
        .expect("read fork");
    assert!(
        forked
            .iter()
            .all(|e| e.run_id == first.run_id || matches!(e.kind, EventKind::SessionForked { .. })),
        "the second run must not be in the fork: {forked:?}"
    );

    // A run id cuts after that run, whichever position it occupies.
    let by_run = service
        .fork_session(&first.session_id, Some(&first.run_id))
        .expect("fork by run id");
    assert_eq!(by_run.events_copied, first_run_len);
    assert_eq!(by_run.at_run_id, first.run_id);
    // ...and the later run forks the whole log.
    let whole = service
        .fork_session(&first.session_id, Some(&second.run_id))
        .expect("fork by later run id");
    assert_eq!(whole.events_copied, all.len());
}

#[tokio::test]
async fn fork_rejects_unknown_sessions_runs_and_positions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = test_service(tmp.path());
    let run = service.run("ask").await.expect("run");
    let events = service
        .sessions()
        .events_for(&run.session_id)
        .expect("read")
        .len();

    for (at, needle) in [
        (Some("0"), "1-based"),
        (Some(&*format!("{}", events + 1)), "no position"),
        (Some("no-such-run"), "has no run"),
    ] {
        let err = service
            .fork_session(&run.session_id, at)
            .expect_err("must reject");
        assert!(matches!(err, ForgeError::Session(_)), "got: {err}");
        assert!(err.to_string().contains(needle), "got: {err}");
    }

    let err = service
        .fork_session("no-such-session", None)
        .expect_err("unknown session");
    assert!(err.to_string().contains("unknown or empty"), "got: {err}");
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
            "assistant_message",     // v3: the call the brain made
            "tool_call_requested",
            "tool_started",
            "tool_completed",
            "tool_result", // v3: its output, for replay
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
        [
            "run_started",
            "routing_decision_made",
            "assistant_message",
            "completed"
        ]
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
