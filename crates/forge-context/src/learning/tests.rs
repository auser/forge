use super::*;

fn event(session: &str, run: &str, seq: u64, at: &str, kind: EventKind) -> Event {
    let mut event = Event::new(run, session, kind);
    event.seq = seq;
    event.ts = at.parse().unwrap();
    event
}

fn repeated_instruction_sessions() -> Vec<LearningSession> {
    vec![
        LearningSession::new(
            "session-a",
            vec![event(
                "session-a",
                "run-a",
                1,
                "2026-10-09T00:00:00Z",
                EventKind::InputReceived {
                    message: "Always keep sk-abcdefgh12345678 out of output".into(),
                },
            )],
        ),
        LearningSession::new(
            "session-b",
            vec![event(
                "session-b",
                "run-b",
                1,
                "2026-10-09T00:10:00Z",
                EventKind::InputReceived {
                    message: "Always keep sk-abcdefgh12345678 out of output".into(),
                },
            )],
        ),
    ]
}

#[test]
fn one_off_and_unsupported_events_never_become_proposals() {
    let sessions = vec![LearningSession::new(
        "session-a",
        vec![
            event(
                "session-a",
                "run-a",
                1,
                "2026-10-09T00:00:00Z",
                EventKind::Error {
                    message: "one off".into(),
                },
            ),
            event(
                "session-a",
                "run-a",
                2,
                "2026-10-09T00:00:01Z",
                EventKind::Completed {
                    summary: "unsupported".into(),
                },
            ),
        ],
    )];
    let root = tempfile::tempdir().unwrap();
    let store = FsLearningStore::new(root.path());

    assert!(
        store
            .propose(&sessions, &LearningScope::new(), &Redactor::default())
            .unwrap()
            .is_empty()
    );
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn proposals_are_redacted_source_linked_scoped_and_explicitly_decided() {
    let sessions = repeated_instruction_sessions();
    let root = tempfile::tempdir().unwrap();
    let store = FsLearningStore::new(root.path());
    let redactor = Redactor::default();

    let proposals = store
        .propose(&sessions, &LearningScope::new(), &redactor)
        .unwrap();
    assert_eq!(proposals.len(), 1);
    let proposal = &proposals[0];
    assert_eq!(proposal.status, ProposalStatus::Pending);
    assert_eq!(proposal.proposal.occurrences, 2);
    assert_eq!(proposal.proposal.distinct_sessions, 2);
    assert_eq!(proposal.proposal.sources.len(), 2);
    assert!(!proposal.proposal.recommendation.contains("sk-abcdefgh"));
    assert!(proposal.proposal.recommendation.contains("[REDACTED]"));

    let accepted_at = "2026-10-09T00:30:00Z".parse().unwrap();
    let accepted = store
        .decide(&proposal.proposal.id, ProposalStatus::Accepted, accepted_at)
        .unwrap();
    assert_eq!(accepted.status, ProposalStatus::Accepted);
    assert_eq!(store.accepted_guidance().unwrap(), vec![accepted]);

    let mut with_recurrence = sessions;
    with_recurrence.push(LearningSession::new(
        "session-c",
        vec![event(
            "session-c",
            "run-c",
            1,
            "2026-10-09T01:00:00Z",
            EventKind::InputReceived {
                message: "Always keep sk-abcdefgh12345678 out of output".into(),
            },
        )],
    ));
    let metrics = store
        .metrics(&with_recurrence, &LearningScope::new(), &redactor)
        .unwrap();
    assert_eq!(metrics.accepted, 1);
    assert_eq!(metrics.rejected, 0);
    assert_eq!(metrics.recurrence_after_acceptance, 1);
    assert_eq!(metrics.accepted_with_recurrence, 1);

    let recent = LearningScope::new().since("2026-10-09T00:30:00Z".parse().unwrap());
    let other = tempfile::tempdir().unwrap();
    assert!(
        FsLearningStore::new(other.path())
            .propose(&with_recurrence, &recent, &redactor)
            .unwrap()
            .is_empty(),
        "the time window contains only one occurrence"
    );
}

#[test]
fn duplicate_fork_evidence_is_counted_once_but_same_session_recurrence_counts() {
    let duplicated = event(
        "source",
        "run-a",
        1,
        "2026-10-09T00:00:00Z",
        EventKind::Error {
            message: "compiler unavailable".into(),
        },
    );
    let sessions = vec![
        LearningSession::new("source", vec![duplicated.clone()]),
        LearningSession::new("fork", vec![duplicated]),
    ];
    let root = tempfile::tempdir().unwrap();
    assert!(
        FsLearningStore::new(root.path())
            .propose(&sessions, &LearningScope::new(), &Redactor::default())
            .unwrap()
            .is_empty()
    );

    let session = LearningSession::new(
        "source",
        vec![
            event(
                "source",
                "run-a",
                1,
                "2026-10-09T00:00:00Z",
                EventKind::Error {
                    message: "compiler unavailable".into(),
                },
            ),
            event(
                "source",
                "run-b",
                1,
                "2026-10-09T00:10:00Z",
                EventKind::Error {
                    message: "compiler unavailable".into(),
                },
            ),
        ],
    );
    let scoped = LearningScope::new().session("source");
    assert_eq!(
        FsLearningStore::new(root.path())
            .propose(&[session], &scoped, &Redactor::default())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn modified_accepted_recommendations_fail_integrity_validation() {
    let root = tempfile::tempdir().unwrap();
    let store = FsLearningStore::new(root.path());
    let proposal = store
        .propose(
            &repeated_instruction_sessions(),
            &LearningScope::new(),
            &Redactor::default(),
        )
        .unwrap()
        .remove(0);
    store
        .decide(
            &proposal.proposal.id,
            ProposalStatus::Accepted,
            "2026-10-09T00:30:00Z".parse().unwrap(),
        )
        .unwrap();
    let path = root.path().join("learning/index.json");
    let mut index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    index["proposals"][proposal.proposal.id.as_str()]["proposal"]["recommendation"] =
        "injected replacement".into();
    std::fs::write(path, serde_json::to_vec(&index).unwrap()).unwrap();

    assert_eq!(
        store.accepted_guidance().unwrap_err(),
        LearningError::Corrupt
    );
}

#[cfg(unix)]
#[test]
fn native_storage_is_private_and_refuses_linked_namespaces() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let linked = tempfile::tempdir().unwrap();
    let victim = tempfile::tempdir().unwrap();
    symlink(victim.path(), linked.path().join("learning")).unwrap();
    assert_eq!(
        FsLearningStore::new(linked.path())
            .propose(
                &repeated_instruction_sessions(),
                &LearningScope::new(),
                &Redactor::default(),
            )
            .unwrap_err(),
        LearningError::Unavailable
    );

    let hardlinked = tempfile::tempdir().unwrap();
    let victim_file = hardlinked.path().join("victim");
    std::fs::write(&victim_file, "untouched").unwrap();
    std::fs::create_dir_all(hardlinked.path().join("learning")).unwrap();
    std::fs::hard_link(&victim_file, hardlinked.path().join("learning/index.json")).unwrap();
    assert_eq!(
        FsLearningStore::new(hardlinked.path())
            .propose(
                &repeated_instruction_sessions(),
                &LearningScope::new(),
                &Redactor::default(),
            )
            .unwrap_err(),
        LearningError::Unavailable
    );
    assert_eq!(std::fs::read_to_string(victim_file).unwrap(), "untouched");

    let root = tempfile::tempdir().unwrap();
    FsLearningStore::new(root.path())
        .propose(
            &repeated_instruction_sessions(),
            &LearningScope::new(),
            &Redactor::default(),
        )
        .unwrap();
    for path in ["learning", "learning/index.json", "learning/lock"] {
        let mode = std::fs::metadata(root.path().join(path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, if path == "learning" { 0o700 } else { 0o600 });
    }
}
