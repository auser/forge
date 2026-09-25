//! Dispatch is pure, so all of it is testable without a process, a
//! client, or a tokio runtime: feed in the JSON a client would send (or
//! the [`Event`]s a run would emit) and assert on the ACP values that come
//! back.

use std::path::PathBuf;

use forge_core::execution::RiskLevel;
use forge_core::{Event, EventKind};
use serde_json::json;

use super::*;
use crate::protocol::{
    ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind,
    RequestPermissionOutcome, SessionUpdate, StopReason, ToolCallStatus, ToolKind,
};

/// The run a turn under test belongs to. Tool-call ids are prefixed with it
/// (see [`TurnState::for_run`]), so tests that assert on ids spell it out.
const RUN: &str = "run-1";
/// An absolute project root, since `ToolCallLocation.path` must be absolute.
const ROOT: &str = "/projects/demo";

fn event(kind: EventKind) -> Event {
    Event::new(RUN, "session-1", kind)
}

// --- initialize ---------------------------------------------------------

#[test]
fn initialize_answers_protocol_v1_and_names_itself_forge() {
    let response = initialize(&InitializeRequest {
        protocol_version: 1,
        ..InitializeRequest::default()
    });

    assert_eq!(response.protocol_version, 1);
    assert_eq!(response.agent_info.name, "forge");
    assert_eq!(response.agent_info.version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn initialize_advertises_capabilities_honestly() {
    let response = initialize(&InitializeRequest::default());

    // v1 of this adapter has no session/load and takes text prompts only.
    assert!(!response.agent_capabilities.load_session);
    assert!(!response.agent_capabilities.prompt_capabilities.image);
    assert!(!response.agent_capabilities.prompt_capabilities.audio);
    assert!(
        !response
            .agent_capabilities
            .prompt_capabilities
            .embedded_context
    );
    assert!(response.auth_methods.is_empty());
}

#[test]
fn a_client_asking_for_a_newer_version_is_answered_with_the_one_we_speak() {
    // The spec's rule: echo the client's version when supported, else
    // reply with our latest and let the client decide to disconnect.
    let response = initialize(&InitializeRequest {
        protocol_version: 99,
        ..InitializeRequest::default()
    });
    assert_eq!(response.protocol_version, 1);
}

#[test]
fn the_initialize_result_serializes_with_the_schemas_field_names() {
    let value = serde_json::to_value(initialize(&InitializeRequest::default())).expect("serialize");
    assert_eq!(value["protocolVersion"], 1);
    assert_eq!(value["agentInfo"]["name"], "forge");
    assert_eq!(value["agentCapabilities"]["loadSession"], false);
    assert_eq!(
        value["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
}

// --- session/new --------------------------------------------------------

#[test]
fn session_new_accepts_an_existing_absolute_cwd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let request = NewSessionRequest {
        cwd: Some(tmp.path().to_path_buf()),
        mcp_servers: Vec::new(),
    };
    assert_eq!(session_root(&request).expect("root"), tmp.path());
}

#[test]
fn session_new_rejects_a_missing_cwd_field() {
    let error = session_root(&NewSessionRequest::default()).expect_err("cwd is required");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("cwd"), "{error}");
}

#[test]
fn session_new_rejects_a_relative_cwd() {
    let request = NewSessionRequest {
        cwd: Some("relative/path".into()),
        mcp_servers: Vec::new(),
    };
    let error = session_root(&request).expect_err("cwd must be absolute");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("absolute"), "{error}");
}

#[test]
fn session_new_rejects_a_cwd_that_does_not_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let request = NewSessionRequest {
        cwd: Some(tmp.path().join("nope")),
        mcp_servers: Vec::new(),
    };
    let error = session_root(&request).expect_err("cwd must exist");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("does not exist"), "{error}");
}

#[test]
fn session_new_rejects_a_cwd_that_is_a_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let file = tmp.path().join("a-file");
    std::fs::write(&file, "x").expect("write");
    let request = NewSessionRequest {
        cwd: Some(file),
        mcp_servers: Vec::new(),
    };
    let error = session_root(&request).expect_err("cwd must be a directory");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("directory"), "{error}");
}

// --- prompt content -----------------------------------------------------

#[test]
fn prompt_text_joins_text_blocks() {
    let blocks = vec![ContentBlock::text("fix the "), ContentBlock::text("parser")];
    assert_eq!(prompt_text(&blocks).expect("text"), "fix the \nparser");
}

#[test]
fn prompt_text_renders_a_resource_link_as_a_mention() {
    // Resource links are part of the baseline every agent must accept, so
    // they must reach the model as *something* rather than be dropped.
    let blocks = vec![
        ContentBlock::text("explain"),
        ContentBlock::ResourceLink {
            uri: "file:///p/src/lib.rs".into(),
            name: Some("src/lib.rs".into()),
        },
    ];
    let text = prompt_text(&blocks).expect("text");
    assert!(text.contains("explain"), "{text}");
    assert!(text.contains("src/lib.rs"), "{text}");
}

#[test]
fn prompt_text_falls_back_to_a_resource_links_uri_when_unnamed() {
    let blocks = vec![ContentBlock::ResourceLink {
        uri: "file:///p/x.rs".into(),
        name: None,
    }];
    assert!(
        prompt_text(&blocks)
            .expect("text")
            .contains("file:///p/x.rs")
    );
}

#[test]
fn prompt_text_rejects_an_empty_prompt() {
    let error = prompt_text(&[]).expect_err("empty prompt is invalid");
    assert_eq!(error.code, -32602);
}

#[test]
fn prompt_text_rejects_blocks_we_never_advertised_support_for() {
    let blocks = vec![ContentBlock::Image { data: None }];
    let error = prompt_text(&blocks).expect_err("image is not advertised");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("image"), "{error}");
}

#[test]
fn an_embedded_resource_is_degraded_to_its_text_rather_than_refused() {
    // We advertise embeddedContext: false, so strictly a client should not
    // send this — but an editor @-mention arriving as an embedded resource
    // must still get an answer, not -32602.
    let blocks = vec![
        ContentBlock::text("what does this do?"),
        ContentBlock::Resource {
            resource: json!({
                "uri": "file:///p/src/lib.rs",
                "text": "pub fn parse() {}",
                "mimeType": "text/x-rust",
            }),
        },
    ];
    let text = prompt_text(&blocks).expect("resource content should be usable");
    assert!(text.contains("what does this do?"), "{text}");
    assert!(text.contains("pub fn parse() {}"), "{text}");
    assert!(text.contains("file:///p/src/lib.rs"), "{text}");
}

#[test]
fn an_embedded_resource_without_text_degrades_to_its_uri() {
    // A blob we cannot read is exactly a link we can name.
    let blocks = vec![ContentBlock::Resource {
        resource: json!({ "uri": "file:///p/logo.png", "blob": "aGk=" }),
    }];
    assert_eq!(
        prompt_text(&blocks).expect("uri is still usable"),
        "file:///p/logo.png"
    );
}

#[test]
fn an_embedded_resource_with_nothing_usable_is_still_an_error() {
    let blocks = vec![ContentBlock::Resource {
        resource: json!({ "mimeType": "text/plain" }),
    }];
    let error = prompt_text(&blocks).expect_err("nothing to degrade to");
    assert_eq!(error.code, -32602);
}

#[test]
fn audio_stays_a_hard_error() {
    // Unlike a resource, there is no text in here to fall back to.
    let error = prompt_text(&[ContentBlock::Audio { data: None }]).expect_err("audio");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("audio"), "{error}");
}

#[test]
fn initialize_advertises_session_close() {
    // Advertising it is what makes a client send it, which is the only way a
    // long-lived process learns a conversation is over.
    let value = serde_json::to_value(initialize(&InitializeRequest::default())).expect("serialize");
    assert_eq!(
        value["agentCapabilities"]["sessionCapabilities"]["close"],
        json!({}),
        "{value}"
    );
}

// --- events → session/update -------------------------------------------

/// Collect the notifications a synthetic event stream produces.
fn updates(state: &mut TurnState, kinds: Vec<EventKind>) -> Vec<SessionUpdate> {
    let mut out = Vec::new();
    for kind in kinds {
        for action in state.on_event(&event(kind)) {
            match action {
                TurnAction::Notify(update) => out.push(update),
                TurnAction::AskPermission { .. } => {}
            }
        }
    }
    out
}

#[test]
fn a_routing_decision_becomes_one_thought_chunk() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![EventKind::RoutingDecisionMade {
            router: "needle".into(),
            selected_model: "mock-local".into(),
            confidence: 0.94,
            fallback_used: false,
            reason: "local is enough".into(),
        }],
    );

    assert_eq!(out.len(), 1, "{out:?}");
    let SessionUpdate::AgentThoughtChunk {
        content: ContentBlock::Text { text },
    } = &out[0]
    else {
        panic!("expected a thought chunk, got {out:?}");
    };
    assert!(text.contains("mock-local"), "{text}");
    assert!(text.contains("needle"), "{text}");
}

#[test]
fn an_activated_skill_becomes_a_thought_chunk() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![EventKind::SkillActivated {
            name: "reviewing".into(),
            path: "/p/.forge/skills/reviewing/SKILL.md".into(),
        }],
    );
    assert_eq!(out.len(), 1);
    let SessionUpdate::AgentThoughtChunk {
        content: ContentBlock::Text { text },
    } = &out[0]
    else {
        panic!("expected a thought chunk, got {out:?}");
    };
    assert!(text.contains("reviewing"), "{text}");
}

#[test]
fn a_tool_call_runs_through_pending_in_progress_and_completed() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::ToolCallRequested {
                tool: "write_file".into(),
                args_summary: r#"{"path":"notes.txt","content":"hi"}"#.into(),
            },
            EventKind::ToolStarted {
                name: "write_file".into(),
            },
            EventKind::ToolCompleted {
                name: "write_file".into(),
                success: true,
            },
        ],
    );

    assert_eq!(out.len(), 3, "{out:?}");

    let SessionUpdate::ToolCall(call) = &out[0] else {
        panic!("expected a tool_call first, got {out:?}");
    };
    assert_eq!(call.status, ToolCallStatus::Pending);
    assert_eq!(call.kind, ToolKind::Edit, "write_file edits files");
    assert_eq!(call.name.as_deref(), Some("write_file"));
    assert!(call.title.contains("notes.txt"), "title: {}", call.title);
    assert_eq!(
        call.locations.first().map(|l| l.path.clone()),
        Some(PathBuf::from("/projects/demo/notes.txt")),
        "the path argument should become an ABSOLUTE location for follow-along"
    );

    let SessionUpdate::ToolCallUpdate(started) = &out[1] else {
        panic!("expected a tool_call_update, got {out:?}");
    };
    assert_eq!(started.tool_call_id, call.tool_call_id, "same id");
    assert_eq!(started.status, Some(ToolCallStatus::InProgress));

    let SessionUpdate::ToolCallUpdate(done) = &out[2] else {
        panic!("expected a tool_call_update, got {out:?}");
    };
    assert_eq!(done.tool_call_id, call.tool_call_id);
    assert_eq!(done.status, Some(ToolCallStatus::Completed));
}

#[test]
fn a_failed_tool_call_reports_failed() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::ToolCallRequested {
                tool: "run_command".into(),
                args_summary: r#"{"command":"false"}"#.into(),
            },
            EventKind::ToolCompleted {
                name: "run_command".into(),
                success: false,
            },
        ],
    );
    let SessionUpdate::ToolCallUpdate(done) = out.last().expect("an update") else {
        panic!("expected a tool_call_update, got {out:?}");
    };
    assert_eq!(done.status, Some(ToolCallStatus::Failed));
}

#[test]
fn tool_kinds_follow_the_schemas_vocabulary() {
    for (tool, expected) in [
        ("read_file", ToolKind::Read),
        ("write_file", ToolKind::Edit),
        ("edit_file", ToolKind::Edit),
        ("delete_file", ToolKind::Delete),
        ("run_command", ToolKind::Execute),
        ("graph_context", ToolKind::Search),
        ("graph_grep", ToolKind::Search),
        ("something_new", ToolKind::Other),
    ] {
        assert_eq!(tool_kind(tool), expected, "{tool}");
    }
}

#[test]
fn a_tool_event_with_no_preceding_request_still_produces_a_tool_call() {
    // The needle fast path dispatches without emitting ToolCallRequested
    // first; the client must still see the work.
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![EventKind::ToolStarted {
            name: "read_file".into(),
        }],
    );
    let SessionUpdate::ToolCall(call) = &out[0] else {
        panic!("expected a synthesized tool_call, got {out:?}");
    };
    assert_eq!(call.kind, ToolKind::Read);
    assert_eq!(call.status, ToolCallStatus::InProgress);
}

#[test]
fn a_status_event_for_a_different_tool_opens_its_own_call() {
    // Defensive: the loop dispatches one call at a time, so this ordering
    // should not occur. If it ever does, relabelling the call in flight
    // would report the wrong tool as completed.
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::ToolCallRequested {
                tool: "write_file".into(),
                args_summary: r#"{"path":"a.rs"}"#.into(),
            },
            EventKind::ToolCompleted {
                name: "run_command".into(),
                success: true,
            },
        ],
    );

    let SessionUpdate::ToolCall(first) = &out[0] else {
        panic!("expected the requested tool_call first, got {out:?}");
    };
    let SessionUpdate::ToolCall(second) = &out[1] else {
        panic!("a mismatched completion must open its own tool_call, got {out:?}");
    };
    assert_eq!(second.name.as_deref(), Some("run_command"));
    assert_eq!(second.status, ToolCallStatus::Completed);
    assert_ne!(
        second.tool_call_id, first.tool_call_id,
        "the write_file call must not be reported as the command's result"
    );
}

#[test]
fn a_changed_file_becomes_a_location_on_the_current_tool_call() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::ToolCallRequested {
                tool: "write_file".into(),
                args_summary: r#"{"path":"notes.txt"}"#.into(),
            },
            EventKind::FileChanged {
                path: "notes.txt".into(),
            },
        ],
    );
    let SessionUpdate::ToolCallUpdate(update) = out.last().expect("an update") else {
        panic!("expected a tool_call_update, got {out:?}");
    };
    assert_eq!(
        update.locations.first().map(|l| l.path.clone()),
        Some(PathBuf::from("/projects/demo/notes.txt"))
    );
}

#[test]
fn a_changed_file_with_no_tool_call_in_flight_becomes_its_own_tool_call() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![EventKind::FileChanged {
            path: "generated.rs".into(),
        }],
    );
    let SessionUpdate::ToolCall(call) = &out[0] else {
        panic!("expected a standalone tool_call, got {out:?}");
    };
    assert_eq!(call.kind, ToolKind::Edit);
    assert_eq!(call.status, ToolCallStatus::Completed);
    assert_eq!(
        call.locations.first().map(|l| l.path.clone()),
        Some(PathBuf::from("/projects/demo/generated.rs"))
    );
}

#[test]
fn bookkeeping_events_produce_no_updates() {
    // Nothing here has an honest ACP slot: inventing one would put noise
    // in the editor's transcript.
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::RunStarted {
                provider: "mock".into(),
                model: "mock-local".into(),
                prompt: "hi".into(),
            },
            EventKind::TurnCompleted { turn: 1 },
            EventKind::InputReceived {
                message: "y".into(),
            },
            EventKind::Note {
                message: "x".into(),
            },
            EventKind::ApprovalDecided {
                command: "rm -rf /".into(),
                approved: false,
            },
            EventKind::Completed {
                summary: "done".into(),
            },
        ],
    );
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn tool_call_ids_are_unique_per_call() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: r#"{"path":"a.rs"}"#.into(),
            },
            EventKind::ToolCompleted {
                name: "read_file".into(),
                success: true,
            },
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: r#"{"path":"b.rs"}"#.into(),
            },
        ],
    );
    let ids: Vec<String> = out
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::ToolCall(call) => Some(call.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2, "{out:?}");
    assert_ne!(ids[0], ids[1], "each tool call needs its own id");
}

#[test]
fn tool_call_ids_do_not_collide_between_turns_of_one_session() {
    // ACP requires `toolCallId` to be unique within the SESSION, not the
    // turn. A per-turn counter would re-issue the same id on the second
    // prompt of a conversation, and a client that upserts tool calls by id
    // (Zed does) would mutate the first turn's entry instead of adding one.
    let ids_for = |run: &str| -> Vec<String> {
        let mut state = TurnState::for_run(run, ROOT);
        let mut out = Vec::new();
        for kind in [
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: r#"{"path":"a.rs"}"#.into(),
            },
            EventKind::ToolCompleted {
                name: "read_file".into(),
                success: true,
            },
            EventKind::ToolCallRequested {
                tool: "write_file".into(),
                args_summary: r#"{"path":"b.rs"}"#.into(),
            },
        ] {
            for action in state.on_event(&Event::new(run, "session-1", kind)) {
                if let TurnAction::Notify(SessionUpdate::ToolCall(call)) = action {
                    out.push(call.tool_call_id);
                }
            }
        }
        out
    };

    // Two turns of the same conversation are two runs.
    let first = ids_for("run-A");
    let second = ids_for("run-B");
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    for id in &first {
        assert!(
            !second.contains(id),
            "turn 2 reused {id} from turn 1; ids must be session-unique: {first:?} vs {second:?}"
        );
    }
}

#[test]
fn a_truncated_args_summary_still_yields_a_usable_tool_call() {
    // `args_summary` is capped at 120 chars, so it is often not valid JSON.
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![EventKind::ToolCallRequested {
            tool: "write_file".into(),
            args_summary: r#"{"path":"big.rs","content":"aaaaaaaaaaaaaaaaaaaa"#.into(),
        }],
    );
    let SessionUpdate::ToolCall(call) = &out[0] else {
        panic!("expected a tool_call, got {out:?}");
    };
    assert_eq!(call.kind, ToolKind::Edit);
    assert!(!call.title.is_empty());
    assert!(
        call.raw_input.is_some(),
        "the summary should still be shown"
    );
}

// --- approvals → session/request_permission ----------------------------

#[test]
fn an_approval_request_asks_the_client_for_permission() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let mut actions = Vec::new();
    for kind in [
        EventKind::ToolCallRequested {
            tool: "run_command".into(),
            args_summary: r#"{"command":"rm","args":["-rf","build"]}"#.into(),
        },
        EventKind::ApprovalRequested {
            command: "rm -rf build".into(),
            risk: RiskLevel::Destructive,
        },
    ] {
        actions.extend(state.on_event(&event(kind)));
    }

    let asked = actions
        .iter()
        .filter_map(|a| match a {
            TurnAction::AskPermission { tool_call, .. } => Some(tool_call),
            TurnAction::Notify(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(asked.len(), 1, "{actions:?}");
    // The permission request must point at the tool call already on
    // screen, not invent a second one.
    assert_eq!(asked[0].tool_call_id, format!("{RUN}/call_1"));
}

#[test]
fn an_approval_request_with_no_tool_call_in_flight_still_asks() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let actions = state.on_event(&event(EventKind::ApprovalRequested {
        command: "rm -rf build".into(),
        risk: RiskLevel::Risky,
    }));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, TurnAction::AskPermission { .. })),
        "{actions:?}"
    );
}

#[test]
fn the_permission_prompt_names_the_command_and_its_risk() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let actions = state.on_event(&event(EventKind::ApprovalRequested {
        command: "rm -rf build".into(),
        risk: RiskLevel::Destructive,
    }));
    let TurnAction::AskPermission { title, .. } = actions
        .iter()
        .find(|a| matches!(a, TurnAction::AskPermission { .. }))
        .expect("a permission request")
    else {
        unreachable!()
    };
    assert!(title.contains("rm -rf build"), "{title}");
    assert!(title.to_lowercase().contains("destructive"), "{title}");
}

#[test]
fn permission_options_offer_one_allow_and_one_reject() {
    let options = permission_options();
    assert_eq!(options.len(), 2, "{options:?}");
    assert_eq!(options[0].kind, PermissionOptionKind::AllowOnce);
    assert_eq!(options[1].kind, PermissionOptionKind::RejectOnce);
    // forge's approval gate is per-operation: it has no "remember this"
    // store, so offering allow_always would be a lie.
    assert!(
        options
            .iter()
            .all(|o| o.kind != PermissionOptionKind::AllowAlways)
    );
}

#[test]
fn choosing_allow_sends_the_runtimes_approval_word() {
    let decision = approval_decision(&RequestPermissionOutcome::Selected {
        option_id: permission_options()[0].option_id.clone(),
    });
    assert_eq!(decision, ApprovalDecision::Approve);
    assert_eq!(decision.input(), "y");
}

#[test]
fn choosing_reject_denies() {
    let decision = approval_decision(&RequestPermissionOutcome::Selected {
        option_id: permission_options()[1].option_id.clone(),
    });
    assert_eq!(decision, ApprovalDecision::Deny);
    assert_eq!(decision.input(), "n");
}

#[test]
fn an_unknown_option_id_denies_rather_than_guessing() {
    let decision = approval_decision(&RequestPermissionOutcome::Selected {
        option_id: "something-we-never-offered".into(),
    });
    assert_eq!(decision, ApprovalDecision::Deny);
}

#[test]
fn a_cancelled_permission_request_counts_as_a_denial() {
    // The spec has clients answer pending permission requests with
    // `cancelled` when they cancel the turn. The parked run still has to
    // be unblocked, and it must not perform the risky operation.
    let decision = approval_decision(&RequestPermissionOutcome::Cancelled);
    assert_eq!(decision, ApprovalDecision::Cancelled);
    assert_eq!(decision.input(), "n");
}

// --- turn end -----------------------------------------------------------

#[test]
fn a_completed_run_ends_the_turn() {
    assert_eq!(
        turn_end(Ok("all done".into()), false),
        TurnEnd::Stop(StopReason::EndTurn)
    );
}

/// A classified failure, as `server.rs::settle` builds one.
fn failure(state: forge_core::RunState, message: &str) -> crate::dispatch::RunFailure {
    crate::dispatch::RunFailure::new(state, message)
}

#[test]
fn a_cancelled_run_reports_the_cancelled_stop_reason() {
    use forge_core::RunState;
    // Both routes to the same answer: we saw the Cancelled event, or the
    // loop returned its cancellation error.
    assert_eq!(
        turn_end(Err(failure(RunState::Cancelled, "run cancelled")), false),
        TurnEnd::Stop(StopReason::Cancelled)
    );
    assert_eq!(
        turn_end(
            Err(failure(RunState::Failed, "model provider exploded")),
            true
        ),
        TurnEnd::Stop(StopReason::Cancelled),
        "an observed cancellation wins over whatever error the loop raised"
    );
    assert_eq!(
        turn_end(Ok("partial".into()), true),
        TurnEnd::Stop(StopReason::Cancelled)
    );
}

#[test]
fn a_failed_run_becomes_a_json_rpc_error() {
    let TurnEnd::Failed(error) = turn_end(
        Err(failure(
            forge_core::RunState::Failed,
            "no model provider configured",
        )),
        false,
    ) else {
        panic!("a failure must not be reported as a normal stop reason");
    };
    assert_eq!(error.code, -32603);
    assert!(error.message.contains("no model provider configured"));
}

#[test]
fn an_unanswerable_approval_is_reported_as_a_refusal() {
    // `ApprovalRequired` escaping the loop means nobody could answer the
    // permission request — the turn stopped without doing the work, which
    // is a refusal rather than an internal error.
    let end = turn_end(
        Err(failure(
            forge_core::RunState::AwaitingApproval,
            "approval required for: rm -rf build (destructive)",
        )),
        false,
    );
    assert_eq!(end, TurnEnd::Stop(StopReason::Refusal));
}

/// The stop reason now follows the *state*, not the message. This is the
/// property the old string matching could not have: an error that merely
/// mentions cancellation is still a failure.
#[test]
fn the_stop_reason_follows_the_state_not_the_message() {
    use forge_core::RunState;
    let end = turn_end(
        Err(failure(
            RunState::Failed,
            "the upstream cancelled our stream and approval required nothing",
        )),
        false,
    );
    assert!(
        matches!(end, TurnEnd::Failed(_)),
        "a failure whose text mentions cancellation is still a failure: {end:?}"
    );
}

// --- malformed input ----------------------------------------------------

#[test]
fn a_valid_request_line_parses() {
    let incoming = parse_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .expect("parses");
    assert_eq!(incoming.method.as_deref(), Some("initialize"));
    assert_eq!(incoming.id, Some(json!(1)));
    assert!(!incoming.is_response());
}

#[test]
fn a_garbage_line_is_a_parse_error_not_a_panic() {
    let error = parse_line("this is not json").expect_err("must not parse");
    assert_eq!(error.code, -32700);
}

#[test]
fn a_json_value_that_is_not_an_object_is_an_invalid_request() {
    let error = parse_line("[1,2,3]").expect_err("must not parse");
    assert_eq!(error.code, -32600);
}

#[test]
fn a_line_with_no_method_and_no_id_is_an_invalid_request() {
    let error = parse_line(r#"{"jsonrpc":"2.0"}"#).expect_err("must not parse");
    assert_eq!(error.code, -32600);
}

#[test]
fn a_response_to_one_of_our_requests_is_recognised() {
    let incoming =
        parse_line(r#"{"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"cancelled"}}}"#)
            .expect("parses");
    assert!(incoming.is_response());
}

#[test]
fn a_permission_response_deserializes_both_outcome_shapes() {
    let selected: crate::protocol::RequestPermissionResponse = serde_json::from_value(
        json!({ "outcome": { "outcome": "selected", "optionId": "allow" } }),
    )
    .expect("selected");
    assert_eq!(
        selected.outcome,
        RequestPermissionOutcome::Selected {
            option_id: "allow".into()
        }
    );

    let cancelled: crate::protocol::RequestPermissionResponse =
        serde_json::from_value(json!({ "outcome": { "outcome": "cancelled" } }))
            .expect("cancelled");
    assert_eq!(cancelled.outcome, RequestPermissionOutcome::Cancelled);
}

// --- wire shapes --------------------------------------------------------

#[test]
fn a_session_update_notification_has_the_schemas_nesting() {
    // params = { sessionId, update: { sessionUpdate: "...", ... } } — the
    // update is nested, and its discriminator is `sessionUpdate`.
    let notification = crate::protocol::SessionNotification {
        session_id: "sess-1".into(),
        update: SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text("hello"),
        },
    };
    let value = serde_json::to_value(&notification).expect("serialize");
    assert_eq!(value["sessionId"], "sess-1");
    assert_eq!(value["update"]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(value["update"]["content"]["type"], "text");
    assert_eq!(value["update"]["content"]["text"], "hello");
}

#[test]
fn a_tool_call_update_only_carries_the_fields_it_changes() {
    let update = SessionUpdate::ToolCallUpdate(
        crate::protocol::ToolCallUpdate::new("call_1").status(ToolCallStatus::Completed),
    );
    let value = serde_json::to_value(&update).expect("serialize");
    assert_eq!(value["sessionUpdate"], "tool_call_update");
    assert_eq!(value["toolCallId"], "call_1");
    assert_eq!(value["status"], "completed");
    assert!(value.get("title").is_none(), "{value}");
    assert!(value.get("locations").is_none(), "{value}");
}

#[test]
fn a_prompt_response_serializes_its_stop_reason_in_snake_case() {
    let value = serde_json::to_value(crate::protocol::PromptResponse {
        stop_reason: StopReason::EndTurn,
    })
    .expect("serialize");
    assert_eq!(value["stopReason"], "end_turn");
}
