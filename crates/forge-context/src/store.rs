use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use forge_core::ForgeError;

use crate::{ContextPlan, ContextPlanDraft};

pub(crate) fn artifact_transaction<T>(
    path: &std::path::Path,
    operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> std::io::Result<T>,
) -> std::io::Result<T> {
    filesystem::artifact_transaction(path, "artifacts", operation)
}

pub(crate) fn observation_transaction<T>(
    path: &std::path::Path,
    operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> std::io::Result<T>,
) -> std::io::Result<T> {
    filesystem::artifact_transaction(path, "observations", operation)
}

pub(crate) fn observer_transaction<T>(
    path: &std::path::Path,
    operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> std::io::Result<T>,
) -> std::io::Result<T> {
    filesystem::artifact_transaction(path, "observer-jobs", operation)
}

pub(crate) fn observer_read(
    path: &std::path::Path,
    bound: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    namespace_read(path, "observer-jobs", bound)
}

pub(crate) fn namespace_read(
    path: &std::path::Path,
    namespace: &str,
    bound: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    filesystem::namespace_read(path, namespace, bound)
}

pub trait ContextStore: Send + Sync {
    fn record(&self, draft: ContextPlanDraft) -> Result<ContextPlan, ForgeError>;
    fn plan(&self, run_id: &str, ordinal: u32) -> Result<Option<ContextPlan>, ForgeError>;
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn plan_from_draft(mut draft: ContextPlanDraft, changed: bool) -> ContextPlan {
    draft.stable_prefix.changed_from_previous = changed;
    let id = format!("{}/{}", draft.run_id, draft.request_ordinal);
    let path = format!(
        ".forge/context/plans/{}/{}.json",
        draft.run_id, draft.request_ordinal
    );
    ContextPlan {
        version: draft.version,
        id,
        run_id: draft.run_id,
        session_id: draft.session_id,
        request_ordinal: draft.request_ordinal,
        components: draft.components,
        stable_prefix: draft.stable_prefix,
        reserved_output_tokens: draft.reserved_output_tokens,
        remaining_context_tokens: draft.remaining_context_tokens,
        path,
    }
}

/// Private, atomic context artifacts on Unix and Windows.
///
/// The project anchor (before `.forge`, or a custom root's parent) is trusted;
/// symlinks/reparse points within the owned subtree are never followed.
/// Unsupported platforms fail open through the caller.
#[derive(Debug)]
pub struct FsContextStore {
    root: PathBuf,
}

impl FsContextStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Native read-only lookup with typed missing/corrupt/unavailable outcomes.
    pub fn inspect_plan(&self, run_id: &str, ordinal: u32) -> std::io::Result<Option<ContextPlan>> {
        if !valid_id(run_id) {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        filesystem::plan(&self.root, run_id, ordinal)
    }

    fn validate(run_id: &str, session_id: Option<&str>) -> Result<(), ForgeError> {
        if !valid_id(run_id) || session_id.is_some_and(|id| !valid_id(id)) {
            return Err(ForgeError::session("invalid context plan identifier"));
        }
        Ok(())
    }
}

impl ContextStore for FsContextStore {
    fn record(&self, draft: ContextPlanDraft) -> Result<ContextPlan, ForgeError> {
        Self::validate(&draft.run_id, Some(&draft.session_id))?;
        filesystem::record(&self.root, draft)
            .map_err(|_| ForgeError::session("context plan storage unavailable"))
    }

    fn plan(&self, run_id: &str, ordinal: u32) -> Result<Option<ContextPlan>, ForgeError> {
        Self::validate(run_id, None)?;
        filesystem::plan(&self.root, run_id, ordinal)
            .map_err(|_| ForgeError::session("context plan storage unavailable"))
    }
}

// All traversal after opening the anchor is descriptor-relative: checking a
// pathname and then opening it would leave a symlink substitution race.
#[cfg(unix)]
mod filesystem {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path};
    use std::time::{Duration, Instant};

    use rustix::fs::{self, AtFlags, Mode, OFlags};

    use super::*;

    fn directory(parent: &File, name: &std::ffi::OsStr, create: bool) -> io::Result<File> {
        if create {
            match fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let file = File::from(fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if create {
            fs::fchmod(&file, Mode::from_raw_mode(0o700))?;
        }
        Ok(file)
    }

    fn root(path: &Path, create: bool) -> io::Result<File> {
        // The project location is trusted (and may contain platform symlinks
        // such as macOS /var). Never resolve symlinks in .forge or the context
        // subtree. For custom roots, only the parent location is trusted.
        let parts: Vec<_> = path.components().collect();
        let boundary = parts
            .iter()
            .position(|part| part.as_os_str() == ".forge")
            .unwrap_or(parts.len().saturating_sub(1));
        let anchor: PathBuf = parts[..boundary].iter().collect();
        let anchor = if anchor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &anchor
        };
        let mut dir = File::open(anchor.canonicalize()?)?;
        let parts = &parts[boundary..];
        for (index, part) in parts.iter().enumerate() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    // Ancestors are traversed without chmod; only newly created
                    // ancestors and the storage root receive private permissions.
                    if create {
                        match fs::mkdirat(&dir, *name, Mode::from_raw_mode(0o700)) {
                            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    dir = directory(&dir, name, create && index + 1 == parts.len())?;
                }
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            }
        }
        Ok(dir)
    }

    fn file(parent: &File, name: &str, flags: OFlags) -> io::Result<File> {
        // APFS can transiently report ENOENT for concurrent O_CREAT opens of
        // the same entry, even with its parent pinned. Retry only that case,
        // with exactly the same security flags and a bounded deadline.
        let deadline = Instant::now() + Duration::from_millis(100);
        let file = loop {
            match fs::openat(
                parent,
                name,
                flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => break File::from(fd),
                Err(rustix::io::Errno::NOENT)
                    if flags.contains(OFlags::CREATE) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => return Err(error.into()),
            }
        };
        let metadata = file.metadata()?;
        // A reader can hold the previous inode across an atomic replacement;
        // zero links is valid then. Multiple links would expose another file.
        if !metadata.is_file() || metadata.nlink() > 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(file)
    }

    fn read<T: serde::de::DeserializeOwned>(parent: &File, name: &str) -> io::Result<Option<T>> {
        let file = match file(parent, name, OFlags::RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let bytes = super::read_bounded(file, 64 * 1024)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| io::ErrorKind::InvalidData.into())
    }

    fn write<T: serde::Serialize>(parent: &File, name: &str, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|_| io::ErrorKind::InvalidData)?;
        let temporary = format!(".{}.tmp", ulid::Ulid::new());
        let mut output = file(
            parent,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        )?;
        let result = (|| {
            output.write_all(&bytes)?;
            output.sync_all()?;
            // Rename replaces the directory entry, never follows a destination
            // symlink, and never exposes a missing/partially written document.
            fs::renameat(parent, &temporary, parent, name)?;
            parent.sync_all()
        })();
        if result.is_err() {
            let _ = fs::unlinkat(parent, &temporary, AtFlags::empty());
        }
        result
    }

    struct ArtifactDirectory {
        root: File,
        objects: File,
    }

    impl ArtifactDirectory {
        fn parent(&self, name: &str) -> io::Result<&File> {
            if !crate::artifact::valid_artifact_filename(name) {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            Ok(if name == "index.json" {
                &self.root
            } else {
                &self.objects
            })
        }
    }

    impl crate::artifact::ArtifactFiles for ArtifactDirectory {
        fn read(&self, name: &str, bound: usize) -> io::Result<Option<Vec<u8>>> {
            let input = match file(self.parent(name)?, name, OFlags::RDONLY) {
                Ok(input) => input,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            fs::fchmod(&input, Mode::from_raw_mode(0o600))?;
            super::read_bounded(input, bound).map(Some)
        }

        fn write(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
            let parent = self.parent(name)?;
            match file(parent, name, OFlags::RDONLY) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            // Fixed crash-recoverable staging name, exclusively protected by
            // the permanent project lock. Validate before truncating/chmod.
            let mut output = file(parent, ".pending", OFlags::WRONLY | OFlags::CREATE)?;
            fs::fchmod(&output, Mode::from_raw_mode(0o600))?;
            output.set_len(0)?;
            output.write_all(bytes)?;
            output.sync_all()?;
            fs::renameat(parent, ".pending", parent, name)?;
            parent.sync_all()
        }

        fn remove(&mut self, name: &str) -> io::Result<()> {
            let parent = self.parent(name)?;
            // Reject hardlinks/symlinks even for deletion. unlinkat never
            // follows a replacement between this check and the unlink.
            match file(parent, name, OFlags::RDONLY) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
            fs::unlinkat(parent, name, AtFlags::empty())?;
            parent.sync_all()
        }
    }

    pub(super) fn artifact_transaction<T>(
        path: &Path,
        namespace: &str,
        operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> io::Result<T>,
    ) -> io::Result<T> {
        let context = root(path, true)?;
        let root = directory(&context, namespace.as_ref(), true)?;
        let lock = file(&root, "lock", OFlags::RDWR | OFlags::CREATE)?;
        fs::fchmod(&lock, Mode::from_raw_mode(0o600))?;
        super::lock_artifacts(&lock)?;
        let objects = directory(&root, "objects".as_ref(), true)?;
        // Recover the only possible staging entries from an interrupted
        // transaction, including when limits have since been lowered.
        for parent in [&root, &objects] {
            match file(parent, ".pending", OFlags::RDONLY) {
                Ok(_) => {
                    fs::unlinkat(parent, ".pending", AtFlags::empty())?;
                    parent.sync_all()?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        operation(&mut ArtifactDirectory { root, objects })
    }

    pub(super) fn namespace_read(
        path: &Path,
        namespace: &str,
        bound: usize,
    ) -> io::Result<Option<Vec<u8>>> {
        let result = (|| {
            let context = root(path, false)?;
            let root = directory(&context, namespace.as_ref(), false)?;
            super::read_bounded(file(&root, "index.json", OFlags::RDONLY)?, bound).map(Some)
        })();
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            result => result,
        }
    }

    pub(super) fn record(path: &Path, draft: ContextPlanDraft) -> io::Result<ContextPlan> {
        let root = root(path, true)?;
        let locks = directory(&root, "locks".as_ref(), true)?;
        let lock = file(&locks, &draft.session_id, OFlags::RDWR | OFlags::CREATE)?;
        fs::fchmod(&lock, Mode::from_raw_mode(0o600))?;
        // Kernel locks are released on crash. Never unlink lock files: doing so
        // could let contenders lock different inodes for the same session.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return Err(io::ErrorKind::WouldBlock.into()),
            }
        }
        let latest = directory(&root, "latest".as_ref(), true)?;
        let latest_name = format!("{}.json", draft.session_id);
        let previous: Option<String> = read(&latest, &latest_name)?;
        let changed = previous.is_some_and(|hash| hash != draft.stable_prefix.combined_hash);
        let plan = plan_from_draft(draft, changed);
        let plans = directory(&root, "plans".as_ref(), true)?;
        let run = directory(&plans, plan.run_id.as_ref(), true)?;
        write(&run, &format!("{}.json", plan.request_ordinal), &plan)?;
        write(&latest, &latest_name, &plan.stable_prefix.combined_hash)?;
        Ok(plan)
    }

    pub(super) fn plan(path: &Path, run_id: &str, ordinal: u32) -> io::Result<Option<ContextPlan>> {
        let result = (|| {
            let root = root(path, false)?;
            let plans = directory(&root, "plans".as_ref(), false)?;
            let run = directory(&plans, run_id.as_ref(), false)?;
            read(&run, &format!("{ordinal}.json"))
        })();
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            result => result,
        }
    }

    #[test]
    fn permanent_create_enoent_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let parent = File::open(temp.path()).unwrap();
        let start = Instant::now();
        let error = file(&parent, "absent/lock", OFlags::RDWR | OFlags::CREATE).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert!(start.elapsed() < Duration::from_secs(1));
        // Non-creating reads are not retried.
        let start = Instant::now();
        assert!(file(&parent, "absent", OFlags::RDONLY).is_err());
        assert!(start.elapsed() < Duration::from_millis(100));
    }
}

#[cfg(windows)]
#[path = "store/windows.rs"]
mod filesystem;

#[cfg(not(any(unix, windows)))]
mod filesystem {
    use super::*;
    use std::{io, path::Path};

    pub(super) fn namespace_read(_: &Path, _: &str, _: usize) -> io::Result<Option<Vec<u8>>> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn artifact_transaction<T>(
        _: &Path,
        _: &str,
        _: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> io::Result<T>,
    ) -> io::Result<T> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn record(_: &Path, _: ContextPlanDraft) -> io::Result<ContextPlan> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn plan(_: &Path, _: &str, _: u32) -> io::Result<Option<ContextPlan>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(any(unix, windows))]
fn read_bounded(input: std::fs::File, bound: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if input.metadata()?.len() > bound as u64 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut bytes = Vec::new();
    input
        .take((bound as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > bound {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

#[cfg(any(unix, windows))]
fn lock_artifacts(lock: &std::fs::File) -> std::io::Result<()> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err(std::io::ErrorKind::WouldBlock.into()),
        }
    }
}

#[derive(Clone, Default)]
pub struct MemoryContextStore {
    state: Arc<Mutex<MemoryState>>,
}

#[derive(Default)]
struct MemoryState {
    plans: HashMap<(String, u32), ContextPlan>,
    latest: HashMap<String, String>,
    failure: Option<String>,
}

impl MemoryContextStore {
    pub fn failing(message: impl Into<String>) -> Self {
        let store = Self::default();
        store.state.lock().unwrap().failure = Some(message.into());
        store
    }
}

impl ContextStore for MemoryContextStore {
    fn record(&self, draft: ContextPlanDraft) -> Result<ContextPlan, ForgeError> {
        if !valid_id(&draft.run_id) || !valid_id(&draft.session_id) {
            return Err(ForgeError::session("invalid context plan identifier"));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(message) = &state.failure {
            return Err(ForgeError::session(message.clone()));
        }
        let changed = state
            .latest
            .get(&draft.session_id)
            .is_some_and(|hash| hash != &draft.stable_prefix.combined_hash);
        let plan = plan_from_draft(draft, changed);
        state.latest.insert(
            plan.session_id.clone(),
            plan.stable_prefix.combined_hash.clone(),
        );
        state
            .plans
            .insert((plan.run_id.clone(), plan.request_ordinal), plan.clone());
        Ok(plan)
    }

    fn plan(&self, run_id: &str, ordinal: u32) -> Result<Option<ContextPlan>, ForgeError> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .plans
            .get(&(run_id.to_string(), ordinal))
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(unix, windows))]
    use std::fs;
    #[cfg(any(unix, windows))]
    use std::sync::Arc;

    use crate::{CONTEXT_PLAN_VERSION, ContextComponents, stable_prefix};

    use super::*;

    pub(super) fn draft(run: &str, session: &str, ordinal: u32, marker: &str) -> ContextPlanDraft {
        ContextPlanDraft {
            version: CONTEXT_PLAN_VERSION,
            run_id: run.into(),
            session_id: session.into(),
            request_ordinal: ordinal,
            components: ContextComponents::default(),
            stable_prefix: stable_prefix(&[forge_core::Message::system(marker)], &[]),
            reserved_output_tokens: None,
            remaining_context_tokens: 100,
        }
    }

    #[test]
    fn memory_store_compares_prefixes() {
        let store = MemoryContextStore::default();
        assert!(
            !store
                .record(draft("r1", "s", 1, "a"))
                .unwrap()
                .stable_prefix
                .changed_from_previous
        );
        assert!(
            !store
                .record(draft("r2", "s", 1, "a"))
                .unwrap()
                .stable_prefix
                .changed_from_previous
        );
        assert!(
            store
                .record(draft("r3", "s", 1, "b"))
                .unwrap()
                .stable_prefix
                .changed_from_previous
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn filesystem_state_survives_restart_and_contains_no_content() {
        let temp = tempfile::tempdir().unwrap();
        FsContextStore::new(temp.path())
            .record(draft("r1", "s", 1, "secret marker"))
            .unwrap();
        let plan = FsContextStore::new(temp.path())
            .record(draft("r2", "s", 1, "changed"))
            .unwrap();
        assert!(plan.stable_prefix.changed_from_previous);
        let bytes = fs::read(temp.path().join("plans/r1/1.json")).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("secret marker"));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn concurrent_records_are_valid_json() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(FsContextStore::new(temp.path()));
        let handles: Vec<_> = (1..=8)
            .map(|ordinal| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .record(draft("run", "session", ordinal, "same"))
                        .unwrap()
                })
            })
            .collect();
        for handle in handles {
            let plan = handle.join().unwrap();
            assert_eq!(store.plan("run", plan.request_ordinal).unwrap(), Some(plan));
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn malformed_latest_state_is_typed_error() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("latest")).unwrap();
        fs::write(temp.path().join("latest/s.json"), b"not json").unwrap();
        let error = FsContextStore::new(temp.path())
            .record(draft("r", "s", 1, "x"))
            .unwrap_err();
        assert!(matches!(error, ForgeError::Session(_)));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn independent_stores_serialize_compare_and_write() {
        let temp = tempfile::tempdir().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|index| {
                let root = temp.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    FsContextStore::new(root)
                        .record(draft("run", "session", index, &index.to_string()))
                        .unwrap()
                })
            })
            .collect();
        let unchanged = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|plan| !plan.stable_prefix.changed_from_previous)
            .count();
        assert_eq!(unchanged, 1);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn process_writer() {
        let Some(root) = std::env::var_os("FORGE_CONTEXT_TEST_ROOT") else {
            return;
        };
        let ordinal: u32 = std::env::var("FORGE_CONTEXT_TEST_ORDINAL")
            .unwrap()
            .parse()
            .unwrap();
        let root = PathBuf::from(root);
        fs::write(root.join(format!("ready-{ordinal}")), "").unwrap();
        let start = std::time::Instant::now();
        while !root.join("start").exists() {
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        FsContextStore::new(root)
            .record(draft("run", "session", ordinal, &ordinal.to_string()))
            .unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn independent_processes_serialize_compare_and_write() {
        let temp = tempfile::tempdir().unwrap();
        let mut children: Vec<_> = (0..8)
            .map(|ordinal| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "store::tests::process_writer"])
                    .env("FORGE_CONTEXT_TEST_ROOT", temp.path())
                    .env("FORGE_CONTEXT_TEST_ORDINAL", ordinal.to_string())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let start = std::time::Instant::now();
        while !(0..8).all(|ordinal| temp.path().join(format!("ready-{ordinal}")).exists()) {
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        fs::write(temp.path().join("start"), "").unwrap();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let store = FsContextStore::new(temp.path());
        assert_eq!(
            (0..8)
                .map(|index| store.plan("run", index).unwrap().unwrap())
                .filter(|plan| !plan.stable_prefix.changed_from_previous)
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_never_modify_victims() {
        use std::os::unix::fs::symlink;
        for component in [
            ".forge",
            ".forge/context",
            ".forge/context/plans",
            ".forge/context/plans/run",
            ".forge/context/latest",
            ".forge/context/locks",
        ] {
            let project = tempfile::tempdir().unwrap();
            let victim = tempfile::tempdir().unwrap();
            let link = project.path().join(component);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(victim.path(), link).unwrap();
            assert!(
                FsContextStore::new(project.path().join(".forge/context"))
                    .record(draft("run", "s", 1, "a"))
                    .is_err()
            );
            assert_eq!(fs::read_dir(victim.path()).unwrap().count(), 0);
        }
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "private content").unwrap();
        let root = temp.path().join("context");
        fs::create_dir_all(root.join("plans/run")).unwrap();
        symlink(
            &victim,
            root.join(format!("plans/run/1.tmp-{}", std::process::id())),
        )
        .unwrap();
        symlink(&victim, root.join("plans/run/1.json")).unwrap();
        let store = FsContextStore::new(&root);
        store.record(draft("run", "s", 1, "a")).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "private content");
        fs::remove_file(root.join("latest/s.json")).unwrap();
        symlink(&victim, root.join("latest/s.json")).unwrap();
        assert!(store.record(draft("run", "s", 2, "b")).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "private content");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn replacement_has_no_missing_or_partial_read_window() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsContextStore::new(temp.path());
        store.record(draft("run", "s", 1, "a")).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for index in 0..100 {
                    FsContextStore::new(temp.path())
                        .record(draft("run", "s", 1, &index.to_string()))
                        .unwrap();
                }
            });
            for _ in 0..1000 {
                assert!(store.plan("run", 1).unwrap().is_some());
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn private_modes_and_project_relative_path() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".forge/context");
        let plan = FsContextStore::new(&root)
            .record(draft("run", "s", 1, "a"))
            .unwrap();
        assert!(temp.path().join(&plan.path).is_file());
        for path in ["", "plans", "plans/run", "latest", "locks"] {
            assert_eq!(
                fs::metadata(root.join(path)).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for path in ["plans/run/1.json", "latest/s.json", "locks/s"] {
            assert_eq!(
                fs::metadata(root.join(path)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn malformed_content_is_never_in_errors() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsContextStore::new(temp.path());
        store.record(draft("run", "s", 1, "a")).unwrap();
        for content in [
            r#"{"secret-value":"private"}"#,
            r#"{"version":"secret-value"}"#,
        ] {
            fs::write(temp.path().join("latest/s.json"), content).unwrap();
            let error = store.record(draft("run", "s", 2, "b")).unwrap_err();
            assert!(!error.to_string().contains("secret-value"));
            fs::write(temp.path().join("plans/run/1.json"), content).unwrap();
            assert!(
                !store
                    .plan("run", 1)
                    .unwrap_err()
                    .to_string()
                    .contains("secret-value")
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn lock_timeout_is_bounded_and_recovers() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsContextStore::new(temp.path());
        store.record(draft("run", "s", 1, "a")).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp.path().join("locks/s"))
            .unwrap();
        lock.lock().unwrap();
        let start = std::time::Instant::now();
        assert!(
            FsContextStore::new(temp.path())
                .record(draft("run", "s", 2, "b"))
                .is_err()
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert!(store.plan("run", 2).unwrap().is_none());
        drop(lock);
        assert!(
            store
                .record(draft("run", "s", 2, "b"))
                .unwrap()
                .stable_prefix
                .changed_from_previous
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn process_lock_holder() {
        let Some(root) = std::env::var_os("FORGE_CONTEXT_TEST_LOCK_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("locks/s"))
            .unwrap();
        lock.lock().unwrap();
        fs::write(root.join("locked"), "").unwrap();
        // Parent forcibly terminates this process to test kernel lock recovery.
        std::thread::sleep(std::time::Duration::from_secs(15));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn process_lock_timeout_and_crash_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsContextStore::new(temp.path());
        store.record(draft("run", "s", 1, "a")).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::tests::process_lock_holder"])
            .env("FORGE_CONTEXT_TEST_LOCK_ROOT", temp.path())
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        while !temp.path().join("locked").exists() {
            if start.elapsed() >= std::time::Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("lock holder did not become ready");
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let start = std::time::Instant::now();
        let result = store.record(draft("run", "s", 2, "b"));
        let elapsed = start.elapsed();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(result.is_err());
        assert!(elapsed >= std::time::Duration::from_secs(1));
        assert!(elapsed < std::time::Duration::from_secs(2));
        assert!(store.plan("run", 2).unwrap().is_none());
        assert!(
            store
                .record(draft("run", "s", 2, "b"))
                .unwrap()
                .stable_prefix
                .changed_from_previous
        );
    }

    #[cfg(not(any(unix, windows)))]
    #[test]
    fn unsupported_platform_returns_fail_open_error() {
        assert!(
            FsContextStore::new("unused")
                .record(draft("run", "s", 1, "a"))
                .is_err()
        );
    }
}
