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
use forge_core::{ForgeError, ProjectGraph};
use forge_execution::ApprovalChannel;
use forge_runtime::AgentService;

use crate::commands::Context;
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
}

impl CliHost {
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
        Ok(Self {
            ctx,
            root,
            service: Arc::new(service),
            options,
            needle,
        })
    }
}

#[async_trait]
impl ChatHost for CliHost {
    fn service(&self) -> Arc<AgentService> {
        Arc::clone(&self.service)
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

    fn environment(&self) -> Environment {
        let config = self.service.config();
        Environment {
            project_root: self.root.clone(),
            model: active_model_label(&config.model),
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

    fn graph_context(&self, query: &str, limit: usize) -> Result<Vec<ContextLine>, ForgeError> {
        context_lines(&self.root, query, limit)
    }
}

/// [`ChatHost::graph_context`]'s body, pulled out so it is testable without
/// a full `CliHost` (which needs a whole runtime to construct).
///
/// Deliberately the plain lexical ranking (`LocalGraph::context`), not
/// `forge_graph::query::blended_context`: the latter needs to *embed the
/// query text*, which is async, and `ChatHost::graph_context` is sync (a
/// deliberate asymmetry with `switch` in the trait itself — everything
/// here is meant to be plain owned data, cheaply available). Without a
/// working needle engine, `blended_context` degrades to exactly this
/// lexical ranking anyway (see `query.rs`'s `lexical_only`), so nothing
/// with a real embedder loses semantic blending that a sync call could
/// have honestly offered — it is the async-only half that is unreachable
/// from here.
fn context_lines(root: &Path, query: &str, limit: usize) -> Result<Vec<ContextLine>, ForgeError> {
    let graph = forge_graph::LocalGraph::open(root)?;
    Ok(graph
        .context(query, limit)
        .into_iter()
        .map(|hit| ContextLine {
            path: hit.path,
            score: hit.score,
        })
        .collect())
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

/// The model this conversation is actually configured with, unfiltered:
/// `/config` and the entry banner report the truth even when it is a mock
/// — hiding it there would be a lie about what is running, not a
/// safeguard. Only the *choice list* ([`visible_models`]) hides mocks.
fn active_model_label(active: &str) -> String {
    active.to_string()
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
        assert_eq!(
            active_model_label("scripted-mock"),
            "scripted-mock",
            "`/config` and the banner report what is actually configured"
        );
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

    /// [`context_lines`] over a real (if tiny) graph: the mapping from the
    /// graph's own `ContextHit` into `ContextLine` must carry the path and
    /// score through unchanged, and honour `limit`.
    #[test]
    fn context_lines_maps_lexical_hits_and_honours_the_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("alpha.rs"), "fn alpha_marker() {}\n").expect("write");
        std::fs::write(tmp.path().join("beta.rs"), "fn beta_other() {}\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
        graph.build().expect("build");

        let hits = context_lines(tmp.path(), "alpha_marker", 10).expect("context");
        assert_eq!(hits[0].path, "alpha.rs");
        assert!(hits[0].score > 0);

        // Both files match (one token each), so the cap is what is actually
        // being exercised here, not "only one file could ever match".
        let capped = context_lines(tmp.path(), "alpha beta", 1).expect("context");
        assert_eq!(capped.len(), 1);
    }

    /// A project with no built graph at all must not error — `open`
    /// degrades to an empty graph state, and `/context` before `forge
    /// graph build` should read as "no matches", not a crash.
    #[test]
    fn context_lines_on_an_unbuilt_project_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hits = context_lines(tmp.path(), "anything", 10).expect("context");
        assert!(hits.is_empty());
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
