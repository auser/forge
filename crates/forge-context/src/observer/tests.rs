use super::*;
use crate::{MemoryObservationStore, ObservationStore};
mod gates;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

fn policy() -> ObserverPolicy {
    ObserverPolicy {
        observer_version: OBSERVER_VERSION.into(),
        model: "scripted".into(),
        prompt_version: OBSERVER_PROMPT_VERSION.into(),
        limits: ObserverLimits::default(),
    }
}
fn event(run: &str, session: &str, kind: EventKind) -> Event {
    let mut event = Event::new(run, session, kind);
    event.ts = "2026-10-09T00:00:00Z".parse().unwrap();
    event
}
fn source(session: &str) -> Vec<Event> {
    vec![
        event(
            "r",
            session,
            EventKind::RunStarted {
                provider: "test".into(),
                model: "test".into(),
                prompt: "Use SQLite for local storage.".into(),
            },
        ),
        event(
            "r",
            session,
            EventKind::AssistantMessage {
                text: "SQLite selected.".into(),
                tool_calls: vec![],
            },
        ),
        event(
            "r",
            session,
            EventKind::Completed {
                summary: "done".into(),
            },
        ),
    ]
}
fn job(session: &str) -> ObserverJob {
    observer_chunks(session, &source(session), &Redactor::new(), &policy())
        .unwrap()
        .remove(0)
        .job
}
fn output(job: &ObserverJob) -> String {
    serde_json::json!({"range":job.range,"observations":[{"scope":"session","kind":"decision","content":"Use SQLite for local storage."}]}).to_string()
}
fn batch(job: &ObserverJob) -> ValidatedObservationBatch {
    parse_observer_output(
        job,
        &source(&job.session_id),
        &Redactor::new(),
        &output(job),
    )
    .unwrap()
}
struct Clock(AtomicU64);
impl ObserverClock for Clock {
    fn now_unix_seconds(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
fn clock() -> Arc<Clock> {
    Arc::new(Clock(AtomicU64::new(86400)))
}

#[test]
fn errored_run_does_not_block_later_completed_observer_chunks() {
    let mut events = source("s");
    events.truncate(1);
    events.push(event(
        "r",
        "s",
        EventKind::Error {
            message: "failed".into(),
        },
    ));
    let mut later = source("s");
    for event in &mut later {
        event.run_id = "r2".into();
    }
    events.extend(later);
    let chunks = observer_chunks("s", &events, &Redactor::new(), &policy()).unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].job.range, SourceRange { start: 1, end: 2 });
    assert_eq!(chunks[1].job.range, SourceRange { start: 3, end: 5 });
    let stable = chunks.iter().map(|chunk| &chunk.job).collect::<Vec<_>>();
    let mut next = source("s");
    for event in &mut next {
        event.run_id = "r3".into();
    }
    events.extend(next);
    let extended = observer_chunks("s", &events, &Redactor::new(), &policy()).unwrap();
    assert_eq!(extended.len(), 3);
    assert_eq!(
        extended[..2]
            .iter()
            .map(|chunk| &chunk.job)
            .collect::<Vec<_>>(),
        stable
    );

    // A terminal failure closes only its own run; a still-active interleaved
    // run must keep the boundary open until its own terminal event.
    let interleaved = vec![events[0].clone(), events[2].clone(), events[1].clone()];
    assert!(
        observer_chunks("s", &interleaved, &Redactor::new(), &policy())
            .unwrap()
            .is_empty()
    );
    let mut completed = interleaved;
    completed.push(events[4].clone());
    let chunks = observer_chunks("s", &completed, &Redactor::new(), &policy()).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].job.range, SourceRange { start: 1, end: 4 });
}

#[test]
fn deterministic_completed_prefix_and_interleaved_runs() {
    let mut events = source("s");
    let first = observer_chunks("s", &events, &Redactor::new(), &policy()).unwrap();
    events.push(event(
        "r2",
        "s",
        EventKind::RunStarted {
            provider: "x".into(),
            model: "x".into(),
            prompt: "later".into(),
        },
    ));
    assert_eq!(
        observer_chunks("s", &events, &Redactor::new(), &policy()).unwrap()[0].job,
        first[0].job
    );
    events.push(event(
        "r3",
        "s",
        EventKind::RunStarted {
            provider: "x".into(),
            model: "x".into(),
            prompt: "parallel".into(),
        },
    ));
    events.push(event(
        "r3",
        "s",
        EventKind::Completed {
            summary: "done".into(),
        },
    ));
    assert_eq!(
        observer_chunks("s", &events, &Redactor::new(), &policy())
            .unwrap()
            .len(),
        1
    );
    events.push(event(
        "r2",
        "s",
        EventKind::Completed {
            summary: "done".into(),
        },
    ));
    let all = observer_chunks("s", &events, &Redactor::new(), &policy()).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].job, first[0].job);
    assert_eq!(all[1].job.range, SourceRange { start: 4, end: 7 });
}
#[test]
fn allowlist_sanitization_full_framing_and_no_truncation() {
    let mut events = source("s");
    events.insert(
        1,
        event(
            "r",
            "s",
            EventKind::ToolResult {
                call_id: "x".into(),
                tool: "shell".into(),
                output: "private-output".into(),
                is_error: false,
            },
        ),
    );
    events.insert(
        2,
        event(
            "r",
            "s",
            EventKind::InputReceived {
                message: "sk-abcdefgh12345678".into(),
            },
        ),
    );
    let chunks = observer_chunks("s", &events, &Redactor::new(), &policy()).unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(!chunks[0].request_json.contains("private-output"));
    assert!(!chunks[0].request_json.contains("sk-abcdefgh12345678"));
    assert!(
        chunks[0].input_tokens > chunks[0].request_json.len() as u64 + OBSERVER_PROMPT.len() as u64
    );
    let mut tiny = policy();
    tiny.limits.max_input_bytes = 32;
    assert!(matches!(
        observer_chunks("s", &events, &Redactor::new(), &tiny),
        Err(ObserverError::Oversize)
    ));
    let mut large = source("s");
    large[0].kind = EventKind::InputReceived {
        message: "x".repeat(40_000),
    };
    assert!(matches!(
        observer_chunks("s", &large, &Redactor::new(), &policy()),
        Err(ObserverError::Oversize)
    ));
}
#[test]
fn strict_bounded_output_and_fresh_source() {
    let job = job("s");
    let src = source("s");
    for bad in [
        r#"{"range":{"start":1,"end":3},"observations":[],"tools":[]}"#,
        r#"{"range":{"start":1,"end":2},"observations":[]}"#,
        r#"{"range":{"start":1,"end":3},"observations":[{"scope":"project","kind":"decision","content":"x"}]}"#,
        r#"{"range":{"start":1,"end":3},"observations":[{"scope":"session","kind":"instruction","content":"x"}]}"#,
        r#"{"range":{"start":1,"end":3},"observations":[{"scope":"session","kind":"decision","content":"x","extra":1}]}"#,
        r#"{"range":{"start":1,"end":3},"observations":[],"observations":[]}"#,
        r#"{"range":{"start":1,"end":3},"observations":[]} trailing"#,
    ] {
        assert!(parse_observer_output(&job, &src, &Redactor::new(), bad).is_err());
    }
    assert!(parse_observer_output(&job, &src, &Redactor::new(), &"x".repeat(16385)).is_err());
    let mut changed = src.clone();
    changed[1].kind = EventKind::AssistantMessage {
        text: "different".into(),
        tool_calls: vec![],
    };
    assert!(observer_request(&job, &changed, &Redactor::new()).is_err());
    assert!(
        parse_observer_output(
            &job,
            &src,
            &Redactor::new(),
            r#"{"range":{"start":1,"end":3},"observations":[]}"#
        )
        .is_ok()
    );
}
fn queue_contract(queue: &dyn ObserverQueue, clock: &Clock) {
    let a = job("s");
    let b = job("t");
    queue.enqueue(a.clone()).unwrap();
    queue.enqueue(a.clone()).unwrap();
    queue.enqueue(b.clone()).unwrap();
    let lease = queue
        .claim(&a.id, 20_000, BudgetLimits::default())
        .unwrap()
        .unwrap();
    assert!(
        queue
            .claim(&b.id, 1, BudgetLimits::default())
            .unwrap()
            .is_none()
    );
    assert_eq!(queue.status().unwrap().reserved_micro_usd, 20_000);
    clock.0.fetch_add(121, Ordering::SeqCst);
    assert_eq!(
        queue
            .jobs()
            .unwrap()
            .iter()
            .find(|r| r.job.id == a.id)
            .unwrap()
            .state,
        ObserverJobState::Retryable
    );
    let newer = queue
        .claim(&a.id, 20_000, BudgetLimits::default())
        .unwrap()
        .unwrap();
    let ledger = MemoryObservationStore::default();
    assert_eq!(
        queue.finalize(&lease, batch(&a), &ledger, None),
        Err(ObserverError::StaleLease)
    );
    assert!(
        ledger
            .projection("s", &source("s"), &Redactor::new())
            .unwrap()
            .batches()
            .is_empty()
    );
    queue.fail(&newer, true).unwrap();
    clock.0.fetch_add(6, Ordering::SeqCst);
    assert!(matches!(
        queue.claim(&a.id, 20_000, BudgetLimits::default()),
        Err(ObserverError::BudgetExceeded)
    ));
    let third = queue
        .claim(&a.id, 10_000, BudgetLimits::default())
        .unwrap()
        .unwrap();
    queue.fail(&third, true).unwrap();
    clock.0.fetch_add(6, Ordering::SeqCst);
    assert!(
        queue
            .claim(&a.id, 0, BudgetLimits::default())
            .unwrap()
            .is_none()
    );
    assert_eq!(queue.status().unwrap().unknown_micro_usd, 50_000);
    assert_eq!(queue.status().unwrap().failed, 1);
    let second = queue
        .claim(&b.id, 5_000, BudgetLimits::default())
        .unwrap()
        .unwrap();
    queue
        .finalize(&second, batch(&b), &ledger, Some(3_000))
        .unwrap();
    assert_eq!(queue.status().unwrap().committed, 1);
    assert_eq!(queue.status().unwrap().known_micro_usd, 3_000);
}
#[test]
fn memory_retry_budget_expiry_and_fencing() {
    let c = clock();
    queue_contract(&MemoryObserverQueue::with_clock(c.clone()), &c);
}
#[cfg(any(unix, windows))]
#[test]
fn native_retry_budget_expiry_fencing_and_restart() {
    let temp = tempfile::tempdir().unwrap();
    let c = clock();
    queue_contract(&FsObserverQueue::with_clock(temp.path(), c.clone()), &c);
    let reopened = FsObserverQueue::with_clock(temp.path(), c);
    assert_eq!(reopened.status().unwrap().committed, 1);
    let bytes = std::fs::read_to_string(temp.path().join("observer-jobs/index.json")).unwrap();
    assert!(!bytes.contains("SQLite"));
}
#[test]
fn crash_reconciliation_requires_exact_job_not_only_range_version() {
    let c = clock();
    let queue = MemoryObserverQueue::with_clock(c);
    let j = job("s");
    queue.enqueue(j.clone()).unwrap();
    queue
        .claim(&j.id, 123, BudgetLimits::default())
        .unwrap()
        .unwrap();
    let ledger = MemoryObservationStore::default();
    ledger.commit(batch(&j)).unwrap(); // Simulate crash after ledger write.
    assert!(
        queue
            .reconcile(&j.id, &source("s"), &Redactor::new(), &ledger)
            .unwrap()
    );
    assert_eq!(queue.status().unwrap().unknown_micro_usd, 123);
    let another = MemoryObserverQueue::default();
    let mut other_policy = policy();
    other_policy.model = "different".into();
    let other = observer_chunks("s", &source("s"), &Redactor::new(), &other_policy)
        .unwrap()
        .remove(0)
        .job;
    another.enqueue(other.clone()).unwrap();
    assert_eq!(
        another.reconcile(&other.id, &source("s"), &Redactor::new(), &ledger),
        Err(ObserverError::Conflict)
    );
    assert_eq!(another.status().unwrap().blocked, 1);
}
#[test]
fn forks_are_child_local_and_require_frozen_ledger() {
    let parent = source("p");
    // Prefix copy is visible before its provenance marker. Copied parent IDs
    // cannot be mistaken for an independently owned root child session.
    assert!(observer_chunks("c", &parent, &Redactor::new(), &policy()).is_err());
    let mut child = parent.clone();
    child.push(event(
        "fork",
        "c",
        EventKind::SessionForked {
            from_session: "p".into(),
            at_position: 3,
        },
    ));
    child.extend(source("c"));
    let j = observer_chunks("c", &child, &Redactor::new(), &policy())
        .unwrap()
        .remove(0)
        .job;
    assert_eq!(j.range, SourceRange { start: 5, end: 7 });
    assert!(observer_chunks("p", &child, &Redactor::new(), &policy()).is_err());
    let ledger = MemoryObservationStore::default();
    assert!(!ledger.fork_initialized("c").unwrap());
    let b = parse_observer_output(&j, &child, &Redactor::new(), &output(&j)).unwrap();
    assert!(ledger.commit(b).is_err());
    ledger
        .fork("p", &parent, "c", &child, 3, &Redactor::new())
        .unwrap();
    assert!(ledger.fork_initialized("c").unwrap());
    ledger
        .commit(parse_observer_output(&j, &child, &Redactor::new(), &output(&j)).unwrap())
        .unwrap();
}
#[test]
fn daily_budget_and_checked_overflow() {
    let c = clock();
    let q = MemoryObserverQueue::with_clock(c.clone());
    let a = job("s");
    let b = job("t");
    q.enqueue(a.clone()).unwrap();
    q.enqueue(b.clone()).unwrap();
    let limits = BudgetLimits {
        session_micro_usd: u64::MAX,
        daily_micro_usd: u64::MAX,
    };
    let l = q.claim(&a.id, u64::MAX, limits).unwrap().unwrap();
    q.fail(&l, false).unwrap();
    assert!(matches!(
        q.claim(&b.id, 1, limits),
        Err(ObserverError::LimitExceeded)
    ));
    c.0.fetch_add(86400, Ordering::SeqCst);
    assert!(q.claim(&b.id, 0, limits).unwrap().is_some());
}
#[cfg(any(unix, windows))]
#[test]
fn queue_process_claim() {
    let Some(root) = std::env::var_os("FORGE_OBSERVER_TEST_ROOT") else {
        return;
    };
    let q = FsObserverQueue::new(&root);
    let j = job("s");
    if q.claim(&j.id, 100, BudgetLimits::default())
        .unwrap()
        .is_some()
    {
        std::fs::write(
            std::path::PathBuf::from(root).join(format!("winner-{}", std::process::id())),
            "",
        )
        .unwrap();
    }
}
#[cfg(any(unix, windows))]
#[test]
fn independent_process_claim_and_reservation_are_atomic() {
    let temp = tempfile::tempdir().unwrap();
    FsObserverQueue::new(temp.path()).enqueue(job("s")).unwrap();
    let mut processes: Vec<_> = (0..6)
        .map(|_| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "observer::tests::queue_process_claim"])
                .env("FORGE_OBSERVER_TEST_ROOT", temp.path())
                .spawn()
                .unwrap()
        })
        .collect();
    for p in &mut processes {
        assert!(p.wait().unwrap().success());
    }
    let winners = std::fs::read_dir(temp.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("winner-")
        })
        .count();
    assert_eq!(winners, 1);
    assert_eq!(
        FsObserverQueue::new(temp.path())
            .status()
            .unwrap()
            .reserved_micro_usd,
        100
    );
}
#[cfg(any(unix, windows))]
#[test]
fn corrupt_and_oversize_index_fails_without_payload() {
    let temp = tempfile::tempdir().unwrap();
    let q = FsObserverQueue::new(temp.path());
    q.enqueue(job("s")).unwrap();
    let path = temp.path().join("observer-jobs/index.json");
    let original = std::fs::read(&path).unwrap();
    for bytes in [b"private-secret".to_vec(), vec![b'x'; 8 * 1024 * 1024 + 1]] {
        std::fs::write(&path, bytes).unwrap();
        let error = q.status().unwrap_err();
        assert!(!error.to_string().contains("private-secret"));
    }
    let mut corrupt: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let r = corrupt["jobs"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    r["attempts"] = serde_json::json!(4);
    std::fs::write(&path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
    assert!(q.status().is_err());
}

/// Scripted, reviewed annotations are deliberately independent of parser
/// acceptance. A structurally valid unsupported claim would FAIL this gate.
#[test]
fn reviewed_scripted_evaluation_reports_precision_and_useful_coverage() {
    #[derive(Deserialize)]
    struct Fixture {
        name: String,
        source: String,
        supported: Vec<String>,
        unsupported: Vec<String>,
        output: serde_json::Value,
        accept: bool,
    }
    let fixtures: Vec<Fixture> = serde_json::from_str(include_str!("fixtures.json")).unwrap();
    let (mut supported, mut unsupported, mut opportunities, mut empty, mut rejected) =
        (0, 0, 0, 0, 0);
    for f in fixtures {
        opportunities += f.supported.len();
        let mut events = source(&f.name);
        events[0].kind = EventKind::RunStarted {
            provider: "scripted".into(),
            model: "scripted".into(),
            prompt: f.source.clone(),
        };
        events[1].kind = EventKind::AssistantMessage {
            text: f.source,
            tool_calls: vec![],
        };
        let job = observer_chunks(&f.name, &events, &Redactor::new(), &policy())
            .unwrap()
            .remove(0)
            .job;
        let parsed = parse_observer_output(&job, &events, &Redactor::new(), &f.output.to_string());
        assert_eq!(parsed.is_ok(), f.accept, "{}", f.name);
        match parsed {
            Ok(batch) => {
                let ledger = MemoryObservationStore::default();
                let committed = ledger.commit(batch).unwrap();
                if committed.observations.is_empty() {
                    empty += 1;
                }
                for claim in committed.observations {
                    if f.supported.contains(&claim.content) {
                        supported += 1;
                    } else {
                        unsupported += 1;
                    }
                    assert!(!f.unsupported.contains(&claim.content), "{}", f.name);
                }
            }
            Err(_) => rejected += 1,
        }
    }
    assert_eq!(unsupported, 0);
    assert_eq!((supported, opportunities, empty, rejected), (3, 5, 1, 2));
    eprintln!(
        "scripted observer: supported=3 unsupported=0 precision=100% useful_coverage=3/5 (60%); empty=1 rejected=2; no live-model qualification"
    );
}
