use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use forge_core::{Event, ForgeError, SessionStore};

use crate::redact::Redactor;

/// New unique run id (ULID: sortable, URL-safe).
pub fn new_run_id() -> String {
    ulid::Ulid::new().to_string()
}

/// New unique session id.
pub fn new_session_id() -> String {
    ulid::Ulid::new().to_string()
}

/// One session file on disk.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub event_count: usize,
    pub path: PathBuf,
}

/// Append-only store: one `{session_id}.jsonl` file per session under
/// `root`, created lazily on first append.
///
/// The store assigns `Event.seq` on append: the next monotonic number per
/// run, starting at 1. The counter is seeded from the existing file (so
/// appending to a v1 log or from a fresh process continues correctly) and
/// cached in memory.
pub struct JsonlSessionStore {
    root: PathBuf,
    session_id: Option<String>,
    redactor: Redactor,
    /// run_id → last assigned seq.
    seq_counters: Mutex<HashMap<String, u64>>,
}

impl JsonlSessionStore {
    /// Unbound store; `events()` reads every session file.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            session_id: None,
            redactor: Redactor::new(),
            seq_counters: Mutex::new(HashMap::new()),
        }
    }

    /// Store bound to one session; `events()` reads only that session.
    pub fn for_session(root: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            session_id: Some(session_id.into()),
            redactor: Redactor::new(),
            seq_counters: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn file_for(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{session_id}.jsonl"))
    }

    /// Events of one session (empty when the file does not exist).
    pub fn events_for(&self, session_id: &str) -> Result<Vec<Event>, ForgeError> {
        let path = self.file_for(session_id);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&path).map_err(ForgeError::Io)?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(i, line)| {
                serde_json::from_str(line).map_err(|e| {
                    ForgeError::session(format!(
                        "corrupt event at {}:{}: {e}",
                        path.display(),
                        i + 1
                    ))
                })
            })
            .collect()
    }

    /// The raw JSONL lines of one session, blank lines dropped (empty when
    /// the file does not exist).
    ///
    /// Callers that need *events* want [`events_for`](Self::events_for).
    /// This exists for copying: a fork must reproduce a prefix byte for
    /// byte, including the original `v`, `seq` and timestamps, rather than
    /// re-serializing through the current schema.
    pub fn raw_lines(&self, session_id: &str) -> Result<Vec<String>, ForgeError> {
        let path = self.file_for(session_id);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        Ok(std::fs::read_to_string(&path)
            .map_err(ForgeError::Io)?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Create `target`'s session file from the first `lines` event lines of
    /// `source`, copied verbatim. Returns the number of lines written.
    ///
    /// The source file is opened read-only and never written: forks are
    /// prefix *copies*, which keeps every session file self-contained and
    /// independently appendable (the price is disk, paid once per fork).
    /// Refuses to overwrite an existing target — session ids are ULIDs, so
    /// a collision means something is wrong rather than something to
    /// silently clobber.
    pub fn copy_prefix(
        &self,
        source: &str,
        target: &str,
        lines: usize,
    ) -> Result<usize, ForgeError> {
        let target_path = self.file_for(target);
        if target_path.exists() {
            return Err(ForgeError::session(format!(
                "session {target} already exists at {}",
                target_path.display()
            )));
        }
        let prefix = self.raw_lines(source)?;
        let take = lines.min(prefix.len());
        std::fs::create_dir_all(&self.root).map_err(ForgeError::Io)?;
        let mut body = String::new();
        for line in &prefix[..take] {
            body.push_str(line);
            body.push('\n');
        }
        std::fs::write(&target_path, body).map_err(ForgeError::Io)?;
        Ok(take)
    }

    /// All sessions known under the root, sorted by session id.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, ForgeError> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&self.root).map_err(ForgeError::Io)? {
            let entry = entry.map_err(ForgeError::Io)?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(session_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let event_count = std::fs::read_to_string(&path)
                .map_err(ForgeError::Io)?
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count();
            out.push(SessionInfo {
                session_id,
                event_count,
                path,
            });
        }
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(out)
    }

    /// Most recently modified session id, if any.
    pub fn latest_session(&self) -> Result<Option<String>, ForgeError> {
        let mut latest: Option<(std::time::SystemTime, String)> = None;
        for info in self.list_sessions()? {
            let modified = std::fs::metadata(&info.path)
                .and_then(|m| m.modified())
                .map_err(ForgeError::Io)?;
            let newer = latest.as_ref().is_none_or(|(ts, _)| modified > *ts);
            if newer {
                latest = Some((modified, info.session_id));
            }
        }
        Ok(latest.map(|(_, id)| id))
    }

    /// Find the session containing a given run id.
    pub fn find_run(&self, run_id: &str) -> Result<Option<String>, ForgeError> {
        for info in self.list_sessions()? {
            if self
                .events_for(&info.session_id)?
                .iter()
                .any(|e| e.run_id == run_id)
            {
                return Ok(Some(info.session_id));
            }
        }
        Ok(None)
    }
    /// Highest seq already stored for `run_id` in the session file (0 when
    /// none / v1 events only). Used to seed the in-memory counter.
    fn stored_max_seq(&self, session_id: &str, run_id: &str) -> Result<u64, ForgeError> {
        let events = self.events_for(session_id)?;
        Ok(events
            .iter()
            .filter(|e| e.run_id == run_id)
            .map(|e| e.seq)
            .max()
            .unwrap_or(0))
    }

    fn next_seq(&self, session_id: &str, run_id: &str) -> Result<u64, ForgeError> {
        let mut counters = self.seq_counters.lock().unwrap_or_else(|e| e.into_inner());
        let counter = match counters.get(run_id) {
            Some(current) => *current,
            None => self.stored_max_seq(session_id, run_id)?,
        };
        let next = counter + 1;
        counters.insert(run_id.to_string(), next);
        Ok(next)
    }
}

impl SessionStore for JsonlSessionStore {
    /// Append an event and return **the redacted event that was written**.
    ///
    /// Returning the redacted form is the whole point: the runtime
    /// broadcasts and collects whatever `append` hands back, so an
    /// unredacted return value means secrets reach SSE subscribers, live
    /// `Attachment` frames, and `--json` run outcomes even though the log on
    /// disk is clean. It also makes the log and the stream byte-identical,
    /// which is what the server's replay-versus-live deduplication compares —
    /// a rewritten event used to be delivered twice, once scrubbed and once
    /// raw.
    ///
    /// The redacted form is read back through serde so there is exactly one
    /// definition of "what was written". `ts` is restored from the original:
    /// it is the one field that is not a `String` in Rust but is one in JSON,
    /// so it is the one field a redaction could make unparseable.
    fn append(&self, mut event: Event) -> Result<Event, ForgeError> {
        std::fs::create_dir_all(&self.root).map_err(ForgeError::Io)?;
        event.seq = self.next_seq(&event.session_id, &event.run_id)?;
        event.v = forge_core::EVENT_SCHEMA_VERSION;
        let mut value = serde_json::to_value(&event)
            .map_err(|e| ForgeError::session(format!("serializing event: {e}")))?;
        self.redactor.redact_value(&mut value);
        let line = serde_json::to_string(&value)
            .map_err(|e| ForgeError::session(format!("serializing event: {e}")))?;
        let mut redacted: Event = serde_json::from_value(value)
            .map_err(|e| ForgeError::session(format!("re-reading a redacted event: {e}")))?;
        redacted.ts = event.ts;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file_for(&event.session_id))
            .map_err(ForgeError::Io)?;
        writeln!(file, "{line}").map_err(ForgeError::Io)?;
        Ok(redacted)
    }

    fn events(&self) -> Result<Vec<Event>, ForgeError> {
        match &self.session_id {
            Some(id) => self.events_for(id),
            None => {
                let mut all = Vec::new();
                for info in self.list_sessions()? {
                    all.extend(self.events_for(&info.session_id)?);
                }
                Ok(all)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use forge_core::EventKind;
    use serial_test::serial;

    use super::*;

    #[test]
    fn append_and_read_back_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path());

        let event = Event::new(
            "run-1",
            "sess-1",
            EventKind::RunStarted {
                provider: "mock".into(),
                model: "mock-local".into(),
                prompt: "do the thing".into(),
            },
        );
        store.append(event).expect("append");
        store
            .append(Event::new(
                "run-1",
                "sess-1",
                EventKind::Completed {
                    summary: "done".into(),
                },
            ))
            .expect("append");

        let events = store.events_for("sess-1").expect("read");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, EventKind::RunStarted { .. }));
        assert!(matches!(events[1].kind, EventKind::Completed { .. }));
        assert_eq!(events[0].v, forge_core::EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn sessions_are_listed_and_latest_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path());
        for (session, n) in [("sess-a", 1usize), ("sess-b", 2)] {
            for _ in 0..n {
                store
                    .append(Event::new(
                        "r",
                        session,
                        EventKind::ToolStarted { name: "t".into() },
                    ))
                    .expect("append");
            }
        }
        let sessions = store.list_sessions().expect("list");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "sess-a");
        assert_eq!(sessions[1].event_count, 2);
        assert_eq!(
            store.latest_session().expect("latest"),
            Some("sess-b".to_string())
        );
    }

    #[test]
    fn find_run_locates_the_owning_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path());
        store
            .append(Event::new(
                "run-x",
                "sess-1",
                EventKind::ToolStarted { name: "t".into() },
            ))
            .expect("append");
        assert_eq!(
            store.find_run("run-x").expect("find"),
            Some("sess-1".to_string())
        );
        assert_eq!(store.find_run("nope").expect("find"), None);
    }

    #[test]
    #[serial]
    fn secrets_are_redacted_before_writing() {
        let secret = "sk-livekey-abcdef123456";
        unsafe { std::env::set_var("FORGE_SESSION_TEST_API_KEY", secret) };
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path()); // env snapshot happens here

        store
            .append(Event::new(
                "run-1",
                "sess-1",
                EventKind::Error {
                    message: format!("call failed with key {secret} and Bearer abcdef123"),
                },
            ))
            .expect("append");
        unsafe { std::env::remove_var("FORGE_SESSION_TEST_API_KEY") };

        let raw = std::fs::read_to_string(tmp.path().join("sess-1.jsonl")).expect("read raw");
        assert!(!raw.contains(secret), "leaked env secret: {raw}");
        assert!(!raw.contains("Bearer abcdef123"), "leaked bearer: {raw}");
        assert!(raw.contains("[REDACTED]"));
    }

    #[test]
    #[serial]
    fn replay_payloads_are_redacted_like_everything_else() {
        // The v3 replay kinds carry verbatim model traffic — tool-call
        // arguments and tool output — which is exactly where a secret is
        // most likely to land. Redaction is deep, and this is the lock.
        let secret = "sk-livekey-abcdef123456";
        unsafe { std::env::set_var("FORGE_SESSION_REPLAY_TEST_TOKEN", secret) };
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path()); // env snapshot here

        store
            .append(Event::new(
                "run-1",
                "sess-1",
                EventKind::AssistantMessage {
                    text: format!("using {secret}"),
                    tool_calls: vec![forge_core::ToolCall::new(
                        "call_1",
                        "run_command",
                        serde_json::json!({ "command": format!("curl -H 'Bearer {secret}'") }),
                    )],
                },
            ))
            .expect("append assistant message");
        store
            .append(Event::new(
                "run-1",
                "sess-1",
                EventKind::ToolResult {
                    call_id: "call_1".into(),
                    tool: "run_command".into(),
                    output: format!("the server echoed {secret}"),
                    is_error: false,
                },
            ))
            .expect("append tool result");
        unsafe { std::env::remove_var("FORGE_SESSION_REPLAY_TEST_TOKEN") };

        let raw = std::fs::read_to_string(tmp.path().join("sess-1.jsonl")).expect("read raw");
        assert!(!raw.contains(secret), "leaked into a replay payload: {raw}");
        assert_eq!(
            raw.matches("[REDACTED]").count(),
            3,
            "text, tool-call arguments and tool output must all be redacted: {raw}"
        );
    }

    #[test]
    #[serial]
    fn append_returns_the_redacted_event_it_wrote() {
        // The gap this closes: `append` used to redact a clone and return the
        // ORIGINAL, so the runtime broadcast and collected the unredacted
        // event — secrets reached SSE, live attachments and `--json` outcomes
        // while the log on disk was clean.
        let secret = "sk-livekey-abcdef123456";
        unsafe { std::env::set_var("FORGE_SESSION_RETURN_TEST_TOKEN", secret) };
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path()); // env snapshot here

        let returned = store
            .append(Event::new(
                "run-1",
                "sess-1",
                EventKind::ToolResult {
                    call_id: "call_1".into(),
                    tool: "read_file".into(),
                    output: format!("the file contained {secret}"),
                    is_error: false,
                },
            ))
            .expect("append");
        unsafe { std::env::remove_var("FORGE_SESSION_RETURN_TEST_TOKEN") };

        match &returned.kind {
            EventKind::ToolResult { output, .. } => {
                assert!(!output.contains(secret), "returned event leaks: {output}");
                assert!(output.contains("[REDACTED]"), "got: {output}");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        // Structural fields survive the round-trip...
        assert_eq!(returned.seq, 1);
        assert_eq!(returned.v, forge_core::EVENT_SCHEMA_VERSION);
        assert_eq!(returned.run_id, "run-1");
        // ...and what was returned is exactly what was written, which is what
        // the server's replay-vs-live deduplication compares.
        let raw = std::fs::read_to_string(tmp.path().join("sess-1.jsonl")).expect("read raw");
        assert_eq!(
            serde_json::to_string(&returned).expect("reserialize") + "\n",
            raw,
            "the stored line and the returned event must be identical"
        );
    }

    #[test]
    fn copy_prefix_reproduces_lines_verbatim_and_leaves_the_source_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A v1 line and a v2 line: a copy must preserve both exactly,
        // schema versions included.
        let original = "{\"v\":1,\"ts\":\"2026-09-22T20:01:39.172579Z\",\"run_id\":\"r1\",\"session_id\":\"src\",\"type\":\"run_started\",\"provider\":\"p\",\"model\":\"m\"}\n{\"v\":2,\"seq\":1,\"ts\":\"2026-09-22T20:01:40.172579Z\",\"run_id\":\"r2\",\"session_id\":\"src\",\"type\":\"completed\",\"summary\":\"done\"}\n";
        std::fs::write(tmp.path().join("src.jsonl"), original).expect("write");

        let store = JsonlSessionStore::new(tmp.path());
        assert_eq!(store.copy_prefix("src", "dst", 1).expect("copy"), 1);

        let copied = std::fs::read_to_string(tmp.path().join("dst.jsonl")).expect("read copy");
        assert_eq!(
            copied,
            original.lines().next().expect("first line").to_string() + "\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src.jsonl")).expect("read source"),
            original,
            "the source must be byte-identical afterwards"
        );
    }

    #[test]
    fn copy_prefix_clamps_to_the_log_length_and_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path());
        store
            .append(Event::new(
                "r",
                "src",
                EventKind::ToolStarted { name: "t".into() },
            ))
            .expect("append");

        assert_eq!(
            store.copy_prefix("src", "dst", 99).expect("copy"),
            1,
            "asking for more lines than exist copies the whole log"
        );
        let err = store
            .copy_prefix("src", "dst", 1)
            .expect_err("must not clobber");
        assert!(matches!(err, ForgeError::Session(_)), "got: {err}");
    }

    #[test]
    fn raw_lines_of_an_unknown_session_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path());
        assert!(store.raw_lines("nope").expect("raw").is_empty());
    }

    #[test]
    fn run_and_session_ids_are_unique_ulids() {
        let a = new_run_id();
        let b = new_run_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 26);
        assert_ne!(new_session_id(), new_session_id());
    }

    #[test]
    fn seq_is_monotonic_per_run_and_seeded_from_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path());

        let e1 = store
            .append(Event::new(
                "run-a",
                "sess",
                EventKind::ToolStarted { name: "t".into() },
            ))
            .expect("append");
        let e2 = store
            .append(Event::new(
                "run-a",
                "sess",
                EventKind::ToolStarted { name: "t".into() },
            ))
            .expect("append");
        // A different run in the same session gets its own counter.
        let other = store
            .append(Event::new(
                "run-b",
                "sess",
                EventKind::ToolStarted { name: "t".into() },
            ))
            .expect("append");
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(other.seq, 1);

        // A fresh store instance (e.g. a new CLI process) continues from
        // the on-disk maximum instead of restarting.
        let fresh = JsonlSessionStore::new(tmp.path());
        let e3 = fresh
            .append(Event::new(
                "run-a",
                "sess",
                EventKind::ToolCompleted {
                    name: "t".into(),
                    success: true,
                },
            ))
            .expect("append");
        assert_eq!(e3.seq, 3);

        let events = store.events_for("sess").expect("read");
        let seqs: Vec<u64> = events
            .iter()
            .filter(|e| e.run_id == "run-a")
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn appending_to_a_v1_log_continues_with_current_schema_events() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Hand-write a v1 line: no seq field.
        std::fs::write(
            tmp.path().join("sess.jsonl"),
            "{\"v\":1,\"ts\":\"2026-09-22T20:01:39.172579Z\",\"run_id\":\"run-a\",\"session_id\":\"sess\",\"type\":\"run_started\",\"provider\":\"mock-local\",\"model\":\"mock-local\"}\n",
        )
        .expect("write v1 line");

        let store = JsonlSessionStore::new(tmp.path());
        // v1 events read back with seq 0 and remain readable.
        let events = store.events_for("sess").expect("read v1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].v, 1);
        assert_eq!(events[0].seq, 0);

        // New appends carry the current schema version and get seq
        // starting at 1 (v1 had none).
        let appended = store
            .append(Event::new(
                "run-a",
                "sess",
                EventKind::Completed {
                    summary: "done".into(),
                },
            ))
            .expect("append");
        assert_eq!(appended.v, forge_core::EVENT_SCHEMA_VERSION);
        assert_eq!(appended.seq, 1);

        let events = store.events_for("sess").expect("read mixed");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .iter()
                .enumerate()
                .map(|(i, e)| e.seq_or_index(i))
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
}
