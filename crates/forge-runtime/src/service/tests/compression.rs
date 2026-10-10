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
fn direct_constructor_honors_config_and_builder_overrides_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.context_compression.enabled = false;
    let service = AgentService::new(
        Arc::new(ScriptedMockModel::new(vec![])),
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
        config,
    )
    .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
    let (_, _, decision) = service.prepare_tool_output(&search_call(), &search_output(), source());
    assert!(decision.is_none());
    let (_, _, decision) = service.with_compression_enabled(true).prepare_tool_output(
        &search_call(),
        &search_output(),
        source(),
    );
    assert!(matches!(
        decision,
        Some(EventKind::ToolOutputCompression {
            reason: ToolCompressionReason::Compressed,
            ..
        })
    ));
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
fn qualified_structured_output_uses_the_runtime_compression_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let repeated = r#"{"status":"ready","requirement":"authorization remains explicit","command":"cargo test --workspace --locked","error_code":"E0042"}"#;
    let exceptional = r#"{"status":"failed","requirement":"authorization remains explicit","command":"cargo test --workspace --locked","error_code":"E0308"}"#;
    let raw = format!("[{},{}]", vec![repeated; 800].join(","), exceptional);
    let call = ToolCall::new(
        "call",
        "read_file",
        serde_json::json!({"path":"validation.json"}),
    );
    let service = test_service(tmp.path())
        .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));

    let (view, grant, decision) = service.prepare_tool_output(&call, &raw, source());

    assert!(grant.is_some());
    assert!(view.contains("records 1..=800 count=800"));
    assert!(view.contains(exceptional));
    assert_eq!(view.matches(repeated).count(), 1);
    let EventKind::ToolOutputCompression {
        kind,
        reason,
        baseline,
        view: size,
        omitted,
        ..
    } = decision.unwrap()
    else {
        panic!("missing decision");
    };
    assert_eq!(kind, ToolCompressionKind::Json);
    assert_eq!(reason, ToolCompressionReason::Compressed);
    assert_eq!(omitted, 0, "record grouping retains every distinct record");
    assert!(size.estimated_tokens * 100 <= baseline.estimated_tokens * 70);
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

#[test]
fn framing_env_secrets_use_persisted_baseline_and_decline_changed_candidates() {
    const CHILD: &str = "FORGE_COMPRESSION_FRAMING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Snapshot environment secrets in a child, without mutating the parallel
        // test process's environment.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service::tests::compression::framing_env_secrets_use_persisted_baseline_and_decline_changed_candidates",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("FORGE_TEST_FRAMING_SECRET", "\"framing_secret\"")
            .env("FORGE_TEST_BASELINE_SECRET", "[forge: tool output truncated,")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let service = test_service(tmp.path())
        .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
    let raw = format!("{}\nlast.rs:1: framing_secret", search_output());
    for call in [
        search_call(),
        ToolCall::new("call", "unknown", serde_json::json!({})),
    ] {
        let (output, grant, decision) = service.prepare_tool_output(&call, &raw, source());
        let sanitized = forge_context::SanitizedOutput::new(service.sessions.redactor(), &raw);
        let old = format!(
            "{}\n[Complete sanitized output: retrieve_tool_output handle={}]\n",
            forge_core::cap_tool_output(sanitized.as_str()),
            grant_handle(grant)
        );
        assert_eq!(output, old);
        let persisted = service
            .sessions
            .redactor()
            .redact_tool_output(&call.name, &old);
        assert_ne!(persisted, old);
        let actual = forge_context::ContextSize::of_serialized(&persisted);
        let Some(EventKind::ToolOutputCompression {
            reason,
            baseline,
            view,
            omitted,
            ..
        }) = decision
        else {
            panic!("missing decision")
        };
        assert_eq!(reason, ToolCompressionReason::Unsupported);
        assert_eq!(omitted, 0);
        assert_eq!(baseline, view);
        assert_eq!(view.chars, actual.chars);
        assert_eq!(view.estimated_tokens, actual.estimated_tokens);
    }
}

struct FixtureGraph {
    trailing_bearer: bool,
}
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
        let mut matches = matches();
        if self.trailing_bearer {
            matches.last_mut().unwrap().text = "Bearer ".into();
        }
        Ok(matches)
    }
}

#[tokio::test]
async fn normal_and_needle_compression_keep_persisted_provider_replay_parity() {
    for (needle, trailing_bearer) in [(false, false), (true, false), (false, true), (true, true)] {
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
        .with_graph(Some(Arc::new(FixtureGraph { trailing_bearer })))
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
        let (reason, baseline, view) = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ToolOutputCompression {
                    reason,
                    baseline,
                    view,
                    ..
                } => Some((reason, baseline, view)),
                _ => None,
            })
            .unwrap();
        let actual = forge_context::ContextSize::of_serialized(persisted);
        assert_eq!(view.chars, actual.chars);
        assert_eq!(view.estimated_tokens, actual.estimated_tokens);
        if trailing_bearer {
            assert_eq!(*reason, ToolCompressionReason::Unsupported);
            assert_eq!(view, baseline);
            assert!(!persisted.contains("[forge compression"));
            let raw = FixtureGraph { trailing_bearer }
                .grep("fixture")
                .unwrap()
                .iter()
                .map(|m| format!("{}:{}: {}", m.file.display(), m.line, m.text))
                .collect::<Vec<_>>()
                .join("\n");
            let handle = events
                .iter()
                .find_map(|event| match &event.kind {
                    EventKind::ToolOutputArtifact { handle, .. } => Some(handle),
                    _ => None,
                })
                .unwrap();
            let sanitized = forge_context::SanitizedOutput::new(service.sessions.redactor(), &raw);
            let old = format!(
                "{}\n[Complete sanitized output: retrieve_tool_output handle={}]\n",
                forge_core::cap_tool_output(sanitized.as_str()),
                handle
            );
            assert_eq!(
                persisted,
                &service
                    .sessions
                    .redactor()
                    .redact_tool_output("graph_grep", &old)
            );
        } else {
            assert_eq!(*reason, ToolCompressionReason::Compressed);
            assert!(view.estimated_tokens * 100 <= baseline.estimated_tokens * 70);
        }
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
