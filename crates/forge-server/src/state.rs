use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use forge_config::Config;
use forge_core::SkillRegistry;
use forge_graph::LocalGraph;
use forge_runtime::AgentService;
use serde::Serialize;

/// Lifecycle status of a run as seen by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed(String),
    Cancelled,
}

/// An in-flight (or recently finished) run started through this server.
pub struct RunEntry {
    pub status: RunStatus,
    pub abort: tokio::task::AbortHandle,
}

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<AgentService>,
    pub skills: Arc<dyn SkillRegistry>,
    /// `None` when no graph file exists (reported as `built: false`).
    pub graph: Option<Arc<LocalGraph>>,
    pub config: Config,
    pub runs: Arc<Mutex<HashMap<String, RunEntry>>>,
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
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = runs.get_mut(run_id) {
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
