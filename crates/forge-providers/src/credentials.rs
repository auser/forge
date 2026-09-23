//! Credential resolution: environment variables first, then CLI
//! subscription stores (Claude Code, Codex). Values are never logged;
//! logs name the source only.

use std::path::PathBuf;

/// What kind of credential was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    ApiKey,
    OAuthToken,
}

/// Where a credential came from (safe to log).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialSource {
    EnvVar(String),
    ClaudeCodeCredentials,
    CodexAuthJson,
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnvVar(name) => write!(f, "env var {name}"),
            Self::ClaudeCodeCredentials => write!(f, "~/.claude/.credentials.json"),
            Self::CodexAuthJson => write!(f, "~/.codex/auth.json"),
        }
    }
}

/// A resolved credential (the `secret` must never be logged).
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub secret: String,
    pub kind: CredentialKind,
    pub source: CredentialSource,
}

impl ResolvedCredential {
    pub fn api_key(secret: impl Into<String>, source: CredentialSource) -> Self {
        Self {
            secret: secret.into(),
            kind: CredentialKind::ApiKey,
            source,
        }
    }

    pub fn oauth_token(secret: impl Into<String>, source: CredentialSource) -> Self {
        Self {
            secret: secret.into(),
            kind: CredentialKind::OAuthToken,
            source,
        }
    }
}

fn from_env(name: &str, kind: CredentialKind) -> Option<ResolvedCredential> {
    let value = std::env::var(name).ok()?;
    if value.is_empty() {
        return None;
    }
    Some(ResolvedCredential {
        secret: value,
        kind,
        source: CredentialSource::EnvVar(name.to_string()),
    })
}

fn home_file(rel: &str) -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(rel))
}

/// Claude Code OAuth store: `~/.claude/.credentials.json` →
/// `claudeOauth.accessToken`. (macOS Keychain is not consulted yet.)
fn claude_code_token() -> Option<ResolvedCredential> {
    let path = home_file(".claude/.credentials.json")?;
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let token = json
        .pointer("/claudeOauth/accessToken")
        .and_then(serde_json::Value::as_str)?;
    if token.is_empty() {
        return None;
    }
    Some(ResolvedCredential::oauth_token(
        token,
        CredentialSource::ClaudeCodeCredentials,
    ))
}

/// What lives in `~/.codex/auth.json` (if anything usable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAuth {
    Missing,
    ApiKey(String),
    /// Subscription OAuth only — the ChatGPT Responses backend is not
    /// supported yet.
    OAuthOnly,
}

pub fn codex_auth() -> CodexAuth {
    let Some(path) = home_file(".codex/auth.json") else {
        return CodexAuth::Missing;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return CodexAuth::Missing;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return CodexAuth::Missing;
    };
    if let Some(key) = json
        .get("OPENAI_API_KEY")
        .and_then(serde_json::Value::as_str)
        && !key.is_empty()
    {
        return CodexAuth::ApiKey(key.to_string());
    }
    if json.get("access_token").is_some() || json.get("tokens").is_some() {
        return CodexAuth::OAuthOnly;
    }
    CodexAuth::Missing
}

/// Resolve a credential for a provider. Order: explicit `key_env` env var,
/// conventional env vars for the provider family, then CLI credential
/// stores. `provider_hint`: "anthropic" | "openai" | "moonshot" | other
/// (env-only).
pub fn resolve_credential(
    key_env: Option<&str>,
    provider_hint: Option<&str>,
) -> Option<ResolvedCredential> {
    if let Some(name) = key_env
        && let Some(cred) = from_env(name, CredentialKind::ApiKey)
    {
        return Some(cred);
    }
    match provider_hint {
        Some("anthropic") => from_env("ANTHROPIC_API_KEY", CredentialKind::ApiKey)
            .or_else(|| from_env("CLAUDE_CODE_OAUTH_TOKEN", CredentialKind::OAuthToken))
            .or_else(claude_code_token),
        Some("openai") => {
            from_env("OPENAI_API_KEY", CredentialKind::ApiKey).or_else(|| match codex_auth() {
                CodexAuth::ApiKey(key) => Some(ResolvedCredential::api_key(
                    key,
                    CredentialSource::CodexAuthJson,
                )),
                _ => None,
            })
        }
        Some("moonshot") => from_env("MOONSHOT_API_KEY", CredentialKind::ApiKey)
            .or_else(|| from_env("KIMI_API_KEY", CredentialKind::ApiKey)),
        Some("deepseek") => from_env("DEEPSEEK_API_KEY", CredentialKind::ApiKey),
        _ => None,
    }
}

/// One row of the `forge auth status` report (values never included).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthProbe {
    pub provider: String,
    pub usable_models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<CredentialSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<CredentialKind>,
    pub detected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Probe all known providers (for `forge auth status` and doctor).
pub fn probe_auth() -> Vec<AuthProbe> {
    let mut out = Vec::new();
    let probe = |provider: &str, models: &[&str]| {
        let credential = resolve_credential(None, Some(provider));
        AuthProbe {
            provider: provider.to_string(),
            usable_models: models.iter().map(|m| m.to_string()).collect(),
            detected: credential.is_some(),
            source: credential.as_ref().map(|c| c.source.clone()),
            kind: credential.as_ref().map(|c| c.kind),
            note: None,
        }
    };
    out.push(probe("anthropic", &["claude-sonnet"]));
    out.push(probe("openai", &["gpt-5"]));
    out.push(probe("moonshot", &["kimi-k2.7-code"]));
    out.push(probe("deepseek", &["deepseek-chat"]));

    if matches!(codex_auth(), CodexAuth::OAuthOnly) {
        out.push(AuthProbe {
            provider: "codex".to_string(),
            usable_models: vec!["gpt-5".to_string()],
            detected: false,
            source: Some(CredentialSource::CodexAuthJson),
            kind: Some(CredentialKind::OAuthToken),
            note: Some(
                "detected Codex subscription (OAuth); the ChatGPT Responses backend is not yet supported — set OPENAI_API_KEY for API access"
                    .to_string(),
            ),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_env() {
        for name in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "MOONSHOT_API_KEY",
            "KIMI_API_KEY",
        ] {
            unsafe { std::env::remove_var(name) };
        }
    }

    #[test]
    #[serial]
    fn env_key_env_wins_and_is_typed_api_key() {
        clear_env();
        unsafe { std::env::set_var("MY_KEY", "sk-test-1") };
        let cred = resolve_credential(Some("MY_KEY"), Some("anthropic")).expect("resolved");
        assert_eq!(cred.kind, CredentialKind::ApiKey);
        assert_eq!(cred.source, CredentialSource::EnvVar("MY_KEY".to_string()));
        unsafe { std::env::remove_var("MY_KEY") };
    }

    #[test]
    #[serial]
    fn conventional_env_and_oauth_kind() {
        clear_env();
        unsafe { std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token-1") };
        let cred = resolve_credential(None, Some("anthropic")).expect("resolved");
        assert_eq!(cred.kind, CredentialKind::OAuthToken);
        unsafe { std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN") };
    }

    #[test]
    #[serial]
    fn claude_credentials_file_is_parsed() {
        clear_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("mkdir");
        std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeOauth": {"accessToken": "sk-ant-oat01-dummy", "expiresAt": 1}}"#,
        )
        .expect("write");
        let cred = resolve_credential(None, Some("anthropic")).expect("resolved");
        assert_eq!(cred.kind, CredentialKind::OAuthToken);
        assert_eq!(cred.source, CredentialSource::ClaudeCodeCredentials);
        match prev_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    #[serial]
    fn codex_auth_states() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        assert_eq!(codex_auth(), CodexAuth::Missing);

        let dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("auth.json"),
            r#"{"access_token": "t", "refresh_token": "r"}"#,
        )
        .expect("write oauth");
        assert_eq!(codex_auth(), CodexAuth::OAuthOnly);

        std::fs::write(
            dir.join("auth.json"),
            r#"{"OPENAI_API_KEY": "sk-from-codex"}"#,
        )
        .expect("write api key");
        assert_eq!(codex_auth(), CodexAuth::ApiKey("sk-from-codex".to_string()));

        match prev_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    #[serial]
    fn probe_returns_one_row_per_provider() {
        clear_env();
        let probes = probe_auth();
        assert!(probes.iter().any(|p| p.provider == "anthropic"));
        assert!(probes.iter().any(|p| p.provider == "openai"));
        assert!(probes.iter().any(|p| p.provider == "moonshot"));
        assert!(probes.iter().any(|p| p.provider == "deepseek"));
    }
}
