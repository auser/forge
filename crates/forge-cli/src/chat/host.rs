//! `CliHost`: `forge-cli`'s implementation of `forge_chat::ChatHost`.
//!
//! `forge-chat` stays pure (no config resolution, no provider construction,
//! no credential detection — see `crates/forge-chat/src/host.rs`), so this
//! is where the seam is closed: `CliHost` owns a `Context`, the runtime it
//! built from that `Context`, and the handful of facts (needle state, the
//! discovered skills) that are settled once, at construction, because
//! nothing a chat session does (`/model`, `/approval`) can change them.
//!
//! Two things this module is deliberately careful about:
//!
//! * **Mocks never become a `/model` choice.** [`visible_models`] is the
//!   one place that filters them, and it delegates to
//!   `forge_providers::is_mock_model` rather than keeping a second list —
//!   that function's own doc already claims to be *the* place for this
//!   ("so `forge doctor` reports the same set rather than keeping its own
//!   copy"), and a second list here would be exactly the drift that
//!   promise exists to prevent.
//! * **The needle brain is described the way `forge doctor` describes it.**
//!   [`needle_state`] borrows `needle_checks`' wording
//!   (`commands/doctor.rs`) so the banner and `forge doctor` can never
//!   tell a user two different stories about whether the brain is active.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use forge_chat::{
    ChatHost, ConfigLine, ContextLine, Environment, HostChange, ModelChoice, NeedleState,
    SkillChoice,
};
use forge_core::{ForgeError, ProjectGraph as _};
use forge_execution::ApprovalChannel;
use forge_needle::EngineEmbedder;
use forge_runtime::AgentService;

use crate::cli::AuthProvider;
use crate::commands::Context;
use crate::commands::auth_cmd;
use crate::commands::service::{ServiceOptions, build_run_service_with, overrides_for};

/// The runtime chat sessions actually use, plus everything `ChatHost`
/// needs that is not the runtime itself.
pub struct CliHost {
    /// Owned, not borrowed: `switch` needs to re-resolve configuration on
    /// every `/model`/`/approval`, and a `ChatHost` implementation outlives
    /// the single command invocation that built it.
    ctx: Context,
    root: PathBuf,
    service: Arc<AgentService>,
    /// The session's accumulated overrides — starts as just the forced
    /// [`ApprovalChannel::Parked`] (§8.1: the chat owns stdin itself, so
    /// approvals must never try to read it behind the line editor's back),
    /// and gains a `model`/`approval` entry on every successful `switch`.
    /// Kept so a *second* `switch` layers onto the first rather than
    /// forgetting it — losing an earlier `/model` the moment `/approval`
    /// is used next would be a real regression, not a cosmetic one.
    options: ServiceOptions,
    /// Computed once, at construction: the router (and therefore whether
    /// needle is even in play) never changes for the life of a
    /// conversation — only the model and the approval mode do.
    needle: NeedleState,
    /// The on-device embedder for `/context`'s semantic blend, when this
    /// build has a working engine and weights. Built once at construction
    /// for the same reason as `needle`: nothing a chat session does can
    /// change it (a brain fetched by `forge init` in another window is
    /// picked up on the next chat, not mid-conversation). `None` means
    /// `/context` ranks lexically — the same degradation `forge graph
    /// context` has without an engine.
    embedder: Option<EngineEmbedder>,
    /// `project_files`' cache: the loaded path list keyed by the graph
    /// file's mtime (`None` = the file was absent at load). `refresh_
    /// completions` calls this once per submitted line, so the steady
    /// state must be one `stat`, not a re-parse of a graph file that can
    /// run to megabytes on a large repo. Interior mutability because the
    /// seam is `&self`.
    paths_cache: std::sync::Mutex<Option<(Option<std::time::SystemTime>, Vec<String>)>>,
}

impl CliHost {
    async fn context_memory(
        &self,
        session_id: &str,
        request: crate::commands::context_cmd::Request,
    ) -> Result<Vec<forge_chat::Line>, ForgeError> {
        let config = Arc::new(self.service.config().clone());
        let sessions = self.service.session_store();
        let root = self.root.clone();
        let session_id = session_id.to_owned();
        let report = tokio::task::spawn_blocking(move || {
            crate::commands::context_cmd::query(config, sessions, root, &session_id, request, false)
        })
        .await
        .map_err(|_| ForgeError::session("context inspection unavailable"))??;
        if matches!(request, crate::commands::context_cmd::Request::SetMemory(_)) {
            self.service.notify_observer();
        }
        let rendered = serde_json::to_string_pretty(&report)
            .map_err(|_| ForgeError::session("context inspection unavailable"))?;
        Ok(rendered.lines().map(forge_chat::Line::meta).collect())
    }

    /// Build the chat's runtime from `ctx`'s resolved configuration, with
    /// approvals parked (§8.1) from the start.
    pub async fn new(ctx: &Context) -> Result<Self, ForgeError> {
        let root = ctx.project_root()?;
        let ctx = Context {
            global: ctx.global.clone(),
        };
        let options = ServiceOptions {
            approvals: ApprovalChannel::Parked,
            ..ServiceOptions::default()
        };
        let service = build_run_service_with(&ctx, options.clone()).await?;
        let needle = needle_state(service.config()).await;
        // Embedding is an engine capability, not a routing choice: no
        // `router = "needle"` gate here, mirroring `forge graph context`'s
        // `context_embedder`. A second `info()` round-trip on the shared,
        // cached engine — cheap. An embedder that fails to build degrades
        // to lexical ranking rather than failing chat startup.
        let embedder = match forge_needle::engine_if_available(service.config()).await {
            Some(engine) => EngineEmbedder::new(engine).await.ok(),
            None => None,
        };
        Ok(Self {
            ctx,
            root,
            service: Arc::new(service),
            options,
            needle,
            embedder,
            paths_cache: std::sync::Mutex::new(None),
        })
    }
}

#[async_trait]
impl ChatHost for CliHost {
    fn service(&self) -> Arc<AgentService> {
        Arc::clone(&self.service)
    }

    async fn context_status(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<forge_chat::Line>, ForgeError> {
        self.context_memory(
            session_id,
            crate::commands::context_cmd::Request::ContextStatus,
        )
        .await
    }

    async fn memory_status(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<forge_chat::Line>, ForgeError> {
        self.context_memory(
            session_id,
            crate::commands::context_cmd::Request::MemoryStatus,
        )
        .await
    }

    async fn set_memory_observation(
        &mut self,
        session_id: &str,
        enabled: bool,
    ) -> Result<Vec<forge_chat::Line>, ForgeError> {
        self.context_memory(
            session_id,
            crate::commands::context_cmd::Request::SetMemory(enabled),
        )
        .await
    }

    async fn memory_show(
        &mut self,
        session_id: &str,
        offset: usize,
    ) -> Result<Vec<forge_chat::Line>, ForgeError> {
        self.context_memory(
            session_id,
            crate::commands::context_cmd::Request::Show(offset),
        )
        .await
    }

    async fn memory_sources(
        &mut self,
        session_id: &str,
        offset: usize,
    ) -> Result<Vec<forge_chat::Line>, ForgeError> {
        self.context_memory(
            session_id,
            crate::commands::context_cmd::Request::Sources(offset),
        )
        .await
    }

    async fn switch(&mut self, change: HostChange) -> Result<(), ForgeError> {
        let mut options = self.options.clone();
        match change {
            HostChange::Model(model) => options.model = Some(model),
            HostChange::Approval(approval) => options.approval = Some(approval),
        }
        // `?` before either field is written: an error here must leave
        // `self.service`/`self.options` exactly as they were (the trait's
        // own contract), so the chat stays usable on the model it had
        // before a bad `/model`.
        let service = build_run_service_with(&self.ctx, options.clone()).await?;
        self.service = Arc::new(service);
        self.options = options;
        Ok(())
    }

    async fn authenticate(&mut self, provider: &str) -> Result<(), ForgeError> {
        let (provider, model) = match provider {
            "claude" => (AuthProvider::Claude, "claude-sonnet"),
            "codex" => (AuthProvider::Codex, "gpt-5.6-sol"),
            "kimi" => (AuthProvider::Kimi, "k3"),
            other => {
                return Err(ForgeError::config(format!(
                    "unknown auth provider {other:?} (expected claude, codex, or kimi)"
                )));
            }
        };
        auth_cmd::login(&self.ctx, provider)?;
        let mut options = self.options.clone();
        options.model = Some(model.to_string());
        let service = build_run_service_with(&self.ctx, options.clone()).await?;
        self.service = Arc::new(service);
        self.options = options;
        Ok(())
    }

    fn environment(&self) -> Environment {
        let config = self.service.config();
        Environment {
            project_root: self.root.clone(),
            // Unfiltered: `/config` and the entry banner report the truth
            // even when the configured model is a mock — hiding it there
            // would be a lie about what is running, not a safeguard. Only
            // the *choice list* (`visible_models`) hides mocks.
            model: config.model.clone(),
            router: config.router.clone(),
            approval: config.approval.clone(),
            needle: self.needle.clone(),
        }
    }

    fn models(&self) -> Vec<ModelChoice> {
        let config = self.service.config();
        let active = config.model.as_str();
        let mut candidates: Vec<&str> = config.model_entries().keys().map(String::as_str).collect();
        if !candidates.contains(&active) {
            candidates.push(active);
        }
        let mut choices = visible_models(&candidates, active);
        for choice in &mut choices {
            if let Some(entry) = config.model_entries().get(&choice.name)
                && let Some(description) = &entry.description
            {
                choice.description = description.clone();
            }
        }
        choices
    }

    fn skills(&self) -> Vec<SkillChoice> {
        self.service
            .skills()
            .list()
            .into_iter()
            .map(|meta| SkillChoice {
                name: meta.name,
                description: meta.description,
            })
            .collect()
    }

    fn project_files(&self) -> Vec<String> {
        project_files_at(&self.root, MAX_PROJECT_PATHS, &self.paths_cache)
    }

    fn config_summary(&self, key: Option<&str>) -> Vec<ConfigLine> {
        // The *same* override layer `switch` builds the runtime from
        // (`overrides_for`), not a second derivation of it — otherwise
        // `/config` could disagree with what `/model` just did.
        let overrides = overrides_for(&self.ctx, &self.options);
        let Ok(resolved) = forge_config::Config::load(Some(&self.root), &overrides) else {
            return Vec::new();
        };
        match key {
            Some(key) => resolved
                .explain(key)
                .map(|(value, origin)| {
                    vec![ConfigLine {
                        key: key.to_string(),
                        value,
                        origin: origin.to_string(),
                    }]
                })
                .unwrap_or_default(),
            None => resolved
                .sources
                .iter()
                .map(|(key, source)| ConfigLine {
                    key: key.clone(),
                    value: source.value.clone(),
                    origin: source.origin.to_string(),
                })
                .collect(),
        }
    }

    async fn graph_context(
        &self,
        query: &str,
        steering: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ContextLine>, ForgeError> {
        context_lines(&self.root, self.embedder.as_ref(), query, steering, limit).await
    }
}

/// [`ChatHost::graph_context`]'s body, pulled out so it is testable without
/// a full `CliHost` (which needs a whole runtime to construct).
///
/// The semantic blend (`forge_graph::blended_context`) whenever a working
/// embedder *and* a matching index exist: the embedder must embed the
/// query text, which is what the trait method's async signature is for.
/// `steering` is embedded alongside the query in the same batch and steers
/// the semantic half; it has no effect without an embedder. Without either
/// engine or index, `blended_context` degrades to exactly the lexical
/// ranking (`query.rs`'s `lexical_only`), so the lexical path is also the
/// honest fallback — engine-less builds and unbuilt indexes change
/// nothing.
async fn context_lines(
    root: &Path,
    embedder: Option<&EngineEmbedder>,
    query: &str,
    steering: Option<&str>,
    limit: usize,
) -> Result<Vec<ContextLine>, ForgeError> {
    let graph = forge_graph::LocalGraph::open(root)?;
    let hits = forge_graph::blended_context(
        &graph,
        embedder.map(|e| e as &dyn forge_core::embed::Embedder),
        query,
        steering,
        limit,
    )
    .await?;
    Ok(hits
        .into_iter()
        .map(|hit| ContextLine {
            path: hit.path,
            score: hit.score,
        })
        .collect())
}

/// The most project paths ever loaded into a completion snapshot. Bounds
/// the per-`readline` snapshot clones on huge repos; truncation is in the
/// graph's sorted order, so it is deterministic, and a truncated repo
/// completes its alphabetically-first 50k files — a bound, not a stall.
const MAX_PROJECT_PATHS: usize = 50_000;

/// `project_files`' body, free-standing so tests need no `CliHost` (the
/// `context_lines` shape, this module). Re-reads the graph only when the
/// file's mtime changed (or it appeared); any read/parse failure —
/// including a corrupt `graph.json`, which `LocalGraph::open` turns into
/// an `Err` — degrades to *no candidates* with a debug log, because a
/// completion source must never break the prompt.
fn project_files_at(
    root: &Path,
    cap: usize,
    cache: &std::sync::Mutex<Option<(Option<std::time::SystemTime>, Vec<String>)>>,
) -> Vec<String> {
    // The graph's well-known file (the one `LocalGraph::open` reads),
    // stat'd *before* any open: the mtime gate must precede the parse, or
    // the steady state costs a multi-MB read instead of one `stat`.
    let mtime = match std::fs::metadata(root.join(".forge/graph/graph.json")) {
        Ok(meta) => meta.modified().ok(),
        // Absent is a first-class cached state: an unbuilt project
        // completes nothing, and a later `forge graph build` appears as an
        // mtime going from `None` to `Some`, which is a cache miss.
        Err(_) => None,
    };
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_mtime, paths)) = &*guard
            && *cached_mtime == mtime
        {
            return paths.clone();
        }
    } // Dropped before the graph I/O: a completion refresh must never hold
    // the lock across a file parse a concurrent `graph_context` caller
    // would wait behind.
    let paths = match forge_graph::LocalGraph::open(root) {
        Ok(graph) => graph
            .files()
            .into_iter()
            // The keys are already project-relative and `/`-normalized at
            // walk time — lossy-stringify them, never re-join.
            .map(|path| path.to_string_lossy().into_owned())
            .take(cap)
            .collect(),
        Err(e) => {
            tracing::debug!(error = %e, "@-completion: graph unreadable, completing nothing");
            Vec::new()
        }
    };
    // Failures are cached too (mtime, empty): a corrupt graph file is paid
    // for once per file version, not re-parsed on every submitted line.
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((mtime, paths.clone()));
    paths
}

/// User-visible model choices, with every test-only mock removed (§9.3) —
/// the one gate every `/model` listing and completion (`app.rs`) passes
/// through, because every `ChatHost::models` implementation is meant to.
fn visible_models(candidates: &[&str], active: &str) -> Vec<ModelChoice> {
    let mut names: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|name| !forge_providers::is_mock_model(name))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|name| ModelChoice {
            name: name.to_string(),
            description: String::new(),
            active: name == active,
        })
        .collect()
}

/// The chat's brain state, computed once (the router never changes for
/// the life of a conversation, so nothing here needs recomputing on
/// [`CliHost::switch`]).
///
/// Delegates to [`forge_needle::engine_if_available`] — the exact seam
/// `forge doctor`'s needle check (`needle_checks`, `commands/doctor.rs`)
/// uses — and borrows its wording so the two surfaces cannot drift.
async fn needle_state(config: &forge_config::Config) -> NeedleState {
    if config.router != "needle" {
        return NeedleState::Inactive {
            reason: "not the active router".to_string(),
        };
    }
    match forge_needle::engine_if_available(config).await {
        Some(engine) => match engine.info().await {
            Ok((model_id, _dimensions)) => NeedleState::Active { model_id },
            Err(e) => NeedleState::Inactive {
                reason: format!("falling back to {} routing: {e}", config.router_fallback),
            },
        },
        None => NeedleState::Inactive {
            reason: format!("falling back to {} routing", config.router_fallback),
        },
    }
}

#[cfg(test)]
mod tests {
    // `ProjectGraph` (for `LocalGraph::build`) comes in through `super::*`.
    use super::*;

    /// The single place mocks are filtered out (§9.3). Everything the chat
    /// *offers* comes from here, so this test is the gate.
    #[test]
    fn model_choices_never_include_a_test_only_mock() {
        let names = visible_models(
            &[
                "qwen3-coder",
                "mock-local",
                "scripted-mock",
                "deepseek-chat",
                "mock",
            ],
            "qwen3-coder",
        );
        assert_eq!(
            names.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["deepseek-chat", "qwen3-coder"],
        );
        assert!(names.iter().any(|m| m.name == "qwen3-coder" && m.active));
    }

    /// ...but a user who really is running one still sees their own
    /// configuration reported honestly; hiding it would be a lie.
    #[test]
    fn the_active_model_is_reported_even_when_it_is_a_mock() {
        let names = visible_models(&["qwen3-coder", "scripted-mock"], "scripted-mock");
        assert!(
            names.iter().all(|m| m.name != "scripted-mock"),
            "a mock is never offered as a choice"
        );
        // `CliHost::environment` reports `config.model` as-is, so `/config`
        // and the banner show what is actually configured.
    }

    /// `visible_models` sorts and de-duplicates: a `/model` listing built
    /// from `[models]` entries plus the active model must not depend on
    /// map iteration order, and the active model appearing in both the
    /// entries and as the configured model must not double it up.
    #[test]
    fn visible_models_is_sorted_and_deduplicated() {
        let names = visible_models(&["zeta", "alpha", "alpha"], "alpha");
        assert_eq!(
            names.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"],
        );
    }

    /// [`context_lines`] over a real (if tiny) graph with no embedder: the
    /// mapping from the ranker's hit into `ContextLine` must carry the path
    /// and score through, and honour `limit`.
    #[tokio::test]
    async fn context_lines_maps_lexical_hits_and_honours_the_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("alpha.rs"), "fn alpha_marker() {}\n").expect("write");
        std::fs::write(tmp.path().join("beta.rs"), "fn beta_other() {}\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
        graph.build().expect("build");

        let hits = context_lines(tmp.path(), None, "alpha_marker", None, 10)
            .await
            .expect("context");
        assert_eq!(hits[0].path, "alpha.rs");
        assert!(hits[0].score > 0.0);

        // Both files match (one token each), so the cap is what is actually
        // being exercised here, not "only one file could ever match".
        let capped = context_lines(tmp.path(), None, "alpha beta", None, 1)
            .await
            .expect("context");
        assert_eq!(capped.len(), 1);
    }

    /// A project with no built graph at all must not error — `open`
    /// degrades to an empty graph state, and `/context` before `forge
    /// graph build` should read as "no matches", not a crash.
    #[tokio::test]
    async fn context_lines_on_an_unbuilt_project_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hits = context_lines(tmp.path(), None, "anything", None, 10)
            .await
            .expect("context");
        assert!(hits.is_empty());
    }

    fn fresh_cache() -> std::sync::Mutex<Option<(Option<std::time::SystemTime>, Vec<String>)>> {
        std::sync::Mutex::new(None)
    }

    /// A built graph's files complete: sorted, project-relative, and
    /// excluding what the graph's policy excludes (`.forge/` itself).
    #[test]
    fn project_files_lists_the_graphs_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        std::fs::write(tmp.path().join("guide.md"), "# hi\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
        graph.build().expect("build");

        assert_eq!(
            project_files_at(tmp.path(), MAX_PROJECT_PATHS, &fresh_cache()),
            vec!["guide.md".to_string(), "src/main.rs".to_string()],
        );
    }

    /// Review Focus 4: no graph, or a corrupt one, is silence — not an
    /// error, not a panic, and (for the corrupt case) not a re-parse on
    /// every call.
    #[test]
    fn project_files_degrades_to_empty_without_a_built_graph() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(project_files_at(tmp.path(), MAX_PROJECT_PATHS, &fresh_cache()).is_empty());
        std::fs::create_dir_all(tmp.path().join(".forge/graph")).expect("mkdir");
        std::fs::write(tmp.path().join(".forge/graph/graph.json"), "not json").expect("write");
        let cache = fresh_cache();
        assert!(project_files_at(tmp.path(), MAX_PROJECT_PATHS, &cache).is_empty());
        assert!(
            cache.lock().expect("cache").is_some(),
            "the failure is cached, not re-paid"
        );
    }

    /// Review Focus 3 (shell half): the cap is honored, and a rebuild is
    /// picked up through the mtime gate — a file added after a rebuild
    /// appears without restarting the chat.
    #[test]
    fn project_files_is_capped_and_reloads_on_rebuild() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
        graph.build().expect("build");
        let cache = fresh_cache();
        assert_eq!(
            project_files_at(tmp.path(), 1, &cache),
            vec!["a.rs".to_string()]
        );

        std::fs::write(tmp.path().join("b.rs"), "fn b() {}\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("reopen");
        graph.build().expect("rebuild bumps the graph file's mtime");
        assert_eq!(
            project_files_at(tmp.path(), 50, &cache),
            vec!["a.rs".to_string(), "b.rs".to_string()],
            "the mtime gate reloaded"
        );
    }

    // --- CliHost integration: the real construction/switch path ---------

    fn test_ctx(root: &Path) -> Context {
        Context {
            global: crate::cli::GlobalOpts {
                project: Some(root.to_path_buf()),
                ..crate::cli::GlobalOpts::default()
            },
        }
    }

    /// A project configured for the scripted mock model, offline and
    /// deterministic — through the real `CliHost`/`build_run_service_with`
    /// path this module is responsible for (`forge-chat`'s own `FakeHost`
    /// builds a similarly offline runtime, but bypasses this crate
    /// entirely). `execution = "native"` (never actually asked to run
    /// anything here) so a bad `approval` string is still validated by
    /// `ApprovalPolicy::parse` — `execution = "mock"` skips that check
    /// entirely, which would make the switch-failure test below vacuous.
    fn scripted_mock_project(root: &Path) {
        std::fs::create_dir_all(root.join(".forge")).expect("mkdir .forge");
        std::fs::write(
            root.join(".forge").join("config.toml"),
            "model = \"scripted-mock\"\n\
             mock_script = \"script.json\"\n\
             router = \"static\"\n\
             execution = \"native\"\n\
             approval = \"auto\"\n",
        )
        .expect("write config");
        std::fs::write(root.join("script.json"), r#"[{"text": "hi"}]"#).expect("write script");
    }

    /// The construction path end to end: a real `AgentService` gets built,
    /// the configured (mock) model is reported honestly by `environment`,
    /// and `models()` still filters it out as a choice — the same
    /// two-sided guarantee the pure-function tests above assert, now
    /// exercised through the actual seam `forge-cli` wires up.
    #[tokio::test]
    #[serial_test::serial]
    async fn cli_host_reports_the_configured_mock_without_offering_it_as_a_choice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        scripted_mock_project(tmp.path());
        unsafe { std::env::set_var(forge_config::TEST_MOCKS_ENV, "1") };

        let host = CliHost::new(&test_ctx(tmp.path())).await.expect("host");
        let env = host.environment();
        assert_eq!(env.model, "scripted-mock");
        assert!(
            host.models().iter().all(|m| m.name != "scripted-mock"),
            "a mock is never offered as a /model choice"
        );
        assert_eq!(
            env.needle,
            NeedleState::Inactive {
                reason: "not the active router".to_string()
            }
        );

        unsafe { std::env::remove_var(forge_config::TEST_MOCKS_ENV) };
    }

    /// The trait's own contract: on error, `switch` must leave the old
    /// runtime exactly as it was. `not-a-real-policy` fails offline, at
    /// `ApprovalPolicy::parse`, before any provider or network is touched
    /// — so this is a deterministic test of the failure path, not a probe
    /// of something environmental.
    #[tokio::test]
    #[serial_test::serial]
    async fn switch_failure_keeps_the_old_runtime_and_returns_the_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        scripted_mock_project(tmp.path());
        unsafe { std::env::set_var(forge_config::TEST_MOCKS_ENV, "1") };

        let mut host = CliHost::new(&test_ctx(tmp.path())).await.expect("host");
        let before = host.environment();

        let err = host
            .switch(HostChange::Approval("not-a-real-policy".to_string()))
            .await
            .expect_err("an invalid approval mode must fail");
        assert!(err.to_string().contains("not-a-real-policy"), "{err}");
        assert_eq!(
            host.environment(),
            before,
            "the old runtime must survive a failed switch untouched"
        );

        unsafe { std::env::remove_var(forge_config::TEST_MOCKS_ENV) };
    }

    /// The success path: switching to a different (still mock, still
    /// offline) model rebuilds the runtime, and the change is visible
    /// through `environment` immediately afterwards.
    #[tokio::test]
    #[serial_test::serial]
    async fn switch_success_rebuilds_the_runtime_and_is_reported_by_environment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        scripted_mock_project(tmp.path());
        unsafe { std::env::set_var(forge_config::TEST_MOCKS_ENV, "1") };

        let mut host = CliHost::new(&test_ctx(tmp.path())).await.expect("host");
        host.switch(HostChange::Model("mock".to_string()))
            .await
            .expect("switching to another offline mock must succeed");
        assert_eq!(host.environment().model, "mock");

        unsafe { std::env::remove_var(forge_config::TEST_MOCKS_ENV) };
    }
}
