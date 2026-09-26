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

/// Everything the chat needs from forge that is not the runtime itself.
#[async_trait]
pub trait ChatHost: Send + Sync {
    fn service(&self) -> Arc<AgentService>;
    /// Rebuild the runtime with one setting overridden. On error the old
    /// runtime is kept and the error is returned unchanged.
    async fn switch(&mut self, change: HostChange) -> Result<(), ForgeError>;
    fn environment(&self) -> Environment;
    /// User-visible model candidates only: this is where mock entries are
    /// filtered out, in one place (§9.3).
    fn models(&self) -> Vec<ModelChoice>;
    fn skills(&self) -> Vec<SkillChoice>;
    /// `forge config show`/`explain` data: key, value, origin.
    fn config_summary(&self, key: Option<&str>) -> Vec<ConfigLine>;
    fn graph_context(&self, query: &str, limit: usize) -> Result<Vec<ContextLine>, ForgeError>;
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

/// One ranked context hit, in the graph's own units (a project-relative
/// path and the graph's integer score), so nothing is converted twice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextLine {
    pub path: String,
    pub score: u32,
}
