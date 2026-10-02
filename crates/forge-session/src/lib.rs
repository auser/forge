//! Append-only JSONL session/run store. One file per session under the
//! store root (`<project>/.forge/sessions/`), one JSON event per line.
//! Secret-looking values are redacted before anything is written.

pub mod decisions;
mod redact;
mod store;

pub use decisions::{
    Decider, DecisionLog, DecisionLogHandle, DecisionRecord, Outcome, RecordDraft, SpendTotals,
    Stage, scan_spend, scan_spend_today,
};
pub use store::{JsonlSessionStore, SessionInfo, new_run_id, new_session_id};
