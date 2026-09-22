use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

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
pub struct JsonlSessionStore {
    root: PathBuf,
    session_id: Option<String>,
    redactor: Redactor,
}

impl JsonlSessionStore {
    /// Unbound store; `events()` reads every session file.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            session_id: None,
            redactor: Redactor::new(),
        }
    }

    /// Store bound to one session; `events()` reads only that session.
    pub fn for_session(root: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            session_id: Some(session_id.into()),
            redactor: Redactor::new(),
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
}

impl SessionStore for JsonlSessionStore {
    fn append(&self, event: &Event) -> Result<(), ForgeError> {
        std::fs::create_dir_all(&self.root).map_err(ForgeError::Io)?;
        let mut value = serde_json::to_value(event)
            .map_err(|e| ForgeError::session(format!("serializing event: {e}")))?;
        self.redactor.redact_value(&mut value);
        let line = serde_json::to_string(&value)
            .map_err(|e| ForgeError::session(format!("serializing event: {e}")))?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file_for(&event.session_id))
            .map_err(ForgeError::Io)?;
        writeln!(file, "{line}").map_err(ForgeError::Io)
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
            },
        );
        store.append(&event).expect("append");
        store
            .append(&Event::new(
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
        assert_eq!(events[0].v, 1);
    }

    #[test]
    fn sessions_are_listed_and_latest_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(tmp.path());
        for (session, n) in [("sess-a", 1usize), ("sess-b", 2)] {
            for _ in 0..n {
                store
                    .append(&Event::new(
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
            .append(&Event::new(
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
            .append(&Event::new(
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
    fn run_and_session_ids_are_unique_ulids() {
        let a = new_run_id();
        let b = new_run_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 26);
        assert_ne!(new_session_id(), new_session_id());
    }
}
