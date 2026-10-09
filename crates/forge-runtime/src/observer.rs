//! Opt-in derived-memory worker. Its task owns dependencies, never AgentService
//! or its own supervisor handle. Dropping the last handle aborts provider work.
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::inspection::memory_observation_enabled;
use forge_context::{
    BudgetLimits, OBSERVER_PROMPT, ObservationStore, ObserverChunk, ObserverJobState,
    ObserverLease, ObserverPolicy, ObserverQueue, ObserverStatus, observer_chunks,
    observer_request, parse_observer_output,
};
use forge_core::{
    CompletionRequest, CompletionResponse, Event, EventKind, ForgeError, Message, ModelProvider,
};
use forge_session::JsonlSessionStore;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Explicit, complete prices, in microdollars per million tokens. Zero is a
/// known zero price; absence must be rejected by the production factory.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ObserverPrices {
    pub input_micro_usd_per_million: u64,
    pub output_micro_usd_per_million: u64,
}

impl ObserverPrices {
    /// Convert dollar amounts without granting spending permission through
    /// rounding: rates round up, budget ceilings round down.
    pub fn micro_usd(value: f64, round_up: bool) -> Result<u64, ForgeError> {
        let scaled = value * 1_000_000.0;
        if !scaled.is_finite() || scaled < 0.0 || scaled >= u64::MAX as f64 {
            return Err(ForgeError::config("observer amount is out of range"));
        }
        Ok(if round_up {
            scaled.ceil()
        } else {
            scaled.floor()
        } as u64)
    }

    pub(crate) fn cost(self, input: u64, output: u64) -> Option<u64> {
        let numerator = u128::from(input)
            .checked_mul(u128::from(self.input_micro_usd_per_million))?
            .checked_add(
                u128::from(output).checked_mul(u128::from(self.output_micro_usd_per_million))?,
            )?;
        u64::try_from(numerator.checked_add(999_999)? / 1_000_000).ok()
    }
}

#[derive(Serialize)]
pub struct ObserverRuntimeStatus {
    pub model: String,
    pub running: bool,
    pub budget: BudgetLimits,
    pub queue: ObserverStatus,
    /// Sessions refused in the latest discovery pass (no contents or ids).
    pub discovery_blocked: usize,
    pub worker_errors: u64,
}

struct Worker {
    provider: Arc<dyn ModelProvider>,
    queue: Arc<dyn ObserverQueue>,
    ledger: Arc<dyn ObservationStore>,
    sessions: Arc<JsonlSessionStore>,
    policy: ObserverPolicy,
    budget: BudgetLimits,
    prices: ObserverPrices,
    timeout: Duration,
    wake: Arc<Notify>,
    discovery_blocked: AtomicUsize,
    errors: AtomicU64,
}

pub struct ObserverSupervisor {
    worker: Arc<Worker>,
    task: Mutex<Option<JoinHandle<()>>>,
}

fn unavailable() -> ForgeError {
    ForgeError::config("observer unavailable")
}

impl ObserverSupervisor {
    /// Injection is a trusted host boundary: the factory must enforce model,
    /// endpoint/redirect egress and complete prices before constructing this.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        queue: Arc<dyn ObserverQueue>,
        ledger: Arc<dyn ObservationStore>,
        sessions: Arc<JsonlSessionStore>,
        policy: ObserverPolicy,
        budget: BudgetLimits,
        prices: ObserverPrices,
    ) -> Result<Self, ForgeError> {
        if policy.model.trim().is_empty()
            || provider.name() != policy.model
            || policy.prompt_version != forge_context::OBSERVER_PROMPT_VERSION
            || policy.limits.max_output_tokens == 0
        {
            return Err(unavailable());
        }
        Ok(Self {
            worker: Arc::new(Worker {
                provider,
                queue,
                ledger,
                sessions,
                policy,
                budget,
                prices,
                timeout: Duration::from_secs(60),
                wake: Arc::new(Notify::new()),
                discovery_blocked: AtomicUsize::new(0),
                errors: AtomicU64::new(0),
            }),
            task: Mutex::new(None),
        })
    }

    /// Set a bounded call timeout before starting. Intended also for scripted
    /// tests; never permits unbounded provider waits.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, ForgeError> {
        if timeout.is_zero() || timeout > Duration::from_secs(110) {
            return Err(unavailable());
        }
        Arc::get_mut(&mut self.worker)
            .ok_or_else(unavailable)?
            .timeout = timeout;
        Ok(self)
    }

    /// Start one coalescing worker. Discovery runs before waiting for any event,
    /// recovering pending work and completed runs missed during an earlier exit.
    pub fn start(&self) -> Result<(), ForgeError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| unavailable())?;
        let mut task = self.task.lock().map_err(|_| unavailable())?;
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let worker = Arc::clone(&self.worker);
        *task = Some(runtime.spawn(worker.run()));
        Ok(())
    }

    /// A single coalesced permit, not a per-event task or growing session list.
    pub fn notify(&self) {
        self.worker.wake.notify_one();
    }

    /// Abort immediately. In-flight reservations remain charged and leases
    /// recover durably after expiry; shutdown never awaits provider completion.
    /// Already-running blocking filesystem work is not cancelled by task abort:
    /// process exit can still wait for filesystem operations and lock timeouts.
    pub fn shutdown(&self) {
        if let Ok(mut task) = self.task.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }

    pub async fn status(&self) -> Result<ObserverRuntimeStatus, ForgeError> {
        let worker = Arc::clone(&self.worker);
        let running = self
            .task
            .lock()
            .map_err(|_| unavailable())?
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        tokio::task::spawn_blocking(move || {
            Ok(ObserverRuntimeStatus {
                model: worker.policy.model.clone(),
                running,
                budget: worker.budget,
                queue: worker.queue.status().map_err(|_| unavailable())?,
                discovery_blocked: worker.discovery_blocked.load(Ordering::Relaxed),
                worker_errors: worker.errors.load(Ordering::Relaxed),
            })
        })
        .await
        .map_err(|_| unavailable())?
    }

    pub(crate) fn uses_sessions(&self, sessions: &Arc<JsonlSessionStore>) -> bool {
        Arc::ptr_eq(&self.worker.sessions, sessions)
    }

    /// Called at the synchronous session-fork boundary, before the child id is
    /// returned to its caller. There is deliberately no later repair/import.
    pub(crate) fn freeze_fork(&self, parent: &str, parent_events: &[Event], child: &str, cut: u64) {
        let worker = &self.worker;
        let frozen = worker.sessions.events_for(child).ok().and_then(|events| {
            worker
                .ledger
                .fork(
                    parent,
                    parent_events,
                    child,
                    &events,
                    cut,
                    worker.sessions.redactor(),
                )
                .ok()
        });
        if frozen.is_none() {
            worker.errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("observer fork unavailable; child observation remains disabled");
        }
    }
}

impl Drop for ObserverSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Worker {
    fn consent(&self, session: &str, since: usize) -> bool {
        self.sessions.events_for(session).is_ok_and(|events| {
            memory_observation_enabled(&events)
                && !events.iter().skip(since).any(|event| {
                    matches!(
                        event.kind,
                        EventKind::MemoryObservationChanged { enabled: false }
                    )
                })
        })
    }

    async fn run(self: Arc<Self>) {
        loop {
            let worker = Arc::clone(&self);
            let prepared = tokio::task::spawn_blocking(move || worker.prepare()).await;
            match prepared {
                Ok(Ok(Some((lease, chunk)))) => {
                    self.complete(lease, chunk).await;
                    // Drain durable work without waiting for another notification.
                    continue;
                }
                Ok(Ok(None)) => {}
                _ => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Polling covers crash recovery and lease/retry expiry, including
            // completed runs written by another process. Notify has one permit.
            tokio::select! {
                _ = self.wake.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
        }
    }

    fn fork_ready(&self, session: &str, events: &[Event]) -> bool {
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::SessionForked { .. }))
            || self.ledger.fork_initialized(session).unwrap_or(false)
    }

    /// All session/queue/ledger filesystem work runs on the blocking pool.
    fn prepare(&self) -> Result<Option<(ObserverLease, ObserverChunk)>, ForgeError> {
        let mut blocked = 0;
        // Enumerate metadata only: list_sessions reads every transcript first,
        // so one invalid UTF-8 source would bypass the per-session isolation.
        for (session_id, _, _) in self.sessions.session_files_by_recency()? {
            let events = match self.sessions.events_for(&session_id) {
                Ok(events) => events,
                Err(_) => {
                    blocked += 1;
                    continue;
                }
            };
            if !memory_observation_enabled(&events) {
                continue;
            }
            if !self.fork_ready(&session_id, &events) {
                blocked += 1;
                continue;
            }
            match observer_chunks(&session_id, &events, self.sessions.redactor(), &self.policy) {
                Ok(chunks) => {
                    for chunk in chunks {
                        match self.queue.enqueue(chunk.job) {
                            Ok(()) => {}
                            Err(forge_context::ObserverError::Conflict) => blocked += 1,
                            Err(_) => return Err(unavailable()),
                        }
                    }
                }
                Err(_) => blocked += 1,
            }
        }
        self.discovery_blocked.store(blocked, Ordering::Relaxed);
        for record in self.queue.jobs().map_err(|_| unavailable())? {
            if matches!(
                record.state,
                ObserverJobState::Committed | ObserverJobState::Failed | ObserverJobState::Blocked
            ) {
                continue;
            }
            let job = record.job;
            // Never dispatch a persisted job through a different model/prompt.
            if job.policy.model != self.policy.model
                || job.policy.observer_version != self.policy.observer_version
                || job.policy.prompt_version != self.policy.prompt_version
            {
                continue;
            }
            let events = match self.sessions.events_for(&job.session_id) {
                Ok(events) => events,
                Err(_) => {
                    // A corrupt or missing source only disables its own job;
                    // it must not starve healthy sessions on every poll.
                    self.queue.block(&job.id).map_err(|_| unavailable())?;
                    continue;
                }
            };
            // Pause durable work in place: off is not a failed/blocked job.
            if !memory_observation_enabled(&events) {
                continue;
            }
            if !self.fork_ready(&job.session_id, &events) {
                self.queue.block(&job.id).map_err(|_| unavailable())?;
                continue;
            }
            // Exact source/version ledger reconciliation precedes spending.
            match self.queue.reconcile(
                &job.id,
                &events,
                self.sessions.redactor(),
                self.ledger.as_ref(),
            ) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(
                    forge_context::ObserverError::Conflict
                    | forge_context::ObserverError::InvalidSource,
                ) => {
                    self.queue.block(&job.id).map_err(|_| unavailable())?;
                    continue;
                }
                Err(_) => return Err(unavailable()),
            }
            let chunk = match observer_request(&job, &events, self.sessions.redactor()) {
                Ok(chunk) => chunk,
                Err(_) => {
                    self.queue.block(&job.id).map_err(|_| unavailable())?;
                    continue;
                }
            };
            let reservation = self
                .prices
                .cost(
                    chunk.input_tokens,
                    u64::from(job.policy.limits.max_output_tokens),
                )
                .ok_or_else(unavailable)?;
            match self.queue.claim(&job.id, reservation, self.budget) {
                Ok(Some(lease)) => return Ok(Some((lease, chunk))),
                Ok(None) | Err(forge_context::ObserverError::BudgetExceeded) => {}
                Err(_) => return Err(unavailable()),
            }
        }
        Ok(None)
    }

    async fn complete(self: &Arc<Self>, lease: ObserverLease, chunk: ObserverChunk) {
        let check = Arc::clone(self);
        let session = lease.job.session_id.clone();
        let dispatch_position = tokio::task::spawn_blocking(move || {
            check
                .sessions
                .events_for(&session)
                .ok()
                .filter(|events| memory_observation_enabled(events))
                .map(|events| events.len())
        })
        .await
        .ok()
        .flatten();
        let Some(dispatch_position) = dispatch_position else {
            let worker = Arc::clone(self);
            // Nothing was dispatched, so this reservation is known zero.
            let _ =
                tokio::task::spawn_blocking(move || worker.queue.discard(&lease, Some(0))).await;
            return;
        };
        let mut request = CompletionRequest::new(
            &lease.job.policy.model,
            vec![
                Message::system(OBSERVER_PROMPT),
                Message::user(chunk.request_json),
            ],
        );
        request.max_tokens = Some(lease.job.policy.limits.max_output_tokens);
        request.temperature = Some(0.0);
        // No tools, streaming, interactive agent loop or approval machinery.
        let result = {
            let completion = tokio::time::timeout(self.timeout, self.provider.complete(request));
            tokio::pin!(completion);
            loop {
                tokio::select! {
                    result = &mut completion => break Some(result),
                    _ = self.wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
                let worker = Arc::clone(self);
                let session = lease.job.session_id.clone();
                if !tokio::task::spawn_blocking(move || worker.consent(&session, dispatch_position))
                    .await
                    .unwrap_or(false)
                {
                    // Dropping completion cancels the local provider future.
                    // Remote execution may already have incurred charges.
                    break None;
                }
            }
        };
        let worker = Arc::clone(self);
        let result = tokio::task::spawn_blocking(move || match result {
            Some(Ok(Ok(response))) => worker.finish(&lease, response, dispatch_position),
            None => worker
                .queue
                .discard(&lease, None)
                .map_err(|_| unavailable()),
            _ => worker.queue.fail(&lease, true).map_err(|_| unavailable()),
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn finish(
        &self,
        lease: &ObserverLease,
        response: CompletionResponse,
        dispatch_position: usize,
    ) -> Result<(), ForgeError> {
        let _gate = crate::inspection::MEMORY_POLICY_GATE
            .lock()
            .map_err(|_| unavailable())?;
        let actual = response.usage.as_ref().and_then(|usage| {
            self.prices.cost(
                u64::from(usage.prompt_tokens),
                u64::from(usage.completion_tokens),
            )
        });
        if !self.consent(&lease.job.session_id, dispatch_position) {
            return self.queue.discard(lease, actual).map_err(|_| unavailable());
        }
        // A provider violating the plain-text contract cannot create a batch.
        if !response.tool_calls.is_empty()
            || response.content.len() > lease.job.policy.limits.max_output_bytes
            || matches!(
                response.finish_reason.as_deref(),
                Some("length" | "tool_calls" | "function_call")
            )
        {
            return self.queue.fail(lease, true).map_err(|_| unavailable());
        }
        let events = self.sessions.events_for(&lease.job.session_id)?;
        let batch = match parse_observer_output(
            &lease.job,
            &events,
            self.sessions.redactor(),
            &response.content,
        ) {
            Ok(batch) => batch,
            Err(error) => {
                let retryable = !matches!(
                    error,
                    forge_context::ObserverError::Conflict
                        | forge_context::ObserverError::InvalidSource
                );
                return self.queue.fail(lease, retryable).map_err(|_| unavailable());
            }
        };
        // Queue owns the critical fence THROUGH ledger.commit. A stale worker
        // cannot commit between lease validation and lease replacement.
        if !self.consent(&lease.job.session_id, dispatch_position) {
            return self.queue.discard(lease, actual).map_err(|_| unavailable());
        }
        self.queue
            .finalize(lease, batch, self.ledger.as_ref(), actual)
            .map_err(|_| unavailable())
    }
}

#[cfg(test)]
mod tests;
