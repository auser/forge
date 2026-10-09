use super::*;
use forge_context::{
    ContextComponents, ContextPlanDraft, ContextStore, ObservationDraft, ObservationScope,
    ObservationStore, ValidatedObservationBatch, stable_prefix,
};

fn fixture() -> (tempfile::TempDir, ContextMemoryService) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.observer.enabled = true;
    config.observer.model = Some("test-observer".into());
    config.models.insert(
        "test-observer".into(),
        forge_config::ModelEntry {
            cost_input_per_mtok: Some(0.0),
            cost_output_per_mtok: Some(0.0),
            ..Default::default()
        },
    );
    let facade = ContextMemoryService::new(
        Arc::new(config),
        Arc::new(JsonlSessionStore::new(dir.path().join(".forge/sessions"))),
        dir.path().join(".forge/context"),
        ObserverReadiness::Ready,
    );
    (dir, facade)
}
fn run(f: &ContextMemoryService, session: &str, id: &str) {
    for kind in [
        EventKind::RunStarted {
            provider: "local".into(),
            model: "local".into(),
            prompt: "raw text must not appear in source metadata".into(),
        },
        EventKind::Completed {
            summary: "done".into(),
        },
    ] {
        f.sessions.append(Event::new(id, session, kind)).unwrap();
    }
}

#[test]
fn absent_queries_are_default_off_and_create_nothing() {
    let (dir, f) = fixture();
    let status = f.memory_status("fresh").unwrap();
    assert!(!status.desired_enabled);
    assert!(!status.effective_eligible);
    assert!(!status.live_prompt_injection);
    assert!(matches!(
        status.session_observations,
        StoreInspection::Missing
    ));
    assert!(matches!(status.session_jobs, StoreInspection::Missing));
    assert!(matches!(
        f.context_status("fresh").unwrap().latest_plan,
        StoreInspection::Missing
    ));
    assert!(f.memory_show("fresh", 0).unwrap().items.is_empty());
    assert!(f.memory_sources("fresh", 0).unwrap().items.is_empty());
    assert!(!dir.path().join(".forge").exists());
    assert!(f.set_memory_enabled("../escape", false).is_err());
}

#[test]
fn consent_is_v9_replay_ignored_and_prerequisites_never_bypassed() {
    let (_dir, mut f) = fixture();
    assert!(f.set_memory_enabled("fresh", true).unwrap().desired_enabled);
    let events = f.events("fresh").unwrap();
    assert_eq!(events[0].v, 9);
    assert!(crate::conversation_from_events(&events).messages.is_empty());
    assert!(
        !f.set_memory_enabled("fresh", false)
            .unwrap()
            .desired_enabled
    );
    f.readiness = ObserverReadiness::Unavailable {
        reason: "egress denied".into(),
    };
    assert!(f.set_memory_enabled("fresh", true).is_err());
    assert_eq!(f.events("fresh").unwrap().len(), 2);
    f.readiness = ObserverReadiness::Ready;
    Arc::make_mut(&mut f.config)
        .models
        .get_mut("test-observer")
        .unwrap()
        .cost_output_per_mtok = None;
    assert!(f.set_memory_enabled("fresh", true).is_err());
    assert_eq!(f.events("fresh").unwrap().len(), 2);
}

#[test]
fn copied_prefix_policy_survives_nested_cuts_and_child_override() {
    let (_dir, f) = fixture();
    f.set_memory_enabled("parent", true).unwrap();
    run(&f, "parent", "run");
    f.set_memory_enabled("parent", false).unwrap();
    f.sessions.copy_prefix("parent", "child", 3).unwrap();
    f.sessions
        .append(Event::new(
            "fork",
            "child",
            EventKind::SessionForked {
                from_session: "parent".into(),
                at_position: 3,
            },
        ))
        .unwrap();
    assert!(f.memory_status("child").unwrap().desired_enabled);
    f.set_memory_enabled("child", false).unwrap();
    f.sessions.copy_prefix("child", "nested", 4).unwrap();
    assert!(f.memory_status("nested").unwrap().desired_enabled);
    assert!(!f.memory_status("child").unwrap().desired_enabled);
    assert!(!f.memory_status("parent").unwrap().desired_enabled);
}

#[test]
fn pages_are_deterministic_bounded_and_never_dump_source_text() {
    let (_dir, f) = fixture();
    run(&f, "s", "r");
    let events = f.events("s").unwrap();
    let drafts = (0..25)
        .map(|_| ObservationDraft {
            scope: ObservationScope::Session,
            kind: ObservationKind::State,
            content: "x\n".repeat(8000),
        })
        .collect();
    let batch = ValidatedObservationBatch::new(
        "s",
        &events,
        SourceRange { start: 1, end: 2 },
        "v1",
        drafts,
        f.sessions.redactor(),
    )
    .unwrap();
    FsObservationStore::new(&f.root).commit(batch).unwrap();
    let mut offset = 0;
    let mut count = 0;
    loop {
        let page = f.memory_show("s", offset).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 16 * 1024);
        assert!(!page.items.is_empty());
        assert!(page.items.len() <= 20);
        assert!(page.items.iter().all(|item| item.truncated));
        count += page.items.len();
        match page.next_offset {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(count, 25);
    let sources = serde_json::to_string(&f.memory_sources("s", 0).unwrap()).unwrap();
    assert!(!sources.contains("raw text"));
    assert!(sources.contains("source_fingerprint"));
    assert!(
        f.memory_show("s", usize::MAX)
            .unwrap()
            .next_offset
            .is_none()
    );
}

#[test]
fn corrupt_stores_are_distinct_from_missing_without_repair() {
    let (_dir, f) = fixture();
    for namespace in ["observations", "artifacts", "observer-jobs"] {
        std::fs::create_dir_all(f.root.join(namespace)).unwrap();
        std::fs::write(f.root.join(namespace).join("index.json"), b"invalid").unwrap();
    }
    let status = f.memory_status("s").unwrap();
    assert!(matches!(
        status.session_observations,
        StoreInspection::Corrupt
    ));
    assert!(matches!(status.session_jobs, StoreInspection::Corrupt));
    assert!(matches!(
        f.context_status("s").unwrap().project_artifact_metadata,
        StoreInspection::Corrupt
    ));
    assert!(matches!(
        f.memory_show("s", 0).unwrap().store,
        StoreInspection::Corrupt
    ));
    for namespace in ["observations", "artifacts", "observer-jobs"] {
        assert_eq!(
            std::fs::read(f.root.join(namespace).join("index.json")).unwrap(),
            b"invalid"
        );
        assert_eq!(
            std::fs::read_dir(f.root.join(namespace)).unwrap().count(),
            1
        );
    }
}

#[test]
fn context_follows_valid_ids_not_paths_and_counts_only_typed_events() {
    let (_dir, f) = fixture();
    let plan = FsContextStore::new(&f.root)
        .record(ContextPlanDraft {
            version: 1,
            run_id: "r".into(),
            session_id: "s".into(),
            request_ordinal: 1,
            components: ContextComponents::default(),
            stable_prefix: stable_prefix(&[], &[]),
            reserved_output_tokens: Some(37),
            remaining_context_tokens: 123,
        })
        .unwrap();
    f.sessions
        .append(Event::new(
            "r",
            "s",
            EventKind::ContextPlanRecorded {
                plan_id: plan.id.clone(),
                request_ordinal: 1,
                stable_prefix_hash: plan.stable_prefix.combined_hash.clone(),
                prefix_changed: false,
                total_estimated_input_tokens: 0,
                reserved_output_tokens: Some(37),
                plan_path: "/never/read/this".into(),
            },
        ))
        .unwrap();
    f.sessions
        .append(Event::new(
            "r",
            "s",
            EventKind::AssistantMessage {
                text: "retrieve_tool_output".into(),
                tool_calls: vec![],
            },
        ))
        .unwrap();
    f.sessions
        .append(Event::new(
            "r",
            "s",
            EventKind::ToolCallRequested {
                tool: "retrieve_tool_output".into(),
                args_summary: "{}".into(),
            },
        ))
        .unwrap();
    let status = f.context_status("s").unwrap();
    let StoreInspection::Available(actual) = status.latest_plan else {
        panic!("missing plan")
    };
    assert_eq!(actual, plan);
    assert_eq!(status.retrieval.attempts, 1);
    assert_eq!(status.retrieval.results, 0);
}

#[cfg(unix)]
#[test]
fn inspection_preserves_permissions_and_rejects_symlink_and_hardlink() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (dir, f) = fixture();
    std::fs::create_dir_all(f.root.join("observations")).unwrap();
    let file = f.root.join("observations/index.json");
    std::fs::write(&file, br#"{"sessions":{}}"#).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(
        f.root.join("observations"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        f.memory_status("s").unwrap().session_observations,
        StoreInspection::Available(0)
    ));
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        std::fs::metadata(f.root.join("observations"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    std::fs::rename(&file, dir.path().join("victim")).unwrap();
    symlink(dir.path().join("victim"), &file).unwrap();
    assert!(matches!(
        f.memory_status("s").unwrap().session_observations,
        StoreInspection::Unavailable
    ));
    std::fs::remove_file(&file).unwrap();
    std::fs::hard_link(dir.path().join("victim"), &file).unwrap();
    assert!(matches!(
        f.memory_status("s").unwrap().session_observations,
        StoreInspection::Unavailable
    ));
}
