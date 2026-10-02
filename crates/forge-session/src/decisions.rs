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
    /// A model answered: the record carries the completion's usage and
    /// cost, which is what spend budgets are enforced against.
    Complete,
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
    Unavailable,
    /// The model returned a completion (`Complete` records).
    Answered,
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
    /// Token usage the provider reported for a `Complete` record; `None`
    /// when it reported none (and always for `Decide`/`Route` records).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<forge_core::Usage>,
    /// USD cost computed from the resolved model entry's per-million-token
    /// prices. `None` when usage or the price is missing — a missing price
    /// must never be written as 0.0, which would read as "known free".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
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
            usage: None,
            cost_usd: None,
        };
        if let Err(e) = self.log.append(&record) {
            tracing::warn!(error = %e, "could not append to the decision log");
        }
    }

    /// Record one model completion's usage and cost. Same never-fail
    /// contract as [`record`](Self::record): spend accounting must not be
    /// able to take a turn down.
    pub fn record_usage(
        &self,
        turn: u32,
        model: &str,
        usage: Option<forge_core::Usage>,
        cost_usd: Option<f64>,
        elapsed_ms: u64,
    ) {
        let record = DecisionRecord {
            ts: Utc::now(),
            session: self.session.clone(),
            turn,
            stage: Stage::Complete,
            // Completing is not a decision: nothing chose anything here, so
            // the decider is None like any other absent engine.
            decider: Decider::None,
            question: "usage".to_string(),
            choice: model.to_string(),
            confidence: None,
            probabilities: BTreeMap::new(),
            candidates: Vec::new(),
            outcome: Outcome::Answered,
            elapsed_ms,
            speculative: false,
            usage,
            cost_usd,
        };
        if let Err(e) = self.log.append(&record) {
            tracing::warn!(error = %e, "could not append to the decision log");
        }
    }
}

/// Token/USD totals accumulated from `Complete` records. A cost that was
/// never recorded simply does not accrue, so a local (zero-priced) model
/// leaves `cost_usd` at 0.0 — which is true, not a placeholder.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SpendTotals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl SpendTotals {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    fn add(&mut self, record: &DecisionRecord) {
        self.calls += 1;
        if let Some(usage) = record.usage {
            self.input_tokens += u64::from(usage.prompt_tokens);
            self.output_tokens += u64::from(usage.completion_tokens);
        }
        if let Some(cost) = record.cost_usd {
            self.cost_usd += cost;
        }
    }
}

/// Sum the `Complete` records under `root` (one `<session>.decisions.jsonl`
/// per session), twice: once scoped to `session_id` (skip with `None`) and
/// once to records timestamped on `today` (UTC). Returns `(session, daily)`.
///
/// Tolerant by construction — this feeds spend ceilings, and a log that
/// cannot be read must not make spend unaccountable *or* take the run down:
/// an unreadable directory reads as empty, an unreadable file as skipped,
/// and a malformed or truncated line (normal for a log that was being
/// written when the process died) as skipped.
pub fn scan_spend(
    root: &Path,
    session_id: Option<&str>,
    today: chrono::NaiveDate,
) -> (SpendTotals, SpendTotals) {
    let mut session = SpendTotals::default();
    let mut daily = SpendTotals::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (session, daily);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".decisions.jsonl") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in raw.lines() {
            let Ok(record) = serde_json::from_str::<DecisionRecord>(line) else {
                continue;
            };
            if record.stage != Stage::Complete {
                continue;
            }
            if session_id == Some(record.session.as_str()) {
                session.add(&record);
            }
            if record.ts.date_naive() == today {
                daily.add(&record);
            }
        }
    }
    (session, daily)
}

/// [`scan_spend`] scoped to today (UTC) — the window `daily_usd` ceilings
/// and `forge doctor` report against.
pub fn scan_spend_today(root: &Path, session_id: Option<&str>) -> (SpendTotals, SpendTotals) {
    scan_spend(root, session_id, Utc::now().date_naive())
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
            usage: None,
            cost_usd: None,
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

    #[test]
    fn old_lines_without_usage_or_cost_still_deserialize() {
        // Written before usage accounting existed: the line must parse with
        // the new fields defaulting to None, and re-serialize to the same
        // shape (absent, not null).
        let old = "{\"ts\":\"2026-09-25T03:14:07.113Z\",\"session\":\"s1\",\"turn\":1,\"stage\":\"route\",\"decider\":\"static\",\"question\":\"model\",\"choice\":\"mock\",\"confidence\":1.0,\"outcome\":\"routed\",\"elapsed_ms\":3,\"speculative\":false}";
        let record: DecisionRecord = serde_json::from_str(old).expect("old line parses");
        assert_eq!(record.stage, Stage::Route);
        assert_eq!(record.usage, None);
        assert_eq!(record.cost_usd, None);
        let line = serde_json::to_string(&record).expect("serializes");
        assert!(!line.contains("usage"), "absent stays absent: {line}");
        assert!(!line.contains("cost_usd"), "absent stays absent: {line}");
    }

    #[test]
    fn a_complete_record_carries_usage_and_cost() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(DecisionLog::new(tmp.path().to_path_buf()));
        let handle = DecisionLog::handle(&log, "sess-1");

        handle.record_usage(
            2,
            "gpt-5",
            Some(forge_core::Usage {
                prompt_tokens: 1_000,
                completion_tokens: 500,
                total_tokens: 1_500,
            }),
            Some(0.00625),
            812,
        );

        let raw = std::fs::read_to_string(tmp.path().join("sess-1.decisions.jsonl"))
            .expect("log file exists");
        let record: DecisionRecord = serde_json::from_str(raw.trim()).expect("line parses");
        assert_eq!(record.stage, Stage::Complete);
        assert_eq!(record.outcome, Outcome::Answered);
        assert_eq!(record.choice, "gpt-5");
        assert_eq!(record.usage.expect("usage").total_tokens, 1_500);
        assert_eq!(record.cost_usd, Some(0.00625));
    }

    #[test]
    fn scan_spend_scopes_by_session_and_day_and_skips_bad_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(DecisionLog::new(tmp.path().to_path_buf()));
        let today = Utc::now().date_naive();
        let usage = forge_core::Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
        };

        let s1 = DecisionLog::handle(&log, "s1");
        s1.record_usage(1, "gpt-5", Some(usage), Some(0.01), 1);
        s1.record_usage(2, "gpt-5", Some(usage), None, 1); // no price: no cost
        let s2 = DecisionLog::handle(&log, "s2");
        s2.record_usage(1, "deepseek-chat", Some(usage), Some(0.02), 1);
        // A route record contributes nothing, and neither does a malformed
        // or truncated line.
        s1.record(
            1,
            RecordDraft {
                stage: Stage::Route,
                decider: Decider::Static,
                question: "model".to_string(),
                choice: "gpt-5".to_string(),
                confidence: Some(1.0),
                probabilities: BTreeMap::new(),
                candidates: Vec::new(),
                outcome: Outcome::Routed,
                elapsed_ms: 1,
                speculative: false,
            },
        );
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join("s1.decisions.jsonl"))
            .expect("open");
        writeln!(file, "{{not json").expect("write garbage");
        writeln!(file, "{{\"ts\":\"2026-09-25T03:14:07.113Z\",\"truncated").expect("write torn");

        let yesterday = today - chrono::Duration::days(1);
        let (session, daily) = scan_spend(tmp.path(), Some("s1"), today);
        assert_eq!(session.calls, 2);
        assert_eq!(session.input_tokens, 200);
        assert_eq!(session.output_tokens, 100);
        assert!((session.cost_usd - 0.01).abs() < 1e-12, "{session:?}");
        // Daily spans sessions: all three completions happened today.
        assert_eq!(daily.calls, 3);
        assert!((daily.cost_usd - 0.03).abs() < 1e-12, "{daily:?}");
        // A different day sees nothing.
        let (_, daily_yesterday) = scan_spend(tmp.path(), Some("s1"), yesterday);
        assert_eq!(daily_yesterday, SpendTotals::default());
        // And a missing directory is empty, not an error.
        let (none, _) = scan_spend(&tmp.path().join("nope"), None, today);
        assert_eq!(none, SpendTotals::default());
    }
}
