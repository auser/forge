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

// --- interleaved runs ---------------------------------------------------

/// The bug this module's grouping exists for.
///
/// Two runs were live in one session at once, so the log interleaves: run
/// `b`'s assistant message sits between run `a`'s tool call and its result.
/// Replayed in file order, `repair_tool_pairs` consumes only the messages
/// *directly* after the assistant call, finds `b`'s assistant message there,
/// discards `a`'s real result as an orphan and synthesizes
/// [`UNANSWERED_TOOL`] for the call — telling the model a successful,
/// possibly side-effecting call went unanswered, which invites it to retry.
///
/// The output stays API-valid either way, which is why this had no visible
/// symptom.
#[test]
fn an_interleaved_tool_result_still_answers_its_own_call() {
    let call = ToolCall::new("call_a", "read_file", serde_json::json!({"path": "a.rs"}));
    let events = vec![
        run_started("a", 1, "ask a"),
        run_started("b", 1, "ask b"),
        assistant("a", 2, "reading a.rs", vec![call.clone()]),
        // Run b's turn lands between run a's call and its answer.
        assistant("b", 2, "thinking about b", Vec::new()),
        tool_result("a", 3, "call_a", "fn a() {}"),
        completed("a", 4, "a.rs defines a()"),
        assistant("b", 3, "b answer", Vec::new()),
        completed("b", 4, "b answer"),
    ];
    let replay = conversation_from_events(&events);

    let answer = replay
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("call_a"))
        .expect("call_a answered");
    assert_eq!(
        answer.content, "fn a() {}",
        "the call's real output must survive: {:?}",
        replay.messages
    );
    assert!(
        !replay.messages.iter().any(|m| m.content == UNANSWERED_TOOL),
        "a call that WAS answered must not be replayed as unanswered: {:?}",
        replay.messages
    );
    // Each run's messages stay contiguous, runs in first-appearance order.
    let shape: Vec<(Role, &str)> = replay
        .messages
        .iter()
        .map(|m| (m.role, m.content.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, "ask a"),
            (Role::Assistant, "reading a.rs"),
            (Role::Tool, "fn a() {}"),
            (Role::User, "ask b"),
            (Role::Assistant, "thinking about b"),
            (Role::Assistant, "b answer"),
        ]
    );
}

#[test]
fn three_interleaved_runs_each_keep_their_own_messages() {
    let events = vec![
        run_started("one", 1, "ask one"),
        run_started("two", 1, "ask two"),
        assistant("one", 2, "answer one", Vec::new()),
        run_started("three", 1, "ask three"),
        assistant("three", 2, "answer three", Vec::new()),
        assistant("two", 2, "answer two", Vec::new()),
        completed("two", 3, "answer two"),
        completed("three", 3, "answer three"),
        completed("one", 3, "answer one"),
    ];
    let replay = conversation_from_events(&events);
    let contents: Vec<&str> = replay.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec![
            "ask one",
            "answer one",
            "ask two",
            "answer two",
            "ask three",
            "answer three",
        ],
        "runs are ordered by first appearance, not by id or completion"
    );
}

/// Run order is *first appearance*, which is neither id order nor the order
/// the runs finished in. `zzz` starts first, so it replays first.
#[test]
fn run_order_follows_first_appearance_not_run_id() {
    let events = vec![
        run_started("zzz", 1, "the earlier ask"),
        run_started("aaa", 1, "the later ask"),
        assistant("aaa", 2, "later answer", Vec::new()),
        assistant("zzz", 2, "earlier answer", Vec::new()),
        completed("aaa", 3, "later answer"),
        completed("zzz", 3, "earlier answer"),
    ];
    let replay = conversation_from_events(&events);
    let contents: Vec<&str> = replay.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec![
            "the earlier ask",
            "earlier answer",
            "the later ask",
            "later answer",
        ]
    );
}

/// Grouping must not cost the pre-v3 fallback: a v1/v2 run interleaved with
/// a v3 one still replays from its `completed` summary and still flags the
/// replay degraded.
#[test]
fn an_interleaved_pre_v3_run_still_degrades_to_its_summary() {
    let events = vec![
        run_started("old", 1, "old ask"),
        run_started("new", 1, "new ask"),
        assistant("new", 2, "new answer", Vec::new()),
        event(
            "old",
            2,
            EventKind::ToolCompleted {
                name: "read_file".into(),
                success: true,
            },
        ),
        completed("old", 3, "truncated old answer"),
        completed("new", 3, "new answer"),
    ];
    let replay = conversation_from_events(&events);
    assert!(replay.degraded, "the v1/v2 run must still be flagged");
    let contents: Vec<&str> = replay.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["old ask", "truncated old answer", "new ask", "new answer"]
    );
}

/// Both repairs compose: grouping reattaches the interleaved result, and
/// `repair_tool_pairs` still answers the genuinely dangling call.
#[test]
fn an_interleaved_log_with_a_dangling_call_gets_both_repairs() {
    let answered = ToolCall::new("call_a", "read_file", serde_json::json!({"path": "a.rs"}));
    let dangling = ToolCall::new(
        "call_b",
        "run_command",
        serde_json::json!({"command": "ls"}),
    );
    let events = vec![
        run_started("a", 1, "ask a"),
        run_started("b", 1, "ask b"),
        assistant("a", 2, "reading a.rs", vec![answered]),
        // b announces a call it never gets to dispatch...
        assistant("b", 2, "listing", vec![dangling]),
        // ...while a's answer arrives after it.
        tool_result("a", 3, "call_a", "fn a() {}"),
        completed("a", 4, "a.rs defines a()"),
        event(
            "b",
            3,
            EventKind::Cancelled {
                reason: "cancelled by user".into(),
            },
        ),
    ];
    let replay = conversation_from_events(&events);

    let shape: Vec<(Role, &str)> = replay
        .messages
        .iter()
        .map(|m| (m.role, m.content.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, "ask a"),
            (Role::Assistant, "reading a.rs"),
            (Role::Tool, "fn a() {}"),
            (Role::User, "ask b"),
            (Role::Assistant, "listing"),
            (Role::Tool, UNANSWERED_TOOL),
        ]
    );
    // And the pairing invariant holds over the whole history.
    let announced: std::collections::HashSet<&str> = replay
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.iter().map(|c| c.id.as_str()))
        .collect();
    let answers: std::collections::HashSet<&str> = replay
        .messages
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert_eq!(announced, answers);
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

/// The regression this exists for: the loop records `assistant_message`
/// *before* it dispatches, so a run that died between the two leaves a call
/// with no result. Replayed as-is, that is a provider 400 — every chat API
/// requires each announced tool call to be answered — and it reaches a real
/// run because any session reusing one session id (ACP turns, `forge
/// resume`, `POST /v1/runs` with a `session_id`) replays the aborted run on
/// the next resume.
#[test]
fn an_unanswered_tool_call_gets_a_synthetic_result_instead_of_dangling() {
    let call = ToolCall::new(
        "call_1",
        "run_command",
        serde_json::json!({"command": "rm -rf x"}),
    );
    let events = vec![
        run_started("r1", 1, "clean up"),
        // Recorded before dispatch...
        assistant("r1", 2, "removing it", vec![call]),
        // ...and then the run was cancelled at the per-call checkpoint.
        event(
            "r1",
            3,
            EventKind::Cancelled {
                reason: "cancelled by user".into(),
            },
        ),
        // A later run in the SAME session is what makes this reachable.
        run_started("r2", 1, "what happened?"),
        assistant("r2", 2, "nothing ran", Vec::new()),
        completed("r2", 3, "nothing ran"),
    ];
    let replay = conversation_from_events(&events);

    let shape: Vec<(Role, &str)> = replay
        .messages
        .iter()
        .map(|m| (m.role, m.content.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, "clean up"),
            (Role::Assistant, "removing it"),
            (Role::Tool, "[forge: run ended before this tool answered]"),
            (Role::User, "what happened?"),
            (Role::Assistant, "nothing ran"),
        ],
        "an announced call must always be answered"
    );
    assert_eq!(replay.messages[2].tool_call_id.as_deref(), Some("call_1"));
    // The call itself survives: the agent did attempt it, and a replay that
    // hid the attempt would misdescribe what happened.
    assert_eq!(replay.messages[1].tool_calls.len(), 1);
}

#[test]
fn every_announced_call_is_answered_even_when_only_some_ran() {
    // A multi-call turn cancelled partway: the first call answered, the
    // second never dispatched.
    let calls = vec![
        ToolCall::new("call_1", "read_file", serde_json::json!({"path": "a.rs"})),
        ToolCall::new("call_2", "read_file", serde_json::json!({"path": "b.rs"})),
    ];
    let events = vec![
        run_started("r1", 1, "read both"),
        assistant("r1", 2, "", calls),
        tool_result("r1", 3, "call_1", "contents of a"),
        event(
            "r1",
            4,
            EventKind::Error {
                message: "dispatch failed".into(),
            },
        ),
    ];
    let replay = conversation_from_events(&events);

    let announced: Vec<&str> = replay.messages[1]
        .tool_calls
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    let answers: Vec<&str> = replay
        .messages
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert_eq!(announced, vec!["call_1", "call_2"]);
    assert_eq!(answers, announced, "every call needs exactly one answer");
    assert_eq!(replay.messages[2].content, "contents of a");
    assert_eq!(
        replay.messages[3].content,
        "[forge: run ended before this tool answered]"
    );
}

#[test]
fn a_result_answering_a_call_nobody_made_is_dropped_not_reattached() {
    // Two assistant turns; the second's "answer" names the first's call id,
    // which would silently mislabel the second turn's history.
    let events = vec![
        run_started("r1", 1, "ask"),
        assistant(
            "r1",
            2,
            "first",
            vec![ToolCall::new("call_1", "read_file", serde_json::json!({}))],
        ),
        tool_result("r1", 3, "call_1", "first output"),
        assistant(
            "r1",
            4,
            "second",
            vec![ToolCall::new("call_2", "read_file", serde_json::json!({}))],
        ),
        tool_result("r1", 5, "call_1", "a stale answer"),
        completed("r1", 6, "done"),
    ];
    let replay = conversation_from_events(&events);
    assert!(
        !replay
            .messages
            .iter()
            .any(|m| m.content == "a stale answer"),
        "got: {:?}",
        replay.messages
    );
    // call_2 still gets an answer, just not that one.
    let answer = replay
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("call_2"))
        .expect("call_2 answered");
    assert_eq!(
        answer.content,
        "[forge: run ended before this tool answered]"
    );
}

#[test]
fn budget_truncation_never_leaves_a_dangling_call_or_orphan_result() {
    // Whatever the budget drops, the survivors must still pair up.
    let long = "x".repeat(300);
    let messages = vec![
        Message::user("anchor"),
        {
            let mut m = Message::assistant_tool_calls(vec![ToolCall::new(
                "call_1",
                "read_file",
                serde_json::json!({}),
            )]);
            m.content = long.clone();
            m
        },
        Message::tool("call_1", long.clone()),
        Message::assistant("final"),
    ];
    for budget in [80, 150, 260, 400, 700, 1_200] {
        let fitted = fit_to_budget(messages.clone(), budget);
        let announced: std::collections::HashSet<&str> = fitted
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|c| c.id.as_str()))
            .collect();
        let answered: std::collections::HashSet<&str> = fitted
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(
            announced, answered,
            "budget {budget} left an unpaired message: {fitted:?}"
        );
    }
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
