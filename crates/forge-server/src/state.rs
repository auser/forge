use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use forge_config::Config;
use forge_core::SkillRegistry;
use forge_graph::LocalGraph;
use forge_runtime::AgentService;
use serde::Serialize;

/// Maximum number of runs tracked in the in-memory status map. Past the
/// cap, the oldest *terminal* entries are evicted (LRU-style by insertion
/// order); in-flight runs are never evicted. Eviction loses only the live
/// status/handle — the session store is the source of truth, so an
/// evicted run's events and derived status remain retrievable.
pub const MAX_TRACKED_RUNS: usize = 1024;

/// Lifecycle status of a run as seen by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    /// Parked in an approval wait; reachable via `POST .../input`.
    WaitingForApproval,
    Completed,
    Failed(String),
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_) | Self::Cancelled)
    }
}

/// An in-flight (or recently finished) run started through this server.
pub struct RunEntry {
    pub status: RunStatus,
    pub abort: tokio::task::AbortHandle,
}

/// Bounded status map with insertion order for eviction.
#[derive(Default)]
pub struct RunRegistry {
    map: HashMap<String, RunEntry>,
    order: VecDeque<String>,
}

impl RunRegistry {
    /// Insert a run, evicting oldest terminal entries beyond the cap.
    pub fn insert(&mut self, run_id: String, entry: RunEntry) {
        self.order.push_back(run_id.clone());
        self.map.insert(run_id, entry);
        self.evict_terminal_over_cap();
    }

    fn evict_terminal_over_cap(&mut self) {
        while self.map.len() > MAX_TRACKED_RUNS {
            let Some(pos) = self
                .order
                .iter()
                .position(|id| self.map.get(id).is_some_and(|e| e.status.is_terminal()))
            else {
                break; // everything in flight: grow beyond the cap
            };
            let id = self.order.remove(pos).expect("position valid");
            self.map.remove(&id);
        }
    }

    pub fn get(&self, run_id: &str) -> Option<&RunEntry> {
        self.map.get(run_id)
    }

    pub fn get_mut(&mut self, run_id: &str) -> Option<&mut RunEntry> {
        self.map.get_mut(run_id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<AgentService>,
    pub skills: Arc<dyn SkillRegistry>,
    /// `None` when no graph file exists (reported as `built: false`).
    pub graph: Option<Arc<LocalGraph>>,
    pub config: Config,
    pub runs: Arc<Mutex<RunRegistry>>,
}

impl AppState {
    pub fn new(
        service: Arc<AgentService>,
        skills: Arc<dyn SkillRegistry>,
        graph: Option<Arc<LocalGraph>>,
        config: Config,
    ) -> Self {
        Self {
            service,
            skills,
            graph,
            config,
            runs: Arc::new(Mutex::new(RunRegistry::default())),
        }
    }

    pub fn insert_run(&self, run_id: String, entry: RunEntry) {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run_id, entry);
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = runs.get_mut(run_id) {
            // Cancelled is sticky: a task finishing after cancel must not
            // overwrite it with the loop's abort error.
            if entry.status == RunStatus::Cancelled {
                return;
            }
            entry.status = status;
        }
    }

    pub fn status_of(&self, run_id: &str) -> Option<RunStatus> {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .map(|e| e.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(status: RunStatus) -> RunEntry {
        RunEntry {
            status,
            abort: tokio::spawn(async {}).abort_handle(),
        }
    }

    #[tokio::test]
    async fn registry_evicts_oldest_terminal_runs_beyond_cap() {
        let mut registry = RunRegistry::default();
        for i in 0..MAX_TRACKED_RUNS + 10 {
            registry.insert(format!("run-{i}"), entry(RunStatus::Completed));
        }
        assert_eq!(registry.len(), MAX_TRACKED_RUNS);
        // Oldest evicted, newest retained.
        assert!(registry.get("run-0").is_none());
        assert!(registry.get("run-9").is_none());
        assert!(
            registry
                .get(&format!("run-{}", MAX_TRACKED_RUNS + 9))
                .is_some()
        );
        assert!(registry.get("run-10").is_some());
    }

    #[tokio::test]
    async fn registry_never_evicts_in_flight_runs() {
        let mut registry = RunRegistry::default();
        for i in 0..MAX_TRACKED_RUNS + 5 {
            registry.insert(format!("run-{i}"), entry(RunStatus::Running));
        }
        // All in-flight: allowed to exceed the cap.
        assert_eq!(registry.len(), MAX_TRACKED_RUNS + 5);
    }
}
