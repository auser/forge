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
fn facade_session_errors_never_expose_private_variants_or_paths() {
    let (dir, f) = fixture();
    run(&f, "s", "r");
    let path = dir.path().join(".forge/sessions/s.jsonl");
    let original = std::fs::read_to_string(&path).unwrap();
    let private = "private_unknown_event_distinctive_82374";
    let corrupt = original.replace("run_started", private);
    assert_ne!(original, corrupt);
    std::fs::write(&path, &corrupt).unwrap();
    let errors = [
        f.context_status("s").unwrap_err(),
        f.memory_status("s").unwrap_err(),
        f.memory_show("s", 0).unwrap_err(),
        f.memory_sources("s", 0).unwrap_err(),
        f.set_memory_enabled("s", true).unwrap_err(),
        f.set_memory_enabled("s", false).unwrap_err(),
    ];
    for error in errors {
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains(private));
            assert!(!rendered.contains(dir.path().to_str().unwrap()));
            assert!(!rendered.contains("s.jsonl"));
            assert!(rendered.contains("session events unavailable"));
        }
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), corrupt);
}

fn commit_records(f: &ContextMemoryService, session: &str, start: u64, count: usize) {
    let events = f.events(session).unwrap();
    let drafts = (0..count)
        .map(|i| ObservationDraft {
            scope: ObservationScope::Session,
            kind: ObservationKind::State,
            content: format!("record-{start}-{i}"),
        })
        .collect();
    FsObservationStore::new(&f.root)
        .commit(
            ValidatedObservationBatch::new(
                session,
                &events,
                SourceRange {
                    start,
                    end: start + 1,
                },
                forge_context::OBSERVER_VERSION,
                drafts,
                f.sessions.redactor(),
            )
            .unwrap(),
        )
        .unwrap();
}

#[test]
fn record_offsets_survive_late_commit_of_earlier_source() {
    let (_dir, f) = fixture();
    run(&f, "s", "early");
    run(&f, "s", "late");
    commit_records(&f, "s", 3, 21);
    let first = f.memory_show("s", 0).unwrap();
    assert_eq!(first.items.len(), 20);
    assert_eq!(first.next_offset, Some(20));
    commit_records(&f, "s", 1, 1);
    let rest = f.memory_show("s", first.next_offset.unwrap()).unwrap();
    assert_eq!(
        rest.items
            .iter()
            .map(|i| i.content.as_str())
            .collect::<Vec<_>>(),
        ["record-3-20", "record-1-0"]
    );
    assert_eq!(rest.next_offset, None);
    let ids = first
        .items
        .iter()
        .chain(&rest.items)
        .map(|i| &i.id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 22);
}

#[test]
fn source_offsets_survive_late_commit_of_earlier_source() {
    let (_dir, f) = fixture();
    for i in 0..22 {
        run(&f, "s", &format!("r{i}"));
    }
    for i in 1..22 {
        commit_records(&f, "s", i * 2 + 1, 1);
    }
    let first = f.memory_sources("s", 0).unwrap();
    assert_eq!(first.items.len(), 20);
    assert_eq!(first.next_offset, Some(20));
    commit_records(&f, "s", 1, 1);
    let rest = f.memory_sources("s", 20).unwrap();
    assert_eq!(
        rest.items.iter().map(|i| i.range.start).collect::<Vec<_>>(),
        [43, 1]
    );
    assert_eq!(rest.next_offset, None);
    let ids = first
        .items
        .iter()
        .chain(&rest.items)
        .map(|i| &i.batch_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 22);
}

#[test]
fn frozen_fork_order_and_live_append_keep_existing_offsets() {
    let (_dir, f) = fixture();
    for i in 0..3 {
        run(&f, "parent", &format!("r{i}"));
    }
    // Freeze a prefix whose commit order intentionally differs from source order.
    commit_records(&f, "parent", 5, 20);
    commit_records(&f, "parent", 3, 1);
    f.sessions.copy_prefix("parent", "child", 6).unwrap();
    f.sessions
        .append(Event::new(
            "fork",
            "child",
            EventKind::SessionForked {
                from_session: "parent".into(),
                at_position: 6,
            },
        ))
        .unwrap();
    FsObservationStore::new(&f.root)
        .fork(
            "parent",
            &f.events("parent").unwrap(),
            "child",
            &f.events("child").unwrap(),
            6,
            f.sessions.redactor(),
        )
        .unwrap();
    let first = f.memory_show("child", 0).unwrap();
    assert_eq!(first.items.len(), 20);
    assert_eq!(first.items[0].content, "record-5-0");
    assert_eq!(first.next_offset, Some(20));
    // Parent commits after the cut are not injected into the frozen projection.
    commit_records(&f, "parent", 1, 1);
    run(&f, "child", "local");
    commit_records(&f, "child", 8, 1);
    let rest = f.memory_show("child", 20).unwrap();
    assert_eq!(
        rest.items
            .iter()
            .map(|i| i.content.as_str())
            .collect::<Vec<_>>(),
        ["record-3-0", "record-8-0"]
    );
    assert_eq!(rest.next_offset, None);
    let sources = f.memory_sources("child", 0).unwrap();
    assert_eq!(
        sources
            .items
            .iter()
            .map(|i| i.range.start)
            .collect::<Vec<_>>(),
        [5, 3, 8]
    );
}

fn prices_and_budget(f: &mut ContextMemoryService, price: f64, session: f64, daily: f64) {
    let config = Arc::make_mut(&mut f.config);
    config.observer.session_usd = session;
    config.observer.daily_usd = daily;
    let entry = config.models.get_mut("test-observer").unwrap();
    entry.cost_input_per_mtok = Some(price);
    entry.cost_output_per_mtok = Some(price);
}

#[test]
fn missing_queue_zero_ceilings_preserve_consent_but_require_affordability() {
    let (_dir, mut f) = fixture();
    // A tiny positive price is rounded up, never silently made free.
    prices_and_budget(&mut f, 0.0000000001, 0.0, 0.0);
    let status = f.set_memory_enabled("s", true).unwrap();
    assert!(status.desired_enabled);
    assert!(!status.effective_eligible);
    assert!(matches!(status.session_jobs, StoreInspection::Missing));
    assert!(
        status
            .unavailable_reasons
            .iter()
            .any(|r| r.starts_with("session observer budget"))
    );
    assert!(
        status
            .unavailable_reasons
            .iter()
            .any(|r| r.starts_with("project daily observer budget"))
    );
    prices_and_budget(&mut f, 0.0, 0.0, 0.0);
    assert!(f.memory_status("s").unwrap().effective_eligible);
}

#[test]
fn known_chunk_reservation_must_fit_both_ceilings() {
    let (_dir, mut f) = fixture();
    run(&f, "s", "r");
    prices_and_budget(&mut f, 1.0, 1.0, 1.0);
    f.set_memory_enabled("s", true).unwrap();
    let events = f.events("s").unwrap();
    let reserve = f
        .inspection_reservation("s", &events, &StoreInspection::Missing)
        .unwrap();
    assert!(reserve > 1);
    let exact = reserve as f64 / 1_000_000.0;
    let short = (reserve - 1) as f64 / 1_000_000.0;
    prices_and_budget(&mut f, 1.0, short, 1.0);
    assert!(!f.memory_status("s").unwrap().effective_eligible);
    prices_and_budget(&mut f, 1.0, 1.0, short);
    assert!(!f.memory_status("s").unwrap().effective_eligible);
    prices_and_budget(&mut f, 1.0, exact, exact);
    assert!(f.memory_status("s").unwrap().effective_eligible);
}

#[test]
fn smaller_affordable_chunk_remains_eligible_when_larger_chunk_is_blocked() {
    let (_dir, mut f) = fixture();
    run(&f, "s", "small");
    for kind in [
        EventKind::RunStarted {
            provider: "local".into(),
            model: "local".into(),
            prompt: "large".repeat(1000),
        },
        EventKind::Completed {
            summary: "done".into(),
        },
    ] {
        f.sessions.append(Event::new("large", "s", kind)).unwrap();
    }
    prices_and_budget(&mut f, 1.0, 1.0, 1.0);
    f.set_memory_enabled("s", true).unwrap();
    let events = f.events("s").unwrap();
    let policy = forge_context::ObserverPolicy {
        observer_version: forge_context::OBSERVER_VERSION.into(),
        model: "test-observer".into(),
        prompt_version: forge_context::OBSERVER_PROMPT_VERSION.into(),
        limits: forge_context::ObserverLimits::default(),
    };
    let chunks =
        forge_context::observer_chunks("s", &events, f.sessions.redactor(), &policy).unwrap();
    let costs = chunks
        .iter()
        .map(|chunk| {
            f.prices()
                .unwrap()
                .cost(
                    chunk.input_tokens,
                    u64::from(policy.limits.max_output_tokens),
                )
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(costs.len(), 2);
    assert!(costs[0] < costs[1]);
    let limit = (costs[0] + 1) as f64 / 1_000_000.0;
    prices_and_budget(&mut f, 1.0, limit, limit);
    assert!(f.memory_status("s").unwrap().effective_eligible);
}

#[test]
fn charged_equality_allows_free_work_but_never_spend_above_either_limit() {
    use forge_context::{BudgetLimits, ObserverQueue};
    let (_dir, mut f) = fixture();
    run(&f, "s", "r");
    let policy = forge_context::ObserverPolicy {
        observer_version: forge_context::OBSERVER_VERSION.into(),
        model: "test-observer".into(),
        prompt_version: forge_context::OBSERVER_PROMPT_VERSION.into(),
        limits: forge_context::ObserverLimits::default(),
    };
    let job = forge_context::observer_chunks(
        "s",
        &f.events("s").unwrap(),
        f.sessions.redactor(),
        &policy,
    )
    .unwrap()
    .remove(0)
    .job;
    let queue = FsObserverQueue::new(&f.root);
    queue.enqueue(job.clone()).unwrap();
    queue
        .claim(&job.id, 10, BudgetLimits::default())
        .unwrap()
        .unwrap();
    for (price, session, daily, eligible) in [
        (0.0, 0.000010, 0.000010, true),
        (1.0, 0.000010, 0.000010, false),
        (0.0, 0.000009, 0.000010, false),
        (0.0, 0.000010, 0.000009, false),
    ] {
        prices_and_budget(&mut f, price, session, daily);
        let status = f.set_memory_enabled("s", true).unwrap();
        assert!(status.desired_enabled);
        assert_eq!(status.effective_eligible, eligible);
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
