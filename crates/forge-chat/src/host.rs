//! The config-shaped seam.
//!
//! Same idea as `forge-mcp`'s `Diagnostics` and `forge-acp`'s
//! `ServiceFactory`: this crate declares what it needs, and `forge-cli` —
//! the only crate that can see config resolution, provider construction
//! and credential detection — implements it. Every method returns owned
//! plain data, which is what lets a `FakeHost` drive the whole loop with
//! no config files and no terminal.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use forge_core::ForgeError;
use forge_runtime::AgentService;

use crate::io::Line;

/// Everything the chat needs from forge that is not the runtime itself.
#[async_trait]
pub trait ChatHost: Send + Sync {
    fn service(&self) -> Arc<AgentService>;
    /// Rebuild the runtime with one setting overridden. On error the old
    /// runtime is kept and the error is returned unchanged.
    async fn switch(&mut self, change: HostChange) -> Result<(), ForgeError>;
    /// Run a provider-owned subscription login, then rebuild the runtime so
    /// the new credential is usable without restarting chat.
    async fn authenticate(&mut self, provider: &str) -> Result<(), ForgeError>;
    /// Inspect the App's current session without starting workers or model calls.
    async fn context_status(&mut self, _session_id: &str) -> Result<Vec<Line>, ForgeError> {
        Err(ForgeError::config(
            "context inspection is unavailable in this host",
        ))
    }
    async fn memory_status(&mut self, _session_id: &str) -> Result<Vec<Line>, ForgeError> {
        Err(ForgeError::config(
            "memory inspection is unavailable in this host",
        ))
    }
    /// Session consent for extraction only, never live prompt injection. The
    /// host enforces project readiness and requests active observer cancellation
    /// on off; neither control alters the conversation or its active model turn.
    async fn set_memory_observation(
        &mut self,
        _session_id: &str,
        _enabled: bool,
    ) -> Result<Vec<Line>, ForgeError> {
        Err(ForgeError::config(
            "memory controls are unavailable in this host",
        ))
    }
    /// Bounded backend page (at most 20 items / 16 KiB), with continuation.
    async fn memory_show(
        &mut self,
        _session_id: &str,
        _offset: usize,
    ) -> Result<Vec<Line>, ForgeError> {
        Err(ForgeError::config(
            "memory inspection is unavailable in this host",
        ))
    }
    /// Source metadata only; never implicitly dump raw source text.
    async fn memory_sources(
        &mut self,
        _session_id: &str,
        _offset: usize,
    ) -> Result<Vec<Line>, ForgeError> {
        Err(ForgeError::config(
            "memory inspection is unavailable in this host",
        ))
    }
    async fn task_list(&mut self) -> Result<Vec<Line>, ForgeError> {
        Err(ForgeError::config(
            "task inspection is unavailable in this host",
        ))
    }
    async fn task_show(&mut self, _task_id: &str) -> Result<Vec<Line>, ForgeError> {
        Err(ForgeError::config(
            "task inspection is unavailable in this host",
        ))
    }
    fn environment(&self) -> Environment;
    /// User-visible model candidates only: this is where mock entries are
    /// filtered out, in one place (§9.3).
    fn models(&self) -> Vec<ModelChoice>;
    fn skills(&self) -> Vec<SkillChoice>;
    /// Project-relative file paths for `@`-completion: sorted,
    /// `/`-separated, and already capped by the implementation. Empty when
    /// no project graph has been built — `@`-completion degrades to
    /// silence, never to an error, so this returns data, not a `Result`.
    fn project_files(&self) -> Vec<String>;
    /// `forge config show`/`explain` data: key, value, origin.
    fn config_summary(&self, key: Option<&str>) -> Vec<ConfigLine>;
    /// Ranked project-graph hits for `/graph`. Async so that semantic
    /// blending — which must *embed the query text*, an inherently async
    /// call into the needle engine — is reachable from chat; a sync
    /// signature here is what previously forbade it outright, even with a
    /// working engine and a built index. `steering` is the optional
    /// free-text half of `/graph <query> -- <steering>`: embedded alongside
    /// the query, it steers the semantic half of the blend (see
    /// `forge_graph::blended_context`).
    async fn graph_context(
        &self,
        query: &str,
        steering: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ContextLine>, ForgeError>;
}

/// The one setting a slash command may change. A session-scoped override,
/// never a config file write (`/model` and `/approval` change the session).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostChange {
    Model(String),
    Approval(String),
}

/// What the banner and `/config` report: the resolved facts of this
/// conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Environment {
    pub project_root: PathBuf,
    pub model: String,
    pub router: String,
    pub approval: String,
    pub needle: NeedleState,
}

/// Whether the embedded brain is actually usable, phrased with the same
/// vocabulary `forge doctor` uses so two surfaces cannot tell a user
/// different stories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NeedleState {
    Active { model_id: String },
    Inactive { reason: String },
}

/// One entry of the `/model` listing, and one `/model` completion
/// candidate. Test-only providers never reach this type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    pub name: String,
    pub description: String,
    /// True for the model this conversation is currently using.
    pub active: bool,
}

/// One discovered skill: its `/name` and its one-line description
/// (metadata only — the instructions stay behind progressive disclosure).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillChoice {
    pub name: String,
    pub description: String,
}

/// One line of `/config` output: the winning value and where it came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigLine {
    pub key: String,
    pub value: String,
    /// Provenance as `forge config explain` words it (`default`,
    /// `user-config`, `project-config`, `environment`, `cli-flag`).
    pub origin: String,
}

/// One ranked context hit. The score is the ranker's own: the lexical
/// graph's integer score as a float, or the blend's 0..~1.5 weighted
/// cosine when semantic blending is in play — `f64` because truncating a
/// blended score to the old integer type reduced every hit to 0 or 1.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextLine {
    pub path: String,
    pub score: f64,
}
