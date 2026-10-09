use super::*;
use forge_context::{
    ArtifactQuery, ArtifactRead, ArtifactRef, ArtifactSource, ArtifactStore, MemoryArtifactStore,
    SanitizedOutput,
};

fn retrieval(handle: &str, start: usize, end: usize) -> ToolCall {
    ToolCall::new(
        "retrieve",
        "retrieve_tool_output",
        serde_json::json!({
            "handle": handle, "start": start, "end": end
        }),
    )
}

fn source() -> ArtifactSource {
    ArtifactSource {
        session_id: "source".into(),
        run_id: "run".into(),
        call_id: "call".into(),
        event_seq: 1,
    }
}

struct FailingArtifacts;
impl ArtifactStore for FailingArtifacts {
    fn put(&self, _: ArtifactSource, _: &SanitizedOutput) -> Result<ArtifactRef, ForgeError> {
        Err(ForgeError::execution(
            "PRIVATE_STORAGE_FAILURE sk-abcdefghijk",
        ))
    }
    fn retrieve(
        &self,
        _: &str,
        _: &[ArtifactRef],
        _: ArtifactQuery,
    ) -> Result<Option<ArtifactRead>, ForgeError> {
        Err(ForgeError::execution("PRIVATE_STORAGE_FAILURE"))
    }
}

#[tokio::test]
async fn sanitized_artifact_tail_and_provider_replay_are_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let raw = format!("sk-abcdefghijklmnop\n{}OMITTED_TAIL", "x".repeat(70_000));
    std::fs::write(tmp.path().join("large.txt"), &raw).unwrap();
    let model = Arc::new(ScriptedMockModel::new(vec![
        tool_reply("read_file", serde_json::json!({"path":"large.txt"})),
        text_reply("done"),
    ]));
    let store = Arc::new(MemoryArtifactStore::default());
    let service = AgentService::new(
        model.clone(),
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(NativeExecution::new(
            forge_core::ApprovalPolicy::Auto,
            tmp.path(),
        )),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
        Config::default(),
    )
    .with_artifact_store(Some(store));
    let outcome = service.run("read large.txt").await.unwrap();
    let (handle, seq) = outcome
        .events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolOutputArtifact {
                handle,
                event_seq,
                source_run_id,
                source_session_id,
                call_id,
            } => {
                assert_eq!(source_run_id, &outcome.run_id);
                assert_eq!(source_session_id, &outcome.session_id);
                assert_eq!(call_id, "call_1");
                Some((handle.clone(), *event_seq))
            }
            _ => None,
        })
        .unwrap();
    assert!(
        outcome
            .events
            .iter()
            .any(|e| e.seq == seq && matches!(e.kind, EventKind::ToolCallRequested { .. }))
    );
    let requests = model.recorded();
    for request in &requests {
        assert!(
            !serde_json::to_string(request)
                .unwrap()
                .contains("sk-abcdefghijklmnop")
        );
    }
    let provided = requests[1]
        .messages
        .iter()
        .find(|m| m.role == forge_core::Role::Tool)
        .unwrap();
    assert!(!provided.content.contains("OMITTED_TAIL"));
    assert!(provided.content.contains(&handle));
    let replay = crate::replay::conversation_from_events(&outcome.events);
    assert_eq!(
        replay
            .messages
            .iter()
            .find(|m| m.role == forge_core::Role::Tool)
            .unwrap()
            .content,
        provided.content
    );
    let sanitized = SanitizedOutput::new(&forge_session::Redactor::new(), &raw);
    let start = sanitized.as_str().len() - "OMITTED_TAIL".len();
    let read =
        service.retrieve_tool_output(&outcome.session_id, &retrieval(&handle, start, start + 12));
    assert!(!read.result.is_error, "{}", read.result.content);
    assert!(read.result.content.contains("OMITTED_TAIL"));
    assert!(
        service
            .retrieve_tool_output("unrelated", &retrieval(&handle, 0, 10))
            .result
            .is_error
    );
    assert!(
        service
            .retrieve_tool_output(&outcome.session_id, &retrieval(&"a".repeat(64), 0, 10))
            .result
            .is_error
    );
    let search = ToolCall::new(
        "search",
        "retrieve_tool_output",
        serde_json::json!({
            "handle": handle, "query": "OMITTED_TAIL", "limit": 100
        }),
    );
    let searched = service.retrieve_tool_output(&outcome.session_id, &search);
    assert!(!searched.result.is_error);
    assert!(searched.result.content.contains("OMITTED_TAIL"));
    let window = service.retrieve_tool_output(&outcome.session_id, &retrieval(&handle, 0, 16384));
    assert!(!window.result.is_error);
    assert!(window.result.content.len() <= 16384);
    let window: serde_json::Value = serde_json::from_str(&window.result.content).unwrap();
    assert!(window["next_start"].as_u64().is_some());
    // Losing the payload while keeping its visible grant must not authorize
    // content from another session or turn an absent object into a success.
    let service = service.with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
    let missing = service.retrieve_tool_output(&outcome.session_id, &retrieval(&handle, 0, 10));
    assert!(missing.result.is_error);
    assert_eq!(missing.result.content, "artifact unavailable");
}

#[test]
fn exact_nested_fork_prefix_grants_not_source_anchor_or_handle_text() {
    let tmp = tempfile::tempdir().unwrap();
    let service = test_service(tmp.path())
        .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
    let call = ToolCall::new("call", "read_file", serde_json::json!({"path":"large"}));
    let (_, grant) = service.prepare_tool_output(&call, &"z".repeat(70_000), source());
    let grant = grant.unwrap();
    let handle = match &grant {
        EventKind::ToolOutputArtifact { handle, .. } => handle.clone(),
        _ => unreachable!(),
    };
    service
        .sessions
        .append(Event::new(
            "run",
            "source",
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: "{}".into(),
            },
        ))
        .unwrap();
    // Another run ends while the artifact-producing run is still in progress.
    service
        .sessions
        .append(Event::new(
            "cut",
            "source",
            EventKind::Completed {
                summary: handle.clone(),
            },
        ))
        .unwrap();
    service
        .sessions
        .append(Event::new("run", "source", grant))
        .unwrap();
    let before = service.fork_session("source", Some("cut")).unwrap();
    let after = service.fork_session("source", None).unwrap();
    let nested_before = service.fork_session(&before.session_id, None).unwrap();
    let nested_after = service.fork_session(&after.session_id, None).unwrap();
    let request = retrieval(&handle, 0, 10);
    for denied in [&before.session_id, &nested_before.session_id] {
        assert!(
            service
                .retrieve_tool_output(denied, &request)
                .result
                .is_error
        );
    }
    for allowed in ["source", &after.session_id, &nested_after.session_id] {
        assert!(
            !service
                .retrieve_tool_output(allowed, &request)
                .result
                .is_error
        );
    }
}

#[test]
fn disabled_and_failed_storage_still_sanitize_and_never_advertise_a_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let call = ToolCall::new("call", "read_file", serde_json::json!({}));
    for store in [
        None,
        Some(Arc::new(FailingArtifacts) as Arc<dyn ArtifactStore>),
    ] {
        let service = test_service(tmp.path()).with_artifact_store(store);
        let raw = format!("{}sk-abcdefghijklmnop", "x".repeat(65_530));
        let (output, grant) = service.prepare_tool_output(&call, &raw, source());
        assert!(grant.is_none());
        assert!(output.contains("unavailable"));
        assert!(!output.contains("handle="));
        assert!(!output.contains("sk-"));
        assert!(!output.contains("PRIVATE_STORAGE_FAILURE"));
    }
}

#[test]
fn retrieval_requests_are_bounded_and_excluded_from_needle() {
    for args in [
        serde_json::json!({"handle":"h"}),
        serde_json::json!({"handle":"h","start":0,"end":16385}),
        serde_json::json!({"handle":"h","start":3,"end":3}),
        serde_json::json!({"handle":"h","start":-1,"end":3}),
        serde_json::json!({"handle":"h","query":"","limit":3}),
        serde_json::json!({"handle":"h","query":"x","limit":0}),
        serde_json::json!({"handle":"h","query":"x","limit":1,"end":3}),
        serde_json::json!({"handle":"h","limit":0}),
        serde_json::json!({"handle":"h","limit":16385}),
    ] {
        assert!(crate::tools::artifact_query(&args).is_err(), "{args}");
    }
    assert!(
        crate::tools::artifact_query(&serde_json::json!({"handle":"h","query":"x","limit":5}))
            .is_ok()
    );
    assert!(matches!(
        crate::tools::artifact_query(&serde_json::json!({"handle":"h","query":"x"})),
        Ok((_, forge_context::ArtifactQuery::Search { limit: 16384, .. }))
    ));
    assert_eq!(minimum_dispatch_risk(&retrieval("h", 0, 10)), None);
    assert!(crate::tools::artifact_query(&serde_json::json!({"handle":"h","limit":5})).is_ok());
}

#[tokio::test]
async fn retrieval_uses_normal_quota_policy_and_never_recursively_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedMockModel::new(vec![
        tool_reply(
            "retrieve_tool_output",
            serde_json::json!({"handle":"missing","start":0,"end":10}),
        ),
        text_reply("done"),
    ]));
    let mut config = Config::default();
    config.tool_limits.insert(
        "retrieve_tool_output".into(),
        forge_config::ToolLimitConfig { per_run: 0 },
    );
    let service = AgentService::new(
        model.clone(),
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
        config,
    )
    .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
    let outcome = service.run("retrieve").await.unwrap();
    assert!(outcome.events.iter().any(|e| matches!(&e.kind, EventKind::ToolPolicyDecision {tool, disposition: forge_core::ToolPolicyDisposition::Block, ..} if tool == "retrieve_tool_output")));
    assert!(outcome.events.iter().any(|e| matches!(&e.kind, EventKind::ToolResult {output, ..} if output.contains("quota exceeded"))));
    assert!(
        !outcome
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolOutputArtifact { .. }))
    );
    assert!(
        model.recorded()[0]
            .tools
            .iter()
            .any(|t| t.name == "retrieve_tool_output")
    );
}

struct SecretExecution {
    fail: bool,
    approve: bool,
}

#[tokio::test]
async fn authorized_retrieval_runs_through_model_loop_without_creating_an_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryArtifactStore::default());
    let reference = store
        .put(
            source(),
            &SanitizedOutput::new(
                &forge_session::Redactor::new(),
                &format!("{}TAIL", "x".repeat(70_000)),
            ),
        )
        .unwrap();
    let model = Arc::new(ScriptedMockModel::new(vec![
        tool_reply(
            "retrieve_tool_output",
            serde_json::json!({
                "handle": reference.handle, "start": 70_000, "end": 70_004
            }),
        ),
        text_reply("retrieved"),
    ]));
    let service = AgentService::new(
        model.clone(),
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
        Config::default(),
    )
    .with_artifact_store(Some(store));
    service
        .sessions
        .append(Event::new(
            "run",
            "source",
            EventKind::ToolOutputArtifact {
                handle: reference.handle,
                source_session_id: "source".into(),
                source_run_id: "run".into(),
                call_id: "call".into(),
                event_seq: 1,
            },
        ))
        .unwrap();
    let outcome = service
        .run_with_options(
            "retrieve the tail",
            RunOptions {
                session_id: Some("source".into()),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.tool_calls, 1);
    assert!(
        !outcome
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolOutputArtifact { .. }))
    );
    assert!(outcome.events.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, is_error: false, .. } if output.contains("TAIL"))));
    assert!(
        model.recorded()[1]
            .messages
            .iter()
            .any(|m| m.role == forge_core::Role::Tool && m.content.contains("TAIL"))
    );
}

const SECRET_OUTPUT: &str = "private output sk-abcdefghijklmnop";

#[async_trait::async_trait]
impl ExecutionProvider for SecretExecution {
    fn name(&self) -> &str {
        "secret-fixture"
    }
    fn approval_policy(&self) -> forge_core::ApprovalPolicy {
        if self.approve {
            forge_core::ApprovalPolicy::Prompt
        } else {
            forge_core::ApprovalPolicy::Auto
        }
    }
    fn file_op_risk(&self, _: &forge_core::FileOp) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        _: forge_core::ExecRequest,
    ) -> Result<forge_core::ExecResult, ForgeError> {
        if self.approve {
            return Err(ForgeError::ApprovalRequired {
                description: "fixture".into(),
                risk: RiskLevel::Risky,
            });
        }
        self.execute_approved(forge_core::ExecRequest::new("fixture", RiskLevel::Risky))
            .await
    }
    async fn execute_approved(
        &self,
        _: forge_core::ExecRequest,
    ) -> Result<forge_core::ExecResult, ForgeError> {
        Ok(forge_core::ExecResult {
            exit_code: i32::from(self.fail),
            stdout: SECRET_OUTPUT.into(),
            stderr: String::new(),
        })
    }
    async fn file_op(&self, _: forge_core::FileOp) -> Result<forge_core::FileOpResult, ForgeError> {
        if self.fail {
            return Err(ForgeError::execution(SECRET_OUTPUT));
        }
        Ok(forge_core::FileOpResult {
            content: Some(SECRET_OUTPUT.into()),
            changed: false,
        })
    }
    async fn spawn(
        &self,
        _: forge_core::ExecRequest,
    ) -> Result<Box<dyn forge_core::RunningProcess>, ForgeError> {
        Err(ForgeError::execution("unused"))
    }
}

#[tokio::test]
async fn all_tool_paths_redact_provider_requests_without_artifact_storage() {
    for (needle, fail, approve) in [
        (false, false, false),
        (false, true, false),
        (false, false, true),
        (false, true, true),
        (true, false, false),
        (true, true, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let tool = if approve { "run_command" } else { "read_file" };
        let args = if approve {
            serde_json::json!({"command":"fixture"})
        } else {
            serde_json::json!({"path":"fixture"})
        };
        let replies = if needle {
            vec![text_reply("recovered")]
        } else {
            vec![tool_reply(tool, args), text_reply("done")]
        };
        let model = Arc::new(ScriptedMockModel::new(replies));
        let execution = Arc::new(SecretExecution { fail, approve });
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
        };
        let run_id = forge_session::new_run_id();
        if approve {
            service.send_input(&run_id, "y").unwrap();
        }
        let outcome = service
            .run_with_options(
                if needle {
                    "read_file: {\"path\":\"fixture\"}"
                } else {
                    "read fixture"
                },
                RunOptions {
                    run_id: Some(run_id),
                    ..RunOptions::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !serde_json::to_string(&outcome.events)
                .unwrap()
                .contains("sk-abcdefghijklmnop")
        );
        assert!(!outcome.text.contains("sk-abcdefghijklmnop"));
        let requests = model.recorded();
        for request in &requests {
            assert!(
                !serde_json::to_string(request)
                    .unwrap()
                    .contains("sk-abcdefghijklmnop")
            );
        }
        if needle && !fail {
            assert!(requests.is_empty());
            assert!(outcome.text.contains("[REDACTED]"));
        } else {
            let logged = outcome
                .events
                .iter()
                .find_map(|e| match &e.kind {
                    EventKind::ToolResult { output, .. } => Some(output),
                    _ => None,
                })
                .unwrap();
            assert!(logged.contains("[REDACTED]"));
            assert!(
                requests
                    .last()
                    .unwrap()
                    .messages
                    .iter()
                    .any(|m| m.role == forge_core::Role::Tool && &m.content == logged)
            );
        }
        if approve {
            assert!(
                outcome
                    .events
                    .iter()
                    .any(|e| matches!(e.kind, EventKind::ApprovalDecided { .. }))
            );
        }
    }
}
