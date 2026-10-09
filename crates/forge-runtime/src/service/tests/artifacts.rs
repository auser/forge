use super::*;
use forge_context::{
    ArtifactQuery, ArtifactRead, ArtifactRef, ArtifactSource, ArtifactStore, MemoryArtifactStore,
    SanitizedArtifactSource, SanitizedOutput,
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
    fn put(
        &self,
        _: SanitizedArtifactSource,
        _: &SanitizedOutput,
    ) -> Result<ArtifactRef, ForgeError> {
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
async fn redactable_provider_call_id_never_enters_artifact_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let secret_id = "sk-abcdefghi123456";
    std::fs::write(tmp.path().join("large.txt"), "x".repeat(70_000)).unwrap();
    let mut reply = tool_reply("read_file", serde_json::json!({"path":"large.txt"}));
    reply.tool_calls[0].id = secret_id.into();
    let model = Arc::new(ScriptedMockModel::new(vec![reply, text_reply("done")]));
    let context_root = tmp.path().join(".forge/context");
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
    .with_artifact_store(Some(Arc::new(forge_context::FsArtifactStore::new(
        &context_root,
        forge_context::ArtifactLimits::default(),
    ))));
    let outcome = service.run("read large.txt").await.unwrap();
    let events = serde_json::to_string(&outcome.events).unwrap();
    assert!(!events.contains(secret_id));
    assert!(
        !outcome
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolOutputArtifact { .. }))
    );
    let output = outcome
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(output.contains("Complete sanitized output unavailable"));
    assert!(!output.contains("handle="));
    let requests = model.recorded();
    let provider_tool = requests[1]
        .messages
        .iter()
        .find(|message| message.role == forge_core::Role::Tool)
        .unwrap();
    assert_eq!(provider_tool.tool_call_id.as_deref(), Some(secret_id));
    assert_eq!(&provider_tool.content, output);

    fn inspect_files(path: &std::path::Path, secret: &[u8]) {
        if !path.exists() {
            return;
        }
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                inspect_files(&path, secret);
            } else {
                let bytes = std::fs::read(path).unwrap();
                assert!(!bytes.windows(secret.len()).any(|window| window == secret));
            }
        }
    }
    inspect_files(&context_root, secret_id.as_bytes());
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
    let (_, grant, _) = service.prepare_tool_output(&call, &"z".repeat(70_000), source());
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
        let (output, grant, decision) = service.prepare_tool_output(&call, &raw, source());
        assert!(decision.is_none());
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
    output: String,
}

#[tokio::test]
async fn authorized_retrieval_runs_through_model_loop_without_creating_an_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryArtifactStore::default());
    let reference = store
        .put(
            SanitizedArtifactSource::new(source(), &forge_session::Redactor::new()).unwrap(),
            &SanitizedOutput::new(
                &forge_session::Redactor::new(),
                &format!("{}Bearer ", "x".repeat(70_000)),
            ),
        )
        .unwrap();
    let model = Arc::new(ScriptedMockModel::new(vec![
        tool_reply(
            "retrieve_tool_output",
            serde_json::json!({
                "handle": reference.handle, "start": 70_000, "end": 70_007
            }),
        ),
        tool_reply(
            "retrieve_tool_output",
            serde_json::json!({
                "handle": reference.handle, "start": 0, "end": 16_384
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
    assert_eq!(outcome.tool_calls, 2);
    assert!(
        !outcome
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolOutputArtifact { .. }))
    );
    let persisted = outcome
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult {
                output,
                is_error: false,
                ..
            } => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(persisted.len() <= 16 * 1024);
    let read: ArtifactRead = serde_json::from_str(persisted).unwrap();
    assert_eq!(read.text, "Bearer ");
    assert_eq!(read.start, 70_000);
    assert_eq!(read.end, 70_007);
    assert_eq!(read.total_bytes, 70_007);
    assert_eq!(read.next_start, None);
    assert!(
        model.recorded()[1]
            .messages
            .iter()
            .any(|message| message.role == forge_core::Role::Tool && &message.content == persisted)
    );
    let requests = model.recorded();
    let provided: Vec<_> = requests[2]
        .messages
        .iter()
        .filter(|message| message.role == forge_core::Role::Tool)
        .collect();
    let logged: Vec<_> = outcome
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(provided.len(), 2);
    for (message, output) in provided.iter().zip(logged) {
        assert_eq!(&message.content, output);
        assert!(output.len() <= 16 * 1024);
        let _: ArtifactRead = serde_json::from_str(output).unwrap();
    }
    let window: ArtifactRead = serde_json::from_str(&provided[1].content).unwrap();
    assert_eq!(window.start, 0);
    assert_eq!(window.end, window.text.len());
    assert_eq!(window.next_start, Some(window.end));
    assert_eq!(window.total_bytes, 70_007);
}

const SECRET_OUTPUT: &str = "private output sk-abcdefghijklmnop";

/// Scripted protocol, not a model-quality test: discovers the opaque handle
/// from the compressed view and explicitly requests an omitted byte window.
struct RetrieveCompressedLog {
    expected_window: String,
}

#[async_trait::async_trait]
impl ModelProvider for RetrieveCompressedLog {
    fn name(&self) -> &str {
        "scripted-retrieval"
    }
    fn capabilities(&self) -> forge_core::ModelCapabilities {
        forge_core::ModelCapabilities {
            tools: true,
            max_context: 100_000,
            ..Default::default()
        }
    }
    async fn complete(
        &self,
        request: forge_core::CompletionRequest,
    ) -> Result<forge_core::CompletionResponse, ForgeError> {
        let tools: Vec<_> = request
            .messages
            .iter()
            .filter(|m| m.role == forge_core::Role::Tool)
            .collect();
        let reply = match tools.len() {
            0 => tool_reply(
                "run_command",
                serde_json::json!({"command":"cargo","args":["test"]}),
            ),
            1 => {
                assert!(tools[0].content.len() < 10_000);
                let handle = tools[0]
                    .content
                    .split("handle=")
                    .nth(1)
                    .unwrap()
                    .split(|c: char| c.is_whitespace() || c == ']')
                    .next()
                    .unwrap();
                tool_reply(
                    "retrieve_tool_output",
                    serde_json::json!({
                        "handle":handle,"start":30000,"end":31024
                    }),
                )
            }
            2 => {
                let read: ArtifactRead = serde_json::from_str(&tools[1].content).unwrap();
                assert_eq!(read.text, self.expected_window);
                assert_eq!(read.start, 30000);
                assert_eq!(read.end, 31024);
                text_reply("verified the omitted original window")
            }
            _ => panic!("unexpected extra model turn"),
        };
        ScriptedMockModel::new(vec![reply]).complete(request).await
    }
}

#[tokio::test]
async fn compressed_log_omission_is_retrieved_through_the_scripted_model_loop() {
    let tmp = tempfile::tempdir().unwrap();
    // Execution's formatter supplies the stdout/stderr envelope.
    let stdout = format!(
        "running 1 test\ntest validation::exceptional ... FAILED\n\nwarning: W0042 deprecated fixture mode\n{}error: E0308 mismatched types\n",
        "warning: W0042 deprecated fixture mode\n".repeat(2400)
    );
    let raw = format!("exit 1\nstdout:\n{stdout}\nstderr:\n");
    // Even a known compressible result must use the old unavailable fallback
    // if storing its complete sanitized source fails.
    let failed = test_service(tmp.path()).with_artifact_store(Some(Arc::new(FailingArtifacts)));
    let call = ToolCall::new(
        "call",
        "run_command",
        serde_json::json!({"command":"cargo","args":["test"]}),
    );
    let (fallback, grant, decision) = failed.prepare_tool_output(&call, &raw, source());
    assert!(grant.is_none() && decision.is_none());
    assert_eq!(
        fallback,
        format!(
            "{}\n[Complete sanitized output unavailable]\n",
            forge_core::cap_tool_output(&raw)
        )
    );
    let model = Arc::new(RetrieveCompressedLog {
        expected_window: raw[30000..31024].to_owned(),
    });
    let service = AgentService::new(
        model,
        Arc::new(MockRouter::selecting("scripted-retrieval")),
        Arc::new(SecretExecution {
            fail: true,
            approve: false,
            output: stdout,
        }),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
        Config::default(),
    )
    .with_artifact_store(Some(Arc::new(MemoryArtifactStore::default())));
    let outcome = service
        .run("test and inspect the original repeated warning window")
        .await
        .unwrap();
    assert_eq!(outcome.text, "verified the omitted original window");
    assert_eq!(outcome.tool_calls, 2);
    assert_eq!(
        outcome
            .events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolOutputCompression { .. }))
            .count(),
        1
    );
    assert!(outcome.events.iter().any(|e| matches!(
        e.kind,
        EventKind::ToolOutputCompression {
            reason: forge_core::events::ToolCompressionReason::Compressed,
            ..
        }
    )));
}

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
            stdout: self.output.clone(),
            stderr: String::new(),
        })
    }
    async fn file_op(&self, _: forge_core::FileOp) -> Result<forge_core::FileOpResult, ForgeError> {
        if self.fail {
            return Err(ForgeError::execution(self.output.clone()));
        }
        Ok(forge_core::FileOpResult {
            content: Some(self.output.clone()),
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
async fn all_tool_paths_use_the_persisted_sanitized_output() {
    for (needle, fail, approve, boundary, store_mode) in [
        (false, false, false),
        (false, true, false),
        (false, false, true),
        (false, true, true),
        (true, false, false),
        (true, true, false),
    ]
    .into_iter()
    .flat_map(|(needle, fail, approve)| {
        [false, true].map(move |boundary| (needle, fail, approve, boundary))
    })
    .flat_map(|(needle, fail, approve, boundary)| {
        [0, 1, 2].map(move |store_mode| (needle, fail, approve, boundary, store_mode))
    }) {
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
        let prefix_len = if approve {
            format!("exit {}\nstdout:\n", i32::from(fail)).len()
        } else if fail {
            "execution error: ".len()
        } else {
            0
        };
        let output = if boundary {
            format!("{}BearerXYZ", "x".repeat(65530 - prefix_len))
        } else {
            SECRET_OUTPUT.into()
        };
        let execution = Arc::new(SecretExecution {
            fail,
            approve,
            output,
        });
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
        let store: Option<Arc<dyn ArtifactStore>> = match store_mode {
            0 => None,
            1 => Some(Arc::new(MemoryArtifactStore::default())),
            _ => Some(Arc::new(FailingArtifacts)),
        };
        let service = service.with_artifact_store(store);
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
            assert!(outcome.events.iter().any(|event| matches!(
                &event.kind, EventKind::ToolResult { output, .. } if output == &outcome.text
            )));
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
