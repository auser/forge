use super::*;
use serial_test::serial;

struct EnvGuard {
    vars: Vec<&'static str>,
}

impl EnvGuard {
    /// Point user config at a temp dir and clear every FORGE_* variable so
    /// tests are hermetic.
    fn isolated(xdg: &Path) -> Self {
        let mut vars: Vec<&'static str> = ENV_KEYS.iter().map(|(name, _)| *name).collect();
        vars.push("XDG_CONFIG_HOME");
        for name in &vars {
            unsafe { std::env::remove_var(name) };
        }
        unsafe { std::env::set_var("XDG_CONFIG_HOME", xdg) };
        Self { vars }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for name in &self.vars {
            unsafe { std::env::remove_var(name) };
        }
    }
}

fn write_user_config(dir: &Path, contents: &str) {
    let path = dir.join("forge");
    std::fs::create_dir_all(&path).expect("mkdir user config dir");
    std::fs::write(path.join("config.toml"), contents).expect("write user config");
}

fn write_project_config(root: &Path, contents: &str) {
    let path = root.join(".forge");
    std::fs::create_dir_all(&path).expect("mkdir project config dir");
    std::fs::write(path.join("config.toml"), contents).expect("write project config");
}

#[test]
#[serial]
fn defaults_when_nothing_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());

    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");

    // Default stack: local oMLX model + embedded Needle router (mocks are
    // opt-in).
    assert_eq!(resolved.config.model, "qwen3-coder");
    assert_eq!(
        resolved.config.model_base_url.as_deref(),
        Some("http://127.0.0.1:8080/v1")
    );
    assert_eq!(resolved.config.router, "needle");
    assert_eq!(resolved.config.router_timeout_ms, 5_000);
    assert_eq!(resolved.config.execution, "native");
    assert_eq!(resolved.config.approval, "prompt");
    assert!(!resolved.config.local_only);
    assert_eq!(resolved.config.server_host, "127.0.0.1");
    assert_eq!(resolved.config.server_port, 7_341);
    assert_eq!(
        resolved.explain("model"),
        Some(("\"qwen3-coder\"".to_string(), Origin::Default))
    );
    // Built-in model registry with cost metadata.
    let models = resolved.config.model_entries();
    assert_eq!(models.len(), 5);
    assert_eq!(models["qwen3-coder"].cost_input_per_mtok, 0.0);
    assert_eq!(models["deepseek-chat"].cost_input_per_mtok, 0.14);
    assert_eq!(
        models["kimi-k2.7-code"].key_env.as_deref(),
        Some("MOONSHOT_API_KEY")
    );
    assert_eq!(
        resolved.explain("models").map(|(_, o)| o),
        Some(Origin::Default)
    );
}

#[test]
#[serial]
fn project_file_overrides_user_file_and_defaults() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let xdg = tmp.path().join("xdg");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let _guard = EnvGuard::isolated(&xdg);

    write_user_config(&xdg, "model = \"user-model\"\napproval = \"deny\"\n");
    write_project_config(&project, "model = \"proj-model\"\n");

    let resolved = Config::load(Some(&project), &CliOverrides::default()).expect("load");

    assert_eq!(resolved.config.model, "proj-model");
    assert_eq!(
        resolved.explain("model"),
        Some(("\"proj-model\"".to_string(), Origin::ProjectFile))
    );
    // Only in the user file: user file wins over defaults.
    assert_eq!(resolved.config.approval, "deny");
    assert_eq!(
        resolved.explain("approval"),
        Some(("\"deny\"".to_string(), Origin::UserFile))
    );
    // Untouched key stays default.
    assert_eq!(
        resolved.explain("router"),
        Some(("\"needle\"".to_string(), Origin::Default))
    );
}

#[test]
#[serial]
fn env_overrides_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let xdg = tmp.path().join("xdg");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let _guard = EnvGuard::isolated(&xdg);

    write_user_config(&xdg, "model = \"user-model\"\n");
    write_project_config(&project, "model = \"proj-model\"\nlocal_only = false\n");
    unsafe {
        std::env::set_var("FORGE_MODEL", "env-model");
        std::env::set_var("FORGE_LOCAL_ONLY", "true");
        std::env::set_var("FORGE_SERVER_PORT", "8080");
    }

    let resolved = Config::load(Some(&project), &CliOverrides::default()).expect("load");

    assert_eq!(resolved.config.model, "env-model");
    assert_eq!(
        resolved.explain("model"),
        Some(("\"env-model\"".to_string(), Origin::Environment))
    );
    assert!(resolved.config.local_only);
    assert_eq!(
        resolved.explain("local_only"),
        Some(("true".to_string(), Origin::Environment))
    );
    assert_eq!(resolved.config.server_port, 8080);
}

#[test]
#[serial]
fn cli_overrides_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());
    unsafe { std::env::set_var("FORGE_MODEL", "env-model") };

    let overrides = CliOverrides {
        model: Some("cli-model".to_string()),
        ..CliOverrides::default()
    };
    let resolved = Config::load(Some(tmp.path()), &overrides).expect("load");

    assert_eq!(resolved.config.model, "cli-model");
    assert_eq!(
        resolved.explain("model"),
        Some(("\"cli-model\"".to_string(), Origin::CliFlag))
    );
}

#[test]
#[serial]
fn unknown_keys_are_tolerated_and_explainable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());
    write_project_config(tmp.path(), "custom_thing = \"hello\"\nanswer = 42\n");

    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");

    assert_eq!(
        resolved.config.extra.get("custom_thing"),
        Some(&toml::Value::String("hello".to_string()))
    );
    assert_eq!(
        resolved.explain("custom_thing"),
        Some(("\"hello\"".to_string(), Origin::ProjectFile))
    );
    assert_eq!(resolved.explain("no_such_key"), None);
}

#[test]
#[serial]
fn invalid_env_bool_is_a_config_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());
    unsafe { std::env::set_var("FORGE_LOCAL_ONLY", "maybe") };

    let err = Config::load(Some(tmp.path()), &CliOverrides::default()).expect_err("must fail");
    assert!(matches!(err, ForgeError::Config(_)));
}

#[test]
#[serial]
fn unparseable_project_file_is_a_config_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());
    write_project_config(tmp.path(), "model = [unclosed\n");

    let err = Config::load(Some(tmp.path()), &CliOverrides::default()).expect_err("must fail");
    assert!(matches!(err, ForgeError::Config(_)));
}

#[test]
#[serial]
fn max_turns_default_env_and_invalid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());

    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.max_turns, 25);
    assert_eq!(
        resolved.explain("max_turns"),
        Some(("25".to_string(), Origin::Default))
    );

    unsafe { std::env::set_var("FORGE_MAX_TURNS", "7") };
    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.max_turns, 7);
    assert_eq!(
        resolved.explain("max_turns"),
        Some(("7".to_string(), Origin::Environment))
    );

    unsafe { std::env::set_var("FORGE_MAX_TURNS", "lots") };
    let err = Config::load(Some(tmp.path()), &CliOverrides::default()).expect_err("must fail");
    assert!(matches!(err, ForgeError::Config(_)));
}

#[test]
#[serial]
fn models_table_deep_merges_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let xdg = tmp.path().join("xdg");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    let _guard = EnvGuard::isolated(&xdg);

    write_user_config(
        &xdg,
        "[models.shared]\ncost_input_per_mtok = 1.0\n\n[models.user-only]\ncost_input_per_mtok = 2.0\n",
    );
    write_project_config(
        &project,
        "[models.shared]\ncost_input_per_mtok = 9.0\ndescription = \"project wins\"\n\n[models.proj-only]\ncost_input_per_mtok = 0.5\n",
    );

    let resolved = Config::load(Some(&project), &CliOverrides::default()).expect("load");
    let models = &resolved.config.models;
    // 5 built-in defaults + 3 from the layered files.
    assert_eq!(models.len(), 8);
    // Project entry replaces the same-named user entry entirely.
    assert_eq!(models["shared"].cost_input_per_mtok, 9.0);
    assert_eq!(
        models["shared"].description.as_deref(),
        Some("project wins")
    );
    assert_eq!(models["user-only"].cost_input_per_mtok, 2.0);
    assert_eq!(models["proj-only"].cost_input_per_mtok, 0.5);
    assert_eq!(
        resolved.explain("models").map(|(_, o)| o),
        Some(Origin::ProjectFile)
    );
}

/// Regression test: the generic nested-section merge added for `[needle]`
/// must never leak stale per-model dotted sources for `[models]`, which has
/// its own dedicated name-keyed merge path. Before the fix, a project-file
/// override of a built-in model landed correctly in `resolved.config` but
/// `explain("models.qwen3-coder")` still reported the stale default value
/// with `Origin::Default`, because the generic branch (mis-)handled the
/// very first (defaults) layer for "models" before the dedicated branch
/// ever got a chance to run.
#[test]
#[serial]
fn models_override_does_not_leak_stale_dotted_source() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let xdg = tmp.path().join("xdg");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    let _guard = EnvGuard::isolated(&xdg);

    write_project_config(
        &project,
        "[models.qwen3-coder]\ncost_input_per_mtok = 999.0\n",
    );

    let resolved = Config::load(Some(&project), &CliOverrides::default()).expect("load");
    // The override is correctly applied to the resolved config...
    assert_eq!(
        resolved.config.models["qwen3-coder"].cost_input_per_mtok,
        999.0
    );
    // ...and per-model dotted keys are never exposed via explain (models
    // only ever gets the aggregate "models" source), so there is no stale
    // default value to leak.
    assert_eq!(resolved.explain("models.qwen3-coder"), None);
    assert_eq!(
        resolved.explain("models").map(|(_, o)| o),
        Some(Origin::ProjectFile)
    );
}

#[test]
fn needle_defaults() {
    let c = Config::default();
    // "full" is the only variant with a hosted, pinned artifact today, so
    // a fresh `forge init` actually fetches working weights out of the
    // box. See the design spec's risks section for reverting this once
    // Cactus hosts a smaller rung.
    assert_eq!(c.needle.variant, "full");
    assert_eq!(c.needle.weights_path, "");
    assert!(c.needle.autofetch);
    assert_eq!(c.needle.weights_sha256, "");
}

#[test]
fn needle_section_parses_and_validates() {
    let c: Config =
        toml::from_str("[needle]\nvariant = \"small\"\nautofetch = false").expect("parses");
    assert_eq!(c.needle.variant, "small");
    assert!(!c.needle.autofetch);
    assert!(c.validate().is_ok());
}

#[test]
fn needle_invalid_variant_names_valid_values() {
    let c: Config = toml::from_str("[needle]\nvariant = \"tiny\"").expect("parses");
    let err = c
        .validate()
        .expect_err("invalid variant rejected")
        .to_string();
    assert!(err.contains("tiny") && err.contains("small") && err.contains("full"));
}

#[test]
fn needle_weights_sha256_parses_and_validates() {
    // Empty (the default) and a well-formed 64-char hex string both pass.
    let empty: Config = toml::from_str("[needle]\nvariant = \"full\"").expect("parses");
    assert!(empty.validate().is_ok());

    let good: Config = toml::from_str(&format!(
        "[needle]\nvariant = \"full\"\nweights_sha256 = \"{}\"",
        "a".repeat(64)
    ))
    .expect("parses");
    assert_eq!(good.needle.weights_sha256, "a".repeat(64));
    assert!(good.validate().is_ok());

    // Wrong length and non-hex characters are both rejected, and the
    // error names the offending field.
    let too_short: Config = toml::from_str("[needle]\nweights_sha256 = \"abcd\"").expect("parses");
    let err = too_short
        .validate()
        .expect_err("short hash rejected")
        .to_string();
    assert!(err.contains("weights_sha256"), "err: {err}");

    let not_hex: Config = toml::from_str(&format!(
        "[needle]\nweights_sha256 = \"{}\"",
        "z".repeat(64)
    ))
    .expect("parses");
    let err = not_hex
        .validate()
        .expect_err("non-hex rejected")
        .to_string();
    assert!(err.contains("weights_sha256"), "err: {err}");
}

#[test]
#[serial]
fn needle_env_overrides_and_explain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());

    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.needle.variant, "full");
    assert_eq!(
        resolved.explain("needle.variant"),
        Some(("\"full\"".to_string(), Origin::Default))
    );
    assert_eq!(
        resolved.explain("needle.autofetch"),
        Some(("true".to_string(), Origin::Default))
    );

    unsafe {
        std::env::set_var("FORGE_NEEDLE_VARIANT", "small");
        std::env::set_var("FORGE_NEEDLE_AUTOFETCH", "false");
    }
    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.needle.variant, "small");
    assert!(!resolved.config.needle.autofetch);
    // weights_path untouched by env should still resolve to its default,
    // even though only two of three needle fields were overridden.
    assert_eq!(resolved.config.needle.weights_path, "");
    assert_eq!(
        resolved.explain("needle.variant"),
        Some(("\"small\"".to_string(), Origin::Environment))
    );
    assert_eq!(
        resolved.explain("needle.autofetch"),
        Some(("false".to_string(), Origin::Environment))
    );

    unsafe { std::env::set_var("FORGE_NEEDLE_VARIANT", "bogus") };
    let err = Config::load(Some(tmp.path()), &CliOverrides::default()).expect_err("must fail");
    assert!(matches!(err, ForgeError::Config(_)));
}

#[test]
#[serial]
fn router_confidence_threshold_and_fallback_defaults_and_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());

    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.router_confidence_threshold, 0.7);
    assert_eq!(resolved.config.router_fallback, "static");
    assert!(resolved.config.router_autostart);
    unsafe { std::env::set_var("FORGE_ROUTER_AUTOSTART", "false") };
    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert!(!resolved.config.router_autostart);
    assert_eq!(
        resolved.explain("router_autostart"),
        Some(("false".to_string(), Origin::Environment))
    );

    unsafe { std::env::set_var("FORGE_ROUTER_CONFIDENCE_THRESHOLD", "0.9") };
    unsafe { std::env::set_var("FORGE_ROUTER_FALLBACK", "cheapest") };
    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.router_confidence_threshold, 0.9);
    assert_eq!(resolved.config.router_fallback, "cheapest");

    unsafe { std::env::set_var("FORGE_ROUTER_CONFIDENCE_THRESHOLD", "high") };
    let err = Config::load(Some(tmp.path()), &CliOverrides::default()).expect_err("must fail");
    assert!(matches!(err, ForgeError::Config(_)));
}

#[test]
fn router_escalate_defaults_to_auto_and_validates() {
    let c = Config::default();
    assert_eq!(c.router_escalate, "auto");
    assert!(c.validate().is_ok());

    let off: Config = toml::from_str("router_escalate = \"off\"").expect("parses");
    assert_eq!(off.router_escalate, "off");
    assert!(off.validate().is_ok());
}

#[test]
fn router_escalate_invalid_value_names_valid_values() {
    let c: Config = toml::from_str("router_escalate = \"always\"").expect("parses");
    let err = c
        .validate()
        .expect_err("invalid router_escalate rejected")
        .to_string();
    assert!(
        err.contains("always") && err.contains("auto") && err.contains("off"),
        "err: {err}"
    );
}

#[test]
#[serial]
fn router_escalate_env_override_and_explain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::isolated(tmp.path());

    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.router_escalate, "auto");
    assert_eq!(
        resolved.explain("router_escalate"),
        Some(("\"auto\"".to_string(), Origin::Default))
    );

    unsafe { std::env::set_var("FORGE_ROUTER_ESCALATE", "off") };
    let resolved = Config::load(Some(tmp.path()), &CliOverrides::default()).expect("load");
    assert_eq!(resolved.config.router_escalate, "off");
    assert_eq!(
        resolved.explain("router_escalate"),
        Some(("\"off\"".to_string(), Origin::Environment))
    );

    unsafe { std::env::set_var("FORGE_ROUTER_ESCALATE", "bogus") };
    let err = Config::load(Some(tmp.path()), &CliOverrides::default()).expect_err("must fail");
    assert!(matches!(err, ForgeError::Config(_)));
}

/// A `[models.<name>]` entry that spells the endpoint/key fields with
/// their *top-level* names parses as TOML and then silently does nothing —
/// the entry has no endpoint, and every request lands on whatever the
/// global `model_base_url` says. That's a UX trap, so validation rejects
/// it and names the field that was meant.
#[test]
fn model_entry_with_top_level_key_names_is_rejected_by_name() {
    for (wrong, right) in WRONG_MODEL_ENTRY_KEYS {
        let c: Config = toml::from_str(&format!(
            "[models.my-model]\n{wrong} = \"whatever\"\ncost_input_per_mtok = 0.0\n"
        ))
        .expect("parses as TOML");
        let err = c
            .validate()
            .expect_err("wrong key name must be rejected")
            .to_string();
        assert!(err.contains("my-model"), "err: {err}");
        assert!(err.contains(wrong), "err must name the wrong key: {err}");
        assert!(err.contains(right), "err must name the right key: {err}");
    }
}

#[test]
fn model_entry_with_correct_key_names_validates() {
    let c: Config = toml::from_str(
        "[models.my-model]\nbase_url = \"http://127.0.0.1:8080/v1\"\nkey_env = \"MY_KEY\"\n",
    )
    .expect("parses");
    c.validate().expect("correct field names are valid");
    let entry = c.models.get("my-model").expect("entry");
    assert_eq!(entry.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
    assert_eq!(entry.key_env.as_deref(), Some("MY_KEY"));
    assert!(entry.extra.is_empty(), "extra: {:?}", entry.extra);
}

/// Every shipped preset in `examples/configs/` must deserialize into the
/// current `Config` and pass `validate()`. Presets are the copy-paste
/// starting point for new users, so a preset that no longer matches the
/// config shape is a first-run failure waiting to happen.
#[test]
fn every_shipped_preset_parses_and_validates() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("configs");
    let mut seen = 0usize;
    for entry in std::fs::read_dir(&dir).expect("examples/configs is readable") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        seen += 1;
        let text = std::fs::read_to_string(&path).expect("read preset");
        let config: Config = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("{} does not parse as Config: {e}", path.display()));
        config
            .validate()
            .unwrap_or_else(|e| panic!("{} fails validation: {e}", path.display()));
        // Needle-era default: only the preset that is explicitly about the
        // Laya adapter may still set `router = "laya"`.
        let is_laya_preset = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.contains("laya"));
        assert!(
            config.router != "laya" || is_laya_preset,
            "{} sets router = \"laya\" but is not the laya preset",
            path.display()
        );
    }
    assert!(seen >= 4, "expected the shipped presets, found {seen}");
}
