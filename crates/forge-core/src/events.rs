use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current version of the event protocol (`Event.v`).
pub const EVENT_PROTOCOL_VERSION: u32 = 1;

/// One append-only entry in a run/session event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Event protocol version; currently always [`EVENT_PROTOCOL_VERSION`].
    pub v: u32,
    pub ts: DateTime<Utc>,
    pub run_id: String,
    pub session_id: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(run_id: impl Into<String>, session_id: impl Into<String>, kind: EventKind) -> Self {
        Self {
            v: EVENT_PROTOCOL_VERSION,
            ts: Utc::now(),
            run_id: run_id.into(),
            session_id: session_id.into(),
            kind,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    RunStarted {
        provider: String,
        model: String,
    },
    RoutingDecisionMade {
        router: String,
        selected_model: String,
        confidence: f32,
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
    FileChanged {
        path: PathBuf,
    },
    /// Run-scoped client input recorded via `POST /v1/runs/:id/input`.
    /// Not a terminal event.
    Note {
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
    fn event_serializes_with_version_and_snake_case_type() {
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
        assert_eq!(value["v"], 1);
        assert_eq!(value["type"], "routing_decision_made");
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["session_id"], "session-1");
        assert_eq!(value["router"], "static");
        assert_eq!(value["fallback_used"], false);
    }

    #[test]
    fn event_roundtrips_through_jsonl_lines() {
        let event = Event::new(
            "run-2",
            "session-2",
            EventKind::FileChanged {
                path: PathBuf::from("src/main.rs"),
            },
        );
        let line = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(back.v, EVENT_PROTOCOL_VERSION);
        assert!(matches!(back.kind, EventKind::FileChanged { .. }));
    }
}
