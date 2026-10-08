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
    ClaudeCodeKeychain,
    CodexAuthJson,
    KimiCodeCredentials,
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnvVar(name) => write!(f, "env var {name}"),
            Self::ClaudeCodeCredentials => write!(f, "~/.claude/.credentials.json"),
            Self::ClaudeCodeKeychain => write!(f, "macOS Keychain (Claude Code)"),
            Self::CodexAuthJson => write!(f, "~/.codex/auth.json"),
            Self::KimiCodeCredentials => {
                write!(f, "~/.kimi-code/credentials/kimi-code.json")
            }
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

/// Claude Code OAuth store. Current Claude Code writes
/// `claudeAiOauth.accessToken`; the older `claudeOauth` spelling remains
/// readable so an upgrade never logs a user out of Forge.
fn claude_code_token() -> Option<ResolvedCredential> {
    if let Some(path) = home_file(".claude/.credentials.json")
        && let Ok(text) = std::fs::read_to_string(path)
        && let Some(credential) =
            claude_credential_from_json(&text, CredentialSource::ClaudeCodeCredentials)
    {
        return Some(credential);
    }
    claude_keychain_token()
}

fn claude_credential_from_json(text: &str, source: CredentialSource) -> Option<ResolvedCredential> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let oauth = ["/claudeAiOauth", "/claudeOauth"]
        .into_iter()
        .find_map(|pointer| json.pointer(pointer))?;
    let expires_at = oauth
        .get("expiresAt")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    if expires_at > 0.0 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64()
            * 1_000.0;
        if expires_at <= now_ms {
            return None;
        }
    }
    let token = oauth
        .get("accessToken")
        .and_then(serde_json::Value::as_str)?;
    if token.is_empty() {
        return None;
    }
    Some(ResolvedCredential::oauth_token(token, source))
}

#[cfg(target_os = "macos")]
fn claude_keychain_token() -> Option<ResolvedCredential> {
    // Hermetic integration suites deliberately unlock mock providers; never
    // let those processes read a developer's real login from Keychain.
    if forge_config::test_mocks_allowed() {
        return None;
    }
    let account = std::env::var("USER").ok()?;
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-a",
            &account,
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    claude_credential_from_json(&text, CredentialSource::ClaudeCodeKeychain)
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_token() -> Option<ResolvedCredential> {
    None
}

/// What lives in `~/.codex/auth.json` (if anything usable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAuth {
    Missing,
    ApiKey(String),
    OAuth {
        access_token: String,
        account_id: String,
    },
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
    if let Some(tokens) = json.get("tokens")
        && let (Some(access_token), Some(account_id)) = (
            tokens
                .get("access_token")
                .and_then(serde_json::Value::as_str),
            tokens.get("account_id").and_then(serde_json::Value::as_str),
        )
        && !access_token.is_empty()
        && !account_id.is_empty()
    {
        return CodexAuth::OAuth {
            access_token: access_token.to_string(),
            account_id: account_id.to_string(),
        };
    }
    CodexAuth::Missing
}

fn kimi_code_token() -> Option<ResolvedCredential> {
    let path = home_file(".kimi-code/credentials/kimi-code.json")?;
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    if let Some(expires_at) = json.get("expires_at").and_then(serde_json::Value::as_f64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        if expires_at <= now {
            return None;
        }
    }
    let token = json
        .get("access_token")
        .and_then(serde_json::Value::as_str)?;
    if token.is_empty() {
        return None;
    }
    Some(ResolvedCredential::oauth_token(
        token,
        CredentialSource::KimiCodeCredentials,
    ))
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
        Some("kimi-code") => kimi_code_token(),
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
    out.push(probe("kimi-code", &["k3"]));
    out.push(probe("deepseek", &["deepseek-chat"]));

    let codex = codex_auth();
    out.push(AuthProbe {
        provider: "codex".to_string(),
        usable_models: vec!["gpt-5.6-sol".to_string()],
        detected: matches!(codex, CodexAuth::OAuth { .. }),
        source: matches!(codex, CodexAuth::OAuth { .. }).then_some(CredentialSource::CodexAuthJson),
        kind: matches!(codex, CodexAuth::OAuth { .. }).then_some(CredentialKind::OAuthToken),
        note: None,
    });
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
            r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat01-dummy"}}"#,
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
            r#"{"tokens": {"access_token": "t", "account_id": "acct-1"}}"#,
        )
        .expect("write oauth");
        assert_eq!(
            codex_auth(),
            CodexAuth::OAuth {
                access_token: "t".to_string(),
                account_id: "acct-1".to_string(),
            }
        );

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
    fn kimi_code_credentials_file_is_parsed() {
        clear_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let dir = tmp.path().join(".kimi-code/credentials");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("kimi-code.json"),
            r#"{"access_token":"kimi-oauth","refresh_token":"refresh"}"#,
        )
        .expect("write");
        let cred = resolve_credential(None, Some("kimi-code")).expect("resolved");
        assert_eq!(cred.kind, CredentialKind::OAuthToken);
        assert_eq!(cred.source, CredentialSource::KimiCodeCredentials);
        match prev_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    #[serial]
    fn expired_float_kimi_credential_is_not_advertised() {
        clear_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let dir = tmp.path().join(".kimi-code/credentials");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("kimi-code.json"),
            r#"{"access_token":"expired","expires_at":1.5}"#,
        )
        .expect("write");
        assert!(resolve_credential(None, Some("kimi-code")).is_none());
        match prev_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
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
        assert!(probes.iter().any(|p| p.provider == "kimi-code"));
        assert!(probes.iter().any(|p| p.provider == "codex"));
        assert!(probes.iter().any(|p| p.provider == "deepseek"));
    }
}
