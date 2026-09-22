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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub model_base_url: Option<String>,
    pub model_key_env: Option<String>,
    pub router: String,
    pub router_url: Option<String>,
    pub router_key_env: Option<String>,
    pub router_timeout_ms: u64,
    pub execution: String,
    pub approval: String,
    pub local_only: bool,
    pub server_host: String,
    pub server_port: u16,
    /// Unknown keys are tolerated and preserved.
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "mock-local".to_string(),
            model_base_url: None,
            model_key_env: None,
            router: "static".to_string(),
            router_url: None,
            router_key_env: None,
            router_timeout_ms: 5_000,
            execution: "native".to_string(),
            approval: "prompt".to_string(),
            local_only: false,
            server_host: "127.0.0.1".to_string(),
            server_port: 7_341,
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
    ("FORGE_ROUTER", "router"),
    ("FORGE_ROUTER_URL", "router_url"),
    ("FORGE_ROUTER_KEY_ENV", "router_key_env"),
    ("FORGE_EXECUTION", "execution"),
    ("FORGE_APPROVAL", "approval"),
    ("FORGE_LOCAL_ONLY", "local_only"),
    ("FORGE_SERVER_HOST", "server_host"),
    ("FORGE_SERVER_PORT", "server_port"),
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
            "local_only" => toml::Value::Boolean(parse_env_bool(env_name, &raw)?),
            "server_port" => toml::Value::Integer(raw.parse::<i64>().map_err(|_| {
                ForgeError::config(format!("{env_name} must be a valid port, got {raw:?}"))
            })?),
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
