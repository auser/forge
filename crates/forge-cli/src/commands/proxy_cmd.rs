//! `forge proxy`: run any forge command with every model request going to a
//! local proxy (e.g. a Claude/OpenAI-compatible ccproxy on
//! `127.0.0.1:8001`), and subscription credentials out of reach.
//!
//! # How the guarantee is built
//!
//! Two layers, each sufficient for forge's own code and together stronger
//! than either:
//!
//! 1. **Re-exec with a hardened environment** ([`env_plan`]): forge re-runs
//!    `current_exe()` with `HOME` moved to a sandbox (`~/.forge-home`), every
//!    provider key/token/base-URL variable cleared, `PATH` stripped of the
//!    real home's directories and of any directory holding a subscription CLI
//!    (`claude`, `codex`, `kimi`), and the proxy client key exported. This is
//!    what the old `forge-proxy` shell wrapper did, and it is the part an
//!    in-process switch cannot replace: it also covers every *child* process
//!    the agent starts (tool commands, hooks), and any home-relative lookup
//!    nobody audited.
//! 2. **In-process enforcement keyed on `FORGE_PROXY_MODE=1`** (see
//!    `forge_config::proxy`), which the wrapper never had:
//!    * credential resolution consults only a model entry's own `key_env` —
//!      no `~/.claude`, `~/.codex`, `~/.kimi-code` or Keychain read, whatever
//!      `HOME` is, and no conventional provider env var;
//!    * `Config::load` pins `local_only = true` and `router_escalate = "off"`
//!      over every layer (CLI flags included), and points every proxy model
//!      entry at the resolved proxy URL — so no code path can reach a
//!      non-loopback endpoint;
//!    * `.env`/`.env.local` cannot put a cleared variable back;
//!    * `forge auth login` refuses, and chat never offers a sign-in.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_config::proxy::{
    DEFAULT_API_KEY, DEFAULT_KEY_ENV, DEFAULT_PROXY_URL, PROXY_API_KEY_ENV, PROXY_KEY_ENV_NAME_ENV,
    PROXY_MODE_ENV, PROXY_URL_ENV, REAL_HOME_ENV, anthropic_base, is_sanitized_env, openai_base,
};
use forge_core::ForgeError;

use crate::cli::GlobalOpts;
use crate::commands::Context;

/// The sandbox `HOME`, relative to the real one.
pub const SANDBOX_HOME_DIR: &str = ".forge-home";

/// Set on the proxied process: where the parent's URL came from (the child
/// itself only ever sees it as `FORGE_PROXY_URL`), for doctor.
const URL_SOURCE_ENV: &str = "FORGE_PROXY_URL_SOURCE";

/// Subscription CLIs whose directory is dropped from `PATH`.
const SUBSCRIPTION_CLIS: &[&str] = &["claude", "codex", "kimi"];

/// Always offered on `PATH` (when they pass the same filter), so a proxied
/// agent can still run `python3`, `git`, `sh`.
const SYSTEM_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

/// Where a resolved setting came from, for `forge doctor` and `init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    Flag,
    Env,
    Config,
    Default,
}

impl SettingSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Env => "env",
            Self::Config => "config [proxy]",
            Self::Default => "default",
        }
    }
}

/// The proxy settings for this invocation. `api_key` is never printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySettings {
    /// Base URL without `/v1`.
    pub url: String,
    pub url_source: SettingSource,
    /// Env var the proxy model entries read their key from.
    pub key_env: String,
    pub key_env_source: SettingSource,
    pub api_key: String,
}

/// Resolve settings. Precedence: flags > env (`FORGE_PROXY_URL`,
/// `FORGE_PROXY_API_KEY`) > `[proxy]` config > defaults. Pure: the
/// environment arrives as a lookup, so precedence is unit-tested.
pub fn resolve_settings(
    flag_url: Option<&str>,
    flag_key_env: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    config: &forge_config::ProxyConfig,
) -> Result<ProxySettings, ForgeError> {
    let non_empty = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    let (url, url_source) = if let Some(url) = non_empty(flag_url.map(str::to_string)) {
        (url, SettingSource::Flag)
    } else if let Some(url) = non_empty(env(PROXY_URL_ENV)) {
        (url, SettingSource::Env)
    } else if let Some(url) = non_empty(config.url.clone()) {
        (url, SettingSource::Config)
    } else {
        (DEFAULT_PROXY_URL.to_string(), SettingSource::Default)
    };
    let url = anthropic_base(&url);
    if !matches!(
        reqwest::Url::parse(&url).map(|u| u.scheme().to_string()),
        Ok(s) if s == "http" || s == "https"
    ) {
        return Err(ForgeError::config(format!(
            "proxy URL {url:?} is not an http(s) URL"
        )));
    }
    if !forge_providers::endpoint_is_local(&url) {
        return Err(ForgeError::config(format!(
            "proxy URL {url} is not on this machine; `forge proxy` only talks to a loopback \
             proxy (it runs with local_only pinned on). For a proxy on another host, tunnel \
             it first — e.g. `ssh -N -L 8001:127.0.0.1:8001 <host>` — and use \
             --url http://127.0.0.1:8001"
        )));
    }

    let (key_env, key_env_source) = if let Some(name) = non_empty(flag_key_env.map(str::to_string))
    {
        (name, SettingSource::Flag)
    } else if let Some(name) = non_empty(config.key_env.clone()) {
        (name, SettingSource::Config)
    } else {
        (DEFAULT_KEY_ENV.to_string(), SettingSource::Default)
    };
    let valid_name = key_env
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && key_env
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !valid_name {
        return Err(ForgeError::config(format!(
            "proxy key_env {key_env:?} is not an environment variable name (A-Z, 0-9, _)"
        )));
    }
    if is_sanitized_env(&key_env) {
        return Err(ForgeError::config(format!(
            "proxy key_env {key_env} is a provider credential variable that `forge proxy` \
             clears; use a name of your own (default {DEFAULT_KEY_ENV})"
        )));
    }

    let api_key = non_empty(env(&key_env))
        .or_else(|| non_empty(env(PROXY_API_KEY_ENV)))
        .unwrap_or_else(|| DEFAULT_API_KEY.to_string());

    Ok(ProxySettings {
        url,
        url_source,
        key_env,
        key_env_source,
        api_key,
    })
}

/// What the re-exec changes in the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvPlan {
    pub set: BTreeMap<String, OsString>,
    pub remove: Vec<String>,
}

/// Facts about the machine [`env_plan`] needs, injected so it stays pure.
pub struct Probe<'a> {
    /// Does this directory hold a subscription CLI?
    pub holds_subscription_cli: &'a dyn Fn(&Path) -> bool,
    /// Does this directory exist?
    pub is_dir: &'a dyn Fn(&Path) -> bool,
}

impl Probe<'static> {
    pub fn real() -> Self {
        Probe {
            holds_subscription_cli: &dir_holds_subscription_cli,
            is_dir: &|p: &Path| p.is_dir(),
        }
    }
}

fn dir_holds_subscription_cli(dir: &Path) -> bool {
    SUBSCRIPTION_CLIS.iter().any(|name| {
        let exts: &[&str] = if cfg!(windows) {
            &[".exe", ".cmd", ".bat", ".ps1"]
        } else {
            &[""]
        };
        exts.iter()
            .any(|ext| dir.join(format!("{name}{ext}")).is_file())
    })
}

/// The real home: `FORGE_REAL_HOME` when this is already a proxied process
/// (a nested `forge proxy` must not nest the sandbox), else `HOME`.
pub fn real_home(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env(REAL_HOME_ENV)
        .filter(|v| !v.is_empty())
        .or_else(|| env("HOME").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
}

/// The hardened environment. `current` is the process environment.
pub fn env_plan(
    current: &[(String, OsString)],
    real_home: &Path,
    settings: &ProxySettings,
    probe: &Probe<'_>,
) -> EnvPlan {
    let get = |key: &str| {
        current
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let sandbox = real_home.join(SANDBOX_HOME_DIR);
    let mut plan = EnvPlan::default();

    for (name, _) in current {
        if is_sanitized_env(name) && !plan.remove.contains(name) {
            plan.remove.push(name.clone());
        }
    }

    let mut set = |k: &str, v: OsString| {
        plan.set.insert(k.to_string(), v);
    };
    set("HOME", sandbox.clone().into_os_string());
    // The real config, not the sandbox's: `forge proxy init` writes it, and
    // the proxied process must read the same file.
    let xdg = get("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty() && !Path::new(v).starts_with(&sandbox))
        .unwrap_or_else(|| real_home.join(".config").into_os_string());
    set("XDG_CONFIG_HOME", xdg);
    set(
        "PATH",
        sandbox_path(get("PATH").as_deref(), real_home, probe),
    );
    // Toolchains that live under the real home keep working (they hold no
    // subscription credential): rustup's `/usr/bin/cargo` proxy needs these.
    for (var, dir) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
        let path = real_home.join(dir);
        if get(var).is_none() && (probe.is_dir)(&path) {
            set(var, path.into_os_string());
        }
    }
    set(PROXY_MODE_ENV, "1".into());
    set(PROXY_URL_ENV, settings.url.clone().into());
    set(URL_SOURCE_ENV, settings.url_source.as_str().into());
    set(PROXY_KEY_ENV_NAME_ENV, settings.key_env.clone().into());
    set(REAL_HOME_ENV, real_home.as_os_str().to_os_string());
    set(&settings.key_env, settings.api_key.clone().into());
    if settings.key_env != PROXY_API_KEY_ENV {
        set(PROXY_API_KEY_ENV, settings.api_key.clone().into());
    }
    plan
}

/// `PATH` for the proxied process: the current entries minus relative ones,
/// ones under the real home, and any directory holding a subscription CLI;
/// then the system directories that pass the same filter.
pub fn sandbox_path(path: Option<&OsStr>, real_home: &Path, probe: &Probe<'_>) -> OsString {
    let keep = |dir: &Path| {
        dir.is_absolute() && !dir.starts_with(real_home) && !(probe.holds_subscription_cli)(dir)
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let candidates = path
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(SYSTEM_PATH.iter().map(PathBuf::from));
    for dir in candidates {
        if keep(&dir) && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The child's argv: the global flags clap already consumed (they may have
/// been written before or after `proxy`), then everything after `proxy`.
pub fn child_args(global: &GlobalOpts, forge_args: &[OsString]) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut flag = |name: &str, value: Option<&OsStr>| {
        args.push(name.into());
        if let Some(value) = value {
            args.push(value.to_os_string());
        }
    };
    for _ in 0..global.verbose {
        flag("-v", None);
    }
    if let Some(v) = &global.config {
        flag("--config", Some(v.as_os_str()));
    }
    if let Some(v) = &global.project {
        flag("--project", Some(v.as_os_str()));
    }
    if let Some(v) = &global.model {
        flag("--model", Some(OsStr::new(v)));
    }
    if let Some(v) = &global.router {
        flag("--router", Some(OsStr::new(v)));
    }
    if let Some(v) = &global.execution {
        flag("--execution", Some(OsStr::new(v)));
    }
    if global.local_only {
        flag("--local-only", None);
    }
    if let Some(v) = &global.approval {
        flag("--approval", Some(OsStr::new(v)));
    }
    if global.json {
        flag("--json", None);
    }
    if global.no_color {
        flag("--no-color", None);
    }
    args.extend(forge_args.iter().cloned());
    args
}

fn process_env() -> Vec<(String, OsString)> {
    std::env::vars_os()
        .filter_map(|(k, v)| k.into_string().ok().map(|k| (k, v)))
        .collect()
}

fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Settings for this invocation, reading `[proxy]` from the resolved config.
fn settings_for(
    ctx: &Context,
    flag_url: Option<&str>,
    flag_key_env: Option<&str>,
) -> Result<(ProxySettings, Option<forge_config::Config>), ForgeError> {
    // A broken config must not stop `forge proxy init` from repairing it, and
    // the proxied process reports it properly anyway.
    let config = ctx.resolve_config().ok().map(|r| r.config);
    let proxy = config.as_ref().map(|c| c.proxy.clone()).unwrap_or_default();
    let settings = resolve_settings(flag_url, flag_key_env, &env_lookup, &proxy)?;
    Ok((settings, config))
}

/// `forge proxy [--url URL] [--key-env NAME] [FORGE_ARGS…]`: re-exec forge
/// with the hardened environment. On Unix this never returns on success.
pub fn run(
    ctx: &Context,
    flag_url: Option<&str>,
    flag_key_env: Option<&str>,
    forge_args: &[OsString],
) -> Result<(), ForgeError> {
    let (settings, config) = settings_for(ctx, flag_url, flag_key_env)?;
    let home = real_home(&env_lookup)
        .ok_or_else(|| ForgeError::config("cannot determine the home directory"))?;
    let sandbox = home.join(SANDBOX_HOME_DIR);
    create_sandbox(&sandbox)?;

    if let Some(config) = &config
        && !config
            .models
            .values()
            .any(|e| e.key_env.as_deref() == Some(settings.key_env.as_str()))
    {
        eprintln!(
            "warning: no [models] entry reads {} — nothing will go through the proxy; \
             run `forge proxy init` first",
            settings.key_env
        );
    }

    let plan = env_plan(&process_env(), &home, &settings, &Probe::real());
    let exe = std::env::current_exe().map_err(ForgeError::Io)?;
    let mut command = std::process::Command::new(exe);
    command.args(child_args(&ctx.global, forge_args));
    for name in &plan.remove {
        command.env_remove(name);
    }
    command.envs(&plan.set);
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(ForgeError::Io(error))
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(ForgeError::Io)?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn create_sandbox(path: &Path) -> Result<(), ForgeError> {
    std::fs::create_dir_all(path).map_err(ForgeError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// forge proxy init
// ---------------------------------------------------------------------------

/// A model the proxy advertises (`GET /v1/models`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertised {
    pub id: String,
    pub created: i64,
}

/// Which entries `init` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPlan {
    /// Anthropic Messages dialect (`provider = "anthropic"`, URL without `/v1`).
    pub anthropic: Vec<String>,
    /// OpenAI Chat dialect (URL with `/v1`; the entry name is the wire id).
    pub openai: Vec<String>,
    /// Where the list came from, for the report.
    pub origin: String,
}

const DEFAULT_ANTHROPIC: &[&str] = &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];
const DEFAULT_OPENAI: &[&str] = &["opus", "sonnet", "haiku"];

/// Choose the entries. Explicit `--models` wins (`claude-*` → Anthropic,
/// anything else → OpenAI). Otherwise from what the proxy advertises: the
/// newest `claude-<family>-<version>` per family for the Anthropic dialect,
/// and bare aliases (`sonnet`, `opus`, …) for the OpenAI dialect — one entry
/// per wire id, because an OpenAI entry's name *is* its wire model. With
/// nothing advertised (unreachable proxy), a fixed default.
pub fn plan_models(advertised: Option<&[Advertised]>, explicit: &[String]) -> ModelPlan {
    if !explicit.is_empty() {
        let (anthropic, openai): (Vec<String>, Vec<String>) = explicit
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .partition(|id| id.starts_with("claude-"));
        return ModelPlan {
            anthropic,
            openai,
            origin: "--models".to_string(),
        };
    }
    let defaults = || ModelPlan {
        anthropic: DEFAULT_ANTHROPIC.iter().map(|s| s.to_string()).collect(),
        openai: DEFAULT_OPENAI.iter().map(|s| s.to_string()).collect(),
        origin: "built-in defaults (proxy model list unavailable)".to_string(),
    };
    let Some(advertised) = advertised.filter(|a| !a.is_empty()) else {
        return defaults();
    };

    let mut newest: BTreeMap<String, &Advertised> = BTreeMap::new();
    for model in advertised {
        let mut parts = model.id.split('-');
        let (Some("claude"), Some(family), Some(version)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if family.is_empty()
            || !family.chars().all(|c| c.is_ascii_lowercase())
            || !version.starts_with(|c: char| c.is_ascii_digit())
        {
            continue;
        }
        let better = newest
            .get(family)
            .is_none_or(|cur| (model.created, &model.id) > (cur.created, &cur.id));
        if better {
            newest.insert(family.to_string(), model);
        }
    }
    let anthropic: Vec<String> = newest.values().map(|m| m.id.clone()).collect();
    let mut openai: Vec<String> = advertised
        .iter()
        .filter(|m| !m.id.is_empty() && m.id.chars().all(|c| c.is_ascii_lowercase()))
        .map(|m| m.id.clone())
        .collect();
    openai.sort();
    openai.dedup();
    if anthropic.is_empty() && openai.is_empty() {
        // Not a Claude proxy: offer what it serves on the OpenAI dialect.
        let mut all: Vec<&Advertised> = advertised.iter().collect();
        all.sort_by(|a, b| (b.created, &b.id).cmp(&(a.created, &a.id)));
        return ModelPlan {
            anthropic: Vec::new(),
            openai: all.into_iter().take(8).map(|m| m.id.clone()).collect(),
            origin: format!("{} models advertised (newest 8)", advertised.len()),
        };
    }
    ModelPlan {
        anthropic,
        openai,
        origin: format!(
            "{} models advertised (newest per family + aliases)",
            advertised.len()
        ),
    }
}

/// Parse a `GET /v1/models` body (OpenAI and Anthropic list shapes both use
/// `data[].id`; `created` is optional).
pub fn parse_models(body: &serde_json::Value) -> Vec<Advertised> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(Advertised {
                        id: item.get("id")?.as_str()?.to_string(),
                        created: item.get("created").and_then(|c| c.as_i64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Edit `text` (a config.toml) so it carries the proxy setup, keeping every
/// other key, table and comment. Returns the new text and one line per
/// change; an unchanged file returns no changes (idempotence).
pub fn apply_init(
    text: &str,
    settings: &ProxySettings,
    plan: &ModelPlan,
) -> Result<(String, Vec<String>), ForgeError> {
    use toml_edit::{DocumentMut, Item, Table};

    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| ForgeError::config(format!("config is not valid TOML: {e}")))?;
    let mut changes = Vec::new();

    /// A value as written, without its decor (whitespace, trailing comment),
    /// so a commented key compares equal to the same bare value.
    fn bare(value: &toml_edit::Value) -> String {
        let mut value = value.clone();
        value.decor_mut().clear();
        value.to_string()
    }
    fn set(
        table: &mut Table,
        key: &str,
        new: toml_edit::Value,
        path: &str,
        changes: &mut Vec<String>,
    ) {
        let old = table.get(key).and_then(Item::as_value).map(bare);
        if old.as_deref() == Some(bare(&new).as_str()) {
            return;
        }
        let was = old.map(|v| format!(" (was {v})")).unwrap_or_default();
        changes.push(format!("{path} = {}{was}", bare(&new)));
        // Keep the existing key's decor (comments) when replacing.
        match table.get_mut(key).and_then(Item::as_value_mut) {
            Some(slot) => {
                let decor = slot.decor().clone();
                *slot = new;
                *slot.decor_mut() = decor;
            }
            None => {
                table.insert(key, Item::Value(new));
            }
        }
    }
    fn set_if_absent(table: &mut Table, key: &str, new: toml_edit::Value) {
        if !table.contains_key(key) {
            table.insert(key, Item::Value(new));
        }
    }

    let root = doc.as_table_mut();
    set(root, "local_only", true.into(), "local_only", &mut changes);
    set(
        root,
        "router_escalate",
        "off".into(),
        "router_escalate",
        &mut changes,
    );
    let preferred = plan
        .anthropic
        .iter()
        .chain(&plan.openai)
        .find(|m| m.contains("sonnet"))
        .or_else(|| plan.anthropic.first())
        .or_else(|| plan.openai.first());
    if !root.contains_key("model")
        && let Some(model) = preferred
    {
        set(root, "model", model.as_str().into(), "model", &mut changes);
    }

    if !root.contains_key("proxy") {
        root.insert("proxy", Item::Table(Table::new()));
    }
    let proxy = root["proxy"]
        .as_table_mut()
        .ok_or_else(|| ForgeError::config("`proxy` in the config is not a table"))?;
    set(
        proxy,
        "url",
        settings.url.as_str().into(),
        "proxy.url",
        &mut changes,
    );
    set(
        proxy,
        "key_env",
        settings.key_env.as_str().into(),
        "proxy.key_env",
        &mut changes,
    );

    if !root.contains_key("models") {
        let mut models = Table::new();
        models.set_implicit(true);
        root.insert("models", Item::Table(models));
    }
    let models = root["models"]
        .as_table_mut()
        .ok_or_else(|| ForgeError::config("`models` in the config is not a table"))?;
    let entries = plan
        .anthropic
        .iter()
        .map(|id| (id, true))
        .chain(plan.openai.iter().map(|id| (id, false)));
    for (id, anthropic) in entries {
        let added = !models.contains_key(id);
        if added {
            models.insert(id, Item::Table(Table::new()));
        }
        // A new entry is reported as one line, not one per key.
        let mut entry_changes = Vec::new();
        let changes_for_entry = &mut entry_changes;
        let entry = models[id.as_str()]
            .as_table_mut()
            .ok_or_else(|| ForgeError::config(format!("`models.{id}` is not a table")))?;
        let path = format!("models.{id}");
        let (dialect, base) = if anthropic {
            ("Anthropic Messages", anthropic_base(&settings.url))
        } else {
            ("OpenAI Chat", openai_base(&settings.url))
        };
        set_if_absent(
            entry,
            "description",
            format!("{id} via forge proxy ({}), {dialect} dialect", settings.url)
                .as_str()
                .into(),
        );
        if anthropic {
            set(
                entry,
                "provider",
                "anthropic".into(),
                &format!("{path}.provider"),
                changes_for_entry,
            );
        }
        set(
            entry,
            "base_url",
            base.as_str().into(),
            &format!("{path}.base_url"),
            changes_for_entry,
        );
        set(
            entry,
            "key_env",
            settings.key_env.as_str().into(),
            &format!("{path}.key_env"),
            changes_for_entry,
        );
        set_if_absent(entry, "tools", true.into());
        set_if_absent(entry, "streaming", true.into());
        set_if_absent(entry, "max_context", 200_000i64.into());
        if added {
            changes.push(format!("[models.{id}] added ({dialect}, {base})"));
        } else {
            changes.extend(entry_changes);
        }
    }
    Ok((doc.to_string(), changes))
}

/// `forge proxy init`.
pub async fn init(
    ctx: &Context,
    flag_url: Option<&str>,
    flag_key_env: Option<&str>,
    models: &[String],
) -> Result<(), ForgeError> {
    let (settings, _) = settings_for(ctx, flag_url, flag_key_env)?;
    let advertised = if models.is_empty() {
        fetch_models(&settings).await
    } else {
        None
    };
    let plan = plan_models(advertised.as_deref(), models);

    let path = forge_config::Config::user_config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(ForgeError::Io(e)),
    };
    let (new_text, changes) = apply_init(&text, &settings, &plan)?;
    // The result must load as a forge config before it replaces anything.
    let parsed: toml::Table = toml::from_str(&new_text)
        .map_err(|e| ForgeError::config(format!("refusing to write invalid config: {e}")))?;
    let _: forge_config::Config = toml::Value::Table(parsed)
        .try_into()
        .map_err(|e| ForgeError::config(format!("refusing to write invalid config: {e}")))?;

    if !changes.is_empty() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
        }
        let tmp = path.with_extension("toml.forge-proxy-init");
        std::fs::write(&tmp, &new_text).map_err(ForgeError::Io)?;
        std::fs::rename(&tmp, &path).map_err(ForgeError::Io)?;
    }

    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({
                "path": path,
                "url": settings.url,
                "key_env": settings.key_env,
                "models_from": plan.origin,
                "anthropic": plan.anthropic,
                "openai": plan.openai,
                "changes": changes,
            })
        );
        return Ok(());
    }
    println!(
        "proxy: {} ({}), key from ${}",
        settings.url,
        settings.url_source.as_str(),
        settings.key_env
    );
    println!("models ({}):", plan.origin);
    if !plan.anthropic.is_empty() {
        println!(
            "  anthropic {}/v1/messages: {}",
            anthropic_base(&settings.url),
            plan.anthropic.join(", ")
        );
    }
    if !plan.openai.is_empty() {
        println!(
            "  openai    {}/chat/completions: {}",
            openai_base(&settings.url),
            plan.openai.join(", ")
        );
    }
    if changes.is_empty() {
        println!("{}: already up to date", path.display());
    } else {
        println!("updated {}:", path.display());
        for change in &changes {
            println!("  {change}");
        }
    }
    println!("next: `forge proxy doctor`, then `forge proxy` (chat) or `forge proxy run \"…\"`");
    Ok(())
}

fn loopback_client(timeout: Duration) -> Option<reqwest::Client> {
    forge_providers::EgressPolicy::LocalOnly
        .client(timeout)
        .ok()
}

async fn fetch_models(settings: &ProxySettings) -> Option<Vec<Advertised>> {
    let client = loopback_client(Duration::from_secs(3))?;
    let response = client
        .get(format!("{}/models", openai_base(&settings.url)))
        .header("x-api-key", &settings.api_key)
        .header("authorization", format!("Bearer {}", settings.api_key))
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    Some(parse_models(&body))
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// The dialect routes `forge proxy` cares about, probed with a bodiless GET:
/// a POST-only route answers 405 (present), an absent one 404. Nothing is
/// forwarded upstream and nothing is billed.
const DIALECTS: &[(&str, &str)] = &[
    ("anthropic", "/v1/messages"),
    ("openai-chat", "/v1/chat/completions"),
    ("openai-responses", "/v1/responses"),
];

/// What the proxy endpoint looks like right now.
#[derive(Debug, Clone, Default)]
pub struct ProxyHealth {
    pub reachable: bool,
    pub dialects: Vec<&'static str>,
    pub models: Option<usize>,
}

pub async fn probe(url: &str) -> ProxyHealth {
    let Some(client) = loopback_client(Duration::from_millis(1_500)) else {
        return ProxyHealth::default();
    };
    let base = anthropic_base(url);
    let mut health = ProxyHealth::default();
    if let Ok(response) = client.get(format!("{base}/v1/models")).send().await {
        health.reachable = true;
        if response.status().is_success()
            && let Ok(body) = response.json::<serde_json::Value>().await
        {
            health.models = Some(parse_models(&body).len());
        }
    }
    for (name, path) in DIALECTS {
        if let Ok(response) = client.get(format!("{base}{path}")).send().await {
            health.reachable = true;
            if response.status() != reqwest::StatusCode::NOT_FOUND {
                health.dialects.push(name);
            }
        }
    }
    health
}

/// Facts about the isolation of *this* process, for doctor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Isolation {
    pub home: Option<PathBuf>,
    pub real_home: Option<PathBuf>,
    pub leaked_env: Vec<String>,
    pub cli_on_path: Vec<PathBuf>,
}

impl Isolation {
    pub fn current() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let real = std::env::var_os(REAL_HOME_ENV).map(PathBuf::from);
        let leaked_env = process_env()
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| is_sanitized_env(k))
            .collect();
        let cli_on_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|d| dir_holds_subscription_cli(d))
            .collect();
        Self {
            home,
            real_home: real,
            leaked_env,
            cli_on_path,
        }
    }

    /// Problems that break the guarantee; empty means isolated.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        match (&self.home, &self.real_home) {
            (Some(home), Some(real)) if home != real && !real.starts_with(home) => {}
            _ => out.push("HOME is the real home".to_string()),
        }
        if !self.leaked_env.is_empty() {
            out.push(format!("set: {}", self.leaked_env.join(", ")));
        }
        if !self.cli_on_path.is_empty() {
            out.push(format!(
                "subscription CLI on PATH: {}",
                self.cli_on_path
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out
    }
}

/// `(level, label, detail)` rows for `forge doctor`.
pub async fn doctor_rows(
    config: Option<&forge_config::Config>,
) -> Vec<(crate::commands::doctor::Level, &'static str, String)> {
    use crate::commands::doctor::Level;
    let proxy_cfg = config.map(|c| c.proxy.clone()).unwrap_or_default();
    let active = forge_config::proxy_mode();
    let settings = match resolve_settings(None, None, &env_lookup, &proxy_cfg) {
        Ok(s) => s,
        Err(e) => return vec![(Level::Fail, "proxy", e.to_string())],
    };
    let entries: Vec<&String> = config
        .map(|c| {
            c.models
                .iter()
                .filter(|(_, e)| e.key_env.as_deref() == Some(settings.key_env.as_str()))
                .map(|(n, _)| n)
                .collect()
        })
        .unwrap_or_default();
    if !active && entries.is_empty() && config.is_none_or(|c| c.proxy == Default::default()) {
        return vec![(
            Level::Ok,
            "proxy",
            "not configured (`forge proxy init` sets it up)".to_string(),
        )];
    }

    let health = probe(&settings.url).await;
    let mut rows = Vec::new();
    let dialects = if health.dialects.is_empty() {
        "none detected".to_string()
    } else {
        health.dialects.join(", ")
    };
    let models = health
        .models
        .map(|n| format!("; {n} models advertised"))
        .unwrap_or_default();
    let url_source = if active {
        format!(
            "pinned by forge proxy from {}",
            env_lookup(URL_SOURCE_ENV).unwrap_or_else(|| "env".to_string())
        )
    } else {
        settings.url_source.as_str().to_string()
    };
    let mode = if active {
        "active"
    } else {
        "configured, not active in this process (run commands as `forge proxy …`)"
    };
    let pinned_model = config.map(|c| c.model.clone()).unwrap_or_default();
    let model_note = if entries.iter().any(|n| **n == pinned_model) {
        String::new()
    } else if entries.is_empty() {
        "; no model entry reads the proxy key — run `forge proxy init`".to_string()
    } else {
        format!("; default model {pinned_model:?} is not a proxy entry")
    };
    rows.push(if health.reachable {
        (
            if model_note.is_empty() { Level::Ok } else { Level::Warn },
            "proxy",
            format!(
                "{mode} — {} ({url_source}) reachable; dialects: {dialects}{models}; {} entries via ${}{model_note}",
                settings.url,
                entries.len(),
                settings.key_env,
            ),
        )
    } else {
        (
            if active { Level::Fail } else { Level::Warn },
            "proxy",
            format!(
                "{mode} — {} ({url_source}) is not reachable; start the proxy or tunnel it to this machine",
                settings.url,
            ),
        )
    });

    if active {
        let isolation = Isolation::current();
        let problems = isolation.problems();
        rows.push(if problems.is_empty() {
            (
                Level::Ok,
                "proxy isolation",
                format!(
                    "subscription credentials hidden: HOME={} (real home not visible), CLI \
                     credential stores disabled in-process (~/.claude, ~/.codex, ~/.kimi-code, \
                     Keychain), no provider key/base-URL env vars set, no subscription CLI on \
                     PATH; local_only and router_escalate=\"off\" pinned",
                    isolation
                        .home
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
            )
        } else {
            (
                Level::Fail,
                "proxy isolation",
                format!("not isolated: {}", problems.join("; ")),
            )
        });
    }
    rows
}

#[cfg(test)]
mod tests;
