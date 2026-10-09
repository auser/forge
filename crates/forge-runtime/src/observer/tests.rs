use super::*;
use async_trait::async_trait;
use forge_context::{
    MemoryObservationStore, MemoryObserverQueue, OBSERVER_PROMPT_VERSION, OBSERVER_VERSION,
    ObserverClock, ObserverLimits,
};
use forge_core::{ModelCapabilities, SessionStore};

#[derive(Default)]
struct Clock(AtomicU64);
impl ObserverClock for Clock {
    fn now_unix_seconds(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

struct Scripted {
    started: Notify,
    release: Notify,
    blocked: bool,
    output: Mutex<String>,
    calls: AtomicUsize,
}
#[async_trait]
impl ModelProvider for Scripted {
    fn name(&self) -> &str {
        "observer-test"
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        assert!(request.tools.is_empty());
        assert_eq!(request.max_tokens, Some(2048));
        assert_eq!(request.messages.len(), 2);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.started.notify_one();
        if self.blocked {
            self.release.notified().await;
        }
        Ok(CompletionResponse {
            model: self.name().into(),
            content: self.output.lock().unwrap().clone(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: None,
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    sessions: Arc<JsonlSessionStore>,
    queue: Arc<MemoryObserverQueue>,
    ledger: Arc<MemoryObservationStore>,
    provider: Arc<Scripted>,
    clock: Arc<Clock>,
    supervisor: ObserverSupervisor,
}
fn policy() -> ObserverPolicy {
    ObserverPolicy {
        observer_version: OBSERVER_VERSION.into(),
        prompt_version: OBSERVER_PROMPT_VERSION.into(),
        model: "observer-test".into(),
        limits: ObserverLimits::default(),
    }
}
fn append_run(sessions: &JsonlSessionStore, session: &str, run: &str) {
    for kind in [
        EventKind::RunStarted {
            provider: "mock".into(),
            model: "mock".into(),
            prompt: "Use Rust".into(),
        },
        EventKind::AssistantMessage {
            text: "Rust selected".into(),
            tool_calls: Vec::new(),
        },
        EventKind::Completed {
            summary: "done".into(),
        },
    ] {
        sessions.append(Event::new(run, session, kind)).unwrap();
    }
}
fn output(range: forge_context::SourceRange) -> String {
    serde_json::json!({"range":range,"observations":[]}).to_string()
}
fn fixture(blocked: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(JsonlSessionStore::new(dir.path().join("sessions")));
    append_run(&sessions, "parent", "run-1");
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let queue = Arc::new(MemoryObserverQueue::with_clock(clock.clone()));
    let ledger = Arc::new(MemoryObservationStore::default());
    let chunks = observer_chunks(
        "parent",
        &sessions.events_for("parent").unwrap(),
        sessions.redactor(),
        &policy(),
    )
    .unwrap();
    assert_eq!(chunks.len(), 1);
    let provider = Arc::new(Scripted {
        started: Notify::new(),
        release: Notify::new(),
        blocked,
        output: Mutex::new(output(chunks[0].job.range)),
        calls: AtomicUsize::new(0),
    });
    let supervisor = ObserverSupervisor::new(
        provider.clone(),
        queue.clone(),
        ledger.clone(),
        sessions.clone(),
        policy(),
        BudgetLimits::default(),
        ObserverPrices {
            input_micro_usd_per_million: 1_000_000,
            output_micro_usd_per_million: 1_000_000,
        },
    )
    .unwrap();
    Fixture {
        _dir: dir,
        sessions,
        queue,
        ledger,
        provider,
        clock,
        supervisor,
    }
}

#[tokio::test]
async fn startup_discovers_completed_work_and_missing_usage_stays_charged() {
    let f = fixture(false);
    f.supervisor.start().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if f.supervisor.status().await.unwrap().queue.committed == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let status = f.supervisor.status().await.unwrap();
    assert!(status.queue.unknown_micro_usd > 0);
    assert_eq!(status.queue.known_micro_usd, 0);
    assert_eq!(f.provider.calls.load(Ordering::Relaxed), 1);
    f.supervisor.shutdown();
}

#[tokio::test]
async fn active_turn_and_shutdown_do_not_wait_for_a_blocked_observer() {
    let f = fixture(true);
    let observer = Arc::new(f.supervisor);
    let service = crate::AgentService::new(
        Arc::new(forge_providers::MockModel::new()),
        Arc::new(forge_providers::MockRouter::selecting("mock-local")),
        Arc::new(forge_execution::MockExecution::new(f._dir.path())),
        Arc::new(crate::NullSkillRegistry),
        f.sessions.clone(),
        forge_config::Config::default(),
    )
    .with_observer(Some(observer.clone()));
    observer.start().unwrap();
    tokio::time::timeout(Duration::from_secs(3), f.provider.started.notified())
        .await
        .unwrap();
    for _ in 0..1000 {
        observer.notify();
    }
    let outcome = tokio::time::timeout(Duration::from_secs(2), service.run("hello"))
        .await
        .unwrap()
        .unwrap();
    assert!(outcome.text.contains("hello"));
    assert_eq!(f.provider.calls.load(Ordering::Relaxed), 1);
    let started = std::time::Instant::now();
    observer.shutdown();
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(f.queue.status().unwrap().reserved_micro_usd > 0);
}

#[tokio::test]
async fn retry_exhaustion_and_timeout_preserve_attempt_charges() {
    let mut f = fixture(true);
    f.supervisor = f
        .supervisor
        .with_timeout(Duration::from_millis(10))
        .unwrap();
    for _ in 0..3 {
        let (lease, chunk) = f.supervisor.worker.prepare().unwrap().unwrap();
        f.supervisor.worker.complete(lease, chunk).await;
        f.clock.0.fetch_add(6, Ordering::Relaxed);
    }
    assert!(f.supervisor.worker.prepare().unwrap().is_none());
    assert_eq!(f.provider.calls.load(Ordering::Relaxed), 3);
    let status = f.queue.status().unwrap();
    assert_eq!(status.failed, 1);
    assert!(status.unknown_micro_usd > 0);
}

#[tokio::test]
async fn malformed_oversized_and_fabricated_ranges_never_commit() {
    for invalid in [
        "not JSON".to_string(),
        "x".repeat(ObserverLimits::default().max_output_bytes + 1),
        r#"{"range":{"start":999,"end":999},"observations":[]}"#.into(),
        r#"{"range":{"start":1,"end":3},"observations":[],"tools":[]}"#.into(),
    ] {
        let f = fixture(false);
        *f.provider.output.lock().unwrap() = invalid;
        let (lease, chunk) = f.supervisor.worker.prepare().unwrap().unwrap();
        f.supervisor.worker.complete(lease, chunk).await;
        assert_eq!(f.queue.status().unwrap().committed, 0);
        assert!(
            f.ledger
                .projection(
                    "parent",
                    &f.sessions.events_for("parent").unwrap(),
                    f.sessions.redactor()
                )
                .unwrap()
                .batches()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn stale_attempt_cannot_commit_after_another_worker_claims() {
    let f = fixture(false);
    let (old, _) = f.supervisor.worker.prepare().unwrap().unwrap();
    f.clock.0.fetch_add(121, Ordering::Relaxed);
    let replacement = f
        .queue
        .claim(&old.job.id, 1, BudgetLimits::default())
        .unwrap()
        .unwrap();
    let response = CompletionResponse {
        model: "observer-test".into(),
        content: f.provider.output.lock().unwrap().clone(),
        tool_calls: vec![],
        finish_reason: None,
        usage: None,
    };
    assert!(f.supervisor.worker.finish(&old, response).is_err());
    assert_eq!(f.queue.status().unwrap().committed, 0);
    let events = f.sessions.events_for("parent").unwrap();
    assert!(
        f.ledger
            .projection("parent", &events, f.sessions.redactor())
            .unwrap()
            .batches()
            .is_empty()
    );
    f.queue.fail(&replacement, false).unwrap();
}

#[test]
fn exact_committed_batch_is_reconciled_without_another_provider_call() {
    let f = fixture(false);
    let (lease, _) = f.supervisor.worker.prepare().unwrap().unwrap();
    let events = f.sessions.events_for("parent").unwrap();
    let batch = parse_observer_output(
        &lease.job,
        &events,
        f.sessions.redactor(),
        &f.provider.output.lock().unwrap(),
    )
    .unwrap();
    f.ledger.commit(batch).unwrap(); // simulate crash before queue mark
    assert!(f.supervisor.worker.prepare().unwrap().is_none());
    assert_eq!(f.queue.status().unwrap().committed, 1);
    assert_eq!(f.provider.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn prices_round_up_and_overflow_refuses_dispatch() {
    let tiny = ObserverPrices {
        input_micro_usd_per_million: 1,
        output_micro_usd_per_million: 0,
    };
    assert_eq!(tiny.cost(1, 0), Some(1));
    let huge = ObserverPrices {
        input_micro_usd_per_million: u64::MAX,
        output_micro_usd_per_million: u64::MAX,
    };
    assert_eq!(huge.cost(u64::MAX, u64::MAX), None);
}

fn service(f: &Fixture, observer: Option<Arc<ObserverSupervisor>>) -> crate::AgentService {
    crate::AgentService::new(
        Arc::new(forge_providers::MockModel::new()),
        Arc::new(forge_providers::MockRouter::selecting("mock-local")),
        Arc::new(forge_execution::MockExecution::new(f._dir.path())),
        Arc::new(crate::NullSkillRegistry),
        f.sessions.clone(),
        forge_config::Config::default(),
    )
    .with_observer(observer)
}

#[tokio::test]
async fn actual_fork_freezes_before_late_parent_commit_and_child_jobs_are_local() {
    let f = fixture(false);
    let (lease, chunk) = f.supervisor.worker.prepare().unwrap().unwrap();
    let sessions = f.sessions.clone();
    let queue = f.queue.clone();
    let ledger = f.ledger.clone();
    let mut agent = service(&f, None);
    let observer = Arc::new(f.supervisor);
    agent = agent.with_observer(Some(observer.clone()));
    let child = agent.fork_session("parent", None).unwrap();
    assert!(ledger.fork_initialized(&child.session_id).unwrap());
    observer.worker.complete(lease, chunk).await;
    assert_eq!(queue.status().unwrap().committed, 1);
    let inherited = ledger
        .projection(
            &child.session_id,
            &sessions.events_for(&child.session_id).unwrap(),
            sessions.redactor(),
        )
        .unwrap();
    assert!(
        inherited.batches().is_empty(),
        "late parent memory leaked into child"
    );
    append_run(&sessions, &child.session_id, "child-run");
    let (lease, _) = observer.worker.prepare().unwrap().unwrap();
    assert_eq!(lease.job.session_id, child.session_id);
    assert!(lease.job.range.start > child.at_position + 1);
}

#[test]
fn startup_never_retroactively_initializes_an_unfrozen_child() {
    let f = fixture(false);
    let agent = service(&f, None); // observations disabled when fork happened
    let child = agent.fork_session("parent", None).unwrap();
    append_run(&f.sessions, &child.session_id, "child-run");
    let _ = f.supervisor.worker.prepare().unwrap();
    assert!(!f.ledger.fork_initialized(&child.session_id).unwrap());
    assert!(
        f.queue
            .jobs()
            .unwrap()
            .iter()
            .all(|record| record.job.session_id != child.session_id)
    );
    assert_eq!(
        f.supervisor
            .worker
            .discovery_blocked
            .load(Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn source_is_reloaded_after_provider_returns() {
    let f = fixture(false);
    let (lease, chunk) = f.supervisor.worker.prepare().unwrap().unwrap();
    let mut events = f.sessions.events_for("parent").unwrap();
    events[0].kind = EventKind::RunStarted {
        provider: "mock".into(),
        model: "mock".into(),
        prompt: "Changed source".into(),
    };
    let mut bytes = String::new();
    for event in &events {
        bytes.push_str(&serde_json::to_string(event).unwrap());
        bytes.push('\n');
    }
    std::fs::write(f.sessions.root().join("parent.jsonl"), bytes).unwrap();
    f.supervisor.worker.complete(lease, chunk).await;
    assert_eq!(f.queue.status().unwrap().committed, 0);
    assert!(
        f.ledger
            .projection("parent", &events, f.sessions.redactor())
            .unwrap()
            .batches()
            .is_empty()
    );
}

#[test]
fn budget_refusal_precedes_any_provider_call() {
    let mut f = fixture(false);
    Arc::get_mut(&mut f.supervisor.worker).unwrap().budget = BudgetLimits {
        session_micro_usd: 0,
        daily_micro_usd: 0,
    };
    assert!(f.supervisor.worker.prepare().unwrap().is_none());
    assert_eq!(f.provider.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn corrupt_queued_source_does_not_starve_healthy_sessions() {
    for corruption in [b"{corrupt\n".as_slice(), b"\xff\xc3".as_slice()] {
        assert_corrupt_source_is_isolated(corruption).await;
    }
}

async fn assert_corrupt_source_is_isolated(corruption: &[u8]) {
    let f = fixture(false);
    let parent_events = f.sessions.events_for("parent").unwrap();
    let bad_job = observer_chunks("parent", &parent_events, f.sessions.redactor(), &policy())
        .unwrap()
        .remove(0)
        .job;
    f.queue.enqueue(bad_job.clone()).unwrap();
    std::fs::write(f.sessions.root().join("parent.jsonl"), corruption).unwrap();
    append_run(&f.sessions, "healthy", "healthy-run");

    let (lease, chunk) = f.supervisor.worker.prepare().unwrap().unwrap();
    assert_eq!(lease.job.session_id, "healthy");
    f.supervisor.worker.complete(lease, chunk).await;
    // Also check the next poll, regardless of the queue's iteration order.
    assert!(f.supervisor.worker.prepare().unwrap().is_none());
    assert!(f.queue.jobs().unwrap().iter().any(|record| {
        record.job.id == bad_job.id && record.state == ObserverJobState::Blocked
    }));
    assert_eq!(f.queue.status().unwrap().committed, 1);
    assert_eq!(f.provider.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn discovery_at_both_fork_publication_windows_cannot_queue_copied_parent_work() {
    let f = fixture(false);
    // Keep the parent's job leased so any additional dispatch must be a child.
    let (parent_lease, _) = f.supervisor.worker.prepare().unwrap().unwrap();
    let parent_events = f.sessions.events_for("parent").unwrap();
    let cut = parent_events.len() as u64;
    let barrier = std::sync::Barrier::new(2);
    let mut windows = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            f.sessions
                .copy_prefix("parent", "child", cut as usize)
                .unwrap();
            barrier.wait(); // child JSONL visible, marker not yet written
            barrier.wait();
            f.sessions
                .append(Event::new(
                    "fork",
                    "child",
                    EventKind::SessionForked {
                        from_session: "parent".into(),
                        at_position: cut,
                    },
                ))
                .unwrap();
            barrier.wait(); // marker visible, frozen ledger not yet written
            barrier.wait();
            f.supervisor
                .freeze_fork("parent", &parent_events, "child", cut);
        });
        for _ in 0..2 {
            barrier.wait();
            let prepared = f.supervisor.worker.prepare();
            let jobs = f.queue.jobs();
            let calls = f.provider.calls.load(Ordering::Relaxed);
            let status = f.queue.status();
            windows.push((prepared, jobs, calls, status));
            // Assert only after joining, so a failure cannot strand the writer.
            barrier.wait();
        }
    });
    for (prepared, jobs, calls, status) in windows {
        assert!(prepared.unwrap().is_none());
        assert!(jobs.unwrap().iter().all(|r| r.job.session_id == "parent"));
        assert_eq!(calls, 0);
        assert_eq!(status.unwrap().committed, 0);
    }
    assert!(f.ledger.fork_initialized("child").unwrap());
    // A parent commit after freeze must not enter the child's inherited memory.
    let batch = parse_observer_output(
        &parent_lease.job,
        &parent_events,
        f.sessions.redactor(),
        &f.provider.output.lock().unwrap(),
    )
    .unwrap();
    f.queue
        .finalize(&parent_lease, batch, f.ledger.as_ref(), None)
        .unwrap();
    assert!(
        f.ledger
            .projection(
                "child",
                &f.sessions.events_for("child").unwrap(),
                f.sessions.redactor(),
            )
            .unwrap()
            .batches()
            .is_empty()
    );
    assert!(f.supervisor.worker.prepare().unwrap().is_none());
    append_run(&f.sessions, "child", "child-run");
    let (child_lease, _) = f.supervisor.worker.prepare().unwrap().unwrap();
    assert_eq!(child_lease.job.session_id, "child");
    assert!(child_lease.job.range.start > cut + 1);
}

#[test]
fn failed_fork_freeze_remains_disabled_during_later_discovery() {
    let f = fixture(false);
    let _parent_lease = f.supervisor.worker.prepare().unwrap().unwrap();
    let child = service(&f, None).fork_session("parent", None).unwrap();
    // A missing parent source makes freeze fail, as a filesystem/source failure
    // would. Ordinary conversation writes remain usable; observation fails shut.
    f.supervisor
        .freeze_fork("parent", &[], &child.session_id, child.at_position);
    assert_eq!(f.supervisor.worker.errors.load(Ordering::Relaxed), 1);
    append_run(&f.sessions, &child.session_id, "child-run");
    for _ in 0..2 {
        assert!(f.supervisor.worker.prepare().unwrap().is_none());
        assert!(!f.ledger.fork_initialized(&child.session_id).unwrap());
        assert!(
            f.queue
                .jobs()
                .unwrap()
                .iter()
                .all(|r| r.job.session_id == "parent")
        );
    }
    assert_eq!(f.provider.calls.load(Ordering::Relaxed), 0);
    assert_eq!(f.queue.status().unwrap().committed, 0);
}
