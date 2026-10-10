use super::*;

fn events(session: &str, count: usize) -> Vec<Event> {
    (0..count)
        .map(|i| {
            let mut e = Event::new(
                "run",
                session,
                EventKind::InputReceived {
                    message: format!("source {i}"),
                },
            );
            e.ts = "2026-10-09T00:00:00Z".parse().unwrap();
            e
        })
        .collect()
}
fn batch(
    session: &str,
    source: &[Event],
    start: u64,
    end: u64,
    version: &str,
) -> ValidatedObservationBatch {
    ValidatedObservationBatch::new(
        session,
        source,
        SourceRange { start, end },
        version,
        vec![
            ObservationDraft {
                scope: ObservationScope::Session,
                kind: ObservationKind::Decision,
                content: "use sk-abcdefgh12345678 safely".into(),
            },
            ObservationDraft {
                scope: ObservationScope::Session,
                kind: ObservationKind::Constraint,
                content: "retain source".into(),
            },
        ],
        &Redactor::new(),
    )
    .unwrap()
}
fn child(source: &[Event], parent: &str, id: &str, cut: usize) -> Vec<Event> {
    let mut out = source[..cut].to_vec();
    let mut marker = Event::new(
        "fork",
        id,
        EventKind::SessionForked {
            from_session: parent.into(),
            at_position: cut as u64,
        },
    );
    marker.ts = source[0].ts;
    out.push(marker);
    out.extend(events(id, 2));
    out
}
fn exercise(store: &dyn ObservationStore) {
    let redactor = Redactor::new();
    let source = events("parent", 8);
    let later = store.commit(batch("parent", &source, 4, 6, "v1")).unwrap();
    let first = store
        .commit(batch("parent", &source[..3], 1, 2, "v1"))
        .unwrap();
    assert_eq!(first.observations.len(), 2);
    assert!(!first.observations[0].content.contains("sk-abcdefgh"));
    let p = store.projection("parent", &source, &redactor).unwrap();
    assert_eq!(p.contiguous_watermark(), 2);
    assert_eq!(p.batches()[0], later);
    assert_eq!(
        p.covered_intervals(),
        vec![
            SourceRange { start: 1, end: 2 },
            SourceRange { start: 4, end: 6 }
        ]
    );
    assert_eq!(
        store
            .commit(batch("parent", &source, 2, 3, "v2"))
            .unwrap_err(),
        ObservationError::Overlap
    );
    let copied = child(&source, "parent", "child", 5);
    let frozen = store
        .fork("parent", &source, "child", &copied, 5, &redactor)
        .unwrap();
    assert_eq!(frozen.batches(), &[first]);
    store.commit(batch("parent", &source, 3, 3, "v1")).unwrap();
    assert_eq!(
        store
            .projection("child", &copied, &redactor)
            .unwrap()
            .batches(),
        frozen.batches()
    );
    store.commit(batch("child", &copied, 7, 8, "v1")).unwrap();
    let nested = child(&copied, "child", "nested", 8);
    let nested_projection = store
        .fork("child", &copied, "nested", &nested, 8, &redactor)
        .unwrap();
    assert_eq!(nested_projection.batches().len(), 2);
    assert_eq!(nested_projection.batches()[0].source_session_id, "parent");
    assert_eq!(nested_projection.batches()[1].source_session_id, "child");
    assert!(
        ValidatedObservationBatch::new(
            "nested",
            &nested,
            SourceRange { start: 3, end: 3 },
            "v1",
            vec![],
            &redactor
        )
        .is_err()
    );
    let mut changed = source.clone();
    changed[0].kind = EventKind::InputReceived {
        message: "changed".into(),
    };
    assert_eq!(
        store.projection("parent", &changed, &redactor).unwrap_err(),
        ObservationError::SourceChanged
    );
    let mut wrong = child(&source, "parent", "badchild", 2);
    wrong[0] = changed[0].clone();
    assert_eq!(
        store
            .fork("parent", &source, "badchild", &wrong, 2, &redactor)
            .unwrap_err(),
        ObservationError::InvalidFork
    );
}
#[test]
fn memory_contract() {
    exercise(&MemoryObservationStore::default());
}

#[test]
fn source_bounds_and_unsupported_values_fail_without_content() {
    let redactor = Redactor::new();
    let mut source = events("s", 1);
    source[0].kind = EventKind::InputReceived {
        message: "private".repeat(200_000),
    };
    assert!(matches!(
        ValidatedObservationBatch::new(
            "s",
            &source,
            SourceRange { start: 1, end: 1 },
            "v1",
            vec![],
            &redactor
        ),
        Err(ObservationError::LimitExceeded)
    ));
    source[0].kind = EventKind::RoutingDecisionMade {
        router: "r".into(),
        selected_model: "m".into(),
        confidence: f64::NAN,
        fallback_used: false,
        reason: String::new(),
    };
    assert!(matches!(
        ValidatedObservationBatch::new(
            "s",
            &source,
            SourceRange { start: 1, end: 1 },
            "v1",
            vec![],
            &redactor
        ),
        Err(ObservationError::InvalidSource)
    ));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        source[0].kind = EventKind::FileChanged {
            path: std::ffi::OsString::from_vec(vec![0xff]).into(),
        };
        assert!(matches!(
            ValidatedObservationBatch::new(
                "s",
                &source,
                SourceRange { start: 1, end: 1 },
                "v1",
                vec![],
                &redactor
            ),
            Err(ObservationError::InvalidSource)
        ));
    }
}

#[cfg(any(unix, windows))]
#[test]
fn native_contract_restart_and_deterministic_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    exercise(&FsObservationStore::new(dir.path()));
    let bytes = std::fs::read(dir.path().join("observations/index.json")).unwrap();
    let source = events("parent", 8);
    assert_eq!(
        FsObservationStore::new(dir.path())
            .projection("parent", &source, &Redactor::new())
            .unwrap()
            .contiguous_watermark(),
        6
    );
    std::fs::remove_file(dir.path().join("observations/index.json")).unwrap();
    exercise(&FsObservationStore::new(dir.path()));
    assert_eq!(
        bytes,
        std::fs::read(dir.path().join("observations/index.json")).unwrap()
    );
}
#[test]
fn invalid_source_metadata_content_and_empty_batches() {
    let redactor = Redactor::new();
    let source = events("s", 2);
    for range in [
        SourceRange { start: 0, end: 1 },
        SourceRange { start: 2, end: 1 },
        SourceRange { start: 1, end: 3 },
    ] {
        assert!(
            ValidatedObservationBatch::new("s", &source, range, "v1", vec![], &redactor).is_err()
        );
    }
    for id in ["../secret", "sk-abcdefgh12345678", ""] {
        assert!(
            ValidatedObservationBatch::new(
                id,
                &source,
                SourceRange { start: 1, end: 1 },
                "v1",
                vec![],
                &redactor
            )
            .is_err()
        );
        assert!(
            ValidatedObservationBatch::new(
                "s",
                &source,
                SourceRange { start: 1, end: 1 },
                id,
                vec![],
                &redactor
            )
            .is_err()
        );
    }
    for content in [" ".into(), "x".repeat(MAX_OBSERVATION_CONTENT_BYTES + 1)] {
        assert!(
            ValidatedObservationBatch::new(
                "s",
                &source,
                SourceRange { start: 1, end: 1 },
                "v1",
                vec![ObservationDraft {
                    scope: ObservationScope::Session,
                    kind: ObservationKind::State,
                    content
                }],
                &redactor
            )
            .is_err()
        );
    }
    let store = MemoryObservationStore::default();
    store
        .commit(
            ValidatedObservationBatch::new(
                "s",
                &source,
                SourceRange { start: 1, end: 2 },
                "v1",
                vec![],
                &redactor,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        store
            .projection("s", &source, &redactor)
            .unwrap()
            .contiguous_watermark(),
        2
    );
}
#[cfg(any(unix, windows))]
#[test]
fn native_corrupt_and_oversized_reads_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsObservationStore::new(dir.path());
    let source = events("s", 1);
    store.commit(batch("s", &source, 1, 1, "v1")).unwrap();
    let path = dir.path().join("observations/index.json");
    for bytes in [
        b"secret-path malformed".to_vec(),
        vec![b' '; MAX_OBSERVATION_LEDGER_BYTES + 1],
    ] {
        std::fs::write(&path, bytes).unwrap();
        let err = store
            .projection("s", &source, &Redactor::new())
            .unwrap_err();
        assert!(!err.to_string().contains("secret-path"));
        assert!(store.commit(batch("s", &source, 1, 1, "v2")).is_err());
    }
}

#[test]
fn fork_reexposes_source_observations_without_inheriting_session_topic_tombstones() {
    let store = MemoryObservationStore::default();
    let redactor = Redactor::default();
    let parent = events("parent", 4);
    let committed = store.commit(batch("parent", &parent, 1, 2, "v1")).unwrap();
    let ids = committed
        .observations
        .iter()
        .map(|observation| observation.id.clone())
        .collect();
    let parent_projection = store.tombstone("parent", &ids, &parent, &redactor).unwrap();
    assert_eq!(parent_projection.tombstoned(), &ids);

    let child_events = child(&parent, "parent", "child", 3);
    let child_projection = store
        .fork("parent", &parent, "child", &child_events, 3, &redactor)
        .unwrap();
    assert!(child_projection.tombstoned().is_empty());
    assert_eq!(child_projection.batches()[0], committed);
}

#[test]
fn tombstones_require_complete_batches() {
    let store = MemoryObservationStore::default();
    let redactor = Redactor::default();
    let source = events("s", 2);
    let committed = store
        .commit(
            ValidatedObservationBatch::new(
                "s",
                &source,
                SourceRange { start: 1, end: 2 },
                "v1",
                vec![
                    ObservationDraft {
                        scope: ObservationScope::Session,
                        kind: ObservationKind::Decision,
                        content: "first".into(),
                    },
                    ObservationDraft {
                        scope: ObservationScope::Session,
                        kind: ObservationKind::Constraint,
                        content: "second".into(),
                    },
                ],
                &redactor,
            )
            .unwrap(),
        )
        .unwrap();
    let partial = BTreeSet::from([committed.observations[0].id.clone()]);

    assert_eq!(
        store
            .tombstone("s", &partial, &source, &redactor)
            .unwrap_err(),
        ObservationError::InvalidSource
    );
    assert!(
        store
            .projection("s", &source, &redactor)
            .unwrap()
            .tombstoned()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn native_symlink_hardlink_and_private_modes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for name in [
        "observations",
        "observations/index.json",
        "observations/lock",
        "observations/.pending",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        std::fs::write(victim.path().join("private"), "untouched").unwrap();
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(
            if name == "observations" {
                victim.path().to_owned()
            } else {
                victim.path().join("private")
            },
            &path,
        )
        .unwrap();
        let source = events("s", 1);
        assert!(
            FsObservationStore::new(dir.path())
                .commit(batch("s", &source, 1, 1, "v1"))
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(victim.path().join("private")).unwrap(),
            "untouched"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let source = events("s", 1);
    let store = FsObservationStore::new(dir.path());
    store.commit(batch("s", &source, 1, 1, "v1")).unwrap();
    for path in ["observations/index.json", "observations/lock"] {
        assert_eq!(
            std::fs::metadata(dir.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        std::fs::metadata(dir.path().join("observations"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    std::fs::hard_link(
        dir.path().join("observations/index.json"),
        dir.path().join("alias"),
    )
    .unwrap();
    assert!(store.projection("s", &source, &Redactor::new()).is_err());
}
