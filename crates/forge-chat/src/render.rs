//! Events to transcript, as pure code.
//!
//! The whole `EventKind` stream becomes [`Line`]s here and nowhere else, so
//! what a user sees is decided by a function with no terminal, no clock the
//! caller cannot see, and no I/O — `cargo test -p forge-chat render` needs
//! no TTY. The mapping is design §4.2, exhaustively: every kind is either
//! rendered or *deliberately* silent, and the `match` has no wildcard arm
//! so a new event kind is a compile error here rather than a line the user
//! never sees.

use std::time::Instant;

use forge_core::{Event, EventKind, RiskLevel};

use crate::io::Line;

/// How long a fallback argument summary may be before it is cut, including
/// the trailing `...`. Nothing in the transcript is width-sensitive, so this
/// is a readability bound, not a terminal width.
const SUMMARY_MAX: usize = 60;

/// Router name the runtime records for the on-device fast path
/// (`forge_runtime::service::NEEDLE_DISPATCH`). Matched as a string because
/// it reaches us through a log line, not through a type.
const NEEDLE_DISPATCH: &str = "needle-dispatch";

/// What has been said so far in one run, which is all the state the §4.2
/// mapping needs: a tool's elapsed-time anchor, whether the answer has
/// already been printed, and the counts the footer reports.
///
/// One instance per run. It is deliberately not `Clone`: two copies would
/// mean two opinions about whether the answer was printed.
#[derive(Debug)]
pub struct TranscriptState {
    /// Start of the run, and the last-resort timing anchor: a
    /// `ToolCompleted` with no preceding `ToolStarted` still reports a
    /// duration rather than panicking.
    started: Instant,
    /// Set by `ToolStarted`, and by `ToolCallRequested` so a stream missing
    /// the former still times the call from something plausible.
    tool_anchor: Option<Instant>,
    rendered_assistant_text: bool,
    tool_calls: usize,
    turns: u32,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptState {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            tool_anchor: None,
            rendered_assistant_text: false,
            tool_calls: 0,
            turns: 0,
        }
    }

    /// The §4.2 mapping. An empty `Vec` is a decision, never an omission.
    ///
    /// The `match` is exhaustive with **no wildcard arm** on purpose: a new
    /// `EventKind` must fail to compile here, so nobody can add an event the
    /// chat silently swallows.
    pub fn on_event(&mut self, event: &Event) -> Vec<Line> {
        match &event.kind {
            // The user just typed it, and piped mode already echoed it.
            EventKind::RunStarted { .. } => Vec::new(),
            EventKind::RoutingDecisionMade {
                router,
                selected_model,
                confidence,
                fallback_used,
                reason,
            } => vec![Line::meta(routing_text(
                router,
                selected_model,
                *confidence,
                *fallback_used,
                reason,
            ))],
            EventKind::SkillActivated { name, .. } => vec![Line::meta(format!("skill: {name}"))],
            EventKind::ToolCallRequested { tool, args_summary } => {
                self.tool_calls += 1;
                // Time the call from here, so a stream that never delivers
                // `ToolStarted` still reports a plausible duration.
                self.tool_anchor = Some(Instant::now());
                let summary = summarize_call(tool, args_summary);
                if summary.is_empty() {
                    vec![Line::tool(tool)]
                } else {
                    vec![Line::tool(format!("{tool} {summary}"))]
                }
            }
            // Always follows `ToolCallRequested` for the same call, so a
            // second line per call buys nothing. The timer restarts here,
            // which is the point at which work actually begins.
            EventKind::ToolStarted { .. } => {
                self.tool_anchor = Some(Instant::now());
                Vec::new()
            }
            EventKind::ToolCompleted { success, .. } => {
                let ms = self
                    .tool_anchor
                    .take()
                    .unwrap_or(self.started)
                    .elapsed()
                    .as_millis();
                vec![if *success {
                    Line::ok(format!("ok ({ms} ms)"))
                } else {
                    Line::failed(format!("failed ({ms} ms)"))
                }]
            }
            EventKind::FileChanged { path } => {
                vec![Line::ok(format!("wrote {}", path.display()))]
            }
            EventKind::ApprovalRequested { command, risk } => vec![Line::warn(format!(
                "approval needed: {command} ({}) - y to approve, anything else denies",
                risk_word(*risk)
            ))],
            EventKind::ApprovalDecided { approved, .. } => vec![if *approved {
                Line::ok("approved")
            } else {
                Line::failed("denied")
            }],
            // The answer itself, with room around it. `tool_calls` are
            // already narrated by the `tool_*` events, so they add nothing.
            // Empty text is the on-device fast path's shape: nothing to
            // print, and nothing recorded, so the driver prints the run
            // outcome instead (§4.3).
            EventKind::AssistantMessage { text, .. } => {
                if text.trim().is_empty() {
                    return Vec::new();
                }
                self.rendered_assistant_text = true;
                let mut lines = vec![Line::plain("")];
                // The writer writes one line at a time, so a paragraph is
                // split here rather than handed over with newlines inside.
                lines.extend(text.lines().map(Line::plain));
                lines.push(Line::plain(""));
                lines
            }
            // The replay record of what the *model* saw, capped at 64 KiB:
            // dumping it would bury the transcript. `ToolCompleted` is the
            // user-facing summary and `forge session show` has the payload.
            EventKind::ToolResult { .. } => Vec::new(),
            // Counted for the footer; silent on its own at default
            // verbosity, which is the only verbosity a transcript has.
            EventKind::TurnCompleted { .. } => {
                self.turns += 1;
                Vec::new()
            }
            // The user is the one who sent it.
            EventKind::InputReceived { .. } => Vec::new(),
            EventKind::Note { message } => vec![Line::meta(message)],
            EventKind::SessionForked {
                from_session,
                at_position,
            } => vec![Line::meta(format!(
                "forked from {from_session} at position {at_position}"
            ))],
            // Forge's error messages already carry their own hints.
            EventKind::Error { message } => vec![Line::bad(format!("error: {message}"))],
            // The reason is internal ("cancelled by user"); the user knows
            // why, having done it.
            EventKind::Cancelled { .. } => vec![Line::bad("cancelled")],
            // Its `summary` is an 80-character digest written by the store;
            // the answer comes from `AssistantMessage` or the run outcome.
            EventKind::Completed { .. } => Vec::new(),
        }
    }

    /// Did any non-empty `AssistantMessage` text reach the transcript?
    ///
    /// The answer-once rule (§4.3) hangs off this: the driver prints the
    /// run outcome's text only when nothing textual was rendered, which is
    /// exactly the on-device fast path's shape (an `AssistantMessage` with
    /// no text and one tool call).
    pub fn rendered_assistant_text(&self) -> bool {
        self.rendered_assistant_text
    }

    /// The one line that closes a turn: what happened, in one short
    /// sentence. No columns and no alignment, so a narrow terminal renders
    /// it exactly like a wide one.
    pub fn footer(&self) -> Line {
        let turns = self.turns;
        let calls = self.tool_calls;
        Line::footer(format!(
            "{turns} turn{}, {calls} tool call{}, {:.1}s",
            plural(u64::from(turns)),
            plural(calls as u64),
            self.started.elapsed().as_secs_f64()
        ))
    }
}

/// One routing decision as a line.
///
/// The on-device fast path is a *different sentence*, not a routing line
/// with an odd model name: it is the product's headline claim, and
/// `routing: needle-dispatch -> none` would read as a failure to route.
fn routing_text(
    router: &str,
    selected_model: &str,
    confidence: f64,
    fallback_used: bool,
    reason: &str,
) -> String {
    if router == NEEDLE_DISPATCH {
        let tool = dispatched_tool(reason).unwrap_or("a tool");
        return format!(
            "on-device: needle called {tool} directly (conf {confidence:.2}, no model call)"
        );
    }
    let mut text = format!("routing: {router} -> {selected_model} (conf {confidence:.2})");
    if fallback_used {
        text.push_str(" [fallback]");
    }
    if !reason.is_empty() {
        text.push_str(" - ");
        text.push_str(reason);
    }
    text
}

/// The tool named in a fast-path reason, which the runtime writes as
/// ``needle filled and dispatched `read_file` on device; no model call``.
/// It is a log string, so a missing name is a possibility, not a bug: the
/// caller says "a tool" and the claim still reads.
fn dispatched_tool(reason: &str) -> Option<&str> {
    let rest = reason.split_once('`')?.1;
    rest.split_once('`')
        .map(|(tool, _)| tool)
        .filter(|t| !t.is_empty())
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The friendly argument form of a tool call: the `src/main.rs` in
/// `read_file src/main.rs`.
///
/// Each tool is *about* one argument, so that is the one shown. The
/// argument is recovered with [`forge_core::tool_arg_field`] — the shared,
/// truncation-tolerant scan, not a hopeful `serde_json::from_str` — because
/// `args_summary` is the call's JSON cut to 120 characters and the most
/// interesting call (`write_file` with real content) is exactly the one cut
/// mid-string. When the field cannot be recovered the raw summary is shown,
/// ellipsized: a line the user can squint at beats no line at all.
pub fn summarize_call(tool: &str, args_summary: &str) -> String {
    let field = |key: &str| forge_core::tool_arg_field(args_summary, key);
    let friendly = match tool {
        "read_file" | "write_file" | "edit_file" | "delete_file" => field("path"),
        "run_command" => field("command"),
        // A query is prose, not a path, so it is quoted — otherwise
        // `graph_grep parse the args` reads as three arguments.
        "graph_context" => field("query").map(|q| format!("{q:?}")),
        "graph_grep" => field("pattern").map(|p| format!("{p:?}")),
        _ => None,
    };
    one_line(&friendly.unwrap_or_else(|| ellipsize(args_summary)))
}

/// Cut a summary to [`SUMMARY_MAX`] characters *including* the trailing
/// `...`, so the marker never pushes the line past the bound it exists to
/// enforce.
fn ellipsize(summary: &str) -> String {
    if summary.chars().count() <= SUMMARY_MAX {
        return summary.to_string();
    }
    let kept: String = summary.chars().take(SUMMARY_MAX - 3).collect();
    format!("{kept}...")
}

/// Fold any newline or tab in a recovered argument into a space: one event
/// is one transcript line, and a path or command with a newline in it must
/// not be able to forge a second one.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ")
}

/// The word shown for a risk level in an approval question.
fn risk_word(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Safe => "safe",
        RiskLevel::Risky => "risky",
        RiskLevel::Destructive => "destructive",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::{Event, EventKind, RiskLevel, ToolCall};

    fn ev(kind: EventKind) -> Event {
        Event::new("run-1", "sess-1", kind)
    }

    fn texts(state: &mut TranscriptState, kind: EventKind) -> Vec<String> {
        state
            .on_event(&ev(kind))
            .into_iter()
            .map(|l| l.text)
            .collect()
    }

    #[test]
    fn routing_reads_as_a_routing_line() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::RoutingDecisionMade {
                router: "needle".into(),
                selected_model: "qwen3-coder".into(),
                confidence: 0.913,
                fallback_used: false,
                reason: String::new(),
            },
        );
        assert_eq!(out, vec!["  - routing: needle -> qwen3-coder (conf 0.91)"]);
    }

    #[test]
    fn a_fallback_is_marked_and_a_reason_is_kept() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::RoutingDecisionMade {
                router: "static".into(),
                selected_model: "qwen3-coder".into(),
                confidence: 0.5,
                fallback_used: true,
                reason: "needle declined".into(),
            },
        );
        assert_eq!(
            out,
            vec!["  - routing: static -> qwen3-coder (conf 0.50) [fallback] - needle declined"]
        );
    }

    /// The product's headline claim must read as itself, not as routing.
    #[test]
    fn the_needle_fast_path_reads_as_an_on_device_call() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::RoutingDecisionMade {
                router: "needle-dispatch".into(),
                selected_model: "none".into(),
                confidence: 0.94,
                fallback_used: false,
                reason: "needle filled and dispatched `read_file` on device; no model call".into(),
            },
        );
        assert_eq!(
            out,
            vec!["  - on-device: needle called read_file directly (conf 0.94, no model call)"]
        );
    }

    /// The reason is a log string, so the tool name may not be recoverable.
    /// The claim still has to read as the fast path rather than as routing
    /// to a model called "none".
    #[test]
    fn an_on_device_call_without_a_recoverable_tool_still_reads_as_on_device() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::RoutingDecisionMade {
                router: "needle-dispatch".into(),
                selected_model: "none".into(),
                confidence: 0.94,
                fallback_used: false,
                reason: String::new(),
            },
        );
        assert_eq!(
            out,
            vec!["  - on-device: needle called a tool directly (conf 0.94, no model call)"]
        );
    }

    #[test]
    fn a_tool_call_renders_its_friendly_arguments() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: r#"{"path":"src/main.rs"}"#.into(),
            },
        );
        assert_eq!(out, vec!["  * read_file src/main.rs"]);
    }

    /// Review Focus 5: the summary is cut mid-content, and the path still
    /// has to appear.
    #[test]
    fn a_truncated_write_still_names_the_file() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::ToolCallRequested {
                tool: "write_file".into(),
                args_summary: r#"{"path":"notes.txt","content":"a very long body that got c"#
                    .into(),
            },
        );
        assert_eq!(out, vec!["  * write_file notes.txt"]);
    }

    /// Each tool is about a different argument (§4.3), and the graph tools'
    /// is a query rather than a path, so it is quoted.
    #[test]
    fn every_builtin_tool_names_the_argument_it_is_about() {
        let cases = [
            (
                "read_file",
                r#"{"path":"src/main.rs"}"#,
                "read_file src/main.rs",
            ),
            (
                "edit_file",
                r#"{"path":"a.rs","old":"x","new":"y"}"#,
                "edit_file a.rs",
            ),
            (
                "delete_file",
                r#"{"path":"junk.txt"}"#,
                "delete_file junk.txt",
            ),
            (
                "run_command",
                r#"{"command":"cargo test","args":[]}"#,
                "run_command cargo test",
            ),
            (
                "graph_context",
                r#"{"query":"the parser"}"#,
                "graph_context \"the parser\"",
            ),
            (
                "graph_grep",
                r#"{"pattern":"parse"}"#,
                "graph_grep \"parse\"",
            ),
        ];
        for (tool, args, expected) in cases {
            let mut s = TranscriptState::new();
            let out = texts(
                &mut s,
                EventKind::ToolCallRequested {
                    tool: tool.into(),
                    args_summary: args.into(),
                },
            );
            assert_eq!(out, vec![format!("  * {expected}")], "tool {tool}");
        }
    }

    #[test]
    fn an_unreadable_summary_is_ellipsized_not_dropped() {
        let mut s = TranscriptState::new();
        let long = "x".repeat(120);
        let out = texts(
            &mut s,
            EventKind::ToolCallRequested {
                tool: "mystery_tool".into(),
                args_summary: long,
            },
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("  * mystery_tool xxx"), "{:?}", out[0]);
        assert!(out[0].ends_with("..."), "{:?}", out[0]);
        assert!(
            out[0].len() <= 4 + "mystery_tool".len() + 1 + 60,
            "{:?}",
            out[0]
        );
    }

    /// A tool whose arguments are empty gets no trailing space: the line is
    /// the tool's name and nothing else.
    #[test]
    fn a_call_with_no_recoverable_arguments_is_just_its_name() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: String::new(),
            },
        );
        assert_eq!(out, vec!["  * read_file"]);
    }

    #[test]
    fn tool_started_is_silent_and_completion_reports_elapsed_time() {
        let mut s = TranscriptState::new();
        assert!(
            texts(
                &mut s,
                EventKind::ToolStarted {
                    name: "read_file".into()
                }
            )
            .is_empty()
        );
        let out = texts(
            &mut s,
            EventKind::ToolCompleted {
                name: "read_file".into(),
                success: true,
            },
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("    -> ok ("), "{:?}", out[0]);
        assert!(out[0].ends_with(" ms)"), "{:?}", out[0]);
    }

    #[test]
    fn a_failed_tool_says_so() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::ToolCompleted {
                name: "run_command".into(),
                success: false,
            },
        );
        assert!(out[0].starts_with("    -> failed ("), "{:?}", out[0]);
    }

    /// A failure shares the `    -> ` slot with a success — same place in
    /// the flow — and differs in style, which is the writer's cue to colour
    /// it.
    #[test]
    fn a_failure_keeps_the_result_gutter_and_changes_style() {
        let mut s = TranscriptState::new();
        let lines = s.on_event(&ev(EventKind::ToolCompleted {
            name: "run_command".into(),
            success: false,
        }));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style, crate::io::Style::Bad);
    }

    #[test]
    fn approval_and_its_verdict_are_both_visible() {
        let mut s = TranscriptState::new();
        let asked = texts(
            &mut s,
            EventKind::ApprovalRequested {
                command: "write notes.txt".into(),
                risk: RiskLevel::Risky,
            },
        );
        assert_eq!(
            asked,
            vec![
                "  ! approval needed: write notes.txt (risky) \
             - y to approve, anything else denies"
            ]
        );
        let decided = texts(
            &mut s,
            EventKind::ApprovalDecided {
                command: "write notes.txt".into(),
                approved: false,
            },
        );
        assert_eq!(decided, vec!["    -> denied"]);
    }

    #[test]
    fn an_approval_that_was_granted_says_so() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::ApprovalDecided {
                command: "rm -rf build".into(),
                approved: true,
            },
        );
        assert_eq!(out, vec!["    -> approved"]);
    }

    #[test]
    fn every_risk_level_has_a_word() {
        for (risk, word) in [
            (RiskLevel::Safe, "safe"),
            (RiskLevel::Risky, "risky"),
            (RiskLevel::Destructive, "destructive"),
        ] {
            let mut s = TranscriptState::new();
            let out = texts(
                &mut s,
                EventKind::ApprovalRequested {
                    command: "c".into(),
                    risk,
                },
            );
            assert!(out[0].contains(&format!("({word})")), "{:?}", out[0]);
        }
    }

    #[test]
    fn assistant_text_is_a_bare_block_and_is_recorded_as_rendered() {
        let mut s = TranscriptState::new();
        assert!(!s.rendered_assistant_text());
        let out = texts(
            &mut s,
            EventKind::AssistantMessage {
                text: "the parser is recursive-descent".into(),
                tool_calls: Vec::new(),
            },
        );
        assert_eq!(
            out.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["", "the parser is recursive-descent", ""]
        );
        assert!(
            s.rendered_assistant_text(),
            "the answer-once rule depends on this"
        );
    }

    /// A paragraph is many lines, and the writer writes one line at a time,
    /// so the block is split here rather than handed over with embedded
    /// newlines.
    #[test]
    fn a_multi_line_answer_is_one_line_per_line() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::AssistantMessage {
                text: "first\nsecond".into(),
                tool_calls: Vec::new(),
            },
        );
        assert_eq!(
            out.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["", "first", "second", ""]
        );
    }

    /// The fast path's assistant record has no text, so the run outcome is
    /// what the driver prints (see `rendered_assistant_text`).
    #[test]
    fn an_empty_assistant_message_renders_nothing_and_counts_as_nothing() {
        let mut s = TranscriptState::new();
        let out = texts(
            &mut s,
            EventKind::AssistantMessage {
                text: String::new(),
                tool_calls: vec![ToolCall::new(
                    "c1",
                    "read_file",
                    serde_json::json!({"path": "x"}),
                )],
            },
        );
        assert!(out.is_empty());
        assert!(!s.rendered_assistant_text());
    }

    #[test]
    fn replay_and_bookkeeping_kinds_render_nothing() {
        let mut s = TranscriptState::new();
        for kind in [
            EventKind::RunStarted {
                provider: "p".into(),
                model: "m".into(),
                prompt: "x".into(),
            },
            EventKind::ToolResult {
                call_id: "c1".into(),
                tool: "read_file".into(),
                output: "a".repeat(4096),
                is_error: false,
            },
            EventKind::TurnCompleted { turn: 1 },
            EventKind::InputReceived {
                message: "y".into(),
            },
            EventKind::Completed {
                summary: "truncated to eighty chars".into(),
            },
        ] {
            assert!(texts(&mut s, kind).is_empty(), "this kind must stay silent");
        }
    }

    #[test]
    fn errors_and_cancellation_are_loud() {
        let mut s = TranscriptState::new();
        assert_eq!(
            texts(
                &mut s,
                EventKind::Error {
                    message: "model endpoint returned 401".into()
                }
            ),
            vec!["  ! error: model endpoint returned 401"]
        );
        assert_eq!(
            texts(
                &mut s,
                EventKind::Cancelled {
                    reason: "cancelled by user".into()
                }
            ),
            vec!["  ! cancelled"]
        );
    }

    /// The v1 `Note` kind is still in old logs, and replay reads them.
    #[test]
    fn a_v1_note_is_a_meta_line() {
        let mut s = TranscriptState::new();
        assert_eq!(
            texts(
                &mut s,
                EventKind::Note {
                    message: "resumed from sess-0".into()
                }
            ),
            vec!["  - resumed from sess-0"]
        );
    }

    #[test]
    fn skills_file_changes_and_forks_each_have_their_line() {
        let mut s = TranscriptState::new();
        assert_eq!(
            texts(
                &mut s,
                EventKind::SkillActivated {
                    name: "tdd".into(),
                    path: "/p/SKILL.md".into()
                }
            ),
            vec!["  - skill: tdd"]
        );
        assert_eq!(
            texts(
                &mut s,
                EventKind::FileChanged {
                    path: "src/main.rs".into()
                }
            ),
            vec!["    -> wrote src/main.rs"]
        );
        assert_eq!(
            texts(
                &mut s,
                EventKind::SessionForked {
                    from_session: "sess-0".into(),
                    at_position: 18
                }
            ),
            vec!["  - forked from sess-0 at position 18"]
        );
    }

    #[test]
    fn no_rendered_line_is_ever_non_ascii() {
        let mut s = TranscriptState::new();
        for kind in [
            EventKind::SkillActivated {
                name: "tdd".into(),
                path: "/p/SKILL.md".into(),
            },
            EventKind::FileChanged {
                path: "src/main.rs".into(),
            },
            EventKind::SessionForked {
                from_session: "sess-0".into(),
                at_position: 18,
            },
        ] {
            for line in s.on_event(&ev(kind)) {
                assert!(line.text.is_ascii(), "non-ascii in {:?}", line.text);
            }
        }
    }

    /// The footer counts what happened, in a form no terminal width can
    /// break: one short line, no columns.
    #[test]
    fn the_footer_counts_turns_and_tool_calls() {
        let mut s = TranscriptState::new();
        for _ in 0..3 {
            let _ = s.on_event(&ev(EventKind::ToolCallRequested {
                tool: "read_file".into(),
                args_summary: r#"{"path":"a"}"#.into(),
            }));
        }
        for turn in 1..=2 {
            let _ = s.on_event(&ev(EventKind::TurnCompleted { turn }));
        }
        let footer = s.footer();
        assert!(
            footer.text.starts_with("  = 2 turns, 3 tool calls, "),
            "{:?}",
            footer.text
        );
        assert!(footer.text.ends_with('s'), "{:?}", footer.text);
        assert!(footer.text.len() <= 48, "no rule may exceed 48 chars");
    }

    #[test]
    fn the_footer_counts_one_of_each_in_the_singular() {
        let mut s = TranscriptState::new();
        let _ = s.on_event(&ev(EventKind::ToolCallRequested {
            tool: "read_file".into(),
            args_summary: r#"{"path":"a"}"#.into(),
        }));
        let _ = s.on_event(&ev(EventKind::TurnCompleted { turn: 1 }));
        assert!(
            s.footer().text.starts_with("  = 1 turn, 1 tool call, "),
            "{:?}",
            s.footer().text
        );
    }
}
