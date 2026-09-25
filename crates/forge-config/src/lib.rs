//! Layered configuration for Forge.
//!
//! Precedence, lowest to highest: built-in defaults →
//! `~/.config/forge/config.toml` (honoring `XDG_CONFIG_HOME`) →
//! `<project>/.forge/config.toml` → `FORGE_*` environment variables →
//! CLI flag overrides. Every key records its winning value and origin so
//! `forge config explain <key>` can report provenance.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use forge_core::ForgeError;
use serde::{Deserialize, Serialize};

pub mod test_mocks;

pub use test_mocks::{TEST_MOCKS_ENV, ensure_test_mocks_allowed, test_mocks_allowed};

/// A `[models.<name>]` entry: cost metadata, optional endpoint, and
/// capability overrides. Cost is USD per million tokens; unset costs mean
/// free (0.0). Entries without any capability override are treated as
/// "unknown capabilities" (optimistic) by routers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelEntry {
    /// Free-text routing criteria (used by laya-style routers).
    pub description: Option<String>,
    pub cost_input_per_mtok: f64,
    pub cost_output_per_mtok: f64,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    /// Provider family for the endpoint: "openai" (default) or
    /// "anthropic". Explicit wins; otherwise inferred from base_url host.
    pub provider: Option<String>,
    /// Max tokens to generate per completion (default 8192).
    pub max_output_tokens: Option<u32>,
    pub tools: Option<bool>,
    pub streaming: Option<bool>,
    pub structured_output: Option<bool>,
    pub vision: Option<bool>,
    pub max_context: Option<usize>,
    /// Unknown keys inside this entry, kept so [`Config::validate`] can
    /// reject the field names that look right but silently do nothing (see
    /// [`WRONG_MODEL_ENTRY_KEYS`]).
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl ModelEntry {
    pub fn costs(&self) -> (f64, f64) {
        (self.cost_input_per_mtok, self.cost_output_per_mtok)
    }

    /// Capabilities when the entry declares at least one override
    /// (unset fields default to false then — declaring is explicit);
    /// `None` when nothing is declared (routers treat as optimistic).
    pub fn capabilities_if_known(&self) -> Option<forge_core::ModelCapabilities> {
        let declared = self.tools.is_some()
            || self.streaming.is_some()
            || self.structured_output.is_some()
            || self.vision.is_some()
            || self.max_context.is_some();
        if !declared {
            return None;
        }
        Some(forge_core::ModelCapabilities {
            tools: self.tools.unwrap_or(false),
            streaming: self.streaming.unwrap_or(false),
            structured_output: self.structured_output.unwrap_or(false),
            vision: self.vision.unwrap_or(false),
            max_context: self.max_context.unwrap_or(8_192),
        })
    }
}

/// `[needle]`: the embedded on-device Needle brain (weights variant, an
/// optional override path, and whether `forge init` may fetch weights).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NeedleConfig {
    /// Weights ladder: "small" (~8 MB), "medium", or "full" (default;
    /// ~35 MB; currently the only variant with a hosted, pinned artifact —
    /// see `forge-needle`'s `weights::VARIANTS`).
    pub variant: String,
    /// Override path to weights; empty means the default cache location
    /// (`~/.cache/forge/models/`).
    pub weights_path: String,
    /// Whether `forge init` downloads and verifies weights automatically.
    pub autofetch: bool,
    /// Operator-supplied SHA-256 override for the expected weights
    /// checksum; empty means use the compiled-in pin (`forge-needle`'s
    /// `weights::VARIANTS` table). Compiled-in pins are the default trust
    /// anchor — this lets an operator consciously supply their own weights
    /// (paired with `weights_path`/a custom base URL) without recompiling
    /// forge. Must be empty or exactly 64 hex characters.
    pub weights_sha256: String,
}

/// Weights ladder values accepted by `[needle].variant`.
const NEEDLE_VARIANTS: &[&str] = &["small", "medium", "full"];

/// Valid values for `router_escalate`.
const ROUTER_ESCALATE_VALUES: &[&str] = &["auto", "off"];

/// Top-level key names that are meaningless *inside* a `[models.<name>]`
/// entry, mapped to the field that was meant. Writing `model_base_url`
/// under `[models.foo]` parses fine and then silently does nothing — the
/// endpoint stays unset and every request goes somewhere else — so
/// [`Config::validate`] rejects it by name instead of letting it no-op.
const WRONG_MODEL_ENTRY_KEYS: &[(&str, &str)] =
    &[("model_base_url", "base_url"), ("model_key_env", "key_env")];

impl Default for NeedleConfig {
    fn default() -> Self {
        Self {
            // "full" is the only variant with a hosted, pinned artifact
            // today, so a fresh `forge init` actually fetches working
            // weights out of the box. Revert to a smaller rung once Cactus
            // hosts one (see the design spec's risks section).
            variant: "full".to_string(),
            weights_path: String::new(),
            autofetch: true,
            weights_sha256: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub model_base_url: Option<String>,
    pub model_key_env: Option<String>,
    /// **Test-only.** Path to a JSON script for `model = "scripted-mock"`
    /// (relative to the project root). Inert for every other model, and
    /// `scripted-mock` itself is refused unless `FORGE_TEST_MOCKS=1` — see
    /// [`test_mocks`].
    pub mock_script: Option<String>,
    pub router: String,
    pub router_url: Option<String>,
    pub router_key_env: Option<String>,
    /// When `router = "needle"`: escalate to the Jev tier
    /// (`forge_providers::JevRouter`) once needle declines/fails, iff
    /// `!local_only` and a Jev credential is present at build time. `"auto"`
    /// (default) or `"off"`.
    pub router_escalate: String,
    /// Jev endpoint, scoped separately from `router_url` so a leftover
    /// `router_url` from an unrelated `http`/`laya` setup can never be
    /// hijacked into carrying the Jev credential to the wrong host (or
    /// vice versa). Resolution: the escalation tier uses `jev_url` or the
    /// compiled-in default only — never `router_url`. `router = "jev"` as
    /// primary uses `jev_url`, then `router_url` (for backwards
    /// compatibility with how other routers already use the generic
    /// field), then the default.
    pub jev_url: Option<String>,
    /// Env var holding the Jev credential; same scoping rationale as
    /// `jev_url`. Escalation uses `jev_key_env` or `TYPESAFE_API_KEY` only;
    /// `router = "jev"` as primary also falls back to `router_key_env`.
    pub jev_key_env: Option<String>,
    pub router_timeout_ms: u64,
    pub execution: String,
    pub approval: String,
    pub local_only: bool,
    pub server_host: String,
    pub server_port: u16,
    /// Agent-loop turn budget.
    pub max_turns: u32,
    /// Reject http/laya routing decisions below this confidence.
    pub router_confidence_threshold: f64,
    /// Fallback router when the primary fails or is below threshold
    /// ("static" or "cheapest").
    pub router_fallback: String,
    /// When `router = "laya"`, `forge serve` auto-starts the local Laya
    /// adapter if the router endpoint is unreachable.
    pub router_autostart: bool,
    /// Named models with cost/capability metadata. Deep-merged by name
    /// across config files; not settable via env/CLI flags.
    pub models: BTreeMap<String, ModelEntry>,
    /// The embedded on-device Needle brain: config for the default
    /// `router = "needle"`. Weights resolution lands in a later phase;
    /// until then, routing built on it degrades to `router_fallback`.
    pub needle: NeedleConfig,
    /// Which keys a real configuration layer set, rather than the
    /// compiled-in defaults (see [`ExplicitKeys`]). Deliberately not part of
    /// the serialized configuration: it records *where* values came from,
    /// while the serialized form is the values themselves.
    #[serde(skip)]
    pub explicit: ExplicitKeys,
    /// Unknown keys are tolerated and preserved.
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// Config keys that a **real** layer set — a user or project config file, an
/// env var, or a CLI flag — as opposed to the compiled-in defaults.
///
/// Two decisions genuinely depend on *who* set a value rather than on what
/// the value is: whether the global `model_base_url` overrides a model
/// entry's own `base_url` (it should, but only if somebody actually asked
/// for it), and whether a `[models.<name>]` entry is a line in the user's
/// file or a built-in default — which changes what an error message can
/// honestly tell them to edit. Comparing a value against the default cannot
/// answer either question: setting a value that happens to equal the default
/// is a real choice, and forge used to discard it.
///
/// [`Config::load`] fills this in from the same source map `forge config
/// explain` reads. A `Config` built directly in code (tests, internal
/// clones) carries none, which reads as "nothing was explicitly configured";
/// [`Config::with_explicit`] is how such a caller says otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExplicitKeys(BTreeSet<String>);

impl ExplicitKeys {
    /// Whether `key` (e.g. `model_base_url`, `models.gpt-5`) was set by a
    /// real layer.
    pub fn contains(&self, key: &str) -> bool {
        self.0.contains(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Every key whose winning value came from something other than the
    /// compiled-in defaults.
    pub fn from_sources(sources: &BTreeMap<String, ConfigSource>) -> Self {
        Self(
            sources
                .iter()
                .filter(|(_, source)| source.origin != Origin::Default)
                .map(|(key, _)| key.clone())
                .collect(),
        )
    }
}

impl<S: Into<String>> FromIterator<S> for ExplicitKeys {
    fn from_iter<I: IntoIterator<Item = S>>(keys: I) -> Self {
        Self(keys.into_iter().map(Into::into).collect())
    }
}

/// Config key names that code has to reason about by name (see
/// [`ExplicitKeys`]), kept in one place so a rename cannot silently turn a
/// lookup into a no-op.
pub mod keys {
    pub const MODEL_BASE_URL: &str = "model_base_url";

    /// Source-map key for one `[models.<name>]` entry.
    pub fn model_entry(name: &str) -> String {
        format!("models.{name}")
    }
}

impl Default for Config {
    fn default() -> Self {
        // Default stack: embedded Needle 3 (on-device decision routing,
        // static fallback when weights are unavailable) → local oMLX
        // model. Laya (open-source System One) and other HTTP-style
        // routers remain available as alternates. The mock providers are
        // test-only and refused unless `FORGE_TEST_MOCKS=1` (see
        // `test_mocks`). Hosted models are only called when a router
        // selects them or the user sets `model` explicitly.
        let models = [
            ModelEntry {
                description: Some("local coding model via oMLX (Qwen3-Coder)".to_string()),
                cost_input_per_mtok: 0.0,
                cost_output_per_mtok: 0.0,
                base_url: Some("http://127.0.0.1:8080/v1".to_string()),
                key_env: None,
                provider: None,
                max_output_tokens: None,
                tools: Some(true),
                streaming: Some(true),
                structured_output: None,
                vision: None,
                max_context: Some(32_768),
                extra: toml::Table::new(),
            },
            ModelEntry {
                description: Some("DeepSeek V4-class chat/coding model, very low cost".to_string()),
                cost_input_per_mtok: 0.14,
                cost_output_per_mtok: 0.28,
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                key_env: Some("DEEPSEEK_API_KEY".to_string()),
                provider: None,
                max_output_tokens: None,
                tools: Some(true),
                streaming: Some(true),
                structured_output: None,
                vision: None,
                max_context: Some(128_000),
                extra: toml::Table::new(),
            },
            ModelEntry {
                description: Some(
                    "Anthropic Claude (subscription via Claude Code, or ANTHROPIC_API_KEY)"
                        .to_string(),
                ),
                // Subscription-served: no per-token cost here.
                cost_input_per_mtok: 0.0,
                cost_output_per_mtok: 0.0,
                base_url: Some("https://api.anthropic.com".to_string()),
                key_env: Some("ANTHROPIC_API_KEY".to_string()),
                provider: Some("anthropic".to_string()),
                max_output_tokens: None,
                tools: Some(true),
                streaming: Some(false),
                structured_output: None,
                vision: None,
                max_context: Some(200_000),
                extra: toml::Table::new(),
            },
            ModelEntry {
                description: Some(
                    "OpenAI GPT-5 via API key (Codex CLI auth.json is detected)".to_string(),
                ),
                // Prices as of Sept 2026; check provider pages.
                cost_input_per_mtok: 1.25,
                cost_output_per_mtok: 10.0,
                base_url: Some("https://api.openai.com/v1".to_string()),
                key_env: Some("OPENAI_API_KEY".to_string()),
                provider: Some("openai".to_string()),
                max_output_tokens: None,
                tools: Some(true),
                streaming: Some(true),
                structured_output: None,
                vision: None,
                max_context: Some(256_000),
                extra: toml::Table::new(),
            },
            ModelEntry {
                description: Some("Moonshot Kimi K2.7 Code, frontier-quality coding".to_string()),
                cost_input_per_mtok: 0.95,
                cost_output_per_mtok: 4.00,
                base_url: Some("https://api.moonshot.ai/v1".to_string()),
                key_env: Some("MOONSHOT_API_KEY".to_string()),
                provider: None,
                max_output_tokens: None,
                tools: Some(true),
                streaming: Some(true),
                structured_output: None,
                vision: None,
                max_context: Some(256_000),
                extra: toml::Table::new(),
            },
        ];
        Self {
            model: "qwen3-coder".to_string(),
            model_base_url: Some("http://127.0.0.1:8080/v1".to_string()),
            model_key_env: None,
            mock_script: None,
            router: "needle".to_string(),
            router_url: None,
            router_key_env: None,
            router_escalate: "auto".to_string(),
            jev_url: None,
            jev_key_env: None,
            router_timeout_ms: 5_000,
            execution: "native".to_string(),
            approval: "prompt".to_string(),
            local_only: false,
            server_host: "127.0.0.1".to_string(),
            server_port: 7_341,
            max_turns: 25,
            router_confidence_threshold: 0.7,
            router_fallback: "static".to_string(),
            router_autostart: true,
            models: [
                ("qwen3-coder".to_string(), models[0].clone()),
                ("deepseek-chat".to_string(), models[1].clone()),
                ("claude-sonnet".to_string(), models[2].clone()),
                ("gpt-5".to_string(), models[3].clone()),
                ("kimi-k2.7-code".to_string(), models[4].clone()),
            ]
            .into_iter()
            .collect(),
            needle: NeedleConfig::default(),
            // Nothing here was explicitly configured — this *is* the
            // defaults layer.
            explicit: ExplicitKeys::default(),
            extra: toml::Table::new(),
        }
    }
}

/// Where a configuration value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    Default,
    UserFile,
    ProjectFile,
    Environment,
    CliFlag,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Default => "default",
            Self::UserFile => "user-file",
            Self::ProjectFile => "project-file",
            Self::Environment => "environment",
            Self::CliFlag => "cli-flag",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSource {
    pub value: String,
    pub origin: Origin,
}

/// CLI flag overrides; `None` means the flag was not given. `config_path`
/// points at an explicit `--config` file, layered after the project file.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub config_path: Option<PathBuf>,
    pub model: Option<String>,
    pub router: Option<String>,
    pub router_url: Option<String>,
    pub router_key_env: Option<String>,
    pub execution: Option<String>,
    pub approval: Option<String>,
    pub local_only: Option<bool>,
    pub server_host: Option<String>,
    pub server_port: Option<u16>,
    pub max_turns: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config: Config,
    pub sources: BTreeMap<String, ConfigSource>,
}

impl ResolvedConfig {
    /// Winning value and origin for a key such as `model`, if it is set
    /// anywhere in the chain.
    pub fn explain(&self, key: &str) -> Option<(String, Origin)> {
        self.sources.get(key).map(|s| (s.value.clone(), s.origin))
    }
}

const ENV_KEYS: &[(&str, &str)] = &[
    ("FORGE_MODEL", "model"),
    ("FORGE_MODEL_BASE_URL", "model_base_url"),
    ("FORGE_MODEL_KEY_ENV", "model_key_env"),
    ("FORGE_MOCK_SCRIPT", "mock_script"),
    ("FORGE_ROUTER", "router"),
    ("FORGE_ROUTER_URL", "router_url"),
    ("FORGE_ROUTER_KEY_ENV", "router_key_env"),
    ("FORGE_ROUTER_ESCALATE", "router_escalate"),
    ("FORGE_JEV_URL", "jev_url"),
    ("FORGE_JEV_KEY_ENV", "jev_key_env"),
    ("FORGE_EXECUTION", "execution"),
    ("FORGE_APPROVAL", "approval"),
    ("FORGE_LOCAL_ONLY", "local_only"),
    ("FORGE_SERVER_HOST", "server_host"),
    ("FORGE_SERVER_PORT", "server_port"),
    ("FORGE_MAX_TURNS", "max_turns"),
    (
        "FORGE_ROUTER_CONFIDENCE_THRESHOLD",
        "router_confidence_threshold",
    ),
    ("FORGE_ROUTER_FALLBACK", "router_fallback"),
    ("FORGE_ROUTER_AUTOSTART", "router_autostart"),
    ("FORGE_NEEDLE_VARIANT", "needle.variant"),
    ("FORGE_NEEDLE_AUTOFETCH", "needle.autofetch"),
    ("FORGE_NEEDLE_WEIGHTS_SHA256", "needle.weights_sha256"),
];

impl Config {
    /// User-level config path: `$XDG_CONFIG_HOME/forge/config.toml`, or
    /// `~/.config/forge/config.toml` when `XDG_CONFIG_HOME` is unset.
    pub fn user_config_path() -> PathBuf {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("forge").join("config.toml");
        }
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("forge")
            .join("config.toml")
    }

    /// All configured `[models]` entries (name → entry), used to build
    /// routing candidates and cost tables.
    pub fn model_entries(&self) -> &BTreeMap<String, ModelEntry> {
        &self.models
    }

    pub fn project_config_path(project_root: &Path) -> PathBuf {
        project_root.join(".forge").join("config.toml")
    }

    /// Cross-field and enum-like validation that TOML deserialization alone
    /// can't express. Called at the end of [`Config::load`] so every caller
    /// gets it for free.
    pub fn validate(&self) -> Result<(), ForgeError> {
        if !ROUTER_ESCALATE_VALUES.contains(&self.router_escalate.as_str()) {
            return Err(ForgeError::config(format!(
                "router_escalate must be one of {} (got {:?})",
                ROUTER_ESCALATE_VALUES.join(", "),
                self.router_escalate
            )));
        }
        if !NEEDLE_VARIANTS.contains(&self.needle.variant.as_str()) {
            return Err(ForgeError::config(format!(
                "needle.variant must be one of {} (got {:?})",
                NEEDLE_VARIANTS.join(", "),
                self.needle.variant
            )));
        }
        let sha = &self.needle.weights_sha256;
        if !sha.is_empty() && !(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err(ForgeError::config(format!(
                "needle.weights_sha256 must be empty or 64 hex characters (got {:?})",
                sha
            )));
        }
        for (name, entry) in &self.models {
            for (wrong, right) in WRONG_MODEL_ENTRY_KEYS {
                if entry.extra.contains_key(*wrong) {
                    return Err(ForgeError::config(format!(
                        "[models.{name}] has {wrong}, which does nothing inside a model entry \
                         (it is a top-level key); rename it to {right}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Load and merge all configuration layers in precedence order.
    pub fn load(
        project_root: Option<&Path>,
        cli_overrides: &CliOverrides,
    ) -> Result<ResolvedConfig, ForgeError> {
        let mut merged = toml::Table::new();
        let mut sources: BTreeMap<String, ConfigSource> = BTreeMap::new();

        let defaults_value = toml::Value::try_from(Config::default())
            .map_err(|e| ForgeError::config(format!("serializing defaults: {e}")))?;
        let toml::Value::Table(defaults) = defaults_value else {
            return Err(ForgeError::config("defaults did not form a TOML table"));
        };
        apply_layer(&mut merged, &mut sources, defaults, Origin::Default);

        let user_path = Self::user_config_path();
        if let Some(table) = read_config_file(&user_path)? {
            apply_layer(&mut merged, &mut sources, table, Origin::UserFile);
        }

        if let Some(root) = project_root {
            let project_path = Self::project_config_path(root);
            if let Some(table) = read_config_file(&project_path)? {
                apply_layer(&mut merged, &mut sources, table, Origin::ProjectFile);
            }
        }

        if let Some(explicit) = &cli_overrides.config_path {
            match read_config_file(explicit)? {
                Some(table) => apply_layer(&mut merged, &mut sources, table, Origin::ProjectFile),
                None => {
                    return Err(ForgeError::config(format!(
                        "config file not found: {}",
                        explicit.display()
                    )));
                }
            }
        }

        apply_layer(&mut merged, &mut sources, env_layer()?, Origin::Environment);

        apply_layer(
            &mut merged,
            &mut sources,
            cli_layer(cli_overrides),
            Origin::CliFlag,
        );

        let mut config: Config = toml::Value::Table(merged)
            .try_into()
            .map_err(|e| ForgeError::config(format!("invalid configuration: {e}")))?;
        // Carry "who set this" into the `Config` itself: consumers get a
        // plain `&Config`, and two of them cannot be correct without it
        // (see `ExplicitKeys`).
        config.explicit = ExplicitKeys::from_sources(&sources);
        config.validate()?;

        Ok(ResolvedConfig { config, sources })
    }

    /// Declare that `keys` came from a real configuration layer — for
    /// callers that build a `Config` in code instead of through
    /// [`Config::load`] (tests, and anything reconstructing a config).
    /// Without this, such a `Config` reads as "nothing was explicitly set".
    pub fn with_explicit<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.explicit = keys.into_iter().collect();
        self
    }
}

fn apply_layer(
    merged: &mut toml::Table,
    sources: &mut BTreeMap<String, ConfigSource>,
    layer: toml::Table,
    origin: Origin,
) {
    for (key, value) in layer {
        // The models table merges by entry name: a later layer's entry
        // replaces the same-named entry, other entries survive. This
        // always takes the dedicated path for "models" (even on the very
        // first layer, when `merged` doesn't have a "models" entry yet) so
        // it never falls into the generic nested-section branch below,
        // which would otherwise leak stale per-model dotted sources that
        // are never refreshed once this branch takes over on later layers.
        if key == "models"
            && let toml::Value::Table(new_entries) = &value
        {
            let mut combined = match merged.get("models") {
                Some(toml::Value::Table(existing)) => existing.clone(),
                _ => toml::Table::new(),
            };
            for (name, entry) in new_entries {
                combined.insert(name.clone(), entry.clone());
                // Per-entry origin as well as the table's: code that must
                // know whether `[models.claude-sonnet]` is a line in the
                // user's file or a compiled-in default asks `ExplicitKeys`,
                // and an error message that tells someone to edit a section
                // they never wrote is the defect this prevents. Recording it
                // here (rather than in the generic nested-section branch) is
                // what keeps it refreshed on every layer.
                sources.insert(
                    keys::model_entry(name),
                    ConfigSource {
                        value: format!("model entry {name}"),
                        origin,
                    },
                );
            }
            sources.insert(
                key.clone(),
                ConfigSource {
                    value: format!("{} model(s)", combined.len()),
                    origin,
                },
            );
            merged.insert(key, toml::Value::Table(combined));
            continue;
        }
        // Generic one-level nested config sections (e.g. `[needle]`): merge
        // field-by-field so a layer that sets only some fields doesn't wipe
        // out the others, and record a dotted source per field this layer
        // actually set (so `forge config explain needle.variant` works).
        if let toml::Value::Table(new_table) = &value {
            let merged_table = match merged.get(&key) {
                Some(toml::Value::Table(existing)) => {
                    let mut combined = existing.clone();
                    for (subkey, subvalue) in new_table {
                        combined.insert(subkey.clone(), subvalue.clone());
                    }
                    combined
                }
                _ => new_table.clone(),
            };
            for (subkey, subvalue) in new_table {
                sources.insert(
                    format!("{key}.{subkey}"),
                    ConfigSource {
                        value: subvalue.to_string(),
                        origin,
                    },
                );
            }
            merged.insert(key, toml::Value::Table(merged_table));
            continue;
        }
        sources.insert(
            key.clone(),
            ConfigSource {
                value: value.to_string(),
                origin,
            },
        );
        merged.insert(key, value);
    }
}

fn read_config_file(path: &Path) -> Result<Option<toml::Table>, ForgeError> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| ForgeError::config(format!("reading {}: {e}", path.display())))?;
    let table = text
        .parse::<toml::Table>()
        .map_err(|e| ForgeError::config(format!("parsing {}: {e}", path.display())))?;
    Ok(Some(table))
}

fn env_layer() -> Result<toml::Table, ForgeError> {
    let mut table = toml::Table::new();
    for (env_name, key) in ENV_KEYS {
        let Ok(raw) = std::env::var(env_name) else {
            continue;
        };
        let value = match *key {
            "local_only" | "router_autostart" | "needle.autofetch" => {
                toml::Value::Boolean(parse_env_bool(env_name, &raw)?)
            }
            "server_port" => toml::Value::Integer(raw.parse::<i64>().map_err(|_| {
                ForgeError::config(format!("{env_name} must be a valid port, got {raw:?}"))
            })?),
            "max_turns" => toml::Value::Integer(raw.parse::<i64>().map_err(|_| {
                ForgeError::config(format!(
                    "{env_name} must be a positive integer, got {raw:?}"
                ))
            })?),
            "router_confidence_threshold" => {
                toml::Value::Float(raw.parse::<f64>().map_err(|_| {
                    ForgeError::config(format!("{env_name} must be a number in [0,1], got {raw:?}"))
                })?)
            }
            _ => toml::Value::String(raw),
        };
        insert_dotted(&mut table, key, value);
    }
    Ok(table)
}

/// Insert `value` into `table` at a possibly dotted key path (e.g.
/// `needle.variant` creates/updates a `needle` sub-table with a `variant`
/// entry), so `ENV_KEYS` can target nested config sections.
fn insert_dotted(table: &mut toml::Table, key: &str, value: toml::Value) {
    match key.split_once('.') {
        Some((head, rest)) => {
            let entry = table
                .entry(head.to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(sub) = entry {
                insert_dotted(sub, rest, value);
            }
        }
        None => {
            table.insert(key.to_string(), value);
        }
    }
}

fn parse_env_bool(env_name: &str, raw: &str) -> Result<bool, ForgeError> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ForgeError::config(format!(
            "{env_name} must be a boolean (true/false/1/0), got {raw:?}"
        ))),
    }
}

fn cli_layer(overrides: &CliOverrides) -> toml::Table {
    let mut table = toml::Table::new();
    let mut insert = |key: &str, value: Option<toml::Value>| {
        if let Some(v) = value {
            table.insert(key.to_string(), v);
        }
    };
    insert("model", overrides.model.clone().map(toml::Value::String));
    insert("router", overrides.router.clone().map(toml::Value::String));
    insert(
        "router_url",
        overrides.router_url.clone().map(toml::Value::String),
    );
    insert(
        "router_key_env",
        overrides.router_key_env.clone().map(toml::Value::String),
    );
    insert(
        "execution",
        overrides.execution.clone().map(toml::Value::String),
    );
    insert(
        "approval",
        overrides.approval.clone().map(toml::Value::String),
    );
    insert("local_only", overrides.local_only.map(toml::Value::Boolean));
    insert(
        "server_host",
        overrides.server_host.clone().map(toml::Value::String),
    );
    insert(
        "server_port",
        overrides
            .server_port
            .map(|p| toml::Value::Integer(i64::from(p))),
    );
    table
}

#[cfg(test)]
mod tests;
