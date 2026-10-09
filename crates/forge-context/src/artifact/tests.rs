use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
struct Clock(AtomicU64);
impl ArtifactClock for Clock {
    fn now_secs(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
impl Clock {
    fn set(&self, value: u64) {
        self.0.store(value, Ordering::SeqCst);
    }
}

fn source(session: &str, call: &str) -> ArtifactSource {
    ArtifactSource {
        session_id: session.into(),
        run_id: "run".into(),
        call_id: call.into(),
        event_seq: 1,
    }
}

fn output(text: &str) -> SanitizedOutput {
    SanitizedOutput::new(&Redactor::new(), text)
}
fn range() -> ArtifactQuery {
    ArtifactQuery::Range {
        start: 0,
        end: 1024,
    }
}

fn read(store: &dyn ArtifactStore, reference: &ArtifactRef) -> Option<ArtifactRead> {
    store
        .retrieve(&reference.handle, std::slice::from_ref(reference), range())
        .unwrap()
}

#[test]
fn sanitized_objects_dedupe_but_handles_require_exact_source() {
    let store = MemoryArtifactStore::default();
    let text = output("tail sk-abcdefgh123456 Bearer abc.def ghp_abcdefgh123456");
    let first = store.put(source("one", "call"), &text).unwrap();
    let second = store.put(source("two", "call"), &text).unwrap();
    assert_ne!(first.handle, second.handle);
    let files = store.files.lock().unwrap();
    let index = load(&*files).unwrap();
    assert_eq!(index.objects.len(), 1);
    assert_eq!(index.manifests.len(), 2);
    assert_eq!(payload(&index), text.as_str().len() as u64);
    let object = index.objects.keys().next().unwrap();
    assert_ne!(object, &first.handle);
    for bytes in files.values() {
        let value = String::from_utf8_lossy(bytes);
        for secret in ["sk-abcdefgh123456", "abc.def", "ghp_abcdefgh123456"] {
            assert!(!value.contains(secret));
        }
    }
    drop(files);
    assert!(
        store
            .retrieve(&first.handle, &[], range())
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .retrieve(&first.handle, std::slice::from_ref(&second), range())
            .unwrap()
            .is_none()
    );
    let mut forged = first.clone();
    forged.source.session_id = "two".into();
    assert!(
        store
            .retrieve(&first.handle, &[forged], range())
            .unwrap()
            .is_none()
    );
    assert_eq!(read(&store, &first).unwrap().text, text.as_str());
    assert_eq!(read(&store, &second).unwrap().text, text.as_str());
    assert_ne!(
        identity_for_policy(text.as_str(), 1),
        identity_for_policy(text.as_str(), 2)
    );
}

#[test]
fn bounds_utf8_literal_search_json_escaping_and_continuation() {
    let store = MemoryArtifactStore::default();
    let text = output(&format!("a😀é tail [.*] {}", "\u{0001}\"\\\n".repeat(8000)));
    let reference = store.put(source("one", "call"), &text).unwrap();
    let query = |q| {
        store
            .retrieve(&reference.handle, std::slice::from_ref(&reference), q)
            .unwrap()
            .unwrap()
    };
    let read = query(ArtifactQuery::Range { start: 2, end: 8 });
    assert_eq!(read.text, "é ");
    assert_eq!(read.start, 5);
    let tiny = query(ArtifactQuery::Range { start: 1, end: 2 });
    assert_eq!(tiny.text, "");
    assert_eq!(tiny.next_start, Some(5));
    let found = query(ArtifactQuery::Search {
        literal: "[.*]".into(),
        start: 0,
        limit: 4,
    });
    assert_eq!(found.text, "[.*]");
    let missing = query(ArtifactQuery::Search {
        literal: "not present".into(),
        start: 0,
        limit: 50,
    });
    assert_eq!(missing.start, text.as_str().len());
    assert_eq!(missing.next_start, None);
    let escaped = query(ArtifactQuery::Range {
        start: 0,
        end: MAX_ARTIFACT_RESPONSE_BYTES,
    });
    assert!(serde_json::to_vec(&escaped).unwrap().len() <= MAX_ARTIFACT_RESPONSE_BYTES - 512);
    assert_eq!(escaped.text, text.as_str()[escaped.start..escaped.end]);
    assert_eq!(escaped.next_start, Some(escaped.end));
    let next = query(ArtifactQuery::Range {
        start: escaped.end,
        end: escaped.end + 100,
    });
    assert_eq!(next.text, text.as_str()[next.start..next.end]);
    for invalid in [
        ArtifactQuery::Range { start: 0, end: 0 },
        ArtifactQuery::Range {
            start: 0,
            end: usize::MAX,
        },
        ArtifactQuery::Range { start: 3, end: 2 },
        ArtifactQuery::Search {
            literal: "".into(),
            start: 0,
            limit: 1,
        },
        ArtifactQuery::Search {
            literal: "x".into(),
            start: 0,
            limit: 0,
        },
        ArtifactQuery::Search {
            literal: "x".into(),
            start: 0,
            limit: usize::MAX,
        },
    ] {
        assert!(
            store
                .retrieve(&reference.handle, std::slice::from_ref(&reference), invalid)
                .is_err()
        );
    }
}

#[test]
fn lru_absolute_age_and_limit_rejection_use_injected_time() {
    let clock = Arc::new(Clock::default());
    let limits = ArtifactLimits {
        max_project_bytes: 8,
        max_artifact_bytes: 4,
        max_age_secs: 10,
    };
    let store = MemoryArtifactStore::with_clock(limits, clock.clone());
    let first = store.put(source("one", "a"), &output("aaaa")).unwrap();
    clock.set(1);
    let second = store.put(source("one", "b"), &output("bbbb")).unwrap();
    clock.set(2);
    assert!(read(&store, &first).is_some()); // first is now most recently used
    let third = store.put(source("one", "c"), &output("cccc")).unwrap();
    assert!(read(&store, &second).is_none());
    assert!(read(&store, &first).is_some());
    assert!(read(&store, &third).is_some());
    let before = store.files.lock().unwrap().clone();
    assert!(
        store
            .put(source("one", "too-big"), &output("xxxxx"))
            .is_err()
    );
    assert_eq!(before, *store.files.lock().unwrap()); // no truncated artifact
    clock.set(10); // expiration is >= age, not > age, and reads don't extend it
    assert!(read(&store, &first).is_none());
    assert!(read(&store, &third).is_some());
    clock.set(12);
    assert!(read(&store, &third).is_none());
    assert_eq!(
        load(&*store.files.lock().unwrap()).unwrap().objects.len(),
        0
    );
}

#[test]
fn equal_time_lru_tie_breaks_by_handle_and_counts_are_bounded() {
    // Seed a bounded metadata table directly, avoiding 4096 repeated serializations
    // merely to arrange the regression. All records still pass normal validation.
    let store =
        MemoryArtifactStore::with_clock(ArtifactLimits::default(), Arc::new(Clock::default()));
    let text = output("shared");
    let reference = store.put(source("one", "first"), &text).unwrap();
    {
        let mut files = store.files.lock().unwrap();
        let mut index = load(&*files).unwrap();
        let original = index.manifests.remove(&reference.handle).unwrap();
        for number in 0..MAX_ARTIFACT_MANIFESTS {
            index.manifests.insert(
                ulid::Ulid::from(number as u128).to_string(),
                Manifest {
                    source: original.source.clone(),
                    object: original.object.clone(),
                    policy: original.policy,
                    bytes: original.bytes,
                    created: 0,
                    accessed: 0,
                },
            );
        }
        save(&mut *files, &index).unwrap();
    }
    store.put(source("two", "new"), &text).unwrap();
    let index = load(&*store.files.lock().unwrap()).unwrap();
    assert_eq!(index.manifests.len(), MAX_ARTIFACT_MANIFESTS);
    assert!(
        !index
            .manifests
            .contains_key(&ulid::Ulid::from(0u128).to_string())
    );
    assert!(
        index
            .manifests
            .contains_key(&ulid::Ulid::from(1u128).to_string())
    );
    assert_eq!(index.objects.len(), 1);
}

struct FailingFiles {
    files: BTreeMap<String, Vec<u8>>,
    step: usize,
    fail_at: usize,
}
impl ArtifactFiles for FailingFiles {
    fn read(&self, name: &str, bound: usize) -> io::Result<Option<Vec<u8>>> {
        self.files.read(name, bound)
    }
    fn write(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.step += 1;
        if self.step == self.fail_at {
            return Err(io::ErrorKind::Other.into());
        }
        self.files.write(name, bytes)
    }
    fn remove(&mut self, name: &str) -> io::Result<()> {
        self.step += 1;
        if self.step == self.fail_at {
            return Err(io::ErrorKind::Other.into());
        }
        ArtifactFiles::remove(&mut self.files, name)
    }
}

#[test]
fn every_interrupted_transaction_is_recoverable_without_orphans() {
    let limits = ArtifactLimits {
        max_project_bytes: 4,
        max_artifact_bytes: 4,
        max_age_secs: 50,
    };
    let mut base = BTreeMap::new();
    put(
        &mut base,
        source("one", "first"),
        &output("aaaa"),
        limits,
        0,
    )
    .unwrap();
    for fail_at in 1..=12 {
        let mut files = FailingFiles {
            files: base.clone(),
            step: 0,
            fail_at,
        };
        let _ = put(
            &mut files,
            source("one", "second"),
            &output("bbbb"),
            limits,
            1,
        );
        files.fail_at = usize::MAX;
        let reference = put(
            &mut files,
            source("one", "retry"),
            &output("cccc"),
            limits,
            2,
        )
        .unwrap();
        let index = load(&files).unwrap();
        assert_eq!(index.manifests.len(), 1, "failure at {fail_at}");
        assert_eq!(index.objects.len(), 1, "failure at {fail_at}");
        assert_eq!(files.files.len(), 2, "failure at {fail_at}");
        assert!(
            retrieve(
                &mut files,
                &reference.handle,
                std::slice::from_ref(&reference),
                &range(),
                limits,
                2
            )
            .unwrap()
            .is_some()
        );
    }
}

#[test]
fn corrupt_index_paths_lengths_and_objects_are_not_trusted() {
    let store = MemoryArtifactStore::default();
    let reference = store.put(source("one", "call"), &output("hello")).unwrap();
    let snapshot = store.files.lock().unwrap().clone();
    for corrupt_hash in ["../victim", "/absolute", "x:y", "index.json"] {
        let mut index = load(&snapshot).unwrap();
        index.objects.insert(
            corrupt_hash.into(),
            Object {
                bytes: 1,
                policy: REDACTION_POLICY_VERSION,
            },
        );
        save(&mut *store.files.lock().unwrap(), &index).unwrap();
        assert!(
            store
                .retrieve(&reference.handle, std::slice::from_ref(&reference), range())
                .is_err()
        );
    }
    *store.files.lock().unwrap() = snapshot.clone();
    let object = identity("hello");
    store
        .files
        .lock()
        .unwrap()
        .insert(object.clone(), b"hellX".to_vec());
    assert!(
        store
            .retrieve(&reference.handle, std::slice::from_ref(&reference), range())
            .is_err()
    );
    *store.files.lock().unwrap() = snapshot;
    store
        .files
        .lock()
        .unwrap()
        .insert(object.clone(), vec![b'x'; 1_000_000]);
    assert!(
        store
            .retrieve(&reference.handle, std::slice::from_ref(&reference), range())
            .is_err()
    );
    store.files.lock().unwrap().remove(&object);
    assert!(read(&store, &reference).is_none());
}

#[cfg(any(unix, windows))]
#[test]
fn native_roundtrip_private_bytes_and_stale_staging_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".forge/context");
    let clock = Arc::new(Clock::default());
    let store = FsArtifactStore::with_clock(&root, ArtifactLimits::default(), clock.clone());
    let text = output("full tail sk-abcdefghi123456");
    let reference = store.put(source("one", "call"), &text).unwrap();
    assert_eq!(read(&store, &reference).unwrap().text, text.as_str());
    let fresh = FsArtifactStore::with_clock(&root, ArtifactLimits::default(), clock);
    assert_eq!(read(&fresh, &reference).unwrap().text, text.as_str());
    let object = root.join("artifacts/objects").join(identity(text.as_str()));
    assert_eq!(std::fs::read(&object).unwrap(), text.as_str().as_bytes());
    for path in [
        root.join("artifacts/.pending"),
        root.join("artifacts/objects/.pending"),
    ] {
        std::fs::write(&path, "interrupted staging").unwrap();
    }
    assert!(read(&fresh, &reference).is_some());
    assert!(!root.join("artifacts/.pending").exists());
    assert!(!root.join("artifacts/objects/.pending").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [
            object,
            root.join("artifacts/index.json"),
            root.join("artifacts/lock"),
        ] {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for directory in [
            &root,
            &root.join("artifacts"),
            &root.join("artifacts/objects"),
        ] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}

#[cfg(any(unix, windows))]
#[test]
fn native_hardlinks_never_modify_victim_or_accept_reference() {
    for relative in [
        "artifacts/lock",
        "artifacts/index.json",
        "artifacts/.pending",
        "artifacts/objects/.pending",
        "artifacts/objects/OBJECT",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".forge/context");
        std::fs::create_dir_all(root.join("artifacts/objects")).unwrap();
        let victim = temp.path().join("victim");
        std::fs::write(&victim, "private victim").unwrap();
        let name = relative.replace("OBJECT", &identity("safe"));
        std::fs::hard_link(&victim, root.join(name)).unwrap();
        let store = FsArtifactStore::new(&root, ArtifactLimits::default());
        assert!(
            store.put(source("one", "call"), &output("safe")).is_err(),
            "{relative}"
        );
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "private victim");
    }
}

#[cfg(unix)]
#[test]
fn native_symlinks_never_escape_owned_subtree() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for relative in [
        "artifacts",
        "artifacts/objects",
        "artifacts/lock",
        "artifacts/index.json",
        "artifacts/.pending",
        "artifacts/objects/.pending",
        "artifacts/objects/OBJECT",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".forge/context");
        let target = root.join(relative.replace("OBJECT", &identity("safe")));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(victim.join("sentinel"), "untouched").unwrap();
        symlink(&victim, &target).unwrap();
        let store = FsArtifactStore::new(&root, ArtifactLimits::default());
        assert!(
            store.put(source("one", "call"), &output("safe")).is_err(),
            "{relative}"
        );
        assert_eq!(
            std::fs::read_to_string(victim.join("sentinel")).unwrap(),
            "untouched"
        );
        assert_eq!(
            std::fs::metadata(victim).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn artifact_process_writer() {
    let Ok(root) = std::env::var("FORGE_TEST_ARTIFACT_ROOT") else {
        return;
    };
    let writer = std::env::var("FORGE_TEST_ARTIFACT_WRITER").unwrap();
    let capacity = std::env::var("FORGE_TEST_ARTIFACT_CAPACITY")
        .unwrap()
        .parse()
        .unwrap();
    let limits = ArtifactLimits {
        max_project_bytes: capacity,
        max_artifact_bytes: capacity.min(ArtifactLimits::default().max_artifact_bytes),
        ..ArtifactLimits::default()
    };
    let store = FsArtifactStore::new(root, limits);
    for ordinal in 0..12 {
        store
            .put(
                source(&writer, &format!("call-{ordinal}")),
                &output(&format!("output-{ordinal}")),
            )
            .unwrap();
    }
}

#[cfg(any(unix, windows))]
fn concurrent_writers(root: &std::path::Path, capacity: u64) -> Index {
    let mut children = Vec::new();
    for writer in 0..4 {
        children.push(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "artifact::tests::artifact_process_writer",
                    "--nocapture",
                ])
                .env("FORGE_TEST_ARTIFACT_ROOT", root)
                .env("FORGE_TEST_ARTIFACT_WRITER", format!("writer-{writer}"))
                .env("FORGE_TEST_ARTIFACT_CAPACITY", capacity.to_string())
                .spawn()
                .unwrap(),
        );
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    serde_json::from_slice(&std::fs::read(root.join("artifacts/index.json")).unwrap()).unwrap()
}

#[cfg(any(unix, windows))]
#[test]
fn native_independent_processes_preserve_dedupe_and_manifests() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".forge/context");
    let index = concurrent_writers(&root, ArtifactLimits::default().max_project_bytes);
    assert_eq!(index.manifests.len(), 48);
    assert_eq!(index.objects.len(), 12);
    let store = FsArtifactStore::new(root, ArtifactLimits::default());
    for (handle, manifest) in index.manifests {
        let reference = ArtifactRef {
            handle,
            source: manifest.source,
        };
        assert!(read(&store, &reference).is_some());
    }
}

#[cfg(any(unix, windows))]
#[test]
fn native_independent_processes_enforce_shared_payload_capacity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".forge/context");
    let index = concurrent_writers(&root, 40);
    assert!(!index.manifests.is_empty());
    assert!(index.manifests.len() < 48);
    assert!(payload(&index) <= 40);
    let disk_bytes: u64 = std::fs::read_dir(root.join("artifacts/objects"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert_eq!(disk_bytes, payload(&index));
    assert_eq!(
        std::fs::read_dir(root.join("artifacts/objects"))
            .unwrap()
            .count(),
        index.objects.len()
    );
    let store = FsArtifactStore::new(
        &root,
        ArtifactLimits {
            max_project_bytes: 40,
            max_artifact_bytes: 40,
            ..ArtifactLimits::default()
        },
    );
    for (handle, manifest) in index.manifests {
        let reference = ArtifactRef {
            handle,
            source: manifest.source,
        };
        assert!(read(&store, &reference).is_some());
    }
}
