use std::io::Write;
use std::sync::Mutex;

use forge_core::{CompletionRequest, ModelCapabilities, ModelProvider};
use tracing::instrument::WithSubscriber;

use super::*;

fn service(
    root: &std::path::Path,
    model: Arc<dyn ModelProvider>,
    store: Option<Arc<dyn ContextStore>>,
) -> AgentService {
    let name = model.name().to_string();
    AgentService::new(
        model,
        Arc::new(MockRouter::selecting(name)),
        Arc::new(NativeExecution::new(forge_core::ApprovalPolicy::Auto, root)),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(root.join("sessions"))),
        Config::default(),
    )
    .with_context_store(store)
    .with_system_context(vec![forge_core::Message::system("Stable guidance: café")])
}

fn request_bytes(requests: &[CompletionRequest]) -> Vec<Vec<u8>> {
    requests
        .iter()
        .map(|request| serde_json::to_vec(request).expect("serialize full request"))
        .collect()
}

fn assert_accounted(outcome: &RunOutcome, enabled: bool, count: usize) {
    assert_eq!(
        outcome
            .events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::ContextPlanRecorded { .. }))
            .count(),
        if enabled { count } else { 0 },
        "the enabled arm must actually perform accounting"
    );
}

#[tokio::test]
async fn accounting_preserves_no_tools_ordinary_request_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut captures = Vec::new();
    for enabled in [false, true] {
        let model = Arc::new(MockModel::new().with_capabilities(ModelCapabilities {
            streaming: false,
            tools: false,
            structured_output: false,
            vision: false,
            max_context: 32_768,
        }));
        let store =
            enabled.then(|| Arc::new(MemoryContextStore::default()) as Arc<dyn ContextStore>);
        let outcome = service(tmp.path(), model.clone(), store)
            .run("A question with Unicode: 日本語")
            .await
            .expect("ordinary run");
        assert_accounted(&outcome, enabled, 1);
        let requests = model.recorded();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tools.is_empty());
        assert!(
            !outcome
                .events
                .iter()
                .any(|event| matches!(event.kind, EventKind::AssistantDelta { .. }))
        );
        captures.push(request_bytes(&requests));
    }
    assert_eq!(captures[0], captures[1]);
}

#[tokio::test]
async fn accounting_preserves_streaming_request_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut captures = Vec::new();
    for enabled in [false, true] {
        let model = Arc::new(ScriptedMockModel::new(vec![text_reply("streamed answer")]));
        let store =
            enabled.then(|| Arc::new(MemoryContextStore::default()) as Arc<dyn ContextStore>);
        let outcome = service(tmp.path(), model.clone(), store)
            .run("A streaming question")
            .await
            .expect("streaming run");
        assert_eq!(outcome.text, "streamed answer");
        assert!(
            outcome
                .events
                .iter()
                .any(|event| matches!(event.kind, EventKind::AssistantDelta { .. }))
        );
        assert_accounted(&outcome, enabled, 1);
        let requests = model.recorded();
        assert_eq!(requests.len(), 1);
        captures.push(request_bytes(&requests));
    }
    assert_eq!(captures[0], captures[1]);
}

#[tokio::test]
async fn accounting_preserves_both_tool_loop_request_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("input.txt"), "tool content: café\n").unwrap();
    let mut captures = Vec::new();
    for enabled in [false, true] {
        let model = Arc::new(ScriptedMockModel::new(vec![
            tool_reply("read_file", serde_json::json!({"path": "input.txt"})),
            text_reply("done"),
        ]));
        let store =
            enabled.then(|| Arc::new(MemoryContextStore::default()) as Arc<dyn ContextStore>);
        let outcome = service(tmp.path(), model.clone(), store)
            .run("Read input.txt")
            .await
            .expect("tool loop");
        assert_eq!(outcome.text, "done");
        assert_accounted(&outcome, enabled, 2);
        let requests = model.recorded();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().any(|message| {
            message.role == forge_core::Role::Tool && message.content.contains("tool content: café")
        }));
        captures.push(request_bytes(&requests));
    }
    assert_eq!(captures[0], captures[1]);
}

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn context_store_warning_logs_category_not_private_error_and_run_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("still completed")]));
    let store = Arc::new(MemoryContextStore::failing(
        "failed to persist PRIVATE_TOOL_OUTPUT at /private/context.json",
    ));
    let logs = LogBuffer(Arc::new(Mutex::new(Vec::new())));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let outcome = service(tmp.path(), model.clone(), Some(store))
        .run("private prompt")
        .with_subscriber(subscriber)
        .await
        .expect("accounting failure must not abort completion");
    assert_eq!(outcome.text, "still completed");
    assert_eq!(model.recorded().len(), 1);
    assert!(outcome.events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ContextPlanUnavailable { error_category, message, .. }
            if error_category == "store" && message == "context accounting unavailable"
    )));
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("WARN"), "warning was not captured: {logs}");
    assert!(logs.contains("context accounting unavailable"), "{logs}");
    assert!(logs.contains("error_category=\"store\""), "{logs}");
    assert!(!logs.contains("PRIVATE_TOOL_OUTPUT"), "{logs}");
    assert!(!logs.contains("/private/context.json"), "{logs}");
}
