//! Layered configuration for Forge.
//!
//! Precedence, lowest to highest: built-in defaults →
//! `~/.config/forge/config.toml` (honoring `XDG_CONFIG_HOME`) →
//! `<project>/.forge/config.toml` → `FORGE_*` environment variables →
//! CLI flag overrides. Every key records its winning value and origin so
//! `forge config explain <key>` can report provenance.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use forge_core::ForgeError;
use serde::{Deserialize, Serialize};

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
    pub tools: Option<bool>,
    pub streaming: Option<bool>,
    pub structured_output: Option<bool>,
    pub vision: Option<bool>,
    pub max_context: Option<usize>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub model_base_url: Option<String>,
    pub model_key_env: Option<String>,
    /// Path to a JSON script for `model = "scripted-mock"` (relative to
    /// the project root).
    pub mock_script: Option<String>,
    pub router: String,
    pub router_url: Option<String>,
    pub router_key_env: Option<String>,
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
    /// Unknown keys are tolerated and preserved.
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Default for Config {
    fn default() -> Self {
        // Default stack: Laya (open-source System One router) → local oMLX
        // model. Mock providers stay available but are opt-in
        // (`model = "mock-local"`). Hosted models are only called when a
        // router selects them or the user sets `model` explicitly.
        let models = [
            ModelEntry {
                description: Some("local coding model via oMLX (Qwen3-Coder)".to_string()),
                cost_input_per_mtok: 0.0,
                cost_output_per_mtok: 0.0,
                base_url: Some("http://127.0.0.1:8080/v1".to_string()),
                key_env: None,
                tools: Some(true),
                streaming: Some(true),
                structured_output: None,
                vision: None,
                max_context: Some(32_768),
            },
            ModelEntry {
                description: Some("DeepSeek V4-class chat/coding model, very low cost".to_string()),
                cost_input_per_mtok: 0.14,
                cost_output_per_mtok: 0.28,
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                key_env: Some("DEEPSEEK_API_KEY".to_string()),
                tools: Some(true),
                streaming: Some(true),
                structured_output: None,
                vision: None,
                max_context: Some(128_000),
            },
            ModelEntry {
                description: Some("Moonshot Kimi K2.7 Code, frontier-quality coding".to_string()),
                cost_input_per_mtok: 0.95,
                cost_output_per_mtok: 4.00,
                base_url: Some("https://api.moonshot.ai/v1".to_string()),
                key_env: Some("MOONSHOT_API_KEY".to_string()),
                tools: Some(true),
                streaming: Some(true),
                structured_output: None,
                vision: None,
                max_context: Some(256_000),
            },
        ];
        Self {
            model: "qwen3-coder".to_string(),
            model_base_url: Some("http://127.0.0.1:8080/v1".to_string()),
            model_key_env: None,
            mock_script: None,
            router: "laya".to_string(),
            router_url: None,
            router_key_env: None,
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
                ("kimi-k2.7-code".to_string(), models[2].clone()),
            ]
            .into_iter()
            .collect(),
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

        let config: Config = toml::Value::Table(merged)
            .try_into()
            .map_err(|e| ForgeError::config(format!("invalid configuration: {e}")))?;

        Ok(ResolvedConfig { config, sources })
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
        // replaces the same-named entry, other entries survive.
        if key == "models"
            && let Some(toml::Value::Table(existing)) = merged.get("models")
            && let toml::Value::Table(new_entries) = &value
        {
            let mut combined = existing.clone();
            for (name, entry) in new_entries {
                combined.insert(name.clone(), entry.clone());
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
            "local_only" | "router_autostart" => {
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
        table.insert((*key).to_string(), value);
    }
    Ok(table)
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
