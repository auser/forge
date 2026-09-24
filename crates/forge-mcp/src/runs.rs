//! Bookkeeping for runs this adapter started.
//!
//! The session store is the source of truth for a run's events, but it
//! only records an 80-character `Completed { summary }`. This registry
//! keeps the *full* outcome of runs we own, so `forge_run_status` can
//! return the complete final text after `forge_run` timed out waiting.
//!
//! Entries are bounded: past [`MAX_TRACKED_RUNS`], the oldest *settled*
//! entries are evicted (in-flight runs are never evicted). Eviction is
//! lossless for status purposes — the derived-from-events path in
//! `tools.rs` still answers for an evicted run, just with the truncated
//! summary instead of the full text.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use forge_core::ForgeError;
use forge_runtime::RunOutcome;

/// Upper bound on tracked runs. A long-lived MCP server (one per editor
/// session) must not grow without limit.
pub(crate) const MAX_TRACKED_RUNS: usize = 256;

/// Terminal state of a run we started.
#[derive(Debug, Clone)]
pub(crate) enum Final {
    Completed(Box<RunOutcome>),
    Failed(String),
    Cancelled,
}

impl Final {
    pub(crate) fn from_join(
        result: Result<Result<RunOutcome, ForgeError>, tokio::task::JoinError>,
    ) -> Self {
        match result {
            Ok(Ok(outcome)) => Self::Completed(Box::new(outcome)),
            Ok(Err(e)) => Self::Failed(e.to_string()),
            Err(e) if e.is_cancelled() => Self::Cancelled,
            Err(e) => Self::Failed(format!("run task did not finish: {e}")),
        }
    }
}

enum Slot {
    /// Started by us, still in flight.
    Running,
    Settled(Final),
}

#[derive(Default)]
pub(crate) struct RunRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<String, Slot>,
    order: VecDeque<String>,
}

impl RunRegistry {
    /// Record a run as started by this adapter.
    pub(crate) fn start(&self, run_id: &str) {
        let mut inner = self.lock();
        inner.slots.insert(run_id.to_string(), Slot::Running);
        inner.order.push_back(run_id.to_string());
        inner.evict_settled_over_cap();
    }

    /// Record a run's terminal state.
    pub(crate) fn settle(&self, run_id: &str, state: Final) {
        let mut inner = self.lock();
        if let Some(slot) = inner.slots.get_mut(run_id) {
            *slot = Slot::Settled(state);
        }
        inner.evict_settled_over_cap();
    }

    /// `None` when this adapter never started (or no longer tracks) the
    /// run; `Some(None)` when it is still in flight.
    pub(crate) fn state(&self, run_id: &str) -> Option<Option<Final>> {
        let inner = self.lock();
        inner.slots.get(run_id).map(|slot| match slot {
            Slot::Running => None,
            Slot::Settled(state) => Some(state.clone()),
        })
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().slots.len()
    }

    /// A poisoned lock carries no invariant worth aborting for: this map
    /// is pure bookkeeping, so recover the data rather than panicking in
    /// the middle of the protocol loop.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Inner {
    fn evict_settled_over_cap(&mut self) {
        while self.slots.len() > MAX_TRACKED_RUNS {
            let Some(pos) = self
                .order
                .iter()
                .position(|id| matches!(self.slots.get(id), Some(Slot::Settled(_))))
            else {
                break; // everything in flight: grow beyond the cap
            };
            if let Some(id) = self.order.remove(pos) {
                self.slots.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(run_id: &str) -> RunOutcome {
        RunOutcome {
            run_id: run_id.to_string(),
            session_id: "s".into(),
            text: "done".into(),
            turns: 1,
            tool_calls: 0,
            events: Vec::new(),
        }
    }

    #[test]
    fn unknown_run_is_none() {
        let registry = RunRegistry::default();
        assert!(registry.state("nope").is_none());
    }

    #[test]
    fn started_run_is_in_flight_until_settled() {
        let registry = RunRegistry::default();
        registry.start("r1");
        assert!(matches!(registry.state("r1"), Some(None)));

        registry.settle("r1", Final::Completed(Box::new(outcome("r1"))));
        let Some(Some(Final::Completed(o))) = registry.state("r1") else {
            panic!("expected a completed outcome");
        };
        assert_eq!(o.text, "done");
    }

    #[test]
    fn settling_an_untracked_run_does_not_resurrect_it() {
        let registry = RunRegistry::default();
        registry.settle("ghost", Final::Cancelled);
        assert!(registry.state("ghost").is_none());
    }

    #[test]
    fn settled_entries_are_evicted_past_the_cap_and_in_flight_ones_are_not() {
        let registry = RunRegistry::default();
        registry.start("keep-me");
        for i in 0..MAX_TRACKED_RUNS + 10 {
            let id = format!("r{i}");
            registry.start(&id);
            registry.settle(&id, Final::Cancelled);
        }
        assert!(registry.len() <= MAX_TRACKED_RUNS);
        assert!(
            matches!(registry.state("keep-me"), Some(None)),
            "an in-flight run must survive eviction"
        );
    }
}
