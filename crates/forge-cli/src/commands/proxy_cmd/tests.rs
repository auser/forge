use std::collections::HashMap;

use clap::Parser;

use super::*;
use crate::cli::{Cli, Command, ProxyCommand};

fn settings() -> ProxySettings {
    ProxySettings {
        url: "http://127.0.0.1:8001".into(),
        url_source: SettingSource::Default,
        key_env: DEFAULT_KEY_ENV.into(),
        key_env_source: SettingSource::Default,
        api_key: "dummy".into(),
    }
}

fn env_of(pairs: &[(&str, &str)]) -> Vec<(String, OsString)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), OsString::from(v)))
        .collect()
}

fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<&str, &str> = pairs.iter().copied().collect();
    move |k| map.get(k).map(|v| v.to_string())
}

// --- environment sanitisation ----------------------------------------------

#[test]
fn every_provider_key_var_is_cleared_and_home_path_are_rewritten() {
    let keys = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "MOONSHOT_API_KEY",
        "KIMI_API_KEY",
        "DEEPSEEK_API_KEY",
        "OPENROUTER_API_KEY",
        "TYPESAFE_API_KEY",
        "FORGE_MODEL_BASE_URL",
        "FORGE_ROUTER_URL",
        "FORGE_JEV_KEY_ENV",
    ];
    let mut current: Vec<(String, OsString)> = keys
        .iter()
        .map(|k| (k.to_string(), OsString::from("secret")))
        .collect();
    current.extend(env_of(&[
        ("HOME", "/home/u"),
        (
            "PATH",
            "/home/u/.local/bin:/home/u/.cargo/bin:/opt/claude/bin:/opt/homebrew/bin:/usr/bin:relative",
        ),
        ("TERM", "xterm"),
        ("FORGE_MODEL", "sonnet"),
    ]));
    let probe = Probe {
        holds_subscription_cli: &|d: &Path| d == Path::new("/opt/claude/bin"),
        is_dir: &|d: &Path| d == Path::new("/home/u/.cargo"),
    };
    let plan = env_plan(&current, Path::new("/home/u"), &settings(), &probe);

    for key in keys {
        assert!(
            plan.remove.contains(&key.to_string()),
            "{key} must be cleared"
        );
        assert!(!plan.set.contains_key(key), "{key} must not be re-set");
    }
    for untouched in ["TERM", "FORGE_MODEL"] {
        assert!(!plan.remove.contains(&untouched.to_string()));
        assert!(!plan.set.contains_key(untouched));
    }
    assert_eq!(plan.set["HOME"], OsString::from("/home/u/.forge-home"));
    assert_eq!(
        plan.set["XDG_CONFIG_HOME"],
        OsString::from("/home/u/.config")
    );
    assert_eq!(
        plan.set["PATH"],
        OsString::from("/opt/homebrew/bin:/usr/bin:/usr/local/bin:/bin"),
        "home dirs, CLI dirs and relative entries dropped; system dirs kept"
    );
    assert_eq!(plan.set[PROXY_MODE_ENV], OsString::from("1"));
    assert_eq!(
        plan.set[PROXY_URL_ENV],
        OsString::from("http://127.0.0.1:8001")
    );
    assert_eq!(plan.set[URL_SOURCE_ENV], OsString::from("default"));
    assert_eq!(plan.set[DEFAULT_KEY_ENV], OsString::from("dummy"));
    assert_eq!(plan.set[REAL_HOME_ENV], OsString::from("/home/u"));
    assert_eq!(plan.set["CARGO_HOME"], OsString::from("/home/u/.cargo"));
    assert!(
        !plan.set.contains_key("RUSTUP_HOME"),
        "only toolchain dirs that exist are exported"
    );
}

#[test]
fn a_custom_key_env_is_exported_and_never_on_the_cleared_list() {
    let s = ProxySettings {
        key_env: "MY_PROXY_KEY".into(),
        api_key: "k".into(),
        ..settings()
    };
    let probe = Probe {
        holds_subscription_cli: &|_| false,
        is_dir: &|_| false,
    };
    let plan = env_plan(&env_of(&[("HOME", "/h")]), Path::new("/h"), &s, &probe);
    assert_eq!(plan.set["MY_PROXY_KEY"], OsString::from("k"));
    assert_eq!(
        plan.set[PROXY_KEY_ENV_NAME_ENV],
        OsString::from("MY_PROXY_KEY")
    );
    assert_eq!(plan.set[PROXY_API_KEY_ENV], OsString::from("k"));
}

#[test]
fn an_xdg_config_home_elsewhere_is_kept_but_never_one_inside_the_sandbox() {
    let probe = Probe {
        holds_subscription_cli: &|_| false,
        is_dir: &|_| false,
    };
    let custom = env_plan(
        &env_of(&[("XDG_CONFIG_HOME", "/srv/cfg")]),
        Path::new("/h"),
        &settings(),
        &probe,
    );
    assert_eq!(custom.set["XDG_CONFIG_HOME"], OsString::from("/srv/cfg"));
    let nested = env_plan(
        &env_of(&[("XDG_CONFIG_HOME", "/h/.forge-home/.config")]),
        Path::new("/h"),
        &settings(),
        &probe,
    );
    assert_eq!(nested.set["XDG_CONFIG_HOME"], OsString::from("/h/.config"));
}

#[test]
fn a_nested_forge_proxy_reuses_the_real_home_instead_of_nesting_the_sandbox() {
    let env = lookup(&[("HOME", "/h/.forge-home"), ("FORGE_REAL_HOME", "/h")]);
    assert_eq!(real_home(&env), Some(PathBuf::from("/h")));
    let env = lookup(&[("HOME", "/h")]);
    assert_eq!(real_home(&env), Some(PathBuf::from("/h")));
}

// --- argument passthrough --------------------------------------------------

fn proxied(argv: &[&str]) -> (Vec<OsString>, Option<ProxyCommand>, Option<String>) {
    let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
    let Some(Command::Proxy(args)) = cli.command else {
        panic!("{argv:?} did not parse as `forge proxy`");
    };
    (
        child_args(&cli.global, &args.forge_args),
        args.command,
        args.url,
    )
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn forge_args_pass_through_unchanged() {
    assert_eq!(proxied(&["forge", "proxy"]).0, os(&[]), "no args → chat");
    assert_eq!(
        proxied(&[
            "forge",
            "proxy",
            "run",
            "create hello.py",
            "--max-turns",
            "3"
        ])
        .0,
        os(&["run", "create hello.py", "--max-turns", "3"])
    );
    assert_eq!(proxied(&["forge", "proxy", "doctor"]).0, os(&["doctor"]));
    // Global flags clap consumed — before or after `proxy` — are re-emitted.
    assert_eq!(
        proxied(&["forge", "proxy", "--model", "claude-opus-5", "run", "x"]).0,
        os(&["--model", "claude-opus-5", "run", "x"])
    );
    assert_eq!(
        proxied(&["forge", "-v", "--json", "proxy", "doctor"]).0,
        os(&["-v", "--json", "doctor"])
    );
    // Flags after the forge subcommand stay where they were.
    assert_eq!(
        proxied(&["forge", "proxy", "run", "--model", "sonnet", "x"]).0,
        os(&["run", "--model", "sonnet", "x"])
    );
}

#[test]
fn proxy_flags_are_the_proxys_and_init_is_its_subcommand() {
    let (args, command, url) =
        proxied(&["forge", "proxy", "--url", "http://127.0.0.1:9", "run", "x"]);
    assert_eq!(args, os(&["run", "x"]));
    assert!(command.is_none());
    assert_eq!(url.as_deref(), Some("http://127.0.0.1:9"));

    let (_, command, _) = proxied(&["forge", "proxy", "init", "--url", "http://localhost:8001"]);
    assert!(matches!(
        command,
        Some(ProxyCommand::Init { url: Some(ref u), .. }) if u == "http://localhost:8001"
    ));
    // `forge init` itself, run through the proxy.
    let (args, command, _) = proxied(&["forge", "proxy", "--", "init"]);
    assert!(command.is_none());
    assert_eq!(args, os(&["init"]));
}

#[test]
fn proxy_collides_with_no_existing_command_or_flag() {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let proxies = cmd
        .get_subcommands()
        .filter(|s| s.get_name() == "proxy")
        .count();
    assert_eq!(proxies, 1);
    // `router serve` and `--local-only` are untouched and still parse.
    assert!(Cli::try_parse_from(["forge", "router", "serve"]).is_ok());
    assert!(Cli::try_parse_from(["forge", "--local-only", "doctor"]).is_ok());
    for arg in cmd.get_arguments() {
        assert_ne!(arg.get_long(), Some("proxy"));
        assert_ne!(
            arg.get_long(),
            Some("url"),
            "--url is proxy-scoped, not global"
        );
    }
}

// --- precedence ------------------------------------------------------------

#[test]
fn url_precedence_is_flag_then_env_then_config_then_default() {
    let config = forge_config::ProxyConfig {
        url: Some("http://127.0.0.1:3".into()),
        key_env: None,
    };
    let env = lookup(&[("FORGE_PROXY_URL", "http://127.0.0.1:2")]);
    let none = lookup(&[]);

    let s = resolve_settings(Some("http://127.0.0.1:1/v1/"), None, &env, &config).unwrap();
    assert_eq!(
        (s.url.as_str(), s.url_source),
        ("http://127.0.0.1:1", SettingSource::Flag)
    );
    let s = resolve_settings(None, None, &env, &config).unwrap();
    assert_eq!(
        (s.url.as_str(), s.url_source),
        ("http://127.0.0.1:2", SettingSource::Env)
    );
    let s = resolve_settings(None, None, &none, &config).unwrap();
    assert_eq!(
        (s.url.as_str(), s.url_source),
        ("http://127.0.0.1:3", SettingSource::Config)
    );
    let s = resolve_settings(None, None, &none, &Default::default()).unwrap();
    assert_eq!(
        (s.url.as_str(), s.url_source),
        ("http://127.0.0.1:8001", SettingSource::Default)
    );
}

#[test]
fn key_precedence_and_the_dummy_default() {
    let config = forge_config::ProxyConfig {
        url: None,
        key_env: Some("CFG_KEY".into()),
    };
    let none = lookup(&[]);
    let s = resolve_settings(None, Some("FLAG_KEY"), &none, &config).unwrap();
    assert_eq!(
        (s.key_env.as_str(), s.key_env_source),
        ("FLAG_KEY", SettingSource::Flag)
    );
    assert_eq!(s.api_key, "dummy");

    let s = resolve_settings(None, None, &none, &config).unwrap();
    assert_eq!(
        (s.key_env.as_str(), s.key_env_source),
        ("CFG_KEY", SettingSource::Config)
    );

    let env = lookup(&[("FORGE_PROXY_API_KEY", "from-forge-proxy-api-key")]);
    let s = resolve_settings(None, None, &env, &Default::default()).unwrap();
    assert_eq!(s.key_env, "FORGE_PROXY_API_KEY");
    assert_eq!(s.api_key, "from-forge-proxy-api-key");

    // A named key_env's own value beats FORGE_PROXY_API_KEY.
    let env = lookup(&[("CFG_KEY", "own"), ("FORGE_PROXY_API_KEY", "generic")]);
    assert_eq!(
        resolve_settings(None, None, &env, &config).unwrap().api_key,
        "own"
    );
}

#[test]
fn a_remote_url_or_a_provider_key_name_is_refused() {
    let none = lookup(&[]);
    let err = resolve_settings(
        Some("https://api.anthropic.com"),
        None,
        &none,
        &Default::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("not on this machine") && err.contains("ssh"),
        "{err}"
    );
    assert!(resolve_settings(Some("ftp://127.0.0.1"), None, &none, &Default::default()).is_err());
    for bad in ["ANTHROPIC_API_KEY", "lower", "1ABC"] {
        assert!(
            resolve_settings(None, Some(bad), &none, &Default::default()).is_err(),
            "{bad}"
        );
    }
    assert!(
        resolve_settings(
            Some("http://localhost:8001"),
            None,
            &none,
            &Default::default()
        )
        .is_ok()
    );
    assert!(resolve_settings(Some("http://[::1]:8001"), None, &none, &Default::default()).is_ok());
}

// --- init ------------------------------------------------------------------

const EXISTING: &str = r#"# my forge config
model = "claude-sonnet-5"   # keep me
local_only = false
router_escalate = "off"          # a comment that must survive, and not count as a change
approval = "prompt-dangerous"

[needle]
variant = "full"

# a hand-written entry, not the proxy's
[models.my-local]
base_url = "http://127.0.0.1:8080/v1"

[models.sonnet]
description = "my own words"
base_url = "http://127.0.0.1:9999/v1"
max_context = 100000
"#;

fn plan() -> ModelPlan {
    ModelPlan {
        anthropic: vec!["claude-sonnet-5-5".into(), "claude-opus-5-5".into()],
        openai: vec!["sonnet".into()],
        origin: "test".into(),
    }
}

#[test]
fn init_writes_the_entries_and_keeps_everything_else() {
    let (text, changes) = apply_init(EXISTING, &settings(), &plan()).unwrap();
    let parsed: toml::Table = toml::from_str(&text).unwrap();

    // The rest of the file survives, comments included.
    assert!(text.contains("# my forge config"));
    assert!(text.contains("# keep me"));
    assert!(text.contains("# a hand-written entry, not the proxy's"));
    assert_eq!(
        parsed["model"].as_str(),
        Some("claude-sonnet-5"),
        "an existing model is kept"
    );
    assert_eq!(parsed["approval"].as_str(), Some("prompt-dangerous"));
    assert_eq!(parsed["needle"]["variant"].as_str(), Some("full"));
    assert_eq!(
        parsed["models"]["my-local"]["base_url"].as_str(),
        Some("http://127.0.0.1:8080/v1")
    );

    assert_eq!(parsed["local_only"].as_bool(), Some(true));
    assert_eq!(parsed["router_escalate"].as_str(), Some("off"));
    assert_eq!(
        parsed["proxy"]["url"].as_str(),
        Some("http://127.0.0.1:8001")
    );
    assert_eq!(
        parsed["proxy"]["key_env"].as_str(),
        Some("FORGE_PROXY_API_KEY")
    );

    let anthropic = &parsed["models"]["claude-opus-5-5"];
    assert_eq!(anthropic["provider"].as_str(), Some("anthropic"));
    assert_eq!(
        anthropic["base_url"].as_str(),
        Some("http://127.0.0.1:8001"),
        "no /v1"
    );
    assert_eq!(anthropic["key_env"].as_str(), Some("FORGE_PROXY_API_KEY"));
    assert_eq!(anthropic["tools"].as_bool(), Some(true));

    // An existing entry of the same name is repointed, its own words kept.
    let openai = &parsed["models"]["sonnet"];
    assert_eq!(
        openai["base_url"].as_str(),
        Some("http://127.0.0.1:8001/v1"),
        "with /v1"
    );
    assert!(openai.get("provider").is_none());
    assert_eq!(openai["description"].as_str(), Some("my own words"));
    assert_eq!(openai["max_context"].as_integer(), Some(100_000));

    assert!(
        changes.iter().any(|c| c == "local_only = true (was false)"),
        "{changes:?}"
    );
    assert!(
        !changes.iter().any(|c| c.starts_with("router_escalate")),
        "an equal value with a trailing comment is not a change: {changes:?}"
    );
    assert!(text.contains("# a comment that must survive"));
    assert!(
        changes
            .iter()
            .any(|c| c
                == "[models.claude-opus-5-5] added (Anthropic Messages, http://127.0.0.1:8001)"),
        "{changes:?}"
    );
    assert!(
        changes
            .iter()
            .any(|c| c.starts_with("models.sonnet.base_url = ")),
        "an existing entry reports what changed in it: {changes:?}"
    );

    // The result is a valid forge config.
    let _: forge_config::Config = toml::Value::Table(parsed).try_into().unwrap();
}

#[test]
fn init_is_idempotent() {
    let (once, first) = apply_init(EXISTING, &settings(), &plan()).unwrap();
    assert!(!first.is_empty());
    let (twice, second) = apply_init(&once, &settings(), &plan()).unwrap();
    assert!(
        second.is_empty(),
        "a second run changes nothing: {second:?}"
    );
    assert_eq!(once, twice, "byte-identical");

    // Same from an empty file, which also gets a default model.
    let (fresh, _) = apply_init("", &settings(), &plan()).unwrap();
    let (again, changes) = apply_init(&fresh, &settings(), &plan()).unwrap();
    assert!(changes.is_empty());
    assert_eq!(fresh, again);
    let parsed: toml::Table = toml::from_str(&fresh).unwrap();
    assert_eq!(parsed["model"].as_str(), Some("claude-sonnet-5-5"));
}

#[test]
fn init_repoints_entries_when_the_url_changes() {
    let (once, _) = apply_init(EXISTING, &settings(), &plan()).unwrap();
    let tunnel = ProxySettings {
        url: "http://127.0.0.1:18001".into(),
        ..settings()
    };
    let (moved, changes) = apply_init(&once, &tunnel, &plan()).unwrap();
    let parsed: toml::Table = toml::from_str(&moved).unwrap();
    assert_eq!(
        parsed["proxy"]["url"].as_str(),
        Some("http://127.0.0.1:18001")
    );
    assert_eq!(
        parsed["models"]["claude-sonnet-5-5"]["base_url"].as_str(),
        Some("http://127.0.0.1:18001")
    );
    assert!(
        changes
            .iter()
            .any(|c| c.contains("was \"http://127.0.0.1:8001\"")),
        "{changes:?}"
    );
}

fn advertised(ids: &[(&str, i64)]) -> Vec<Advertised> {
    ids.iter()
        .map(|(id, created)| Advertised {
            id: id.to_string(),
            created: *created,
        })
        .collect()
}

#[test]
fn discovery_picks_the_newest_model_per_family_and_the_bare_aliases() {
    let list = advertised(&[
        ("claude-haiku-5-5", 50),
        ("claude-sonnet-5-5", 40),
        ("claude-opus-5-5", 30),
        ("claude-opus-5", 20),
        ("claude-sonnet-5", 10),
        ("claude-haiku-4-5-20251001", 5),
        ("claude-3-5-sonnet-latest", 1),
        ("o3-mini", 0),
        ("sonnet", 0),
        ("opus", 0),
        ("haiku", 0),
    ]);
    let plan = plan_models(Some(&list), &[]);
    assert_eq!(
        plan.anthropic,
        vec!["claude-haiku-5-5", "claude-opus-5-5", "claude-sonnet-5-5"]
    );
    assert_eq!(plan.openai, vec!["haiku", "opus", "sonnet"]);

    let fallback = plan_models(None, &[]);
    assert_eq!(
        fallback.anthropic,
        vec!["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"]
    );
    assert_eq!(fallback.openai, vec!["opus", "sonnet", "haiku"]);

    let explicit = plan_models(Some(&list), &["claude-opus-5".into(), "sonnet".into()]);
    assert_eq!(explicit.anthropic, vec!["claude-opus-5"]);
    assert_eq!(explicit.openai, vec!["sonnet"]);

    // A proxy serving no Claude ids: its models on the OpenAI dialect.
    let other = plan_models(
        Some(&advertised(&[("gpt-4o", 2), ("llama-3.3-70b", 1)])),
        &[],
    );
    assert!(other.anthropic.is_empty());
    assert_eq!(other.openai, vec!["gpt-4o", "llama-3.3-70b"]);
}

#[test]
fn the_models_list_parses_in_both_list_shapes() {
    let body = serde_json::json!({"object": "list", "data": [
        {"id": "claude-opus-5", "object": "model", "created": 7},
        {"id": "sonnet", "type": "model"},
        {"no": "id"}
    ]});
    assert_eq!(
        parse_models(&body),
        advertised(&[("claude-opus-5", 7), ("sonnet", 0)])
    );
    assert!(parse_models(&serde_json::json!({"error": "nope"})).is_empty());
}

// --- doctor ----------------------------------------------------------------

#[test]
fn isolation_problems_name_what_leaks() {
    let ok = Isolation {
        home: Some("/h/.forge-home".into()),
        real_home: Some("/h".into()),
        leaked_env: vec![],
        cli_on_path: vec![],
    };
    assert!(ok.problems().is_empty());
    let leaky = Isolation {
        home: Some("/h".into()),
        real_home: None,
        leaked_env: vec!["ANTHROPIC_API_KEY".into()],
        cli_on_path: vec!["/h/.local/bin".into()],
    };
    let problems = leaky.problems().join("; ");
    assert!(problems.contains("HOME is the real home"), "{problems}");
    assert!(problems.contains("ANTHROPIC_API_KEY"), "{problems}");
    assert!(problems.contains("/h/.local/bin"), "{problems}");
}
