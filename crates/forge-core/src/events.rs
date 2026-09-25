use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::execution::RiskLevel;

/// Current event schema version.
///
/// * v2 added `seq` (monotonic per run), `f64` routing confidence, and
///   tool/approval/turn event kinds.
/// * v3 adds the *replay* kinds — [`EventKind::AssistantMessage`],
///   [`EventKind::ToolResult`] and [`EventKind::SessionForked`] — so a
///   session's model conversation can be reconstructed verbatim.
///
/// The change is purely additive: no existing kind or field changed
/// meaning, so v1 and v2 logs remain readable (missing `seq` deserializes
/// to 0, a missing `prompt`/`reason` to `""`). A v3 log simply carries
/// event kinds an older reader does not know; runs recorded before v3
/// replay as well as their data allows (see
/// `forge_runtime::replay::conversation_from_events`).
pub const EVENT_SCHEMA_VERSION: u32 = 3;

/// Cap on the tool output stored in an [`EventKind::ToolResult`].
///
/// A tool can return megabytes (a big `read_file`, a chatty command) and
/// the session log is append-only, so storing results verbatim without a
/// bound would let one run make a session file unreadable. Past the cap
/// the output is cut and an explicit marker is appended, so a replayed
/// conversation is *visibly* partial rather than quietly wrong.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// Cut `output` to [`MAX_TOOL_OUTPUT_BYTES`], appending an explicit marker
/// when anything was dropped. Cuts on a char boundary — a truncated log
/// line still has to be valid UTF-8 JSON.
pub fn cap_tool_output(output: &str) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output.to_string();
    }
    let mut cut = MAX_TOOL_OUTPUT_BYTES;
    while cut > 0 && !output.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = output.len() - cut;
    format!(
        "{}\n[forge: tool output truncated, {dropped} bytes dropped]",
        &output[..cut]
    )
}

/// One append-only entry in a run/session event stream.
///
/// `seq` is assigned by the session store on append (see
/// `JsonlSessionStore`): the next monotonic sequence number per run,
/// starting at 1. Events built in memory carry `seq: 0` until stored;
/// events read from v1 logs also have `seq: 0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Event schema version; written as [`EVENT_SCHEMA_VERSION`].
    pub v: u32,
    /// Monotonic per-run sequence number; 0 means "unassigned" (in-memory
    /// or read from a v1 log).
    #[serde(default)]
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub run_id: String,
    pub session_id: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(run_id: impl Into<String>, session_id: impl Into<String>, kind: EventKind) -> Self {
        Self {
            v: EVENT_SCHEMA_VERSION,
            seq: 0,
            ts: Utc::now(),
            run_id: run_id.into(),
            session_id: session_id.into(),
            kind,
        }
    }

    /// Sequence number, falling back to the event's position when reading
    /// v1 logs whose events have no `seq`.
    pub fn seq_or_index(&self, index: usize) -> u64 {
        if self.seq == 0 {
            index as u64
        } else {
            self.seq
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    RunStarted {
        provider: String,
        model: String,
        /// The original prompt (v2+; v1 logs deserialize with "").
        #[serde(default)]
        prompt: String,
    },
    RoutingDecisionMade {
        router: String,
        selected_model: String,
        confidence: f64,
        fallback_used: bool,
        /// Why this model was selected ("" for older logs).
        #[serde(default)]
        reason: String,
    },
    SkillActivated {
        name: String,
        path: PathBuf,
    },
    ToolStarted {
        name: String,
    },
    ToolCompleted {
        name: String,
        success: bool,
    },
    /// The model requested a tool call. `args_summary` is a short,
    /// redaction-safe summary of the arguments.
    ToolCallRequested {
        tool: String,
        args_summary: String,
    },
    FileChanged {
        path: PathBuf,
    },
    /// Approval was requested for a risky/destructive operation.
    ApprovalRequested {
        command: String,
        risk: RiskLevel,
    },
    ApprovalDecided {
        command: String,
        approved: bool,
    },
    /// One agent-loop turn (model call + tool dispatch) finished.
    TurnCompleted {
        turn: u32,
    },
    /// Run-scoped client input recorded via `POST /v1/runs/:id/input`
    /// (v1 name; superseded by `InputReceived`, kept for old logs).
    Note {
        message: String,
    },
    /// User input delivered to a run (v2 name).
    InputReceived {
        message: String,
    },
    Error {
        message: String,
    },
    Cancelled {
        reason: String,
    },
    Completed {
        summary: String,
    },

    // --- v3: replay kinds ------------------------------------------------
    //
    // The `tool_*` and `completed` kinds above are the *observability*
    // stream: short, redaction-safe summaries an editor or a human reads.
    // The three kinds below are the *replay* stream: the verbatim model
    // conversation, written so a later run can reconstruct the history
    // exactly as the model saw it. Keeping them separate is deliberate —
    // every pre-v3 consumer keeps reading exactly what it read before.
    /// One assistant response, verbatim: its text and the tool calls it
    /// requested (`tool_calls` empty for a plain answer). Emitted for every
    /// model response, including the final one — where `Completed.summary`
    /// stays the 80-character digest and this carries the whole answer.
    AssistantMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<crate::tool::ToolCall>,
    },
    /// A tool's output as the model saw it, keyed by the call it answers.
    /// Capped by [`cap_tool_output`].
    ToolResult {
        call_id: String,
        tool: String,
        output: String,
        is_error: bool,
    },
    /// Provenance of a session created by `forge session fork`: the source
    /// session and the 1-based position of the last copied source event.
    SessionForked {
        from_session: String,
        at_position: u64,
    },
}

impl EventKind {
    /// Terminal events end a run: SSE streams close after one of these.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Error { .. } | Self::Cancelled { .. } | Self::Completed { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_kinds_serialize_snake_case_and_roundtrip() {
        let assistant = Event::new(
            "r",
            "s",
            EventKind::AssistantMessage {
                text: "thinking".into(),
                tool_calls: vec![crate::tool::ToolCall::new(
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "a.rs"}),
                )],
            },
        );
        let value = serde_json::to_value(&assistant).expect("serialize");
        assert_eq!(value["type"], "assistant_message");
        assert_eq!(value["v"], 3);
        assert_eq!(value["tool_calls"][0]["name"], "read_file");

        let result = Event::new(
            "r",
            "s",
            EventKind::ToolResult {
                call_id: "call_1".into(),
                tool: "read_file".into(),
                output: "fn main() {}".into(),
                is_error: false,
            },
        );
        assert_eq!(
            serde_json::to_value(&result).expect("serialize")["type"],
            "tool_result"
        );

        let forked = Event::new(
            "r",
            "s",
            EventKind::SessionForked {
                from_session: "src".into(),
                at_position: 7,
            },
        );
        let value = serde_json::to_value(&forked).expect("serialize");
        assert_eq!(value["type"], "session_forked");
        assert_eq!(value["at_position"], 7);

        // Every new kind reads back from its own line.
        for event in [assistant, result, forked] {
            let line = serde_json::to_string(&event).expect("ser");
            let back: Event = serde_json::from_str(&line).expect("de");
            assert_eq!(back.v, EVENT_SCHEMA_VERSION);
        }
    }

    #[test]
    fn an_assistant_message_without_tool_calls_omits_the_field() {
        let event = Event::new(
            "r",
            "s",
            EventKind::AssistantMessage {
                text: "done".into(),
                tool_calls: Vec::new(),
            },
        );
        let value = serde_json::to_value(&event).expect("serialize");
        assert!(value.get("tool_calls").is_none(), "got: {value}");
        // ...and reads back as an empty list.
        let back: Event = serde_json::from_value(value).expect("de");
        assert!(matches!(
            back.kind,
            EventKind::AssistantMessage { ref tool_calls, .. } if tool_calls.is_empty()
        ));
    }

    #[test]
    fn tool_output_is_capped_with_an_explicit_marker() {
        let short = "small output";
        assert_eq!(cap_tool_output(short), short);

        let long = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 500);
        let capped = cap_tool_output(&long);
        assert!(capped.len() < long.len());
        assert!(
            capped.contains("[forge: tool output truncated, 500 bytes dropped]"),
            "missing marker: {}",
            &capped[capped.len().saturating_sub(80)..]
        );

        // A multi-byte char straddling the cap must not be split.
        let wide = "é".repeat(MAX_TOOL_OUTPUT_BYTES);
        let capped = cap_tool_output(&wide);
        assert!(capped.starts_with('é'));
        assert!(capped.contains("truncated"));
    }

    #[test]
    fn event_serializes_with_the_current_schema_and_snake_case_type() {
        let event = Event::new(
            "run-1",
            "session-1",
            EventKind::RoutingDecisionMade {
                router: "static".into(),
                selected_model: "mock-local".into(),
                confidence: 1.0,
                fallback_used: false,
                reason: "test".into(),
            },
        );
        let value = serde_json::to_value(&event).expect("serialize");
        assert_eq!(value["v"], EVENT_SCHEMA_VERSION);
        assert_eq!(value["type"], "routing_decision_made");
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["session_id"], "session-1");
        assert_eq!(value["router"], "static");
        assert_eq!(value["fallback_used"], false);
        assert_eq!(value["confidence"], 1.0);
    }

    #[test]
    fn v1_log_line_without_seq_reads_with_seq_zero() {
        // A v1 line: no `seq`, and confidence serialized from f64-clean f32.
        let line = "{\"v\":1,\"ts\":\"2026-09-22T20:01:39.172579Z\",\"run_id\":\"r\",\"session_id\":\"s\",\"type\":\"routing_decision_made\",\"router\":\"static\",\"selected_model\":\"mock-local\",\"confidence\":0.9,\"fallback_used\":false}";
        let event: Event = serde_json::from_str(line).expect("v1 line parses");
        assert_eq!(event.v, 1);
        assert_eq!(event.seq, 0);
        assert_eq!(event.seq_or_index(7), 7);
        match &event.kind {
            EventKind::RoutingDecisionMade { confidence, .. } => {
                assert!((confidence - 0.9).abs() < 0.001);
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn event_roundtrips_through_jsonl_lines() {
        let mut event = Event::new(
            "run-2",
            "session-2",
            EventKind::FileChanged {
                path: PathBuf::from("src/main.rs"),
            },
        );
        event.seq = 3;
        let line = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(back.v, EVENT_SCHEMA_VERSION);
        assert_eq!(back.seq, 3);
        assert_eq!(back.seq_or_index(0), 3);
        assert!(matches!(back.kind, EventKind::FileChanged { .. }));
    }

    #[test]
    fn new_v2_variants_serialize_snake_case() {
        for (kind, expected) in [
            (
                EventKind::ToolCallRequested {
                    tool: "write_file".into(),
                    args_summary: "path=src/main.rs".into(),
                },
                "tool_call_requested",
            ),
            (
                EventKind::ApprovalRequested {
                    command: "rm -rf x".into(),
                    risk: RiskLevel::Destructive,
                },
                "approval_requested",
            ),
            (
                EventKind::ApprovalDecided {
                    command: "rm -rf x".into(),
                    approved: false,
                },
                "approval_decided",
            ),
            (EventKind::TurnCompleted { turn: 2 }, "turn_completed"),
            (
                EventKind::InputReceived {
                    message: "hi".into(),
                },
                "input_received",
            ),
        ] {
            let event = Event::new("r", "s", kind);
            let value = serde_json::to_value(&event).expect("serialize");
            assert_eq!(value["type"], expected);
        }
        // RiskLevel round-trips snake_case inside the variant.
        let event = Event::new(
            "r",
            "s",
            EventKind::ApprovalRequested {
                command: "x".into(),
                risk: RiskLevel::Destructive,
            },
        );
        assert_eq!(
            serde_json::to_value(&event).expect("ser")["risk"],
            "destructive"
        );
    }
}
