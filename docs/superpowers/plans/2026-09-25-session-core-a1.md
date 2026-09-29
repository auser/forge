# Session Core A1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract a `Session` object, move the on-device dispatch decision above routing, move engine warmth and the decision log to runtime scope, and start recording every routing/dispatch decision to a local JSONL log.

**Architecture:** `AgentService` becomes a factory for `Session`s. State that is genuinely shared — the single non-thread-safe needle engine and the decision log — moves behind handles that all sessions clone, so the ~8 s tool-surface install is paid once per runtime and one log spans every surface. `run_inner`'s turn is reordered so the dispatch decision runs before routing: choosing a model is only meaningful once a model is known to be answering.

**Tech Stack:** Rust 2024, tokio, serde/serde_json, thiserror, tracing. Existing crates: `forge-runtime`, `forge-session`, `forge-needle`, `forge-cli`.

**Spec:** `docs/superpowers/specs/2026-09-25-session-core-design.md` — phase A1 in §21. Read §2 (Session), §4 (decision log), §20 (runtime scope) before starting.

## Global Constraints

- Rust edition 2024; workspace toolchain is stable (see `rust-toolchain` resolution in CI). Do not add dependencies outside the workspace `[workspace.dependencies]` table without adding them there first.
- `just verify` must pass at every commit: `cargo check --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, `just lint-ffi`, `just lint-no-needle`, `cargo test --workspace`, `cargo test -p forge-cli --test bdd`, `cargo fmt --all --check`.
- The decision log records **decision shape only** — never prompt text, tool arguments, or file contents. Records join to the transcript by `session` + `turn`. This is §4 and it is not negotiable.
- Nothing in A1 may fail a turn that would otherwise proceed. A log that cannot be written is a warning, not an error.
- `forge_needle::HAS_EMBEDDED_BACKEND` is the only correct test for "can this build run inference". Never `cfg!(feature = "needle-ffi")`.
- Out of scope for A1, do not implement: `DecisionPlane` trait or any impl, gate or risk-taxonomy changes, approval contract, egress tiers, graph changes, daemon, context budget, earned autonomy, spend budgets.

## Review Focus

Five things the spec implies that no task's happy path exercises. Each has a test pinned to the task that owns the code.

1. **A read-only filesystem or unwritable `.forge/sessions/`** — the log must degrade to a warning and the turn must still complete. (Task 1, Step 9)
2. **Two sessions created from one service, used concurrently** — they must share one engine and one warmth flag, and their records must interleave in the log without corrupting lines. (Task 2 Step 7, Task 5 Step 7)
3. **A build with no engine (`HAS_EMBEDDED_BACKEND == false`)** — `EngineHandle::None`, no dispatch attempt, routing still happens, and a record is still written saying why. (Task 4, Step 9)
4. **A chat-only routed model** — after the reorder the model must still receive zero tools in its completion request, even though needle is now offered the full tool surface. (Task 4, Step 7)
5. **A session id containing a path separator or `..`** — the log path must not escape `.forge/sessions/`. (Task 1, Step 11)

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/forge-session/src/decisions.rs` **(create)** | `DecisionRecord`, `DecisionLog`, `DecisionLogHandle`. Serialization and append-only writing. Knows nothing about agents. |
| `crates/forge-session/src/lib.rs` **(modify)** | Export the above. |
| `crates/forge-runtime/src/engine_handle.rs` **(create)** | `EngineHandle` — the shared engine plus its shared warmth flag. |
| `crates/forge-runtime/src/session.rs` **(create)** | `Session`, and `AgentService::session()` / `resume()`. |
| `crates/forge-runtime/src/service.rs` **(modify)** | Reorder the turn; record decisions; drop `needle_warmed`. |
| `crates/forge-runtime/src/service/tests.rs` **(modify)** | Update tests whose premise the reorder changes. |
| `crates/forge-runtime/src/lib.rs` **(modify)** | Declare and export the new modules. |
| `crates/forge-cli/src/commands/session_cmd.rs` **(modify)** | `forge session decisions` readout — A1's measurement is useless unread. |

---

### Task 1: Decision record and log writer

**Files:**
- Create: `crates/forge-session/src/decisions.rs`
- Modify: `crates/forge-session/src/lib.rs`
- Test: in-file `#[cfg(test)] mod tests` (matches this crate's existing style — see `store.rs`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `DecisionRecord { ts: String, session: String, turn: u32, stage: Stage, decider: Decider, question: String, choice: String, confidence: Option<f64>, probabilities: BTreeMap<String, f64>, candidates: Vec<String>, outcome: Outcome, elapsed_ms: u64, speculative: bool }`
  - `enum Stage { Decide, Route, Generate }`
  - `enum Decider { Needle, Static, Llm, None }`
  - `enum Outcome { Dispatched, Declined, Routed, Errored, Unavailable }`
  - `DecisionLog::new(root: PathBuf) -> DecisionLog`
  - `DecisionLog::handle(&Arc<DecisionLog>, session_id: &str) -> DecisionLogHandle`
  - `DecisionLogHandle::record(&self, turn: u32, partial: RecordDraft)`
  - `RecordDraft { stage, decider, question, choice, confidence, probabilities, candidates, outcome, elapsed_ms, speculative }`

- [ ] **Step 1: Write the failing test for record serialization**

Add to `crates/forge-session/src/decisions.rs` (create the file with just this test module plus `use` lines for now):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_serializes_to_one_json_line_with_no_free_text() {
        let record = DecisionRecord {
            ts: "2026-09-25T03:14:07.113Z".to_string(),
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
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p forge-session decisions:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type DecisionRecord`.

- [ ] **Step 3: Write the types**

Put this above the test module in `crates/forge-session/src/decisions.rs`:

```rust
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
    pub ts: String,
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p forge-session decisions:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Write the failing test for appending**

Add to the same test module:

```rust
    #[test]
    fn appending_writes_one_line_per_record_to_the_session_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(DecisionLog::new(tmp.path().to_path_buf()));
        let handle = DecisionLog::handle(&log, "sess-1");

        handle.record(1, RecordDraft {
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
        });
        handle.record(2, RecordDraft {
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
        });

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
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test -p forge-session decisions::tests::appending 2>&1 | tail -20`
Expected: FAIL — `cannot find type DecisionLog`.

- [ ] **Step 7: Implement the log and handle**

Append to `crates/forge-session/src/decisions.rs`, above the test module:

```rust
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
            ts: now_rfc3339(),
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

fn now_rfc3339() -> String {
    chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
```

- [ ] **Step 8: Wire the module and run the test**

In `crates/forge-session/src/lib.rs`, add `pub mod decisions;` beside the existing module declarations and extend the re-export line:

```rust
pub use decisions::{
    Decider, DecisionLog, DecisionLogHandle, DecisionRecord, Outcome, RecordDraft, Stage,
};
```

Confirm `chrono` and `tracing` are in `crates/forge-session/Cargo.toml`; if either is missing add `chrono.workspace = true` / `tracing.workspace = true`.

Run: `cargo test -p forge-session decisions:: 2>&1 | tail -20`
Expected: PASS, both tests.

- [ ] **Step 9: Write the failing test for an unwritable log (Review Focus 1)**

```rust
    #[test]
    fn an_unwritable_log_warns_and_does_not_panic() {
        // A path whose parent is a *file* can never be a directory, so
        // create_dir_all fails on every platform without needing permissions.
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write blocker");

        let log = Arc::new(DecisionLog::new(blocker.join("nested")));
        let handle = DecisionLog::handle(&log, "sess-1");

        handle.record(1, RecordDraft {
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
        });
        // Reaching here without panicking is the assertion.
    }
```

- [ ] **Step 10: Run it**

Run: `cargo test -p forge-session decisions::tests::an_unwritable 2>&1 | tail -20`
Expected: PASS (the implementation already swallows the error; this test pins that it stays true).

- [ ] **Step 11: Write the failing test for a hostile session id (Review Focus 5)**

```rust
    #[test]
    fn a_session_id_cannot_escape_the_log_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).expect("mkdir");

        let log = Arc::new(DecisionLog::new(root.clone()));
        for hostile in ["../escaped", "..", ".", "a/b/../../../c"] {
            let handle = DecisionLog::handle(&log, hostile);
            handle.record(1, RecordDraft {
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
            });
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
```

- [ ] **Step 12: Run it**

Run: `cargo test -p forge-session decisions:: 2>&1 | tail -20`
Expected: PASS, all four tests.

- [ ] **Step 13: Commit**

```bash
git add crates/forge-session/src/decisions.rs crates/forge-session/src/lib.rs crates/forge-session/Cargo.toml
git commit -m "feat(session): decision log records what forge decided, not what about

Append-only JSONL beside the transcript, carrying decision shape only --
stage, decider, choice, confidence, the whole probability vector, outcome.
Prompt text, tool arguments and file contents are deliberately absent;
records join to the transcript by session and turn.

The full vector rather than just the winner is what makes thresholds
re-derivable from real traffic instead of inherited from someone else's
benchmark.

A log that cannot be written warns rather than failing the turn, and a
session id is reduced to its final path component so a crafted id cannot
write outside the log directory."
```

---

### Task 2: `EngineHandle` — warmth belongs to the engine

**Files:**
- Create: `crates/forge-runtime/src/engine_handle.rs`
- Modify: `crates/forge-runtime/src/lib.rs`
- Test: in-file `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `EngineHandle` (implements `Clone`, `Default`)
  - `EngineHandle::none() -> EngineHandle`
  - `EngineHandle::new(engine: Arc<NeedleEngine>) -> EngineHandle`
  - `EngineHandle::engine(&self) -> Option<&Arc<NeedleEngine>>`
  - `EngineHandle::claim_warmup(&self) -> bool` — returns true exactly once per underlying engine
  - `EngineHandle::is_warm(&self) -> bool`

**Why this task exists:** `AgentService` currently holds `needle_warmed: AtomicBool`. Spec §2 records that this was wrong: the tool surface is installed *into the engine*, so warmth is the engine's property. A per-service (and, later, per-session) flag would charge the ~8 s install again for every new conversation against the same runtime.

- [ ] **Step 1: Write the failing test**

Create `crates/forge-runtime/src/engine_handle.rs` with only this test module plus `use super::*;`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmth_is_shared_between_clones_of_one_handle() {
        let handle = EngineHandle::new(Arc::new(forge_needle::NeedleEngine::spawn(
            forge_needle::HashBackend::new(),
        )));
        let second = handle.clone();

        assert!(!handle.is_warm());
        assert!(handle.claim_warmup(), "first claim wins");
        assert!(
            !second.claim_warmup(),
            "a clone must not be able to claim the same warm-up again"
        );
        assert!(second.is_warm(), "warmth is visible through every clone");
    }

    #[test]
    fn a_handle_with_no_engine_never_claims_a_warmup() {
        let handle = EngineHandle::none();
        assert!(handle.engine().is_none());
        assert!(!handle.claim_warmup(), "nothing to warm");
        assert!(!handle.is_warm());
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p forge-runtime engine_handle:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type EngineHandle`.

- [ ] **Step 3: Implement**

Put above the test module in the same file:

```rust
//! A shared handle to the one needle engine, and to whether its tool surface
//! has been installed yet.
//!
//! `libneedle` is a single process-global, non-thread-safe model, so a runtime
//! has at most one engine and every session borrows it. Warmth lives here
//! rather than on a session because the tool surface is installed *into the
//! engine*: a second conversation against the same runtime must not pay the
//! ~8 s install again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use forge_needle::NeedleEngine;

#[derive(Clone, Default)]
pub struct EngineHandle {
    inner: Option<Inner>,
}

#[derive(Clone)]
struct Inner {
    engine: Arc<NeedleEngine>,
    warmed: Arc<AtomicBool>,
}

impl EngineHandle {
    pub fn new(engine: Arc<NeedleEngine>) -> Self {
        Self {
            inner: Some(Inner {
                engine,
                warmed: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// No engine: this build has none (`HAS_EMBEDDED_BACKEND == false`), or the
    /// caller chose not to attach one.
    pub fn none() -> Self {
        Self { inner: None }
    }

    pub fn engine(&self) -> Option<&Arc<NeedleEngine>> {
        self.inner.as_ref().map(|i| &i.engine)
    }

    /// True for exactly one caller across every clone of this handle. The
    /// winner owns warming the engine; everyone else proceeds as if warm.
    pub fn claim_warmup(&self) -> bool {
        match &self.inner {
            Some(i) => !i.warmed.swap(true, Ordering::SeqCst),
            None => false,
        }
    }

    pub fn is_warm(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.warmed.load(Ordering::SeqCst))
    }
}
```

- [ ] **Step 4: Declare the module**

In `crates/forge-runtime/src/lib.rs` add beside the existing `mod` lines:

```rust
pub mod engine_handle;
pub use engine_handle::EngineHandle;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p forge-runtime engine_handle:: 2>&1 | tail -20`
Expected: PASS, both tests.

- [ ] **Step 6: Replace `needle_warmed` on `AgentService`**

In `crates/forge-runtime/src/service.rs`:

1. Delete the `needle_warmed: AtomicBool` field and its initializer in `AgentService::new`.
2. Replace the `needle: Option<Arc<NeedleEngine>>` field with `needle: EngineHandle`, initialized to `EngineHandle::none()`.
3. Change `with_needle` to:

```rust
    pub fn with_needle(mut self, engine: Option<Arc<NeedleEngine>>) -> Self {
        self.needle = match engine {
            Some(engine) => EngineHandle::new(engine),
            None => EngineHandle::none(),
        };
        self
    }
```

4. In `needle_fast_path`, replace `let engine = self.needle.as_ref()?;` with `let engine = self.needle.engine()?;` and replace `!self.needle_warmed.swap(true, Ordering::SeqCst)` with `self.needle.claim_warmup()`.
5. Remove the now-unused `use std::sync::atomic::{AtomicBool, Ordering};` if nothing else in the file uses it.
6. Add `use crate::EngineHandle;`.

In `crates/forge-runtime/src/service/tests.rs`, `warm_needle_service` currently reaches into `service.needle_warmed`. Replace its body's store with:

```rust
    let service = needle_service(root, model, execution);
    // Steady state: claim the one-shot warm-up so tests exercise the warm path.
    service.needle.claim_warmup();
    service
```

- [ ] **Step 7: Write the failing test for shared warmth across sessions (Review Focus 2, first half)**

Add to `crates/forge-runtime/src/service/tests.rs`:

```rust
#[tokio::test]
async fn two_runs_against_one_service_share_the_engine_warmup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![
        text_reply("first"),
        text_reply("second"),
    ]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("fn main() {}\n"));
    // Cold on purpose.
    let service = needle_service(tmp.path(), model.clone(), exec.clone());

    let prompt = "read_file: {\"path\": \"Cargo.toml\"}";
    let first = service.run(prompt).await.expect("first run");
    let second = service.run(prompt).await.expect("second run");

    // The first run spends the warm-up; the second must find a warm engine and
    // dispatch. If warmth were per-run, the second would warm again and this
    // would fail.
    assert!(
        !has_needle_dispatch(&first),
        "first run spends the warm-up: {:?}",
        event_kinds(&first)
    );
    assert!(
        has_needle_dispatch(&second),
        "second run must find the engine warm: {:?}",
        event_kinds(&second)
    );
}
```

- [ ] **Step 8: Run the whole runtime suite**

Run: `cargo test -p forge-runtime 2>&1 | tail -25`
Expected: PASS. If `needle_fast_path_first_prompt_warms_instead_of_dispatching` or the `SlowBackend` tests fail, they are asserting the same property through a different door — read them before changing them.

- [ ] **Step 9: Commit**

```bash
git add crates/forge-runtime/src/engine_handle.rs crates/forge-runtime/src/lib.rs \
        crates/forge-runtime/src/service.rs crates/forge-runtime/src/service/tests.rs
git commit -m "refactor(runtime): move engine warmth onto the engine handle

The tool surface is installed into the engine, so warmth is the engine's
property, not the service's and not a session's. A per-conversation flag
would charge the ~8 s install again for every new conversation against the
same runtime -- the exact cost this design exists to pay once.

EngineHandle carries the engine and its warmth together and is cheap to
clone, so every session borrows the same one."
```

---

### Task 3: The `Session` object

**Files:**
- Create: `crates/forge-runtime/src/session.rs`
- Modify: `crates/forge-runtime/src/service.rs`, `crates/forge-runtime/src/lib.rs`
- Test: in `crates/forge-runtime/src/session.rs`

**Interfaces:**
- Consumes: `EngineHandle` (Task 2); `DecisionLog`, `DecisionLogHandle` (Task 1).
- Produces:
  - `Session` with `id(&self) -> &str`, `async turn(&self, prompt: &str) -> Result<RunOutcome, ForgeError>`, `cancel(&self)`, `decisions(&self) -> &DecisionLogHandle`
  - `AgentService::session(&self) -> Session`
  - `AgentService::session_with_id(&self, session_id: &str) -> Session`

**Scope guard:** A1's `Session` is a thin owner. `turn` delegates to the existing `run_inner`. Do **not** move the turn loop's body into `session.rs` in this task — that is A2's work and doing it here makes the reorder in Task 4 unreviewable.

- [ ] **Step 1: Write the failing test**

Create `crates/forge-runtime/src/session.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use crate::service::tests_support::mock_service;

    #[tokio::test]
    async fn sessions_from_one_service_share_a_log_and_keep_distinct_ids() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // `session()` takes `self: &Arc<Self>`, so the service is shared from
        // the start — every surface will hold it this way.
        let service = std::sync::Arc::new(mock_service(tmp.path()));

        let a = service.session();
        let b = service.session();

        assert_ne!(a.id(), b.id(), "each session gets its own id");
        assert_eq!(
            a.decisions().session_id(),
            a.id(),
            "a session's log handle is bound to its own id"
        );

        let outcome = a.turn("hello").await.expect("turn runs");
        assert_eq!(outcome.session_id, a.id(), "the run is attributed to the session");
    }
}
```

- [ ] **Step 2: Add the test-support seam**

`mock_service` must be reachable from `session.rs`. In `crates/forge-runtime/src/service.rs`, add near the existing `#[cfg(test)] mod tests;` declaration:

```rust
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// A service with mock model, router and execution — enough for tests that
    /// care about session plumbing rather than agent behaviour.
    pub(crate) fn mock_service(root: &std::path::Path) -> AgentService {
        AgentService::new(
            Arc::new(crate::service::tests::ScriptedMockModel::new(vec![
                crate::service::tests::text_reply("ok"),
            ])),
            Arc::new(crate::service::tests::MockRouter::selecting("scripted-mock")),
            Arc::new(crate::service::tests::MockExecution::new(root)),
            Arc::new(NullSkillRegistry),
            Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions"))),
            Config::default(),
        )
    }
}
```

If `ScriptedMockModel`, `MockRouter`, `MockExecution` or `text_reply` are private in `service/tests.rs`, mark them `pub(crate)`.

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p forge-runtime session:: 2>&1 | tail -20`
Expected: FAIL — `no method named session`.

- [ ] **Step 4: Implement `Session`**

Above the test module in `crates/forge-runtime/src/session.rs`:

```rust
//! A conversation.
//!
//! `Session` owns what is per-conversation and outlives a single turn; what is
//! genuinely shared — the one needle engine, the decision log — is reached
//! through handles so every session borrows the same instance.
//!
//! In A1 `turn` delegates to `AgentService::run_with_options`. The turn loop
//! moves in later; the point of this type now is that warmth, the log and
//! cancellation acquire an owner whose lifetime matches a conversation.

use std::sync::Arc;

use forge_core::ForgeError;
use forge_session::{DecisionLogHandle, new_session_id};
use tokio_util::sync::CancellationToken;

use crate::service::{AgentService, RunOptions, RunOutcome};

pub struct Session {
    id: String,
    service: Arc<AgentService>,
    decisions: DecisionLogHandle,
    cancel: CancellationToken,
}

impl Session {
    pub(crate) fn new(
        id: String,
        service: Arc<AgentService>,
        decisions: DecisionLogHandle,
    ) -> Self {
        Self {
            id,
            service,
            decisions,
            cancel: CancellationToken::new(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn decisions(&self) -> &DecisionLogHandle {
        &self.decisions
    }

    /// Run one turn in this conversation.
    pub async fn turn(&self, prompt: &str) -> Result<RunOutcome, ForgeError> {
        self.service
            .run_with_options(
                prompt,
                RunOptions {
                    session_id: Some(self.id.clone()),
                    ..RunOptions::default()
                },
            )
            .await
    }

    /// Request cancellation of in-flight work for this conversation.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
```

- [ ] **Step 5: Add the factory to `AgentService`**

In `crates/forge-runtime/src/service.rs`:

1. Add a field `decision_log: Arc<forge_session::DecisionLog>` to `AgentService`, initialized in `new` as:

```rust
            decision_log: Arc::new(forge_session::DecisionLog::new(
                sessions.root().to_path_buf(),
            )),
```

If `JsonlSessionStore` has no `root()` accessor, add one to `crates/forge-session/src/store.rs`:

```rust
    /// Directory holding this store's transcripts. The decision log lives
    /// beside them.
    pub fn root(&self) -> &Path {
        &self.root
    }
```

2. Add the factory methods:

```rust
impl AgentService {
    /// Start a new conversation.
    pub fn session(self: &Arc<Self>) -> Session {
        self.session_with_id(&new_session_id())
    }

    /// Attach to a conversation by id — used to resume, and by surfaces that
    /// mint their own ids.
    pub fn session_with_id(self: &Arc<Self>, session_id: &str) -> Session {
        Session::new(
            session_id.to_string(),
            Arc::clone(self),
            forge_session::DecisionLog::handle(&self.decision_log, session_id),
        )
    }

    /// The runtime-scoped decision log, for surfaces that read it.
    pub fn decision_log(&self) -> &Arc<forge_session::DecisionLog> {
        &self.decision_log
    }
}
```

3. Declare the module in `crates/forge-runtime/src/lib.rs`:

```rust
pub mod session;
pub use session::Session;
```

- [ ] **Step 6: Check every construction site still compiles**

`AgentService` gained a field, so any construction outside `AgentService::new`
breaks. Find them:

Run: `cargo check --workspace --all-targets 2>&1 | grep -E "^error" -A6 | head -40`
Expected: either no output, or `missing field decision_log` at sites to fix by
routing them through `AgentService::new`.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p forge-runtime session:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 8: Run the whole workspace**

Run: `cargo test --workspace 2>&1 | grep -E "^test result|^error" | tail -20`
Expected: all PASS. `AgentService` gained a field, so any struct-literal construction outside `new` will fail to compile — fix those by calling `AgentService::new`.

- [ ] **Step 9: Commit**

```bash
git add crates/forge-runtime/src/session.rs crates/forge-runtime/src/lib.rs \
        crates/forge-runtime/src/service.rs crates/forge-session/src/store.rs
git commit -m "feat(runtime): add Session, with AgentService as its factory

Session owns what is per-conversation; the engine and the decision log are
runtime-scoped and reached through handles, so every session borrows the
same instance rather than minting its own.

turn() delegates to run_with_options for now -- the turn loop moves in
later. The point of the type today is that warmth, the log and cancellation
acquire an owner whose lifetime matches a conversation."
```

---

### Task 4: Decide before routing

**Files:**
- Modify: `crates/forge-runtime/src/service.rs` (`run_inner`, lines currently ~676-790)
- Modify: `crates/forge-runtime/src/service/tests.rs`
- Test: `crates/forge-runtime/src/service/tests.rs`

**Interfaces:**
- Consumes: `EngineHandle` (Task 2).
- Produces: no new public API. Behavioural change only.

**The behaviour change this task makes, stated plainly.** Today the fast path is offered `tools` derived from the *resolved* provider's capabilities, so a chat-only model suppresses dispatch. After the reorder there is no resolved provider yet, so needle is offered `tool_definitions()` unconditionally. This is correct: needle's dispatch executes a local tool and never calls the model, so the model's capabilities are irrelevant to it. What the capability check legitimately protects — a chat-only model must not be *sent* tools — is preserved, and Step 7 pins it.

- [ ] **Step 1: Write the failing test for ordering**

Add to `crates/forge-runtime/src/service/tests.rs`:

```rust
#[tokio::test]
async fn a_dispatched_turn_never_makes_a_model_routing_decision() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("unused")]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("fn main() {}\n"));
    let service = warm_needle_service(tmp.path(), model.clone(), exec.clone());

    let outcome = service
        .run("read_file: {\"path\": \"Cargo.toml\"}")
        .await
        .expect("run");

    assert!(has_needle_dispatch(&outcome));

    // Exactly one RoutingDecisionMade, and it is needle's. Before the reorder
    // there were two: the model router ran first and its decision was wasted.
    let routers: Vec<String> = outcome
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::RoutingDecisionMade { router, .. } => Some(router.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        routers,
        vec!["needle-dispatch".to_string()],
        "a dispatched turn must not route a model it never calls: {routers:?}"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p forge-runtime a_dispatched_turn_never 2>&1 | tail -20`
Expected: FAIL — two routers, `["static", "needle-dispatch"]` or similar.

- [ ] **Step 3: Hoist the dispatch decision**

In `run_inner`, move the fast-path block so it sits immediately after the `RunStarted` (and resume `InputReceived`) emission and **before** the `candidates`/`routing_request`/`self.router.route(...)` block.

Replace the tools expression at the hoisted site with the unconditional surface:

```rust
        // The dispatch decision comes before routing: which model should answer
        // is only a meaningful question once a model is known to be answering.
        //
        // `tool_definitions()` unconditionally, not the routed provider's
        // capabilities -- needle dispatches a *local* tool and never calls the
        // model, so the model's tool support is irrelevant here. The capability
        // check still applies to the tools handed to the model below.
        let dispatch_tools = tool_definitions();
        if resume_from.is_none()
            && !dispatch_tools.is_empty()
            && let Some(fast) = self.needle_fast_path(prompt, &run_id, &dispatch_tools).await
        {
            // ... existing dispatch body, unchanged ...
        }
```

Leave the model-facing computation where it is, after provider resolution:

```rust
        let tools = if model.capabilities().tools {
            tool_definitions()
        } else {
            Vec::new()
        };
```

- [ ] **Step 4: Run the ordering test**

Run: `cargo test -p forge-runtime a_dispatched_turn_never 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Run the runtime suite and expect one deliberate failure**

Run: `cargo test -p forge-runtime 2>&1 | grep -E "^test result|FAILED|failures:" | tail -10`
Expected: `needle_fast_path_requires_a_tool_capable_model` FAILS. That is this task's intended behaviour change, not a regression.

- [ ] **Step 6: Rewrite that test to assert what it actually protects**

Replace `needle_fast_path_requires_a_tool_capable_model` in `crates/forge-runtime/src/service/tests.rs` with:

```rust
/// A chat-only provider must never be *sent* tools.
///
/// This test used to assert something stronger — that a chat-only model also
/// suppressed needle's dispatch — which only held because the fast path ran
/// after provider resolution and inherited its capabilities. Once the dispatch
/// decision moved above routing there is no resolved provider yet, and needle's
/// dispatch executes a local tool without calling the model at all, so the
/// model's tool support has no bearing on it. What the original test legitimately
/// protected is the half asserted here.
#[tokio::test]
async fn a_chat_only_model_is_never_sent_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(
        MockModel::new().with_capabilities(forge_core::ModelCapabilities {
            streaming: true,
            tools: false,
            structured_output: false,
            vision: false,
            max_context: 8_192,
        }),
    );
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("secret"));
    let service = warm_needle_service(tmp.path(), model.clone(), exec.clone());

    // A prompt needle cannot fill, so the turn reaches the model.
    let outcome = service.run("explain the architecture").await.expect("run");

    assert!(!has_needle_dispatch(&outcome));
    let requests = model.recorded();
    assert!(!requests.is_empty(), "the model must have been called");
    for request in &requests {
        assert!(
            request.tools.is_empty(),
            "a chat-only model must receive no tools"
        );
    }
}
```

If `MockModel::recorded()` does not expose the `CompletionRequest`, extend it to store and return the requests it received — `ScriptedMockModel` already does this; mirror its implementation.

- [ ] **Step 7: Write the failing test for the model-facing tools (Review Focus 4)**

The test in Step 6 *is* Review Focus 4. Run it:

Run: `cargo test -p forge-runtime a_chat_only_model_is_never_sent_tools 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 8: Run the runtime suite**

Run: `cargo test -p forge-runtime 2>&1 | grep -E "^test result|FAILED" | tail -10`
Expected: all PASS.

- [ ] **Step 9: Write the failing test for a build with no engine (Review Focus 3)**

```rust
#[tokio::test]
async fn a_service_with_no_engine_routes_normally_and_dispatches_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("from the model")]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("fn main() {}\n"));

    // No `.with_needle(..)` at all: the EngineHandle::none() path, which is also
    // what a build with HAS_EMBEDDED_BACKEND == false produces.
    let service = AgentService::new(
        model.clone(),
        Arc::new(MockRouter::selecting("scripted-mock")),
        exec.clone(),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join(".forge").join("sessions"))),
        Config::default(),
    );

    let outcome = service
        .run("read_file: {\"path\": \"Cargo.toml\"}")
        .await
        .expect("run");

    assert!(!has_needle_dispatch(&outcome), "nothing to dispatch with");
    assert_eq!(outcome.text, "from the model");
    let routers: Vec<String> = outcome
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::RoutingDecisionMade { router, .. } => Some(router.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        routers.len(),
        1,
        "routing must still happen when there is no engine: {routers:?}"
    );
}
```

- [ ] **Step 10: Run it**

Run: `cargo test -p forge-runtime a_service_with_no_engine 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 11: Run the full gate**

Run: `just verify 2>&1 | tail -20`
Expected: exit 0. The BDD suite drives the compiled binary and exercises the reordered turn end to end.

- [ ] **Step 12: Commit**

```bash
git add crates/forge-runtime/src/service.rs crates/forge-runtime/src/service/tests.rs
git commit -m "feat(runtime): decide before routing

Which model should answer is only a meaningful question once a model is
known to be answering. The dispatch decision now runs first, so a turn
needle answers on device never routes a model it will not call, and never
resolves a provider it will not use.

One deliberate behaviour change: needle is now offered the full tool
surface rather than the routed provider's capabilities, because its
dispatch executes a local tool and never calls the model. What the old
capability check legitimately protected -- a chat-only model must not be
*sent* tools -- is unchanged and now has its own test."
```

---

### Task 5: Record the decisions

**Files:**
- Modify: `crates/forge-runtime/src/service.rs`
- Test: `crates/forge-runtime/src/service/tests.rs`

**Interfaces:**
- Consumes: `DecisionLogHandle`, `RecordDraft`, `Stage`, `Decider`, `Outcome` (Task 1); `Session` (Task 3).
- Produces: no new public API.

**Why:** A1 exists to be the measurement. Without this task the reorder is a modest latency win and nothing is learned.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_dispatched_turn_records_a_decide_stage_with_the_dispatch_outcome() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![text_reply("unused")]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("fn main() {}\n"));
    let service = std::sync::Arc::new(warm_needle_service(tmp.path(), model, exec));

    let session = service.session();
    session
        .turn("read_file: {\"path\": \"Cargo.toml\"}")
        .await
        .expect("turn");

    let path = tmp
        .path()
        .join(".forge")
        .join("sessions")
        .join(format!("{}.decisions.jsonl", session.id()));
    let raw = std::fs::read_to_string(&path).expect("decision log written");
    let records: Vec<forge_session::DecisionRecord> = raw
        .lines()
        .map(|l| serde_json::from_str(l).expect("record parses"))
        .collect();

    let decide = records
        .iter()
        .find(|r| r.stage == forge_session::Stage::Decide)
        .expect("a decide-stage record");
    assert_eq!(decide.decider, forge_session::Decider::Needle);
    assert_eq!(decide.outcome, forge_session::Outcome::Dispatched);
    assert_eq!(decide.choice, "read_file");
    assert_eq!(decide.question, "tool");

    assert!(
        !records.iter().any(|r| r.stage == forge_session::Stage::Route),
        "a dispatched turn routes nothing, so it records no route stage"
    );

    // The log carries decision shape, never content.
    assert!(
        !raw.contains("Cargo.toml"),
        "tool arguments must never reach the decision log: {raw}"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p forge-runtime a_dispatched_turn_records 2>&1 | tail -20`
Expected: FAIL — no such file.

- [ ] **Step 3: Thread the log handle into `run_inner`**

`run_inner` already receives `session_id`. Derive a handle at the top of the function:

```rust
        let decisions = forge_session::DecisionLog::handle(&self.decision_log, session_id);
        // A1 records one turn per run. The turn counter becomes real when the
        // loop moves into Session.
        let turn_no: u32 = 1;
```

- [ ] **Step 4: Record the dispatch decision**

In the hoisted fast-path block (Task 4), wrap the call to time it and record both outcomes:

```rust
        let dispatch_tools = tool_definitions();
        let fast = if resume_from.is_none() && !dispatch_tools.is_empty() {
            let started = std::time::Instant::now();
            let outcome = self.needle_fast_path(prompt, &run_id, &dispatch_tools).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let candidates: Vec<String> =
                dispatch_tools.iter().map(|t| t.name.clone()).collect();
            decisions.record(
                turn_no,
                forge_session::RecordDraft {
                    stage: forge_session::Stage::Decide,
                    decider: if self.needle.engine().is_some() {
                        forge_session::Decider::Needle
                    } else {
                        forge_session::Decider::None
                    },
                    question: "tool".to_string(),
                    choice: outcome
                        .as_ref()
                        .map(|f| f.call.name.clone())
                        .unwrap_or_else(|| "none".to_string()),
                    confidence: outcome.as_ref().map(|f| f.confidence),
                    probabilities: std::collections::BTreeMap::new(),
                    candidates,
                    outcome: match (&outcome, self.needle.engine().is_some()) {
                        (Some(_), _) => forge_session::Outcome::Dispatched,
                        (None, true) => forge_session::Outcome::Declined,
                        (None, false) => forge_session::Outcome::Unavailable,
                    },
                    elapsed_ms,
                    speculative: false,
                },
            );
            outcome
        } else {
            None
        };

        if let Some(fast) = fast {
            // ... existing dispatch body, unchanged ...
        }
```

- [ ] **Step 5: Record the routing decision**

Immediately after the existing `tracing::info!(... "routing decision")` call:

```rust
        decisions.record(
            turn_no,
            forge_session::RecordDraft {
                stage: forge_session::Stage::Route,
                // Map from the router that actually answered, not from
                // `fallback_used`: a primary `static` router is not needle
                // having fallen back, and conflating them would corrupt the
                // decline rate this log exists to measure.
                decider: match decision.router_name.as_str() {
                    "needle" | "needle-dispatch" => forge_session::Decider::Needle,
                    "static" | "cheapest" | "mock" => forge_session::Decider::Static,
                    _ => forge_session::Decider::Llm,
                },
                question: "model".to_string(),
                choice: decision.selected_model.clone(),
                confidence: Some(decision.confidence),
                probabilities: std::collections::BTreeMap::new(),
                candidates: routing_request.candidates.clone(),
                outcome: forge_session::Outcome::Routed,
                elapsed_ms: route_started.elapsed().as_millis() as u64,
                speculative: false,
            },
        );
```

Add `let route_started = std::time::Instant::now();` immediately before `self.router.route(&routing_request).await`.

- [ ] **Step 6: Run the test**

Run: `cargo test -p forge-runtime a_dispatched_turn_records 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 7: Write the failing test for interleaved writes (Review Focus 2, second half)**

```rust
#[tokio::test]
async fn concurrent_sessions_write_whole_lines_to_one_log() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScriptedMockModel::new(vec![
        text_reply("a"),
        text_reply("b"),
        text_reply("c"),
        text_reply("d"),
    ]));
    let exec = Arc::new(MockExecution::new(tmp.path()).with_read_content("fn main() {}\n"));
    let service = std::sync::Arc::new(warm_needle_service(tmp.path(), model, exec));

    let a = service.session();
    let b = service.session();
    let (ra, rb) = tokio::join!(
        a.turn("read_file: {\"path\": \"Cargo.toml\"}"),
        b.turn("read_file: {\"path\": \"Cargo.toml\"}"),
    );
    ra.expect("a");
    rb.expect("b");

    // Each session writes its own file; every line in both must parse whole.
    for session_id in [a.id(), b.id()] {
        let path = tmp
            .path()
            .join(".forge")
            .join("sessions")
            .join(format!("{session_id}.decisions.jsonl"));
        let raw = std::fs::read_to_string(&path).expect("log exists");
        for line in raw.lines() {
            serde_json::from_str::<forge_session::DecisionRecord>(line)
                .unwrap_or_else(|e| panic!("torn line {line:?}: {e}"));
        }
    }
}
```

- [ ] **Step 8: Run it**

Run: `cargo test -p forge-runtime concurrent_sessions_write 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 9: Run the full gate**

Run: `just verify 2>&1 | tail -20`
Expected: exit 0.

- [ ] **Step 10: Commit**

```bash
git add crates/forge-runtime/src/service.rs crates/forge-runtime/src/service/tests.rs
git commit -m "feat(runtime): record dispatch and routing decisions

A1 exists to be the measurement. Every turn now records what was decided,
by whom, with what confidence, over which candidates, and what came of it --
decision shape only, never the prompt or the arguments.

A declined dispatch is recorded as explicitly as a successful one, and a
build with no engine records Unavailable rather than going silent, so the
decline rate is readable from real work rather than assumed."
```

---

### Task 6: `forge session decisions`

**Files:**
- Modify: `crates/forge-cli/src/commands/session_cmd.rs`
- Test: `crates/forge-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `DecisionRecord`, `Stage`, `Outcome` (Task 1).
- Produces: a CLI subcommand. No library API.

**Why:** an unread measurement is not a measurement. This is the surface that answers A2's question — how often does needle decline on real work?

- [ ] **Step 1: Write the failing test**

Add to `crates/forge-cli/tests/cli.rs`:

```rust
#[test]
fn session_decisions_summarises_the_decision_log() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    let sessions = project.join(".forge").join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join("sess-1.decisions.jsonl"),
        concat!(
            r#"{"ts":"2026-09-25T00:00:00.000Z","session":"sess-1","turn":1,"stage":"decide","decider":"needle","question":"tool","choice":"read_file","confidence":1.0,"outcome":"dispatched","elapsed_ms":900,"speculative":false}"#,
            "\n",
            r#"{"ts":"2026-09-25T00:00:01.000Z","session":"sess-1","turn":2,"stage":"decide","decider":"needle","question":"tool","choice":"none","confidence":0.47,"outcome":"declined","elapsed_ms":1100,"speculative":false}"#,
            "\n",
        ),
    )
    .expect("write log");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["session", "decisions", "--json"])
        .output()
        .expect("run");
    assert!(output.status.success(), "{:?}", String::from_utf8_lossy(&output.stderr));

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json on stdout");
    assert_eq!(value["decide"]["total"], 2);
    assert_eq!(value["decide"]["dispatched"], 1);
    assert_eq!(value["decide"]["declined"], 1);
    // The number A2 is designed against.
    assert_eq!(value["decide"]["decline_rate"], 0.5);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p forge-cli --test cli session_decisions 2>&1 | tail -20`
Expected: FAIL — unknown subcommand `decisions`.

- [ ] **Step 3: Implement the subcommand**

In `crates/forge-cli/src/commands/session_cmd.rs`, add a `Decisions` variant to the existing session subcommand enum (match the file's existing clap style) and implement:

```rust
/// Summarise `.forge/sessions/*.decisions.jsonl`.
///
/// The decline rate is the number A2's design hangs on: whether falling back to
/// a second decider is worth its latency depends entirely on how often the first
/// one declines on real work. Published benchmarks do not transfer; this does.
fn decisions(ctx: &Context) -> Result<(), ForgeError> {
    let root = ctx.project_root().join(".forge").join("sessions");
    let mut total = 0u64;
    let mut dispatched = 0u64;
    let mut declined = 0u64;
    let mut unavailable = 0u64;
    let mut elapsed_ms_total = 0u64;
    let mut routed = 0u64;

    if root.is_dir() {
        for entry in std::fs::read_dir(&root)
            .map_err(|e| ForgeError::config(format!("reading {}: {e}", root.display())))?
        {
            let entry = entry
                .map_err(|e| ForgeError::config(format!("reading {}: {e}", root.display())))?;
            let path = entry.path();
            if !path.to_string_lossy().ends_with(".decisions.jsonl") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| ForgeError::config(format!("reading {}: {e}", path.display())))?;
            for line in raw.lines() {
                // A truncated final line is normal for an append-only log that
                // was being written when the process died. Skip, never fail.
                let Ok(record) = serde_json::from_str::<forge_session::DecisionRecord>(line)
                else {
                    continue;
                };
                match record.stage {
                    forge_session::Stage::Decide => {
                        total += 1;
                        elapsed_ms_total += record.elapsed_ms;
                        match record.outcome {
                            forge_session::Outcome::Dispatched => dispatched += 1,
                            forge_session::Outcome::Declined => declined += 1,
                            forge_session::Outcome::Unavailable => unavailable += 1,
                            _ => {}
                        }
                    }
                    forge_session::Stage::Route => routed += 1,
                    _ => {}
                }
            }
        }
    }

    let decline_rate = if total == 0 {
        0.0
    } else {
        declined as f64 / total as f64
    };
    let mean_ms = if total == 0 {
        0
    } else {
        elapsed_ms_total / total
    };

    let report = serde_json::json!({
        "decide": {
            "total": total,
            "dispatched": dispatched,
            "declined": declined,
            "unavailable": unavailable,
            "decline_rate": decline_rate,
            "mean_elapsed_ms": mean_ms,
        },
        "route": { "total": routed },
    });

    if ctx.global.json {
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| {
            ForgeError::config(format!("serializing decision summary: {e}"))
        })?);
    } else {
        println!("decide  {total} total — {dispatched} dispatched, {declined} declined, {unavailable} unavailable");
        println!("        decline rate {:.0}%, mean {mean_ms} ms", decline_rate * 100.0);
        println!("route   {routed} total");
    }
    Ok(())
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p forge-cli --test cli session_decisions 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Run the full gate**

Run: `just verify 2>&1 | tail -20`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/forge-cli/src/commands/session_cmd.rs crates/forge-cli/tests/cli.rs
git commit -m "feat(cli): forge session decisions

An unread measurement is not a measurement. Summarises the decision log:
how many dispatch decisions were made, how many dispatched, declined or
found no engine, the decline rate, and mean latency.

The decline rate is the number A2's design hangs on -- whether falling back
to a second decider is worth its latency depends entirely on how often the
first declines on real work, and published benchmarks do not transfer.

A truncated final line is skipped rather than failing: that is the normal
state of an append-only log whose writer died."
```

---

## Done when

- `just verify` passes.
- `forge session decisions` reports a non-zero decline rate after a few real turns.
- A dispatched turn emits exactly one `RoutingDecisionMade`, and it is `needle-dispatch`.
- Two turns against one runtime pay the engine warm-up once.
