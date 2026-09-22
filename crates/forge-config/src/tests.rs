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

    assert_eq!(resolved.config.model, "mock-local");
    assert_eq!(resolved.config.router, "static");
    assert_eq!(resolved.config.router_timeout_ms, 5_000);
    assert_eq!(resolved.config.execution, "native");
    assert_eq!(resolved.config.approval, "prompt");
    assert!(!resolved.config.local_only);
    assert_eq!(resolved.config.server_host, "127.0.0.1");
    assert_eq!(resolved.config.server_port, 7_341);
    assert_eq!(
        resolved.explain("model"),
        Some(("\"mock-local\"".to_string(), Origin::Default))
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
        Some(("\"static\"".to_string(), Origin::Default))
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
