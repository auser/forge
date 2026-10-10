use std::path::Path;
use std::sync::Arc;

use forge_config::CliOverrides;
use forge_core::{ApprovalPolicy, ExecutionProvider, ForgeError};
use forge_execution::{ApprovalChannel, MockExecution, NativeExecution};
use forge_runtime::AgentService;
use forge_session::JsonlSessionStore;
use forge_skills::FsSkillRegistry;

use crate::commands::Context;

/// Build the execution provider from configuration. The project root is
/// used for file-op risk classification.
///
/// `mock` is a **test-only** provider and gated like the mock models and
/// router (`forge_config::test_mocks`). It deserves the gate more than they
/// do: `MockExecution` *reports* commands as run and files as written while
/// doing neither, so a user who selected it would watch forge narrate work
/// that never happened. Tests construct `MockExecution` directly, which is
/// unaffected — the gate is on what configuration may select.
pub fn build_execution(
    config: &forge_config::Config,
    project_root: &Path,
) -> Result<Arc<dyn ExecutionProvider>, ForgeError> {
    build_execution_with(config, project_root, ApprovalChannel::default())
}

/// [`build_execution`] with an explicit approval channel: a front end that
/// owns stdin itself (the interactive chat) passes
/// [`ApprovalChannel::Parked`] so a risky operation pauses the run instead
/// of reading stdin behind the line editor's back.
fn build_execution_with(
    config: &forge_config::Config,
    project_root: &Path,
    approvals: ApprovalChannel,
) -> Result<Arc<dyn ExecutionProvider>, ForgeError> {
    match config.execution.as_str() {
        "native" => Ok(Arc::new(NativeExecution::with_channel(
            ApprovalPolicy::parse(&config.approval)?,
            project_root,
            approvals,
        ))),
        "mock" => {
            forge_config::ensure_test_mocks_allowed("execution = \"mock\"")?;
            Ok(Arc::new(MockExecution::new(project_root)))
        }
        other => Err(ForgeError::execution(format!(
            "unknown execution provider {other:?} (expected native)"
        ))),
    }
}

/// [`build_service`] plus the on-device Needle brain, which enables the
/// direct-dispatch fast path for fresh prompts (`AgentService::with_needle`).
///
/// The engine is attached only when it can actually answer — that check is
/// `forge_needle::engine_if_available`, the same seam `forge graph build`
/// uses, so env `FORGE_NEEDLE_BACKEND=hash` and the `[needle]` config are
/// honoured in exactly one place. Unavailable (engine-less build, weights not
/// fetched) yields `None` and the plain agent loop.
///
/// Commands that never start a fresh prompt (`resume`, `cancel`, session
/// inspection) use plain [`build_service`]: the fast path cannot apply to
/// them, so probing the engine would only cost them latency.
pub async fn build_run_service(ctx: &Context) -> Result<AgentService, ForgeError> {
    build_run_service_with(ctx, ServiceOptions::default()).await
}

/// [`build_run_service`] with explicit [`ServiceOptions`].
pub async fn build_run_service_with(
    ctx: &Context,
    options: ServiceOptions,
) -> Result<AgentService, ForgeError> {
    let service = build_service_with(ctx, options)?;
    let engine = forge_needle::engine_if_available(service.config()).await;
    Ok(service.with_needle(engine))
}

/// Non-default choices a front end makes about its runtime.
///
/// `model`/`approval` hold the same strings the `--model`/`--approval`
/// flags take and are applied into `CliOverrides` before `Config::load`,
/// so the chat's `/model` and `/approval` go through the *one* override
/// path rather than a second one that could resolve differently.
#[derive(Debug, Clone, Default)]
pub struct ServiceOptions {
    /// How a risky operation asks. Default: today's behaviour.
    pub approvals: ApprovalChannel,
    pub model: Option<String>,
    pub approval: Option<String>,
}

/// The flags' own override layer with a front end's choices applied on top.
/// One function, so there is exactly one answer to "what did this runtime
/// resolve from" for `forge config show` and the chat alike.
///
/// `pub(crate)`, not private: `chat::host::CliHost::config_summary` (§9)
/// calls this directly to resolve `/config` against the *current*
/// session overrides (post-`/model`/`/approval`), rather than re-deriving
/// the override chain a second time and risking it disagreeing with this
/// one.
pub(crate) fn overrides_for(ctx: &Context, options: &ServiceOptions) -> CliOverrides {
    let mut overrides = ctx.cli_overrides();
    if let Some(model) = &options.model {
        overrides.model = Some(model.clone());
    }
    if let Some(approval) = &options.approval {
        overrides.approval = Some(approval.clone());
    }
    overrides
}

fn is_zero_config_model_selection(config: &forge_config::Config) -> bool {
    !config.explicit.contains("model")
        && !config.explicit.iter().any(|key| key.starts_with("models."))
        && (!config.explicit.contains("router") || config.router == "needle")
}

/// Build the transport-neutral agent runtime from the resolved
/// configuration: model provider, decision router (with fallback),
/// execution provider, filesystem skill registry, and the JSONL session
/// store.
pub fn build_service(ctx: &Context) -> Result<AgentService, ForgeError> {
    build_service_with(ctx, ServiceOptions::default())
}

/// [`build_service`] with explicit [`ServiceOptions`]. `ServiceOptions::default()`
/// *is* [`build_service`], so no existing call site changes.
pub fn build_service_with(
    ctx: &Context,
    options: ServiceOptions,
) -> Result<AgentService, ForgeError> {
    let root = ctx.project_root()?;
    let resolved = forge_config::Config::load(Some(&root), &overrides_for(ctx, &options))?;
    let mut config = resolved.config.clone();

    // A model named by a real config layer is a deliberate choice and must
    // never be silently replaced. With only built-in defaults, choose from
    // providers proven usable in this environment so a first run does not
    // assume a particular local server is already running.
    let auto_selected = !resolved.config.explicit.contains("model");
    let zero_config_selection = is_zero_config_model_selection(&resolved.config);
    if auto_selected {
        config.model = forge_providers::automatic_model(&config)
            .unwrap_or_else(|| forge_providers::AUTH_REQUIRED_MODEL.to_string());
    }

    // One availability resolution feeds both routing and provider creation.
    // Keep an explicitly selected model so its construction can return the
    // precise credential/endpoint error the user asked for.
    let available: std::collections::BTreeSet<String> = forge_providers::available_models(&config)
        .into_iter()
        .map(|model| model.name)
        .collect();
    let selected_model = config.model.clone();
    if zero_config_selection {
        // Zero-config selection is the decision: do not immediately ask the
        // router to reconsider every other detected subscription. Besides
        // making first-run behavior deterministic, this leaves one eligible
        // model, so model routing itself is unnecessary. Needle remains
        // attached below for its direct tool-decision fast path; using static
        // here only avoids asking it to choose between one model.
        config.models.retain(|name, _| name == &selected_model);
    } else {
        config
            .models
            .retain(|name, _| available.contains(name) || name == &selected_model);
    }

    let model = forge_providers::model_from_config(&config, &root)?;
    // Routing registry: `[models]` entries that declare capabilities are
    // known; the rest stay optimistic-unknown.
    let mut registry: Vec<(String, forge_core::ModelCapabilities)> = config
        .model_entries()
        .iter()
        .filter_map(|(name, entry)| entry.capabilities_if_known().map(|c| (name.clone(), c)))
        .collect();
    if let Some((_, capabilities)) = registry.iter_mut().find(|(name, _)| name == model.name()) {
        *capabilities = model.capabilities();
    } else {
        registry.push((model.name().to_string(), model.capabilities()));
    }
    let mut router_config = config.clone();
    if zero_config_selection {
        router_config.router = "static".to_string();
    }
    let router = forge_providers::router_from_config(&router_config, &registry)?;

    let execution = build_execution_with(&config, &root, options.approvals)?;
    let skills = Arc::new(FsSkillRegistry::new(&root, Some(execution.clone())));

    let sessions = Arc::new(JsonlSessionStore::new(root.join(".forge").join("sessions")));
    let context_store = Arc::new(forge_context::FsContextStore::new(
        root.join(".forge").join("context"),
    ));
    let artifact_store = Arc::new(forge_context::FsArtifactStore::new(
        root.join(".forge").join("context"),
        forge_context::ArtifactLimits {
            max_project_bytes: config.context_artifacts.max_project_bytes,
            max_age_secs: config.context_artifacts.max_age_secs,
            max_artifact_bytes: config.context_artifacts.max_artifact_bytes,
        },
    ));

    // Wire the project graph when one has been built; absence never
    // blocks a run.
    let graph = forge_graph::LocalGraph::open(&root)
        .ok()
        .filter(|g| g.graph_file().is_file())
        .map(|g| Arc::new(g) as Arc<dyn forge_core::ProjectGraph>);

    // Resolve a provider per routed model name: `[models]` entries supply
    // endpoint/key/capability overrides; anything else falls back to the
    // global model_* settings or the built-in mock.
    let cfg = config.clone();
    let root_for_factory = root.clone();
    let factory = move |name: &str| -> Result<Arc<dyn forge_core::ModelProvider>, ForgeError> {
        let mut cfg = cfg.clone();
        cfg.model = name.to_string();
        forge_providers::model_from_config(&cfg, &root_for_factory)
    };
    let compression_enabled = config.context_compression.enabled;
    let observer = match super::observer_cmd::build(&config, &root, sessions.clone()) {
        Ok(observer) => observer,
        Err(_) => {
            // Background observation setup must not abort ordinary conversation
            // or leak provider diagnostics into logs.
            tracing::warn!("observer unavailable under configured policy");
            None
        }
    };

    Ok(
        AgentService::new(model, router, execution, skills, sessions, config)
            .with_task_store(Arc::new(forge_task::JsonlTaskStore::for_project(&root)))
            .with_graph(graph)
            .with_context_store(Some(context_store))
            .with_artifact_store(Some(artifact_store))
            .with_compression_enabled(compression_enabled)
            .with_observer(observer)
            .with_system_context(crate::commands::guidance::system_context(&root))
            .with_model_factory(Arc::new(factory)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalOpts;

    #[test]
    fn configured_artifact_defaults_match_storage_defaults() {
        let config = forge_config::Config::default().context_artifacts;
        let storage = forge_context::ArtifactLimits::default();
        assert_eq!(config.max_project_bytes, storage.max_project_bytes);
        assert_eq!(config.max_age_secs, storage.max_age_secs);
        assert_eq!(config.max_artifact_bytes, storage.max_artifact_bytes);
    }

    /// The default must *be* today's runtime, because every existing call
    /// site reaches `build_service_with` through it.
    #[test]
    fn service_options_default_is_todays_inline_channel() {
        let options = ServiceOptions::default();
        assert_eq!(
            options.approvals,
            forge_execution::ApprovalChannel::InlineTty
        );
        assert_eq!(options.model, None);
        assert_eq!(options.approval, None);
    }

    /// `/model` and `/approval` must ride the *same* override layer the
    /// `--model`/`--approval` flags do, not a second resolution path that
    /// could disagree with `forge config show`.
    #[test]
    fn model_and_approval_options_ride_the_one_override_path() {
        let ctx = Context {
            global: GlobalOpts {
                model: Some("from-flag".to_string()),
                router: Some("static".to_string()),
                ..GlobalOpts::default()
            },
        };

        // Nothing chosen: exactly the flags' own overrides.
        let untouched = overrides_for(&ctx, &ServiceOptions::default());
        assert_eq!(untouched.model.as_deref(), Some("from-flag"));
        assert_eq!(untouched.approval, None);
        assert_eq!(untouched.router.as_deref(), Some("static"));

        // Chosen in the front end: layered over the flags, everything else
        // still the flags'.
        let chosen = overrides_for(
            &ctx,
            &ServiceOptions {
                model: Some("from-chat".to_string()),
                approval: Some("deny".to_string()),
                ..ServiceOptions::default()
            },
        );
        assert_eq!(chosen.model.as_deref(), Some("from-chat"));
        assert_eq!(chosen.approval.as_deref(), Some("deny"));
        assert_eq!(chosen.router.as_deref(), Some("static"));
    }

    #[test]
    fn only_an_unconfigured_model_registry_is_narrowed_after_auto_selection() {
        assert!(is_zero_config_model_selection(
            &forge_config::Config::default()
        ));
        assert!(is_zero_config_model_selection(
            &forge_config::Config::default().with_explicit(["router"])
        ));
        assert!(!is_zero_config_model_selection(
            &forge_config::Config {
                router: "cheapest".to_string(),
                ..forge_config::Config::default()
            }
            .with_explicit(["router"])
        ));
        assert!(!is_zero_config_model_selection(
            &forge_config::Config::default().with_explicit(["models.a-expensive"])
        ));
        assert!(!is_zero_config_model_selection(
            &forge_config::Config::default().with_explicit(["model"])
        ));
        assert!(is_zero_config_model_selection(
            &forge_config::Config::default().with_explicit(["approval"])
        ));
    }

    #[test]
    fn generated_starter_router_still_uses_zero_config_model_selection() {
        let project = tempfile::tempdir().expect("project");
        std::fs::create_dir_all(project.path().join(".forge")).expect("forge dir");
        std::fs::write(
            project.path().join(".forge/config.toml"),
            "router = \"needle\"\nexecution = \"native\"\napproval = \"prompt-dangerous\"\n",
        )
        .expect("starter config");
        let resolved = forge_config::Config::load(
            Some(project.path()),
            &forge_config::CliOverrides::default(),
        )
        .expect("config");
        assert!(resolved.config.explicit.contains("router"));
        assert!(is_zero_config_model_selection(&resolved.config));
    }
}
