//! The decision log: what forge decided, how confident it was, and what
//! happened — never what it was decided *about*.
//!
//! Records carry decision *shape* only. Prompt text, tool arguments and file
//! contents are deliberately absent: a record joins to the session transcript
//! by `session` + `turn`, so full context stays recoverable locally without the
//! log inheriting the sensitivity of any secret that appeared in a file the
//! agent read.
//!
//! The whole probability vector is kept, not just the winner. That is what
//! makes thresholds re-derivable from real traffic instead of inherited from
//! someone else's benchmark.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where in a turn a decision was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Choosing a tool (and filling it) on device.
    Decide,
    /// Choosing which model answers.
    Route,
    /// The model call itself.
    Generate,
}

/// Who decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decider {
    Needle,
    Static,
    Llm,
    /// No decider was available — recorded so an absent engine is visible in
    /// the data rather than showing up as missing rows.
    None,
}

/// What came of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Dispatched,
    Declined,
    Routed,
    Errored,
    Unavailable,
}

/// One decision. Serializes to exactly one JSONL line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub ts: DateTime<Utc>,
    pub session: String,
    pub turn: u32,
    pub stage: Stage,
    pub decider: Decider,
    /// Which question was answered: "tool", "model", …
    pub question: String,
    pub choice: String,
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probabilities: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
    /// True when the answer was fetched concurrently and discarded. Always
    /// false in A1; the field exists because A2 will set it.
    pub speculative: bool,
}

/// Everything about a decision except the parts the log fills in itself.
#[derive(Debug, Clone)]
pub struct RecordDraft {
    pub stage: Stage,
    pub decider: Decider,
    pub question: String,
    pub choice: String,
    pub confidence: Option<f64>,
    pub probabilities: BTreeMap<String, f64>,
    pub candidates: Vec<String>,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
    pub speculative: bool,
}

/// Runtime-scoped, append-only. One log spans every session and every surface,
/// which is what lets thresholds be fitted from enough traffic to mean
/// something; records carry a `session` so rows stay attributable.
///
/// The mutex guards the append, not the file: two handles writing at once must
/// not interleave half-lines.
pub struct DecisionLog {
    root: PathBuf,
    write_lock: Mutex<()>,
}

impl DecisionLog {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            write_lock: Mutex::new(()),
        }
    }

    /// A handle bound to one session. Cheap to clone.
    pub fn handle(log: &Arc<Self>, session_id: &str) -> DecisionLogHandle {
        DecisionLogHandle {
            log: Arc::clone(log),
            session: session_id.to_string(),
        }
    }

    fn append(&self, record: &DecisionRecord) -> std::io::Result<()> {
        use std::io::Write;

        let path = self.path_for(&record.session);
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');

        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(line.as_bytes())
    }

    /// `<root>/<session>.decisions.jsonl`, beside the transcript.
    ///
    /// The session id is reduced to its final path component so a crafted id
    /// cannot write outside `root`.
    fn path_for(&self, session_id: &str) -> PathBuf {
        let safe = Path::new(session_id)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty() && s != "." && s != "..")
            .unwrap_or_else(|| "unknown-session".to_string());
        self.root.join(format!("{safe}.decisions.jsonl"))
    }
}

/// A session's view of the shared log.
#[derive(Clone)]
pub struct DecisionLogHandle {
    log: Arc<DecisionLog>,
    session: String,
}

impl DecisionLogHandle {
    /// Record a decision. Never fails a turn: a log that cannot be written is a
    /// warning, because losing a measurement is strictly better than losing the
    /// work being measured.
    pub fn record(&self, turn: u32, draft: RecordDraft) {
        let record = DecisionRecord {
            ts: Utc::now(),
            session: self.session.clone(),
            turn,
            stage: draft.stage,
            decider: draft.decider,
            question: draft.question,
            choice: draft.choice,
            confidence: draft.confidence,
            probabilities: draft.probabilities,
            candidates: draft.candidates,
            outcome: draft.outcome,
            elapsed_ms: draft.elapsed_ms,
            speculative: draft.speculative,
        };
        if let Err(e) = self.log.append(&record) {
            tracing::warn!(error = %e, "could not append to the decision log");
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_serializes_to_one_json_line_with_no_free_text() {
        let record = DecisionRecord {
            ts: "2026-09-25T03:14:07.113Z".parse().expect("timestamp"),
            session: "01M3B5BG88KCD5QBAG92VCZNKG".to_string(),
            turn: 3,
            stage: Stage::Decide,
            decider: Decider::Needle,
            question: "tool".to_string(),
            choice: "graph_grep".to_string(),
            confidence: Some(0.91),
            probabilities: BTreeMap::from([
                ("graph_grep".to_string(), 0.91),
                ("read_file".to_string(), 0.09),
            ]),
            candidates: vec!["graph_grep".to_string(), "read_file".to_string()],
            outcome: Outcome::Dispatched,
            elapsed_ms: 1104,
            speculative: false,
        };

        let line = serde_json::to_string(&record).expect("serializes");
        assert!(!line.contains('\n'), "a record must be exactly one line");

        let back: serde_json::Value = serde_json::from_str(&line).expect("round-trips");
        assert_eq!(back["stage"], "decide");
        assert_eq!(back["decider"], "needle");
        assert_eq!(back["outcome"], "dispatched");
        assert_eq!(back["choice"], "graph_grep");
        assert_eq!(back["probabilities"]["graph_grep"], 0.91);
    }

    #[test]
    fn appending_writes_one_line_per_record_to_the_session_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(DecisionLog::new(tmp.path().to_path_buf()));
        let handle = DecisionLog::handle(&log, "sess-1");

        handle.record(
            1,
            RecordDraft {
                stage: Stage::Decide,
                decider: Decider::Needle,
                question: "tool".to_string(),
                choice: "read_file".to_string(),
                confidence: Some(1.0),
                probabilities: BTreeMap::new(),
                candidates: vec!["read_file".to_string()],
                outcome: Outcome::Dispatched,
                elapsed_ms: 12,
                speculative: false,
            },
        );
        handle.record(
            2,
            RecordDraft {
                stage: Stage::Route,
                decider: Decider::Static,
                question: "model".to_string(),
                choice: "mock".to_string(),
                confidence: Some(1.0),
                probabilities: BTreeMap::new(),
                candidates: vec!["mock".to_string()],
                outcome: Outcome::Routed,
                elapsed_ms: 3,
                speculative: false,
            },
        );

        let raw = std::fs::read_to_string(tmp.path().join("sess-1.decisions.jsonl"))
            .expect("log file exists");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record: {raw}");

        let first: DecisionRecord = serde_json::from_str(lines[0]).expect("line 1 parses");
        assert_eq!(first.turn, 1);
        assert_eq!(first.session, "sess-1");
        assert_eq!(first.stage, Stage::Decide);

        let second: DecisionRecord = serde_json::from_str(lines[1]).expect("line 2 parses");
        assert_eq!(second.turn, 2);
        assert_eq!(second.stage, Stage::Route);
    }

    #[test]
    fn an_unwritable_log_warns_and_does_not_panic() {
        // A path whose parent is a *file* can never be a directory, so
        // create_dir_all fails on every platform without needing permissions.
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write blocker");

        let log = Arc::new(DecisionLog::new(blocker.join("nested")));
        let handle = DecisionLog::handle(&log, "sess-1");

        handle.record(
            1,
            RecordDraft {
                stage: Stage::Decide,
                decider: Decider::Needle,
                question: "tool".to_string(),
                choice: "read_file".to_string(),
                confidence: None,
                probabilities: BTreeMap::new(),
                candidates: Vec::new(),
                outcome: Outcome::Declined,
                elapsed_ms: 1,
                speculative: false,
            },
        );
        // Reaching here without panicking is the assertion.
    }

    #[test]
    fn a_session_id_cannot_escape_the_log_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).expect("mkdir");

        let log = Arc::new(DecisionLog::new(root.clone()));
        for hostile in ["../escaped", "..", ".", "a/b/../../../c"] {
            let handle = DecisionLog::handle(&log, hostile);
            handle.record(
                1,
                RecordDraft {
                    stage: Stage::Decide,
                    decider: Decider::Needle,
                    question: "tool".to_string(),
                    choice: "read_file".to_string(),
                    confidence: None,
                    probabilities: BTreeMap::new(),
                    candidates: Vec::new(),
                    outcome: Outcome::Declined,
                    elapsed_ms: 1,
                    speculative: false,
                },
            );
        }

        // Everything written must live directly under `root`.
        for entry in std::fs::read_dir(tmp.path()).expect("read tmp") {
            let entry = entry.expect("entry");
            assert!(
                entry.path() == root,
                "a log file escaped the directory: {}",
                entry.path().display()
            );
        }
    }
}
