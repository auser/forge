use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::execution::RiskLevel;

/// Current event schema version. v2 adds `seq` (monotonic per run),
/// `f64` routing confidence, and tool/approval/turn event kinds. v1 logs
/// remain readable: missing `seq` deserializes to 0.
pub const EVENT_SCHEMA_VERSION: u32 = 2;

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
    fn event_serializes_with_schema_v2_and_snake_case_type() {
        let event = Event::new(
            "run-1",
            "session-1",
            EventKind::RoutingDecisionMade {
                router: "static".into(),
                selected_model: "mock-local".into(),
                confidence: 1.0,
                fallback_used: false,
            },
        );
        let value = serde_json::to_value(&event).expect("serialize");
        assert_eq!(value["v"], 2);
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
