use super::*;
use forge_context::{ArtifactSource, MemoryArtifactStore};
use forge_core::events::{ToolCompressionKind, ToolCompressionReason};

fn source() -> ArtifactSource {
    ArtifactSource {
        session_id: "session".into(),
        run_id: "run".into(),
        call_id: "call".into(),
        event_seq: 1,
    }
}

fn search_call() -> ToolCall {
    ToolCall::new(
        "call",
        "graph_grep",
        serde_json::json!({"pattern":"fixture"}),
    )
}

fn grant_handle(grant: Option<EventKind>) -> String {
    match grant.unwrap() {
        EventKind::ToolOutputArtifact { handle, .. } => handle,
        _ => unreachable!(),
    }
}

fn matches() -> Vec<forge_core::GrepMatch> {
    (1..=800)
        .map(|line| forge_core::GrepMatch {
            file: format!("src/{}/fixture.rs", "nested/".repeat(20)).into(),
            line,
            text: format!("fixture_{line} sk-abcdefghijklmnop"),
        })
        .collect()
}

fn search_output() -> String {
    matches()
        .iter()
        .map(|m| format!("{}:{}: {}", m.file.display(), m.line, m.text))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn compression_requires_storage_and_disabled_is_exact_context2() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryArtifactStore::default());
    let call = search_call();
    let raw = search_output();
    let service = test_service(tmp.path()).with_artifact_store(Some(store.clone()));
    let (compressed, grant, decision) = service.prepare_tool_output(&call, &raw, source());
    let handle = match grant.unwrap() {
        EventKind::ToolOutputArtifact { handle, .. } => handle,
        _ => unreachable!(),
    };
    assert!(compressed.contains(&handle));
    assert!(!compressed.contains("sk-abcdefghijklmnop"));
    let EventKind::ToolOutputCompression {
        kind,
        reason,
        baseline,
        view,
        event_seq,
        ..
    } = decision.unwrap()
    else {
        panic!("missing decision");
    };
    assert_eq!(kind, ToolCompressionKind::Search);
    assert_eq!(reason, ToolCompressionReason::Compressed);
    assert_eq!(event_seq, 1);
    assert!(view.estimated_tokens * 100 <= baseline.estimated_tokens * 70);
    let service = service.with_compression_enabled(false);
    let (disabled, grant, decision) = service.prepare_tool_output(&call, &raw, source());
    let handle = grant_handle(grant);
    assert!(decision.is_none());
    let sanitized = forge_context::SanitizedOutput::new(service.sessions.redactor(), &raw);
    assert_eq!(
        disabled,
        format!(
            "{}\n[Complete sanitized output: retrieve_tool_output handle={}]\n",
            forge_core::cap_tool_output(sanitized.as_str()),
            handle
        )
    );
    assert!(compressed.len() < disabled.len());

    let service = service.with_compression_enabled(true);
    let mut invalid = source();
    invalid.call_id = "sk-abcdefghijklmnop".into();
    let (text, grant, decision) = service.prepare_tool_output(&call, &raw, invalid);
    assert!(grant.is_none() && decision.is_none());
    assert!(text.ends_with("[Complete sanitized output unavailable]\n"));
    let service = service.with_artifact_store(None);
    let (_, grant, decision) = service.prepare_tool_output(&call, &raw, source());
    assert!(grant.is_none() && decision.is_none());
}

#[test]
fn unsupported_malformed_protected_and_no_savings_preserve_exact_old_view() {
    let tmp = tempfile::tempdir().unwrap();
    let raw = search_output();
    let mut unique = String::new();
    for line in 1..=800 {
        unique.push_str(&format!("a.rs:{line}: {}\n", "payload".repeat(20)));
    }
    for (call, text) in [
        (
            ToolCall::new("call", "read_file", serde_json::json!({"path":"unknown"})),
            raw.clone(),
        ),
        (
            ToolCall::new(
                "call",
                "run_command",
                serde_json::json!({"command":"git","args":["status"]}),
            ),
            raw.clone(),
        ),
        (search_call(), format!("malformed\n{raw}")),
        (search_call(), unique),
    ] {
        let store = Arc::new(MemoryArtifactStore::default());
        let service = test_service(tmp.path()).with_artifact_store(Some(store));
        let (enabled, grant, decision) = service.prepare_tool_output(&call, &text, source());
        let enabled_handle = grant_handle(grant);
        assert!(matches!(
            decision,
            Some(EventKind::ToolOutputCompression {
                reason: ToolCompressionReason::Unsupported | ToolCompressionReason::NoSavings,
                ..
            })
        ));
        let (disabled, grant, _) =
            service
                .with_compression_enabled(false)
                .prepare_tool_output(&call, &text, source());
        let disabled_handle = grant_handle(grant);
        // Storage creates a fresh grant; only its opaque value differs.
        assert_eq!(
            enabled.replace(&enabled_handle, "<handle>"),
            disabled.replace(&disabled_handle, "<handle>")
        );
    }
}

#[test]
fn threshold_and_retrieval_never_compress() {
    let tmp = tempfile::tempdir().unwrap();
    let service = test_service(tmp.path())
        .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
    for len in [0, 8192, 65536] {
        let raw = "x".repeat(len);
        let (text, grant, decision) = service.prepare_tool_output(&search_call(), &raw, source());
        assert_eq!(text, raw);
        assert!(grant.is_none() && decision.is_none());
    }
    let call = ToolCall::new("call", "retrieve_tool_output", serde_json::json!({}));
    let (_, grant, decision) = service.prepare_tool_output(&call, &search_output(), source());
    assert!(grant.is_none() && decision.is_none());
}

struct FixtureGraph;
impl ProjectGraph for FixtureGraph {
    fn build(&mut self) -> Result<forge_core::GraphStats, ForgeError> {
        unreachable!("prebuilt fixture")
    }
    fn is_fresh(&self) -> bool {
        true
    }
    fn files(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn symbols(&self) -> Vec<forge_core::SymbolInfo> {
        Vec::new()
    }
    fn grep(&self, _: &str) -> Result<Vec<forge_core::GrepMatch>, ForgeError> {
        Ok(matches())
    }
}

#[tokio::test]
async fn normal_and_needle_compression_keep_persisted_provider_replay_parity() {
    for needle in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedMockModel::new(vec![
            tool_reply("graph_grep", serde_json::json!({"pattern":"fixture"})),
            text_reply("done"),
        ]));
        let execution = Arc::new(MockExecution::new(tmp.path()));
        let service = if needle {
            needle_service(tmp.path(), model.clone(), execution)
        } else {
            AgentService::new(
                model.clone(),
                Arc::new(MockRouter::selecting("scripted-mock")),
                execution,
                Arc::new(NullSkillRegistry),
                Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
                Config::default(),
            )
        }
        .with_graph(Some(Arc::new(FixtureGraph)))
        .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
        let outcome = service
            .run(if needle {
                "graph_grep: {\"pattern\":\"fixture\"}"
            } else {
                "search fixture"
            })
            .await
            .unwrap();
        let events = service.sessions.events_for(&outcome.session_id).unwrap();
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            serde_json::to_value(&outcome.events).unwrap()
        );
        let persisted = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ToolResult { output, .. } => Some(output),
                _ => None,
            })
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            e.kind,
            EventKind::ToolOutputCompression {
                reason: ToolCompressionReason::Compressed,
                ..
            }
        )));
        let replay = crate::replay::conversation_from_events(&events);
        assert!(
            replay
                .messages
                .iter()
                .any(|m| m.role == forge_core::Role::Tool && &m.content == persisted)
        );
        if needle {
            assert!(model.recorded().is_empty());
            assert_eq!(&outcome.text, persisted);
        } else {
            assert!(
                model.recorded()[1]
                    .messages
                    .iter()
                    .any(|m| m.role == forge_core::Role::Tool && &m.content == persisted)
            );
        }
        let decisions: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolOutputCompression { .. }))
            .collect();
        let json = serde_json::to_string(&decisions).unwrap();
        assert!(json.len() < 1024);
        assert!(!json.contains("fixture"));
        assert!(!json.contains("sk-abcdefghijklmnop"));
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("sk-abcdefghijklmnop")
        );
    }
}
