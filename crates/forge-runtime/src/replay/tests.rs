use forge_core::{Event, EventKind, Role, ToolCall};

use super::*;

/// Build a stored-looking event (seq assigned, as the store would).
fn event(run: &str, seq: u64, kind: EventKind) -> Event {
    let mut event = Event::new(run, "sess", kind);
    event.seq = seq;
    event
}

fn run_started(run: &str, seq: u64, prompt: &str) -> Event {
    event(
        run,
        seq,
        EventKind::RunStarted {
            provider: "scripted-mock".into(),
            model: "scripted-mock".into(),
            prompt: prompt.into(),
        },
    )
}

fn assistant(run: &str, seq: u64, text: &str, calls: Vec<ToolCall>) -> Event {
    event(
        run,
        seq,
        EventKind::AssistantMessage {
            text: text.into(),
            tool_calls: calls,
        },
    )
}

fn tool_result(run: &str, seq: u64, call_id: &str, output: &str) -> Event {
    event(
        run,
        seq,
        EventKind::ToolResult {
            call_id: call_id.into(),
            tool: "read_file".into(),
            output: output.into(),
            is_error: false,
        },
    )
}

fn completed(run: &str, seq: u64, summary: &str) -> Event {
    event(
        run,
        seq,
        EventKind::Completed {
            summary: summary.into(),
        },
    )
}

// --- reconstruction -----------------------------------------------------

#[test]
fn a_single_text_run_replays_as_prompt_and_answer() {
    let events = vec![
        run_started("r1", 1, "explain the parser"),
        assistant("r1", 2, "the parser is recursive descent", Vec::new()),
        completed("r1", 3, "the parser is recursive descent"),
    ];
    let replay = conversation_from_events(&events);

    assert!(!replay.degraded);
    let roles: Vec<Role> = replay.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant]);
    assert_eq!(replay.messages[0].content, "explain the parser");
    assert_eq!(
        replay.messages[1].content,
        "the parser is recursive descent"
    );
}

#[test]
fn tool_calls_and_results_replay_in_order_with_their_ids() {
    let call = ToolCall::new("call_1", "read_file", serde_json::json!({"path": "a.rs"}));
    let events = vec![
        run_started("r1", 1, "read a.rs"),
        // Observability events are interleaved exactly as the loop emits
        // them; replay must ignore them without losing ordering.
        event(
            "r1",
            2,
            EventKind::RoutingDecisionMade {
                router: "static".into(),
                selected_model: "scripted-mock".into(),
                confidence: 1.0,
                fallback_used: false,
                reason: String::new(),
            },
        ),
        assistant("r1", 3, "let me look", vec![call.clone()]),
        event(
            "r1",
            4,
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: "{\"path\":\"a.rs\"}".into(),
            },
        ),
        event(
            "r1",
            5,
            EventKind::ToolStarted {
                name: "read_file".into(),
            },
        ),
        tool_result("r1", 6, "call_1", "fn a() {}"),
        event(
            "r1",
            7,
            EventKind::ToolCompleted {
                name: "read_file".into(),
                success: true,
            },
        ),
        event("r1", 8, EventKind::TurnCompleted { turn: 1 }),
        assistant("r1", 9, "a.rs defines a()", Vec::new()),
        completed("r1", 10, "a.rs defines a()"),
    ];

    let replay = conversation_from_events(&events);
    assert!(!replay.degraded);

    let shape: Vec<(Role, &str)> = replay
        .messages
        .iter()
        .map(|m| (m.role, m.content.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, "read a.rs"),
            (Role::Assistant, "let me look"),
            (Role::Tool, "fn a() {}"),
            (Role::Assistant, "a.rs defines a()"),
        ]
    );
    assert_eq!(replay.messages[1].tool_calls, vec![call]);
    assert_eq!(
        replay.messages[2].tool_call_id.as_deref(),
        Some("call_1"),
        "the tool message must answer the call the assistant made"
    );
}

#[test]
fn a_multi_run_session_replays_every_run_in_order() {
    let events = vec![
        run_started("r1", 1, "first ask"),
        assistant("r1", 2, "first answer", Vec::new()),
        completed("r1", 3, "first answer"),
        run_started("r2", 1, "second ask"),
        event(
            "r2",
            2,
            EventKind::InputReceived {
                message: "resume of run r1".into(),
            },
        ),
        assistant("r2", 3, "second answer", Vec::new()),
        completed("r2", 4, "second answer"),
    ];
    let replay = conversation_from_events(&events);

    let contents: Vec<&str> = replay.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["first ask", "first answer", "second ask", "second answer"],
        "the resume marker is bookkeeping, not a user turn"
    );
}

#[test]
fn real_user_input_is_replayed_but_the_resume_marker_is_not() {
    let events = vec![
        run_started("r1", 1, "start"),
        event(
            "r1",
            2,
            EventKind::InputReceived {
                message: "also check b.rs".into(),
            },
        ),
        event(
            "r1",
            3,
            EventKind::InputReceived {
                message: "resume of run r0".into(),
            },
        ),
        assistant("r1", 4, "ok", Vec::new()),
        completed("r1", 5, "ok"),
    ];
    let contents: Vec<String> = conversation_from_events(&events)
        .messages
        .into_iter()
        .map(|m| m.content)
        .collect();
    assert_eq!(contents, vec!["start", "also check b.rs", "ok"]);
}

#[test]
fn a_pre_v3_run_degrades_to_its_completion_summary() {
    // v1/v2 logs have no assistant_message/tool_result events at all.
    let events = vec![
        run_started("r1", 1, "old ask"),
        event(
            "r1",
            2,
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: "{\"path\":\"a.rs\"}".into(),
            },
        ),
        event(
            "r1",
            3,
            EventKind::ToolCompleted {
                name: "read_file".into(),
                success: true,
            },
        ),
        completed("r1", 4, "truncated answer"),
    ];
    let replay = conversation_from_events(&events);

    assert!(replay.degraded, "an old run must be flagged as degraded");
    let shape: Vec<(Role, &str)> = replay
        .messages
        .iter()
        .map(|m| (m.role, m.content.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, "old ask"),
            (Role::Assistant, "truncated answer"),
        ],
        "no tool history survives a pre-v3 log, but the ask and answer do"
    );
}

#[test]
fn a_v3_run_after_a_v1_run_still_replays_fully_and_flags_degradation() {
    let events = vec![
        run_started("old", 1, "old ask"),
        completed("old", 2, "old summary"),
        run_started("new", 1, "new ask"),
        assistant("new", 2, "full new answer", Vec::new()),
        completed("new", 3, "full new answer"),
    ];
    let replay = conversation_from_events(&events);
    assert!(replay.degraded);
    let contents: Vec<&str> = replay.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["old ask", "old summary", "new ask", "full new answer"]
    );
}

#[test]
fn a_v3_run_does_not_duplicate_its_answer_from_completed() {
    let events = vec![
        run_started("r1", 1, "ask"),
        assistant(
            "r1",
            2,
            "the full answer, much longer than the summary",
            vec![],
        ),
        completed("r1", 3, "the full answer, much longer"),
    ];
    let replay = conversation_from_events(&events);
    assert_eq!(replay.messages.len(), 2, "got: {:?}", replay.messages);
    assert_eq!(
        replay.messages[1].content,
        "the full answer, much longer than the summary"
    );
}

#[test]
fn a_tool_result_without_its_assistant_call_is_dropped() {
    // A log where the assistant message never made it (crash between
    // events) would otherwise produce an orphan tool message, which real
    // providers reject.
    let events = vec![
        run_started("r1", 1, "ask"),
        tool_result("r1", 2, "call_ghost", "output nobody asked for"),
        assistant("r1", 3, "done", Vec::new()),
        completed("r1", 4, "done"),
    ];
    let replay = conversation_from_events(&events);
    assert!(
        replay.messages.iter().all(|m| m.role != Role::Tool),
        "got: {:?}",
        replay.messages
    );
}

#[test]
fn a_cancelled_run_replays_what_it_produced_without_a_completion() {
    let call = ToolCall::new("call_1", "read_file", serde_json::json!({"path": "a.rs"}));
    let events = vec![
        run_started("r1", 1, "ask"),
        assistant("r1", 2, "starting", vec![call]),
        tool_result("r1", 3, "call_1", "contents"),
        event(
            "r1",
            4,
            EventKind::Cancelled {
                reason: "cancelled by user".into(),
            },
        ),
    ];
    let replay = conversation_from_events(&events);
    assert!(
        !replay.degraded,
        "no completion is not the same as an old log"
    );
    let roles: Vec<Role> = replay.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool]);
}

#[test]
fn a_fork_marker_is_not_part_of_the_conversation() {
    let events = vec![
        run_started("r1", 1, "ask"),
        assistant("r1", 2, "answer", Vec::new()),
        completed("r1", 3, "answer"),
        event(
            "fork",
            1,
            EventKind::SessionForked {
                from_session: "src".into(),
                at_position: 3,
            },
        ),
    ];
    let replay = conversation_from_events(&events);
    assert_eq!(replay.messages.len(), 2);
}

#[test]
fn an_empty_log_replays_as_nothing() {
    let replay = conversation_from_events(&[]);
    assert!(replay.messages.is_empty());
    assert!(!replay.degraded);
}

// --- budget -------------------------------------------------------------

#[test]
fn history_that_fits_is_returned_unchanged() {
    let messages = vec![
        Message::user("a"),
        Message::assistant("b"),
        Message::user("c"),
    ];
    assert_eq!(fit_to_budget(messages.clone(), 10_000), messages);
}

#[test]
fn oversized_history_keeps_the_first_ask_and_the_most_recent_messages() {
    let messages = vec![
        Message::user("FIRST ASK"),
        Message::assistant("old".repeat(100)),
        Message::user("middle".repeat(100)),
        Message::assistant("recent answer"),
    ];
    let fitted = fit_to_budget(messages, 200);

    assert_eq!(fitted[0].content, "FIRST ASK", "the anchor must survive");
    assert_eq!(fitted[1].role, Role::System);
    assert!(fitted[1].content.contains("omitted"), "{:?}", fitted[1]);
    assert_eq!(
        fitted.last().expect("at least one").content,
        "recent answer",
        "truncation drops from the FRONT"
    );
    assert!(
        !fitted.iter().any(|m| m.content.contains("middle")),
        "the oversized middle must be gone: {fitted:?}"
    );
}

#[test]
fn dropping_an_assistant_call_also_drops_its_tool_result() {
    let call = ToolCall::new("call_1", "read_file", serde_json::json!({"path": "a.rs"}));
    let mut assistant_msg = Message::assistant_tool_calls(vec![call]);
    assistant_msg.content = "x".repeat(400);
    let messages = vec![
        Message::user("anchor"),
        assistant_msg,
        Message::tool("call_1", "the tool output"),
        Message::assistant("final"),
    ];

    let fitted = fit_to_budget(messages, 150);
    assert!(
        fitted.iter().all(|m| m.role != Role::Tool),
        "an orphan tool message must not survive: {fitted:?}"
    );
    assert_eq!(fitted[0].content, "anchor");
    assert_eq!(fitted.last().expect("last").content, "final");
}

#[test]
fn an_anchor_bigger_than_the_budget_is_kept_but_cut() {
    let messages = vec![Message::user("x".repeat(5_000)), Message::assistant("b")];
    let fitted = fit_to_budget(messages, 300);

    assert_eq!(fitted.len(), 1);
    assert_eq!(fitted[0].role, Role::User);
    assert!(fitted[0].content.contains("message truncated"));
    assert!(fitted[0].content.chars().count() < 400, "still oversized");
}

#[test]
fn an_empty_history_survives_any_budget() {
    assert!(fit_to_budget(Vec::new(), 0).is_empty());
}

#[test]
fn the_budget_follows_the_models_context_window_with_a_floor() {
    let big = ModelCapabilities {
        max_context: 200_000,
        ..ModelCapabilities::default()
    };
    assert_eq!(history_budget_chars(&big), 400_000);

    let tiny = ModelCapabilities {
        max_context: 8,
        ..ModelCapabilities::default()
    };
    assert_eq!(
        history_budget_chars(&tiny),
        MIN_HISTORY_CHARS,
        "a misreported window must not reduce replay to nothing"
    );
}
