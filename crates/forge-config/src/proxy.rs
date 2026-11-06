//! `forge proxy`: run forge with every model request pinned to a local,
//! OpenAI/Anthropic-compatible proxy, and with subscription credentials out
//! of reach.
//!
//! The CLI half (`forge-cli`'s `proxy_cmd`) re-execs forge with a hardened
//! environment and sets [`PROXY_MODE_ENV`]. This module is the half every
//! crate below the CLI can see:
//!
//! * [`proxy_mode`] — "this process is the proxied one". `forge-providers`
//!   reads it to stop consulting CLI credential stores (`~/.claude`,
//!   `~/.codex`, `~/.kimi-code`, the macOS Keychain) and conventional
//!   provider env vars at all, whatever `HOME` points at.
//! * [`ProxyPin`] — applied by [`crate::Config::load`] *after* every other
//!   layer, CLI flags included, so no config file, env var or flag can undo
//!   it: `local_only = true`, `router_escalate = "off"`, and every proxy
//!   model entry (the ones reading the proxy's `key_env`) pointed at the
//!   resolved proxy URL.
//! * The sanitisation lists ([`is_sanitized_env`]) shared by the re-exec
//!   (which clears them) and `.env` loading (which must not put them back).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Config, ConfigSource, ExplicitKeys, Origin, keys};

/// Set (to `1`) on the process `forge proxy` execs. Only ever *restricts*
/// what forge does, so setting it by hand is harmless.
pub const PROXY_MODE_ENV: &str = "FORGE_PROXY_MODE";
/// Proxy base URL (no `/v1`). Precedence: `--url` > this > `[proxy] url` >
/// [`DEFAULT_PROXY_URL`].
pub const PROXY_URL_ENV: &str = "FORGE_PROXY_URL";
/// The client key sent to the proxy (the default `key_env` reads it).
/// Defaults to [`DEFAULT_API_KEY`]: a local proxy that authenticates
/// upstream itself accepts any client key.
pub const PROXY_API_KEY_ENV: &str = "FORGE_PROXY_API_KEY";
/// Set on the proxied process: the env var name the proxy model entries
/// read their key from (`[proxy] key_env`), so the pin knows which entries
/// are the proxy's.
pub const PROXY_KEY_ENV_NAME_ENV: &str = "FORGE_PROXY_KEY_ENV";
/// Set on the proxied process: the user's real home, which `HOME` no longer
/// is. Lets a nested `forge proxy` (an agent tool call) find the same
/// sandbox instead of nesting another one inside it.
pub const REAL_HOME_ENV: &str = "FORGE_REAL_HOME";

pub const DEFAULT_PROXY_URL: &str = "http://127.0.0.1:8001";
pub const DEFAULT_KEY_ENV: &str = PROXY_API_KEY_ENV;
pub const DEFAULT_API_KEY: &str = "dummy";

/// `[proxy]`: where `forge proxy` sends model traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    /// Proxy base URL, without `/v1` (Anthropic entries use it as is,
    /// OpenAI entries append `/v1`).
    pub url: Option<String>,
    /// Env var the proxy model entries read their client key from.
    pub key_env: Option<String>,
}

/// Is this the process `forge proxy` exec'd?
pub fn proxy_mode() -> bool {
    std::env::var_os(PROXY_MODE_ENV).is_some_and(|v| v == "1")
}

/// Env var name prefixes cleared from the proxied process: every provider
/// family forge (or a provider CLI an agent might run) reads keys, tokens,
/// base URLs or config dirs from.
pub const SANITIZED_ENV_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_",
    "OPENAI_",
    "CODEX_",
    "MOONSHOT_",
    "KIMI_",
    "DEEPSEEK_",
    "OPENROUTER_",
    "TYPESAFE_",
];

/// Exact names cleared as well: forge's own endpoint/credential redirects,
/// any of which could point a request somewhere other than the proxy.
pub const SANITIZED_ENV_VARS: &[&str] = &[
    "FORGE_MODEL_BASE_URL",
    "FORGE_MODEL_KEY_ENV",
    "FORGE_ROUTER_URL",
    "FORGE_ROUTER_KEY_ENV",
    "FORGE_JEV_URL",
    "FORGE_JEV_KEY_ENV",
    "FORGE_OPENROUTER_BASE_URL",
];

/// Whether `name` is cleared from (and never re-loaded into) the proxied
/// process.
pub fn is_sanitized_env(name: &str) -> bool {
    SANITIZED_ENV_VARS.contains(&name)
        || SANITIZED_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// The proxy URL as Anthropic entries want it: no trailing `/`, no `/v1`
/// (forge appends `/v1/messages` itself).
pub fn anthropic_base(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// The proxy URL as OpenAI-compatible entries want it: with `/v1`.
pub fn openai_base(url: &str) -> String {
    format!("{}/v1", anthropic_base(url))
}

/// What a proxied process pins on top of all configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyPin {
    pub url: String,
    pub key_env: String,
}

impl ProxyPin {
    /// The pin for this process, if it is the proxied one.
    pub fn from_env() -> Option<Self> {
        if !proxy_mode() {
            return None;
        }
        let non_empty = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        Some(Self {
            url: non_empty(PROXY_URL_ENV).unwrap_or_else(|| DEFAULT_PROXY_URL.to_string()),
            key_env: non_empty(PROXY_KEY_ENV_NAME_ENV)
                .unwrap_or_else(|| DEFAULT_KEY_ENV.to_string()),
        })
    }

    /// Apply the pin to a fully merged configuration. Runs last in
    /// [`Config::load`], so nothing can override it.
    pub fn apply(&self, config: &mut Config, sources: &mut BTreeMap<String, ConfigSource>) {
        let mut pin = |key: &str, value: String| {
            sources.insert(
                key.to_string(),
                ConfigSource {
                    value,
                    origin: Origin::Proxy,
                },
            );
        };
        config.local_only = true;
        pin("local_only", "true".to_string());
        config.router_escalate = "off".to_string();
        pin("router_escalate", "off".to_string());

        for (name, entry) in config.models.iter_mut() {
            if entry.key_env.as_deref() != Some(self.key_env.as_str()) {
                continue;
            }
            let url = if entry.provider.as_deref() == Some("anthropic") {
                anthropic_base(&self.url)
            } else {
                openai_base(&self.url)
            };
            pin(&format!("models.{name}.base_url"), url.clone());
            entry.base_url = Some(url);
        }

        // An explicit global `model_base_url` would override every entry's
        // own URL (see forge-providers' `resolve_endpoint`), proxy entries
        // included. Under the pin, the entries win.
        if config.explicit.contains(keys::MODEL_BASE_URL) {
            config.explicit = config
                .explicit
                .iter()
                .filter(|key| *key != keys::MODEL_BASE_URL)
                .map(str::to_string)
                .collect::<ExplicitKeys>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelEntry;

    #[test]
    fn every_provider_key_family_and_forge_redirect_is_sanitized() {
        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "CODEX_HOME",
            "MOONSHOT_API_KEY",
            "KIMI_API_KEY",
            "DEEPSEEK_API_KEY",
            "OPENROUTER_API_KEY",
            "TYPESAFE_API_KEY",
            "FORGE_MODEL_BASE_URL",
            "FORGE_ROUTER_URL",
            "FORGE_JEV_URL",
            "FORGE_OPENROUTER_BASE_URL",
        ] {
            assert!(is_sanitized_env(name), "{name} must be cleared");
        }
        for name in [
            "FORGE_PROXY_API_KEY",
            "FORGE_PROXY_URL",
            "FORGE_MODEL",
            "TERM",
            "LANG",
        ] {
            assert!(!is_sanitized_env(name), "{name} must pass through");
        }
    }

    #[test]
    fn anthropic_entries_drop_v1_and_openai_entries_add_it() {
        for url in [
            "http://127.0.0.1:8001",
            "http://127.0.0.1:8001/",
            "http://127.0.0.1:8001/v1",
            "http://127.0.0.1:8001/v1/",
        ] {
            assert_eq!(anthropic_base(url), "http://127.0.0.1:8001", "{url}");
            assert_eq!(openai_base(url), "http://127.0.0.1:8001/v1", "{url}");
        }
    }

    fn entry(provider: Option<&str>, base: &str, key_env: &str) -> ModelEntry {
        ModelEntry {
            provider: provider.map(str::to_string),
            base_url: Some(base.to_string()),
            key_env: Some(key_env.to_string()),
            ..ModelEntry::default()
        }
    }

    #[test]
    fn the_pin_forces_local_only_and_points_only_proxy_entries_at_the_proxy() {
        let mut config = Config {
            local_only: false,
            router_escalate: "auto".to_string(),
            ..Config::default()
        }
        .with_explicit(["model_base_url"]);
        config.models.insert(
            "claude-x".into(),
            entry(Some("anthropic"), "http://127.0.0.1:8001", DEFAULT_KEY_ENV),
        );
        config.models.insert(
            "sonnet".into(),
            entry(None, "http://127.0.0.1:8001/v1", DEFAULT_KEY_ENV),
        );
        let mut sources = BTreeMap::new();
        ProxyPin {
            url: "http://127.0.0.1:18001".into(),
            key_env: DEFAULT_KEY_ENV.into(),
        }
        .apply(&mut config, &mut sources);

        assert!(config.local_only);
        assert_eq!(config.router_escalate, "off");
        assert_eq!(sources["local_only"].origin, Origin::Proxy);
        assert_eq!(
            config.models["claude-x"].base_url.as_deref(),
            Some("http://127.0.0.1:18001")
        );
        assert_eq!(
            config.models["sonnet"].base_url.as_deref(),
            Some("http://127.0.0.1:18001/v1")
        );
        // Not a proxy entry: untouched (local_only is what guards it).
        assert_eq!(
            config.models["claude-sonnet"].base_url.as_deref(),
            Some("https://api.anthropic.com")
        );
        assert!(
            !config.explicit.contains(keys::MODEL_BASE_URL),
            "a global base URL must not override the pinned entries"
        );
    }
}
