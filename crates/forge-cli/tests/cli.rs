use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FORGE_ENV_VARS: &[&str] = &[
    "FORGE_MODEL",
    "FORGE_ROUTER",
    "FORGE_ROUTER_URL",
    "FORGE_ROUTER_KEY_ENV",
    "FORGE_EXECUTION",
    "FORGE_APPROVAL",
    "FORGE_LOCAL_ONLY",
    "FORGE_SERVER_HOST",
    "FORGE_SERVER_PORT",
    // needle config/env knobs (see forge-config's ENV_KEYS and
    // forge-needle's weights.rs/lib.rs): removed so a developer's shell
    // can't perturb a supposedly-hermetic run (e.g. a real
    // FORGE_NEEDLE_WEIGHTS_BASE_URL pointed at a personal mirror, or
    // FORGE_NEEDLE_AUTOFETCH=true left set from other work).
    "FORGE_NEEDLE_VARIANT",
    "FORGE_NEEDLE_AUTOFETCH",
    "FORGE_NEEDLE_WEIGHTS_SHA256",
    "FORGE_NEEDLE_BACKEND",
    "FORGE_NEEDLE_WEIGHTS_BASE_URL",
    "FORGE_NEEDLE_TEST_SHA256",
    // Mock providers are test-only and refused by configuration unless this
    // is set; scrubbed here and set back below, so the value is this
    // harness's, never the developer's shell's.
    "FORGE_TEST_MOCKS",
    // OpenRouter catalogue endpoint override (see
    // `forge_providers::openrouter::BASE_URL_ENV`): scrubbed, then pointed
    // at a dead port below, so `forge init`'s best-effort catalogue refresh
    // fails fast instead of reaching openrouter.ai.
    "FORGE_OPENROUTER_BASE_URL",
];

/// A `forge` invocation isolated from the developer's real user
/// config/environment/cache: `HOME` and `XDG_CONFIG_HOME` point at temp
/// subdirs, FORGE_*/FORGE_NEEDLE_* vars are removed, and autofetch is
/// forced off. Without this, `forge init` (which defaults to
/// `router = "needle"` with `needle.autofetch = true`) would resolve
/// `~/.cache/forge/models/` to the developer's *real* home directory and
/// attempt a real fetch from Hugging Face on every test run touching init.
/// This must hold independent of whether the build linked an engine — the
/// engine check in `forge init` itself (see
/// `commands::init::needle_weights_item`) already prevents the fetch in an
/// engine-less build, but hermeticity here must not depend on that; an
/// engine-linked test run must stay just as isolated.
fn forge(tmp: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
    for var in FORGE_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env("HOME", tmp.join("home"));
    cmd.env("XDG_CONFIG_HOME", tmp.join("xdg"));
    cmd.env("FORGE_NEEDLE_AUTOFETCH", "false");
    // `forge init` also refreshes the OpenRouter catalogue (best-effort):
    // a dead port fails that fast and offline, and a test wanting a real
    // refresh overrides this with its own wiremock URI via `.env(...)`.
    cmd.env("FORGE_OPENROUTER_BASE_URL", "http://127.0.0.1:9");
    // These tests configure `model = "mock-local"` / `"scripted-mock"`,
    // which `model_from_config` refuses without the explicit opt-in.
    cmd.env("FORGE_TEST_MOCKS", "1");
    cmd.env("NO_COLOR", "1");
    cmd
}

#[test]
fn context_memory_inspection_is_json_read_only_and_session_explicit() {
    use forge_core::{Event, EventKind, SessionStore};
    let tmp = tempfile::tempdir().unwrap();
    let sessions = forge_session::JsonlSessionStore::new(tmp.path().join(".forge/sessions"));
    sessions
        .append(Event::new(
            "r",
            "session-a",
            EventKind::InputReceived {
                message: "UNIQUE_SOURCE_TEXT_NOT_STATUS sk-abcdefghijklmnop".into(),
            },
        ))
        .unwrap();
    for command in [
        vec!["context", "status"],
        vec!["memory", "status"],
        vec!["memory", "show"],
        vec!["memory", "sources"],
    ] {
        let output = forge(tmp.path())
            .arg("--project")
            .arg(tmp.path())
            .args(&command)
            .args(["--session", "session-a", "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value.is_object());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("UNIQUE_SOURCE_TEXT_NOT_STATUS"));
        assert!(!text.contains("sk-abcdefghijklmnop"));
        assert!(!text.contains('\u{1b}'));
        assert!(!tmp.path().join(".forge/context").exists());

        let missing = forge(tmp.path())
            .arg("--project")
            .arg(tmp.path())
            .args(&command)
            .output()
            .unwrap();
        assert!(!missing.status.success());
    }
    let refused = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args(["memory", "on", "--session", "session-a", "--json"])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "on must not bypass disabled project observer"
    );
    assert_eq!(sessions.events_for("session-a").unwrap().len(), 1);
}

#[test]
fn corrupt_session_inspection_never_prints_private_error_data() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join(".forge/sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let secret = "PRIVATE_UNKNOWN_EVENT_TYPE_sk-secret123456";
    std::fs::write(
        session_dir.join("corrupt.jsonl"),
        serde_json::json!({
            "v": 9, "seq": 1, "ts": "2026-10-10T00:00:00Z",
            "session_id": "corrupt", "run_id": "r", "type": secret,
        })
        .to_string(),
    )
    .unwrap();
    for command in [
        ["context", "status"],
        ["memory", "status"],
        ["memory", "show"],
        ["memory", "sources"],
        ["memory", "on"],
        ["memory", "off"],
    ] {
        let output = forge(tmp.path())
            .arg("--project")
            .arg(tmp.path())
            .args(command)
            .args(["--session", "corrupt", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!printed.contains(secret), "{printed}");
        assert!(!printed.contains(tmp.path().to_str().unwrap()), "{printed}");
        assert!(
            printed.contains("session inspection unavailable"),
            "{printed}"
        );
    }
}

#[test]
fn cli_memory_consent_persists_without_starting_observer_jobs() {
    use forge_core::{Event, EventKind, SessionStore};
    let tmp = tempfile::tempdir().unwrap();
    let sessions = forge_session::JsonlSessionStore::new(tmp.path().join(".forge/sessions"));
    sessions
        .append(Event::new(
            "r",
            "session-a",
            EventKind::InputReceived {
                message: "fixture".into(),
            },
        ))
        .unwrap();
    std::fs::write(
        tmp.path().join(".forge/config.toml"),
        concat!(
            "[observer]\nenabled=true\nmodel='mock-local'\n",
            "[models.mock-local]\ncost_input_per_mtok=0.0\ncost_output_per_mtok=0.0\n"
        ),
    )
    .unwrap();
    for (verb, expected) in [("on", true), ("off", false)] {
        let output = forge(tmp.path())
            .arg("--project")
            .arg(tmp.path())
            .args(["memory", verb, "--session", "session-a", "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["desired_enabled"], expected);
        let check = forge(tmp.path())
            .arg("--project")
            .arg(tmp.path())
            .args(["memory", "status", "--session", "session-a", "--json"])
            .output()
            .unwrap();
        assert!(check.status.success());
        let value: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
        assert_eq!(value["desired_enabled"], expected);
        assert_eq!(value["live_prompt_injection"], false);
        assert!(
            !tmp.path().join(".forge/context").exists(),
            "inspection/control started a worker or mutated derived storage"
        );
    }
    let unknown = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args(["memory", "off", "--session", "unknown"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(!tmp.path().join(".forge/sessions/unknown.jsonl").exists());
}

#[test]
fn consolidated_memory_is_durable_searchable_and_promoted_only_with_confirmation() {
    use forge_context::{
        FsObservationStore, ObservationDraft, ObservationKind, ObservationScope, ObservationStore,
        SourceRange, ValidatedObservationBatch,
    };
    use forge_core::{Event, EventKind, SessionStore};
    let tmp = tempfile::tempdir().unwrap();
    let sessions = forge_session::JsonlSessionStore::new(tmp.path().join(".forge/sessions"));
    for session in ["session-a", "session-b"] {
        sessions
            .append(Event::new(
                "r",
                session,
                EventKind::InputReceived {
                    message: "source fixture".into(),
                },
            ))
            .unwrap();
    }
    let source = sessions.events_for("session-a").unwrap();
    let observations = FsObservationStore::new(tmp.path().join(".forge/context"));
    observations
        .commit(
            ValidatedObservationBatch::new(
                "session-a",
                &source,
                SourceRange { start: 1, end: 1 },
                "observer-v1",
                vec![ObservationDraft {
                    scope: ObservationScope::Session,
                    kind: ObservationKind::Decision,
                    content: "use the lexical durable memory index".into(),
                }],
                sessions.redactor(),
            )
            .unwrap(),
        )
        .unwrap();

    let consolidated = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args(["memory", "consolidate", "--session", "session-a", "--json"])
        .output()
        .unwrap();
    assert!(
        consolidated.status.success(),
        "{}",
        String::from_utf8_lossy(&consolidated.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&consolidated.stdout).unwrap();
    assert_eq!(report["claims_written"], 1);
    let projection = observations
        .projection("session-a", &source, sessions.redactor())
        .unwrap();
    assert_eq!(projection.tombstoned().len(), 1);
    let status = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args(["memory", "status", "--session", "session-a", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["consolidated_memory"]["state"], "available");
    assert_eq!(status["consolidated_memory"]["value"]["session_claims"], 1);

    let search = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args([
            "memory",
            "search",
            "--session",
            "session-a",
            "--query",
            "lexical",
            "--json",
        ])
        .output()
        .unwrap();
    let matches: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(matches[0]["project_memory"], false);
    assert_eq!(matches[0]["session_id"], "session-a");

    let refused = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args([
            "memory",
            "promote",
            "--session",
            "session-a",
            "--topic",
            "decisions",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("requires --yes"));
    let promoted = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args([
            "memory",
            "promote",
            "--session",
            "session-a",
            "--topic",
            "decisions",
            "--yes",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(promoted.status.success());
    std::fs::remove_file(tmp.path().join(".forge/sessions/session-a.jsonl")).unwrap();
    let project_search = forge(tmp.path())
        .arg("--project")
        .arg(tmp.path())
        .args([
            "memory",
            "search",
            "--session",
            "session-b",
            "--query",
            "lexical",
            "--json",
        ])
        .output()
        .unwrap();
    let matches: serde_json::Value = serde_json::from_slice(&project_search.stdout).unwrap();
    assert_eq!(matches[0]["project_memory"], true);
    assert!(!tmp.path().join("AGENTS.md").exists());
}

#[test]
fn version_prints_name_and_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = forge(tmp.path()).arg("version").output().expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(stdout.starts_with("forge "), "unexpected: {stdout}");
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn version_build_identifies_the_exact_binary_without_changing_plain_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = forge(tmp.path())
        .args(["version", "--build"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(stdout.starts_with("forge "), "unexpected: {stdout}");
    assert!(stdout.contains("commit "), "unexpected: {stdout}");
    assert!(stdout.contains("target "), "unexpected: {stdout}");

    let output = forge(tmp.path())
        .args(["--json", "version", "--build"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        value["commit"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(
        value["target"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
}

#[cfg(unix)]
#[test]
fn auth_login_delegates_to_the_official_cli_and_detects_its_store() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&bin).expect("bin");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nmkdir -p \"$HOME/.claude\"\nprintf '%s' \
         '{\"claudeAiOauth\":{\"accessToken\":\"test-oauth\"}}' \
         > \"$HOME/.claude/.credentials.json\"\n",
    )
    .expect("script");
    let mut permissions = std::fs::metadata(&claude).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&claude, permissions).expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["auth", "login", "claude"])
        .env("PATH", path)
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("authenticated with Claude"));
}

#[test]
fn config_explain_reports_environment_origin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["config", "explain", "model"])
        .env("FORGE_MODEL", "env-model")
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(stdout.trim(), "model = \"env-model\" (source: environment)");
}

#[test]
fn cli_flag_beats_env_and_reports_cli_flag_origin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["--model", "cli-model", "config", "explain", "model"])
        .env("FORGE_MODEL", "env-model")
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(stdout.trim(), "model = \"cli-model\" (source: cli-flag)");
}

#[test]
fn json_config_show_is_pure_json_on_stdout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["--json", "config", "show"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(parsed["config"]["model"], "qwen3-coder");
    assert_eq!(parsed["sources"]["model"]["origin"], "default");
}

#[test]
fn init_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let first = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .arg("init")
        .output()
        .expect("run");
    assert!(first.status.success());
    let first_stdout = String::from_utf8(first.stdout).expect("utf8");
    assert!(
        first_stdout.contains("created"),
        "first run: {first_stdout}"
    );

    let second = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .arg("init")
        .output()
        .expect("run");
    assert!(second.status.success());
    let second_stdout = String::from_utf8(second.stdout).expect("utf8");
    assert!(
        !second_stdout.contains("created"),
        "second run must change nothing: {second_stdout}"
    );

    let gitignore = std::fs::read_to_string(project.join(".gitignore")).expect("gitignore");
    assert_eq!(
        gitignore.lines().filter(|l| *l == ".forge/").count(),
        1,
        "gitignore must contain exactly one .forge/ line"
    );
    assert!(project.join(".forge/graph").is_dir());
    assert!(project.join(".forge/sessions").is_dir());
    assert!(project.join(".forge/config.toml").is_file());
}

#[test]
fn bare_forge_bootstraps_a_fresh_project_before_chat() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["--model", "mock-local"])
        .stdin(Stdio::null())
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(project.join(".forge/config.toml").is_file());
    assert!(project.join(".forge/graph/graph.json").is_file());
    assert!(project.join(".forge/sessions").is_dir());
}

#[test]
fn compiled_forge_initializes_then_edits_and_validates_a_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::write(project.join("main.rs"), "fn value() -> u8 { 1 }\n").expect("source");

    // Start with no Forge state and use the compiled CLI for initialization,
    // so this covers the same boundary as a newly installed binary.
    let init = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .arg("init")
        .output()
        .expect("init");
    assert!(
        init.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(project.join(".forge/config.toml").is_file());

    std::fs::write(
        project.join("script.json"),
        r#"[
          {"tool_calls":[{"id":"edit-1","name":"edit_file","arguments":{
            "path":"main.rs",
            "old":"fn value() -> u8 { 1 }",
            "new":"fn value() -> u8 { 2 }"
          }}]},
          {"tool_calls":[{"id":"check-1","name":"run_command","arguments":{
            "command":"sh",
            "args":["-c","pwd; grep -q 'value() -> u8 { 2 }' main.rs"],
            "risk":"risky"
          }}]},
          {"text":"Implemented and validated the change."}
        ]"#,
    )
    .expect("script");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"scripted-mock\"\nmock_script = \"script.json\"\n\
         router = \"static\"\napproval = \"prompt-dangerous\"\n",
    )
    .expect("config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "make value return two and validate it"])
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(project.join("main.rs")).expect("changed source"),
        "fn value() -> u8 { 2 }\n"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Implemented and validated"));
    let sessions = std::fs::read_dir(project.join(".forge/sessions"))
        .expect("sessions")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .map(|entry| std::fs::read_to_string(entry.path()).expect("session log"))
        .collect::<String>();
    let canonical_project = std::fs::canonicalize(&project).expect("canonical project");
    assert!(
        sessions.contains(&format!("stdout:\\n{}\\n", canonical_project.display())),
        "run_command must execute from --project:\n{sessions}"
    );
    assert!(
        sessions.contains(r#""tool":"run_command","output":"exit 0"#),
        "the validation command must actually succeed:\n{sessions}"
    );
}

#[test]
fn one_command_workflow_records_plan_checks_review_and_diff() {
    use forge_task::{JsonlTaskStore, TaskState, VerificationStatus};

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(project.join("main.rs"), "fn value() -> u8 { 1 }\n").expect("source");
    std::fs::write(
        project.join("check.sh"),
        "#!/bin/sh\ngrep -q 'value() -> u8 { 2 }' main.rs\n",
    )
    .expect("check");
    let init = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(&project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init: {init:?}");
    let add = Command::new("git")
        .current_dir(&project)
        .args(["add", "main.rs", "check.sh"])
        .output()
        .expect("git add");
    assert!(add.status.success(), "git add: {add:?}");

    std::fs::write(
        project.join("script.json"),
        r#"[
          {"tool_calls":[{"id":"inspect-1","name":"read_file","arguments":{"path":"main.rs"}}]},
          {"tool_calls":[{"id":"edit-1","name":"edit_file","arguments":{
            "path":"main.rs",
            "old":"fn value() -> u8 { 1 }",
            "new":"fn value() -> u8 { 2 }"
          }}]},
          {"tool_calls":[{"id":"check-1","name":"run_command","arguments":{
            "command":"sh","args":["check.sh"],"risk":"risky"
          }}]},
          {"tool_calls":[{"id":"diff-1","name":"run_command","arguments":{
            "command":"git","args":["diff","--no-ext-diff","--"],"risk":"safe"
          }}]},
          {"text":"Changed value to two; the focused check passed and the diff is ready for review."}
        ]"#,
    )
    .expect("script");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"scripted-mock\"\nmock_script = \"script.json\"\n\
         router = \"static\"\napproval = \"auto\"\n",
    )
    .expect("config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args([
            "--json",
            "run",
            "change value to two, check it, and review the diff",
        ])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json outcome");
    let task_id = outcome["task_id"].as_str().expect("task id");
    assert_eq!(outcome["changed_paths"], serde_json::json!(["main.rs"]));
    assert_eq!(outcome["checks"][0]["status"], "passed");
    assert!(
        outcome["review"]
            .as_str()
            .is_some_and(|review| review.contains("focused check passed"))
    );
    assert!(
        outcome["diff"]
            .as_str()
            .is_some_and(|diff| diff.contains("-fn value() -> u8 { 1 }")
                && diff.contains("+fn value() -> u8 { 2 }"))
    );

    let task = JsonlTaskStore::for_project(&project)
        .load(task_id)
        .expect("durable task");
    for node in ["inspect", "edit", "check", "review"] {
        assert_eq!(task.checkpoint.state(node), Some(TaskState::Succeeded));
    }
    assert!(
        task.checkpoint.nodes["check"]
            .verifications
            .iter()
            .any(|verification| verification.status == VerificationStatus::Passed)
    );

    let shown = forge(tmp.path())
        .arg("--project")
        .arg(&project)
        .args(["--json", "task", "show", task_id])
        .output()
        .expect("task show");
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("task json");
    assert_eq!(shown["state"], "succeeded");
    assert_eq!(shown["route"]["model"], "scripted-mock");
    assert_eq!(shown["changed_files"], serde_json::json!(["main.rs"]));
    assert_eq!(shown["checks"][0]["status"], "passed");
    assert!(
        shown["terminal_result"]
            .as_str()
            .is_some_and(|result| { result.contains("focused check passed") })
    );

    let listed = forge(tmp.path())
        .arg("--project")
        .arg(&project)
        .args(["--json", "task"])
        .output()
        .expect("task list");
    assert!(listed.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("task list json");
    assert_eq!(listed[0]["task_id"], task_id);
}

#[test]
fn task_id_resume_continues_an_approval_interruption() {
    use forge_task::{JsonlTaskStore, TaskState};

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join("script.json"),
        r#"[
          {"tool_calls":[{"id":"write-1","name":"write_file","arguments":{
            "path":"note.txt","content":"resumed safely\n"
          }}]},
          {"text":"The parked task resumed and completed."}
        ]"#,
    )
    .expect("script");
    let config = |approval: &str| {
        format!(
            "model = \"scripted-mock\"\nmock_script = \"script.json\"\n\
             router = \"static\"\napproval = \"{approval}\"\n"
        )
    };
    std::fs::write(project.join(".forge/config.toml"), config("prompt")).expect("config");

    let first = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "write the note"])
        .stdin(Stdio::null())
        .output()
        .expect("initial run");
    assert!(
        !first.status.success(),
        "initial run must park for approval"
    );
    assert!(!project.join("note.txt").exists());

    let store = JsonlTaskStore::for_project(&project);
    let task_id = std::fs::read_dir(store.root())
        .expect("task directory")
        .flatten()
        .find_map(|entry| entry.path().file_stem()?.to_str().map(str::to_string))
        .expect("task id");
    assert_eq!(
        store.load(&task_id).unwrap().checkpoint.state("edit"),
        Some(TaskState::Interrupted)
    );

    std::fs::write(project.join(".forge/config.toml"), config("auto")).expect("config");
    let resumed = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["resume", &task_id])
        .output()
        .expect("resume");
    assert!(
        resumed.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(project.join("note.txt")).expect("resumed effect"),
        "resumed safely\n"
    );
    assert_eq!(
        store.load(&task_id).unwrap().checkpoint.state("review"),
        Some(TaskState::Succeeded)
    );
}

#[tokio::test]
async fn explicit_multi_model_router_keeps_every_available_candidate() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn serve_model(server: &MockServer, reply: &str) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": []
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "completion",
                "model": "test",
                "choices": [{
                    "message": {"role": "assistant", "content": reply},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .mount(server)
            .await;
    }

    let expensive = MockServer::start().await;
    let cheap = MockServer::start().await;
    serve_model(&expensive, "expensive answered").await;
    serve_model(&cheap, "cheap answered").await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".forge")).expect("forge dir");
    std::fs::write(
        project.join(".forge/config.toml"),
        format!(
            r#"
router = "cheapest"
local_only = true
approval = "auto"

[models.a-expensive]
base_url = "{}/v1"
cost_input_per_mtok = 10.0
cost_output_per_mtok = 10.0
tools = true
streaming = false

[models.z-cheap]
base_url = "{}/v1"
cost_input_per_mtok = 1.0
cost_output_per_mtok = 1.0
tools = true
streaming = false
"#,
            expensive.uri(),
            cheap.uri()
        ),
    )
    .expect("config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "answer once"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("cheap answered"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let expensive_requests = expensive.received_requests().await.expect("requests");
    let cheap_requests = cheap.received_requests().await.expect("requests");
    assert_eq!(
        expensive_requests
            .iter()
            .filter(|request| request.method.as_str() == "POST")
            .count(),
        0
    );
    assert_eq!(
        cheap_requests
            .iter()
            .filter(|request| request.method.as_str() == "POST")
            .count(),
        1
    );
}

/// `forge init` must never fetch the ~35 MB needle weights artifact in a
/// build that has no inference engine to use it — when the workspace built
/// engine-less (`needle-sys` resolved nothing), it should report the skip
/// rather than silently succeeding after a real network fetch. An
/// engine-linked build takes the fetch path instead, so this test returns
/// early there.
#[test]
fn init_skips_needle_weights_fetch_without_an_engine() {
    if forge_needle::HAS_EMBEDDED_BACKEND {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .arg("init")
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(
        stdout.contains("no embedded inference backend"),
        "expected the engine-less skip message: {stdout}"
    );
    assert!(
        stdout.contains("skipped"),
        "expected the engine-less skip message: {stdout}"
    );
    assert!(
        !tmp.path().join("home/.cache/forge/models").exists(),
        "init must not create/fetch into the weights cache dir without an engine"
    );
}

/// A binary-content `.rs` file in the project (e.g. an accidentally
/// committed object file, or corrupted text) must not take down `forge
/// init`'s graph build: one bad byte in one file must never kill the run.
/// Regression test for the "stream did not contain valid UTF-8" crash.
#[test]
fn init_tolerates_a_binary_source_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join("src")).expect("mkdir");

    std::fs::write(
        project.join("src/main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .expect("write main.rs");
    // Every byte value 0..=255: guaranteed non-UTF-8, deterministic (no
    // `rand`, no flakiness), and disguised as a Rust source file.
    let binary: Vec<u8> = (0u8..=255u8).cycle().take(512).collect();
    std::fs::write(project.join("src/notes.rs"), &binary).expect("write binary .rs");

    let first = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .arg("init")
        .output()
        .expect("run");
    assert!(
        first.status.success(),
        "init must exit 0 despite a binary .rs file: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stdout = String::from_utf8(first.stdout).expect("utf8");
    assert!(
        first_stdout.contains("graph:") && first_stdout.contains("files"),
        "expected the graph build to be reported: {first_stdout}"
    );

    // `forge graph build` (invoked again via a second `init`) is
    // idempotent: nothing changed, so the graph is reported unchanged.
    let second = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .arg("init")
        .output()
        .expect("run");
    assert!(second.status.success(), "second init must also succeed");
    let second_stdout = String::from_utf8(second.stdout).expect("utf8");
    assert!(
        second_stdout.contains("graph: unchanged"),
        "second run must find the graph fresh: {second_stdout}"
    );
}

#[test]
fn serve_serves_health_on_ephemeral_port() {
    use std::io::{Read, Write};

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    // Reserve an ephemeral port, release it, then serve on it.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();

    let server_log = tmp.path().join("serve.log");
    let log_file = std::fs::File::create(&server_log).expect("log file");
    let mut child = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["serve", "--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .stdout(log_file.try_clone().expect("clone"))
        .stderr(log_file)
        .spawn()
        .expect("spawn forge serve");

    let mut body = String::new();
    let mut ok = false;
    for _ in 0..100 {
        if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            // A connection accepted before the server is really ready can
            // already be reset here (EINVAL/ECONNRESET on macOS); that is a
            // retry, not a test failure.
            if stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .is_err()
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            let attempt = stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .and_then(|()| stream.read_to_string(&mut body).map(|_| ()));
            if attempt.is_ok() && body.contains("\"status\":\"ok\"") {
                ok = true;
                break;
            }
            body.clear();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    child.kill().ok();
    child.wait().ok();
    let log = std::fs::read_to_string(&server_log).unwrap_or_default();
    assert!(ok, "server never came up; server log:\n{log}");
    assert!(body.contains("\"status\":\"ok\""), "body: {body}");
}

/// The third message path, and the one the user actually pasted:
///
/// ```text
/// WARN primary router failed; using fallback
///      error=router error: needle: needle weights missing at ~/.cache/forge/models/needle3.cact
///            (run `forge init` to fetch)
/// ```
///
/// In a build with no inference backend that hint is a dead end — this
/// binary's `forge init` skips the weights fetch precisely *because* there is
/// no backend, so following it changes nothing and the user is back where they
/// started. Verbose fallback diagnostics must name the real cause and the one
/// command that fixes it without polluting the default chat transcript.
///
/// Driven through the real binary rather than a unit test because the bug was
/// in the composition: each layer's message was defensible on its own, and
/// only the string that actually reaches a terminal shows whether the story
/// holds together.
#[test]
fn a_failed_needle_route_never_tells_a_backend_less_build_to_run_forge_init() {
    if forge_needle::HAS_EMBEDDED_BACKEND {
        return; // this binary has a backend; the hint is not reachable
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    // `router = "needle"` is the default; spelled out so the test does not
    // silently stop covering this if the default ever moves.
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"mock-local\"\nrouter = \"needle\"\n",
    )
    .expect("write config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        // Expected fallback is debug-level: visible on demand, silent by
        // default because the structured routing line already explains it.
        .args(["-vv", "run", "hello"])
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "a brain-less build must still complete the run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("primary router failed"),
        "expected the fallback diagnostic; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("no embedded inference backend"),
        "the warning must name the real cause; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("forge init"),
        "it must not send the reader to a `forge init` that skips the fetch; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cargo install"),
        "and it must carry the one command that fixes it; stderr:\n{stderr}"
    );
}

#[test]
fn run_works_offline_with_mock_model() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    // Mock is opt-in: request it explicitly.
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"mock-local\"\n",
    )
    .expect("write config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "hello", "world"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(stdout.trim(), "mock response to: hello world");

    // The session was persisted under .forge/sessions/ — one transcript,
    // plus its decision log (dispatch/routing records, not events).
    let sessions_dir = project.join(".forge").join("sessions");
    assert!(sessions_dir.is_dir());
    let entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("read sessions")
        .flatten()
        .collect();
    let transcripts = entries
        .iter()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".jsonl") && !name.ends_with(".decisions.jsonl")
        })
        .count();
    assert_eq!(transcripts, 1, "one session transcript: {entries:?}");
    assert!(
        entries.iter().any(|e| e
            .file_name()
            .to_string_lossy()
            .ends_with(".decisions.jsonl")),
        "the run's decision log should be there too: {entries:?}"
    );
}

/// The one session transcript of a project that ran exactly one run.
fn session_log(project: &Path) -> String {
    let sessions = project.join(".forge").join("sessions");
    let transcripts: Vec<_> = std::fs::read_dir(&sessions)
        .expect("sessions dir")
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".jsonl") && !name.ends_with(".decisions.jsonl")
        })
        .collect();
    assert_eq!(
        transcripts.len(),
        1,
        "one session transcript: {transcripts:?}"
    );
    std::fs::read_to_string(transcripts[0].path()).expect("read transcript")
}

/// Scaffold a mock-model project with one `demo` skill.
fn project_with_demo_skill(tmp: &Path) -> PathBuf {
    let project = tmp.join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"mock-local\"\n",
    )
    .expect("write config");
    let skill = project.join(".forge").join("skills").join("demo");
    std::fs::create_dir_all(&skill).expect("skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill description\n---\n# Demo\n\nDo the demo thing.\n",
    )
    .expect("write skill");
    project
}

/// `forge run --skill <name>` activates the skill explicitly: the prompt
/// shares no >=3-char token with the skill's name or description, so the
/// activation cannot have come from lexical matching.
#[test]
fn run_with_skill_activates_it_without_a_matching_prompt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = project_with_demo_skill(tmp.path());

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "--skill", "demo", "zz unrelated qq"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = session_log(&project);
    let activation = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "skill_activated")
        .unwrap_or_else(|| panic!("no skill_activated in session log: {log}"));
    assert_eq!(activation["name"], "demo");
}

/// An unknown `--skill` name is an error, not a run without the skill:
/// non-zero exit, the name on stderr, and no session events written.
#[test]
fn run_with_an_unknown_skill_fails_loudly_and_writes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = project_with_demo_skill(tmp.path());

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        // Flag after the prompt: both orders must parse.
        .args(["run", "zz unrelated qq", "--skill", "nosuch"])
        .output()
        .expect("run");
    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(stderr.contains("unknown skill: nosuch"), "stderr: {stderr}");

    // Entry validation precedes the session claim: nothing was written.
    let sessions = project.join(".forge").join("sessions");
    if sessions.is_dir() {
        let transcripts = std::fs::read_dir(&sessions)
            .expect("read sessions")
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(".jsonl") && !name.ends_with(".decisions.jsonl")
            })
            .count();
        assert_eq!(transcripts, 0, "a refused run leaves no transcript");
    }
}

#[test]
fn run_json_mode_is_pure_json_and_session_list_shows_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"mock-local\"\n",
    )
    .expect("write config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["--json", "run", "hi"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(parsed["text"], "mock response to: hi");
    let session_id = parsed["session_id"].as_str().expect("session id");
    let run_id = parsed["run_id"].as_str().expect("run id");

    let list = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["session", "list"])
        .output()
        .expect("run");
    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).expect("utf8");
    assert!(stdout.contains(session_id), "list output: {stdout}");
    // run_started, routing_decision_made, context_plan_recorded, usage_recorded,
    // assistant_message (the v3 replay record of the model's answer),
    // turn_completed, completed.
    assert!(stdout.contains("7 events"), "list output: {stdout}");
    let plan_path = project
        .join(".forge/context/plans")
        .join(run_id)
        .join("1.json");
    let plan = std::fs::read_to_string(plan_path).expect("context plan");
    assert!(!plan.contains("\"hi\""), "plan leaked prompt: {plan}");
    assert!(
        !plan.contains("mock response"),
        "plan leaked response: {plan}"
    );

    // resume continues the completed run: a NEW run in the same session,
    // replaying the session's conversation, printing the new run's output.
    let resume = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["resume", run_id])
        .output()
        .expect("run");
    assert!(
        resume.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resume.stderr)
    );
    let stdout = String::from_utf8(resume.stdout).expect("utf8");
    // The mock echoes the prompt it received, which for a resume is the
    // continuation instruction — the conversation itself is replayed as
    // history above it rather than re-asked.
    assert!(
        stdout.contains("Continue the work in the conversation above"),
        "resume output: {stdout}"
    );
    // The resumed run landed in the same session (session show reveals
    // both runs' events plus the resume marker).
    let show = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["session", "show", session_id])
        .output()
        .expect("run");
    let show_out = String::from_utf8(show.stdout).expect("utf8");
    assert!(
        show_out.contains("input_received"),
        "resume marker missing: {show_out}"
    );
    assert!(
        show_out.contains("context_plan_recorded"),
        "context plan event missing: {show_out}"
    );

    let cancel = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["cancel", run_id])
        .output()
        .expect("run");
    assert!(cancel.status.success());

    let show = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["session", "show", session_id])
        .output()
        .expect("run");
    assert!(show.status.success());
    let stdout = String::from_utf8(show.stdout).expect("utf8");
    assert!(stdout.contains("cancelled"), "show output: {stdout}");
}

#[test]
fn model_list_and_test_work_offline() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let list = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["model", "list"])
        .output()
        .expect("run");
    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).expect("utf8");
    assert!(stdout.contains("mock-local"), "list output: {stdout}");
    assert!(stdout.contains("tools=true"), "list output: {stdout}");

    let test = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["model", "test", "mock-local"])
        .output()
        .expect("run");
    assert!(test.status.success());
    let stdout = String::from_utf8(test.stdout).expect("utf8");
    assert!(stdout.contains("mock-local: ok"), "test output: {stdout}");
}

#[test]
fn graph_build_check_map_and_stale_detection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join("src")).expect("mkdir");
    std::fs::write(
        project.join("src/main.rs"),
        "use crate::util;\nfn main() {\n    util::help();\n}\n",
    )
    .expect("write");
    std::fs::write(project.join("src/util.rs"), "pub fn help() {}\n").expect("write");

    // init builds the graph.
    let init = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .arg("init")
        .output()
        .expect("run");
    assert!(init.status.success());
    assert!(project.join(".forge/graph/graph.json").is_file());

    // check: fresh, exit 0.
    let check = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["graph", "check"])
        .output()
        .expect("run");
    assert!(check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("fresh"));

    // map shows per-dir summary.
    let map = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["graph", "map"])
        .output()
        .expect("run");
    let stdout = String::from_utf8(map.stdout).expect("utf8");
    assert!(stdout.contains("src:"), "map: {stdout}");

    // grep finds a symbol.
    let grep = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["graph", "grep", "help"])
        .output()
        .expect("run");
    assert!(String::from_utf8_lossy(&grep.stdout).contains("help"));

    // blast: main.rs imports util.rs.
    let blast = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["graph", "blast", "src/util.rs"])
        .output()
        .expect("run");
    assert!(String::from_utf8_lossy(&blast.stdout).contains("src/main.rs"));

    // Edit a file → check exits 1 and reports stale.
    std::fs::write(
        project.join("src/util.rs"),
        "pub fn help() {\n    println!(\"x\");\n}\n",
    )
    .expect("write");
    let check = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["graph", "check"])
        .output()
        .expect("run");
    assert!(!check.status.success());
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(stdout.contains("stale"), "check: {stdout}");
    assert!(stdout.contains("modified"), "check: {stdout}");
}

#[test]
fn graph_semantic_grep_needs_needle_weights_without_an_engine() {
    // No engine available to this run (engine-less build, or engine-linked
    // but the hermetic HOME has no weights, and no FORGE_NEEDLE_BACKEND hook):
    // `engine_if_available` must report unavailable, and `graph grep
    // --semantic` must fail loudly rather than silently returning nothing.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join("src")).expect("mkdir");
    std::fs::write(project.join("src/main.rs"), "fn main() {}\n").expect("write");

    let build = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["graph", "build"])
        .output()
        .expect("run");
    assert!(build.status.success());

    let grep = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["graph", "grep", "--semantic", "main"])
        .output()
        .expect("run");
    assert!(!grep.status.success());
    let stderr = String::from_utf8_lossy(&grep.stderr);
    assert!(
        stderr.contains("semantic search needs needle weights (run forge init)"),
        "stderr: {stderr}"
    );
}

#[test]
fn graph_build_embeds_symbols_and_semantic_grep_ranks_by_meaning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join("src")).expect("mkdir");
    std::fs::write(
        project.join("src/parser.rs"),
        "pub fn parse_document(input: &str) -> usize {\n    input.len()\n}\n",
    )
    .expect("write");
    std::fs::write(
        project.join("src/color.rs"),
        "pub fn mix_paint_colors() -> u8 {\n    42\n}\n",
    )
    .expect("write");

    // An ordinary build remains structural even when an embedding backend is
    // available.
    let structural = forge(tmp.path())
        .env("FORGE_NEEDLE_BACKEND", "hash")
        .args(["--project"])
        .arg(&project)
        .args(["--json", "graph", "build"])
        .output()
        .expect("run");
    assert!(structural.status.success(), "{structural:?}");
    let structural_json: serde_json::Value =
        serde_json::from_slice(&structural.stdout).expect("structural build json");
    assert!(
        structural_json.get("embedded").is_none(),
        "structural build must not report embeddings: {structural_json}"
    );
    let index_path = project.join(".forge/graph/embeddings.bin");
    assert!(
        !index_path.exists(),
        "ordinary graph build must not create an embedding index"
    );

    // With --semantic, FORGE_NEEDLE_BACKEND=hash gives a real, working
    // (deterministic) engine, so the build must embed every symbol and write
    // the semantic index.
    let build = forge(tmp.path())
        .env("FORGE_NEEDLE_BACKEND", "hash")
        .args(["--project"])
        .arg(&project)
        .args(["--json", "graph", "build", "--semantic"])
        .output()
        .expect("run");
    assert!(build.status.success(), "{build:?}");
    let stdout = String::from_utf8_lossy(&build.stdout);
    let first: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    let first_embedded = first["embedded"].as_u64().expect("embedded count");
    assert!(first_embedded >= 2, "expected >=2 embedded, got {first}");

    assert!(
        index_path.is_file(),
        "embeddings.bin must exist after build"
    );

    // `graph grep --semantic` over a needle-shaped query: the hash
    // backend's deterministic trigram embeddings mean a query sharing
    // trigrams with "parse_document" (via the embedded text "function
    // parse_document in src/parser.rs") scores higher than the unrelated
    // "mix_paint_colors" symbol.
    let grep = forge(tmp.path())
        .env("FORGE_NEEDLE_BACKEND", "hash")
        .args(["--project"])
        .arg(&project)
        .args(["graph", "grep", "--semantic", "parse document"])
        .output()
        .expect("run");
    assert!(grep.status.success(), "{grep:?}");
    let grep_stdout = String::from_utf8_lossy(&grep.stdout);
    let parse_line = grep_stdout
        .lines()
        .position(|l| l.contains("src/parser.rs::parse_document"))
        .unwrap_or_else(|| panic!("parse_document missing from: {grep_stdout}"));
    let color_line = grep_stdout
        .lines()
        .position(|l| l.contains("src/color.rs::mix_paint_colors"))
        .unwrap_or_else(|| panic!("mix_paint_colors missing from: {grep_stdout}"));
    assert!(
        parse_line < color_line,
        "expected parse_document ranked above mix_paint_colors: {grep_stdout}"
    );

    // Second build with no source changes: every symbol's content hash is
    // unchanged, so nothing should be re-embedded.
    let rebuild = forge(tmp.path())
        .env("FORGE_NEEDLE_BACKEND", "hash")
        .args(["--project"])
        .arg(&project)
        .args(["--json", "graph", "build", "--semantic"])
        .output()
        .expect("run");
    assert!(rebuild.status.success());
    let rebuild_stdout = String::from_utf8_lossy(&rebuild.stdout);
    let second: serde_json::Value = serde_json::from_str(rebuild_stdout.trim()).expect("json");
    assert_eq!(
        second["embedded"].as_u64(),
        Some(0),
        "no-op rebuild must re-embed nothing: {second}"
    );
}

#[test]
fn skill_list_show_test_and_activation_logging() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    let skill_dir = project.join(".forge/skills/demo");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill for tests\n---\n# Demo\n\nDo the demo thing.\n",
    )
    .expect("write");
    std::fs::write(skill_dir.join("test.sh"), "echo demo-ok\n").expect("write");
    // Skill test scripts are Risky; approve automatically in this fixture.
    std::fs::write(project.join(".forge/config.toml"), "approval = \"auto\"\n")
        .expect("write config");

    let list = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["skill", "list"])
        .output()
        .expect("run");
    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).expect("utf8");
    assert!(
        stdout.contains("demo — Demo skill for tests"),
        "list: {stdout}"
    );

    let show = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["skill", "show", "demo"])
        .output()
        .expect("run");
    assert!(show.status.success());
    let stdout = String::from_utf8(show.stdout).expect("utf8");
    assert!(stdout.contains("Do the demo thing."), "show: {stdout}");

    // Activation was logged to the cli session.
    let cli_log = project.join(".forge/sessions/cli.jsonl");
    assert!(cli_log.is_file(), "activation log exists");
    let raw = std::fs::read_to_string(&cli_log).expect("read");
    assert!(raw.contains("\"skill_activated\""), "log: {raw}");
    assert!(raw.contains("demo"), "log: {raw}");

    // skill test runs the script through native execution.
    let test = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["skill", "test", "demo"])
        .output()
        .expect("run");
    assert!(
        test.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&test.stderr)
    );
    let stdout = String::from_utf8(test.stdout).expect("utf8");
    assert!(stdout.contains("exit 0"), "test: {stdout}");
    assert!(stdout.contains("demo-ok"), "test: {stdout}");

    // With --execution mock the request is recorded, not run.
    let mocked = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["--execution", "mock", "skill", "test", "demo"])
        .output()
        .expect("run");
    assert!(mocked.status.success());
    let stdout = String::from_utf8(mocked.stdout).expect("utf8");
    assert!(
        stdout.contains("mock execution recorded: sh"),
        "mock: {stdout}"
    );
}

#[test]
fn scripted_mock_loop_edits_file_and_emits_tool_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::write(project.join("main.rs"), "fn main() {}\n").expect("write");
    std::fs::write(
        project.join("script.json"),
        r#"[
            {"tool_calls": [{"id": "call_1", "name": "edit_file", "arguments": {"path": "main.rs", "old": "fn main() {}", "new": "fn main() { println!(\"hello\"); }"}}]},
            {"text": "added hello to main.rs"}
        ]"#,
    )
    .expect("write script");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir .forge");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"scripted-mock\"\nmock_script = \"script.json\"\napproval = \"auto\"\n",
    )
    .expect("write config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "add a hello function to main.rs"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(stdout.trim(), "added hello to main.rs");

    // The loop actually edited the file.
    let content = std::fs::read_to_string(project.join("main.rs")).expect("read");
    assert!(
        content.contains("println!(\"hello\")"),
        "content: {content}"
    );

    // And the full tool trail is in the session log. The directory also
    // holds the decision log (`*.decisions.jsonl`); the trail is in the
    // transcript.
    let transcript = std::fs::read_dir(project.join(".forge/sessions"))
        .expect("sessions")
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.ends_with(".jsonl") && !name.ends_with(".decisions.jsonl")
        })
        .expect("one session transcript");
    let log = std::fs::read_to_string(transcript).expect("log");
    for needle in [
        "tool_call_requested",
        "tool_started",
        "tool_completed",
        "file_changed",
        "turn_completed",
        "completed",
    ] {
        assert!(log.contains(needle), "missing {needle} in {log}");
    }
}

#[test]
fn max_turns_flag_bounds_the_loop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    // Script always requests another tool call.
    std::fs::write(
        project.join("script.json"),
        r#"[
            {"tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "a"}}]},
            {"tool_calls": [{"id": "c2", "name": "read_file", "arguments": {"path": "b"}}]},
            {"tool_calls": [{"id": "c3", "name": "read_file", "arguments": {"path": "c"}}]},
            {"tool_calls": [{"id": "c4", "name": "read_file", "arguments": {"path": "d"}}]}
        ]"#,
    )
    .expect("write script");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir .forge");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"scripted-mock\"\nmock_script = \"script.json\"\n",
    )
    .expect("write config");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "--max-turns", "2", "spin"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(
        stderr.contains("max turns (2) exhausted"),
        "stderr: {stderr}"
    );
}

#[test]
fn router_serve_help_and_prerequisite_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    // Help always works.
    let help = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["router", "serve", "--help"])
        .output()
        .expect("run");
    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout).expect("utf8");
    assert!(stdout.contains("Laya"), "help: {stdout}");

    // Branch on the real environment: laya importable or not.
    let laya_present = std::process::Command::new("python3")
        .args(["-c", "import laya"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !laya_present {
        let output = forge(tmp.path())
            .args(["--project"])
            .arg(&project)
            .args(["router", "serve"])
            .output()
            .expect("run");
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("utf8");
        assert!(stderr.contains("pip install laya"), "stderr was: {stderr}");
    } else {
        // Smoke: start the adapter on an ephemeral port, probe liveness, kill.
        // Child stdio goes to files so the adapter (a grandchild process)
        // can never hold the test harness's pipes open after cleanup.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let log = std::fs::File::create(tmp.path().join("router-serve.log")).expect("log");
        let mut child = forge(tmp.path())
            .args(["--project"])
            .arg(&project)
            .args(["router", "serve", "--port"])
            .arg(port.to_string())
            .stdout(log.try_clone().expect("clone"))
            .stderr(log)
            .spawn()
            .expect("spawn");

        // Laya preloads its model checkpoint on first start — allow 120s.
        let mut up = false;
        for _ in 0..600 {
            if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                use std::io::{Read, Write};
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .expect("timeout");
                let mut body = String::new();
                let attempt = stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .and_then(|()| stream.read_to_string(&mut body).map(|_| ()));
                if attempt.is_ok() && body.contains("\"status\": \"ok\"") {
                    up = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        child.kill().ok();
        child.wait().ok();
        // Kill the adapter grandchild too (it outlives forge otherwise).
        std::process::Command::new("pkill")
            .args(["-f", &format!("laya-http.py {port}")])
            .output()
            .ok();
        let log_text =
            std::fs::read_to_string(tmp.path().join("router-serve.log")).unwrap_or_default();
        assert!(up, "adapter never came up; log:\n{log_text}");
    }
}

#[test]
fn dotenv_loading_precedence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::write(project.join(".env"), "FORGE_MODEL=dotenv-model\n").expect("write .env");

    // .env provides the model; origin is environment.
    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["config", "explain", "model"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(
        stdout.trim(),
        "model = \"dotenv-model\" (source: environment)"
    );

    // .env.local overrides .env.
    std::fs::write(
        project.join(".env.local"),
        "FORGE_MODEL=local-override-model\n",
    )
    .expect("write .env.local");
    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["config", "explain", "model"])
        .output()
        .expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(
        stdout.trim(),
        "model = \"local-override-model\" (source: environment)"
    );

    // A real shell env var beats both files.
    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["config", "explain", "model"])
        .env("FORGE_MODEL", "shell-model")
        .output()
        .expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(
        stdout.trim(),
        "model = \"shell-model\" (source: environment)"
    );
}

#[test]
fn dotenv_loaded_keys_are_redacted_from_session_logs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::write(
        project.join(".env"),
        "FORGE_MODEL=mock-local\nFORGE_ROUTER=static\nDEEPSEEK_API_KEY=dotenvvalue98765432\n",
    )
    .expect("write .env");

    // The key matches no built-in redaction pattern; only the
    // store's env snapshot (taken after .env bootstrap) can redact it.
    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "the key is dotenvvalue98765432 ok"])
        .output()
        .expect("run");
    assert!(output.status.success());

    let sessions = project.join(".forge").join("sessions");
    let mut log = String::new();
    for entry in std::fs::read_dir(&sessions).expect("sessions dir") {
        log.push_str(&std::fs::read_to_string(entry.expect("entry").path()).expect("read"));
    }
    assert!(
        !log.contains("dotenvvalue98765432"),
        "key leaked into session log: {log}"
    );
    assert!(log.contains("[REDACTED]"), "expected redaction: {log}");
}

#[test]
fn serve_laya_autostart() {
    let laya_present = std::process::Command::new("python3")
        .args(["-c", "import laya"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");

    let free_port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port()
    };
    let server_port = free_port();
    let adapter_port = free_port();

    std::fs::write(
        project.join(".forge/config.toml"),
        format!(
            "model = \"mock-local\"\nrouter = \"laya\"\nrouter_url = \"http://127.0.0.1:{adapter_port}/decide\"\nrouter_timeout_ms = 1000\n"
        ),
    )
    .expect("write config");

    let log_path = tmp.path().join("serve-autostart.log");
    let log = std::fs::File::create(&log_path).expect("log file");
    let mut child = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["serve", "--host", "127.0.0.1", "--port"])
        .arg(server_port.to_string())
        .stdout(log.try_clone().expect("clone"))
        .stderr(log)
        .spawn()
        .expect("spawn forge serve");

    let http_get = |port: u16, path: &str| -> Option<String> {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        use std::io::{Read, Write};
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok()?;
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .ok()?;
        let mut body = String::new();
        stream.read_to_string(&mut body).ok()?;
        Some(body)
    };

    // Wait for the server (adapter preload can take 2 minutes cold).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(360);
    let mut health = None;
    while std::time::Instant::now() < deadline {
        if let Some(body) = http_get(server_port, "/health")
            && body.contains("\"status\":\"ok\"")
        {
            health = Some(body);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    if !laya_present {
        // Server must still come up; the adapter stays down (warned).
        assert!(health.is_some(), "server did not start without laya");
        let log_text = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log_text.contains("laya adapter not started") || log_text.contains("listening on"),
            "log: {log_text}"
        );
    } else {
        assert!(
            health.is_some(),
            "server did not start; log: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        // The adapter was autostarted and answers liveness. The server's own
        // /health going green does not imply the adapter has bound its port
        // yet — it is a separate process the server only spawns — so poll for
        // it instead of sampling once. (Sampling once passed when this test
        // ran alone and failed under a loaded parallel test run.)
        let adapter_deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut adapter = None;
        while std::time::Instant::now() < adapter_deadline {
            if let Some(body) = http_get(adapter_port, "/")
                && body.contains("\"status\": \"ok\"")
            {
                adapter = Some(body);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        assert!(
            adapter.is_some(),
            "adapter did not answer liveness on port {adapter_port}; log: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        // The server itself works end to end.
        let run = {
            let mut stream =
                std::net::TcpStream::connect(("127.0.0.1", server_port)).expect("connect");
            use std::io::{Read, Write};
            let body = r#"{"prompt":"hi"}"#;
            let request = format!(
                "POST /v1/runs HTTP/1.1\r\nHost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(request.as_bytes()).expect("write");
            let mut response = String::new();
            stream.read_to_string(&mut response).expect("read");
            response
        };
        assert!(run.contains("202"), "run response: {run}");
    }

    // SIGINT → graceful shutdown → adapter must be gone.
    std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .output()
        .ok();
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(_)) => break,
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    child.kill().ok();
    child.wait().ok();
    if laya_present {
        let gone = std::net::TcpStream::connect(("127.0.0.1", adapter_port)).is_err();
        if !gone {
            // Cleanup fallback so the machine never keeps an orphan.
            std::process::Command::new("pkill")
                .args(["-f", &format!("laya-http.py {adapter_port}")])
                .output()
                .ok();
        }
        assert!(gone, "adapter still listening on {adapter_port}");
    }
}

#[test]
fn run_fast_paths_a_tool_prompt_through_the_needle_brain() {
    // End-to-end proof that `forge run` wires the brain into AgentService:
    // with FORGE_NEEDLE_BACKEND=hash the deterministic engine fills a
    // `read_file` call for an exact "<tool>: <json>" prompt, so the run is
    // answered by the tool itself and the model is never called.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::write(project.join("hello.txt"), "hello from disk\n").expect("write");
    let prompt = "read_file: {\"path\": \"hello.txt\"}";
    let args = [
        "--model",
        "mock-local",
        "--router",
        "static",
        "--json",
        "run",
        prompt,
    ];
    let routers = |outcome: &serde_json::Value| -> Vec<String> {
        outcome["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter_map(|e| e["router"].as_str().map(str::to_string))
            .collect()
    };

    let fast = forge(tmp.path())
        .env("FORGE_NEEDLE_BACKEND", "hash")
        .args(["--project"])
        .arg(&project)
        .args(args)
        .output()
        .expect("run");
    assert!(fast.status.success(), "{fast:?}");
    let outcome: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&fast.stdout).trim()).expect("run json");
    assert_eq!(outcome["text"], "hello from disk\n");
    assert_eq!(outcome["turns"], 0, "no model turn: {outcome}");
    assert!(
        routers(&outcome).iter().any(|r| r == "needle-dispatch"),
        "routers: {:?}",
        routers(&outcome)
    );

    // Without an available engine the very same prompt runs the model loop.
    let normal = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(args)
        .output()
        .expect("run");
    assert!(normal.status.success(), "{normal:?}");
    let outcome: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&normal.stdout).trim()).expect("run json");
    assert_eq!(outcome["turns"], 1, "plain loop: {outcome}");
    assert!(
        !routers(&outcome).iter().any(|r| r == "needle-dispatch"),
        "routers: {:?}",
        routers(&outcome)
    );
}

/// A `forge` invocation that is *not* allowed to use mocks: same hermetic
/// environment as [`forge`], minus the `FORGE_TEST_MOCKS` unlock. This is
/// what a user's shell looks like.
fn forge_without_mocks(tmp: &Path) -> Command {
    let mut cmd = forge(tmp);
    cmd.env_remove("FORGE_TEST_MOCKS");
    cmd
}

/// The user-facing half of the mock gate: a config that selects a mock is
/// refused, by name, with the fix in the message — and `forge doctor` says
/// so instead of reporting a healthy setup.
#[test]
fn a_mock_model_is_refused_without_the_test_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"mock-local\"\nrouter = \"static\"\n",
    )
    .expect("write config");

    let output = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "hello"])
        .output()
        .expect("run");
    assert!(
        !output.status.success(),
        "a mock model must not run without the gate; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("test-only"), "stderr: {stderr}");
    assert!(stderr.contains("FORGE_TEST_MOCKS=1"), "stderr: {stderr}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("mock response to"),
        "no mock output may reach stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Doctor turns the same situation into an actionable failing check.
    let doctor = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["--json", "doctor"])
        .output()
        .expect("doctor");
    let report: serde_json::Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor --json is JSON");
    let model_check = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["check"] == "model provider")
        .expect("a model provider check")
        .clone();
    assert_eq!(model_check["status"], "fail", "check: {model_check}");
    assert!(
        model_check["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("test-only mock"),
        "check: {model_check}"
    );
    assert_eq!(report["healthy"], false);
}

/// The mock *router* is gated too, and refused just as clearly.
#[test]
fn the_mock_router_is_refused_without_the_test_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(project.join(".forge/config.toml"), "router = \"mock\"\n")
        .expect("write config");

    let output = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "hello"])
        .output()
        .expect("run");
    assert!(!output.status.success(), "the mock router must be refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("router = \"mock\""), "stderr: {stderr}");
    assert!(stderr.contains("FORGE_TEST_MOCKS=1"), "stderr: {stderr}");
}

/// `forge model list` must not offer a provider the user cannot select.
#[test]
fn model_list_does_not_advertise_mocks_without_the_test_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"qwen3-coder\"\nmodel_base_url = \"http://127.0.0.1:8080/v1\"\n",
    )
    .expect("write config");

    let output = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["model", "list"])
        .output()
        .expect("model list");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("mock"),
        "model list must not mention mocks: {stdout}"
    );

    // With the gate open it is listed, and labelled for what it is.
    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["model", "list"])
        .output()
        .expect("model list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mock-local (test-only mock"),
        "the gate being open should surface it, labelled: {stdout}"
    );
}

#[test]
fn model_list_and_doctor_explain_static_model_eligibility() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join(".forge/config.toml"),
        r#"
model = "local-test"

[models.local-test]
base_url = "http://127.0.0.1:9/v1"

[models.missing-credential]
base_url = "https://example.invalid/v1"
key_env = "FORGE_ELIGIBILITY_MISSING_KEY"
"#,
    )
    .expect("write config");

    let output = forge_without_mocks(tmp.path())
        .env_remove("FORGE_ELIGIBILITY_MISSING_KEY")
        .args(["--project"])
        .arg(&project)
        .args(["--json", "model", "list"])
        .output()
        .expect("model list");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let missing = value["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|model| model["name"] == "missing-credential")
        .expect("configured model");
    assert_eq!(missing["eligible"], false);
    assert_eq!(missing["availability"], "credential unavailable");

    let checks = doctor_checks(tmp.path(), &project, false);
    let eligibility = checks
        .iter()
        .find(|check| check["check"] == "model eligibility")
        .expect("eligibility check");
    assert!(
        eligibility["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("missing-credential (credential unavailable)")),
        "check: {eligibility}"
    );
}

/// Read `forge --json doctor`'s check list for a project.
fn doctor_checks(tmp: &Path, project: &Path, mocks: bool) -> Vec<serde_json::Value> {
    let mut cmd = if mocks {
        forge(tmp)
    } else {
        forge_without_mocks(tmp)
    };
    let output = cmd
        .args(["--project"])
        .arg(project)
        .args(["--json", "doctor"])
        .output()
        .expect("doctor");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json is JSON");
    report["checks"].as_array().expect("checks array").clone()
}

fn check_named<'a>(checks: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    checks
        .iter()
        .find(|c| c["check"] == name)
        .unwrap_or_else(|| panic!("no {name:?} check in {checks:#?}"))
}

/// The mock *execution* provider is the worst of the mocks to leak: it
/// reports commands as run and files as written while doing neither. It
/// must be refused exactly like the mock model, and doctor must say so.
#[test]
fn the_mock_execution_provider_is_refused_without_the_test_env() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join(".forge/config.toml"),
        "execution = \"mock\"\nrouter = \"static\"\n",
    )
    .expect("write config");

    let output = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "hello"])
        .output()
        .expect("run");
    assert!(
        !output.status.success(),
        "a mock execution provider must not run without the gate"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("execution = \"mock\""), "stderr: {stderr}");
    assert!(stderr.contains("FORGE_TEST_MOCKS=1"), "stderr: {stderr}");

    // `forge skill test` reads the same config key by its own path; it must
    // not be a way around the gate.
    let output = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["skill", "test", "anything"])
        .output()
        .expect("skill test");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("FORGE_TEST_MOCKS=1"),
        "skill test must honour the gate too: {stderr}"
    );

    let checks = doctor_checks(tmp.path(), &project, false);
    let execution = check_named(&checks, "execution provider");
    assert_eq!(execution["status"], "fail", "{execution}");
    assert!(
        execution["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("test-only mock"),
        "{execution}"
    );
    assert!(
        !execution["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("available offline"),
        "a mock that pretends to run commands is not \"available\": {execution}"
    );
}

/// Doctor must not bless a configured mock router that `router_from_config`
/// would refuse.
#[test]
fn doctor_fails_the_router_check_for_a_configured_mock_router() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    std::fs::write(
        project.join(".forge/config.toml"),
        "router = \"mock\"\nmodel = \"qwen3-coder\"\n",
    )
    .expect("write config");

    let checks = doctor_checks(tmp.path(), &project, false);
    let router = check_named(&checks, "decision router");
    assert_eq!(router["status"], "fail", "{router}");
    assert!(
        router["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("test-only mock"),
        "{router}"
    );

    // With the gate open it is usable, but still not "fine": a warning.
    let checks = doctor_checks(tmp.path(), &project, true);
    let router = check_named(&checks, "decision router");
    assert_eq!(router["status"], "warn", "{router}");
}

/// An exported FORGE_TEST_MOCKS outlives the test run that needed it, so
/// doctor reports it whatever the configuration says.
#[test]
fn doctor_warns_whenever_the_mock_gate_is_open() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");
    // Deliberately a fully real configuration: the warning is about the
    // environment, not about what is configured.
    std::fs::write(
        project.join(".forge/config.toml"),
        "model = \"qwen3-coder\"\nrouter = \"static\"\nexecution = \"native\"\n",
    )
    .expect("write config");

    let checks = doctor_checks(tmp.path(), &project, true);
    let gate = check_named(&checks, "test mocks");
    assert_eq!(gate["status"], "warn", "{gate}");
    assert!(
        gate["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("FORGE_TEST_MOCKS=1 is set"),
        "{gate}"
    );

    // And it says nothing at all when the gate is closed.
    let checks = doctor_checks(tmp.path(), &project, false);
    assert!(
        !checks.iter().any(|c| c["check"] == "test mocks"),
        "no gate line without the env: {checks:#?}"
    );
}

/// Unknown-name errors must not advertise the test-only mocks as options.
#[test]
fn unknown_provider_errors_do_not_advertise_mocks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir");

    std::fs::write(
        project.join(".forge/config.toml"),
        "router = \"nonsense\"\n",
    )
    .expect("write config");
    let output = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "hello"])
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown router"), "stderr: {stderr}");
    assert!(!stderr.contains("mock"), "stderr advertises mock: {stderr}");

    std::fs::write(
        project.join(".forge/config.toml"),
        "execution = \"nonsense\"\n",
    )
    .expect("write config");
    let output = forge_without_mocks(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["run", "hello"])
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown execution provider"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("mock"), "stderr advertises mock: {stderr}");
}

/// A project directory for the chat tests: a real project root, and a
/// config that names no provider the chat could leak into its banner. The
/// chat shell never calls a model, so it needs no mock — and `forge()`
/// leaves `FORGE_TEST_MOCKS=1` set, which is precisely what makes the
/// "no mock in the banner" assertion below worth making.
fn scaffold(tmp: &Path) -> PathBuf {
    let project = tmp.join("proj");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir .forge");
    std::fs::write(
        project.join(".forge/config.toml"),
        "approval = \"prompt\"\n",
    )
    .expect("write config");
    project
}

/// Wait for a child, bounded, killing and failing rather than hanging —
/// the same guard (and the same name) as `forge-cli/tests/chat.rs`'s.
///
/// Every chat test is in the hang class: a regression that stops the chat
/// noticing EOF, or wedges its editor thread on the way out, leaves the
/// process alive for ever. A plain `wait()`/`wait_with_output()` hands
/// that to the whole suite as a hang; this hands it to one test as a
/// failure.
fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().ok();
            let _ = child.wait();
            panic!("timed out after {timeout:?} waiting for the chat to exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn bare_forge_opens_the_chat_and_exits_at_eof() {
    use std::io::Read as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path());
    // stdin is an empty pipe: the chat starts, reads EOF, exits cleanly.
    let mut child = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bare forge runs");
    drop(child.stdin.take());
    // The banner is a few hundred bytes, far inside the pipe buffer, so
    // the child never blocks on a writer nobody is draining.
    let status = wait_for_exit(&mut child, std::time::Duration::from_secs(20));
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    assert!(
        status.success(),
        "bare forge should exit 0, got {status:?}\n{stdout}"
    );
    assert!(stdout.contains("forge "), "banner missing: {stdout}");
    assert!(
        stdout.contains("/help"),
        "banner should point at /help: {stdout}"
    );
    assert!(
        !stdout.to_lowercase().contains("mock"),
        "no mock may be named: {stdout}"
    );
}

#[test]
fn chat_refuses_json_output() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path());
    let out = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["--json", "chat"])
        .output()
        .expect("runs");
    assert!(!out.status.success(), "--json chat must fail");
    assert!(
        out.stdout.is_empty(),
        "nothing may reach stdout: {:?}",
        out.stdout
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--json is not supported by the interactive chat"),
        "{stderr}"
    );
}

/// `--help` must not offer the test-only router either.
#[test]
fn help_does_not_advertise_the_mock_router() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = forge_without_mocks(tmp.path())
        .arg("--help")
        .output()
        .expect("help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--router"), "help: {stdout}");
    assert!(
        !stdout.contains("mock"),
        "help must not advertise mocks: {stdout}"
    );
}

#[test]
fn session_decisions_summarises_the_decision_log() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    let sessions = project.join(".forge").join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join("sess-1.decisions.jsonl"),
        concat!(
            r#"{"ts":"2026-09-25T00:00:00.000Z","session":"sess-1","turn":1,"stage":"decide","decider":"needle","question":"tool","choice":"read_file","confidence":1.0,"outcome":"dispatched","elapsed_ms":900,"speculative":false}"#,
            "\n",
            r#"{"ts":"2026-09-25T00:00:01.000Z","session":"sess-1","turn":2,"stage":"decide","decider":"needle","question":"tool","choice":"none","confidence":0.47,"outcome":"declined","elapsed_ms":1100,"speculative":false}"#,
            "\n",
            // A truncated final line is the normal state of an append-only
            // log whose writer died: skipped, never fatal.
            r#"{"ts":"2026-09-25T00:00:02.000Z","session":"sess-"#,
        ),
    )
    .expect("write log");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["session", "decisions", "--json"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json on stdout");
    assert_eq!(value["decide"]["total"], 2);
    assert_eq!(value["decide"]["dispatched"], 1);
    assert_eq!(value["decide"]["declined"], 1);
    // The number the next phase is designed against.
    assert_eq!(value["decide"]["decline_rate"], 0.5);
    assert_eq!(value["decide"]["mean_elapsed_ms"], 1000);
    assert_eq!(value["route"]["total"], 0);
}

// --- OpenRouter catalogue: `model list --catalogue` / `model add` / `model refresh` ---

/// Write a catalogue cache into the sandbox HOME, in the on-disk shape
/// `forge_config::catalogue` reads.
fn seed_catalogue_cache(tmp: &Path, fetched_at: &str, models: serde_json::Value) {
    let path = tmp.join("home/.cache/forge/openrouter/models.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        path,
        serde_json::json!({ "fetched_at": fetched_at, "models": models }).to_string(),
    )
    .expect("seed catalogue");
}

fn catalogue_model(id: &str, context: u64, input: f64, output: f64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "context_length": context,
        "cost_input_per_mtok": input,
        "cost_output_per_mtok": output,
    })
}

#[test]
fn model_list_catalogue_lists_the_seeded_cache_with_prices_and_context() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Far-future fetch time: a fresh cache regardless of the clock.
    seed_catalogue_cache(
        tmp.path(),
        "2099-01-01T00:00:00Z",
        serde_json::json!([catalogue_model(
            "anthropic/claude-sonnet-4.5",
            200000,
            3.0,
            15.0
        ),]),
    );

    let output = forge(tmp.path())
        .args(["model", "list", "--catalogue"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(stdout.contains("anthropic/claude-sonnet-4.5"), "{stdout}");
    assert!(stdout.contains("cost_in=$3/1M"), "{stdout}");
    assert!(stdout.contains("cost_out=$15/1M"), "{stdout}");
    assert!(stdout.contains("max_context=200000"), "{stdout}");
}

#[test]
fn model_list_catalogue_without_a_cache_is_a_note_not_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = forge(tmp.path())
        .args(["model", "list", "--catalogue"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(stdout.contains("forge model refresh"), "{stdout}");
}

#[test]
fn model_list_catalogue_warns_naming_the_age_when_the_cache_is_stale() {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_catalogue_cache(
        tmp.path(),
        "2020-01-01T00:00:00Z",
        serde_json::json!([catalogue_model("a/b", 1000, 1.0, 1.0)]),
    );

    let output = forge(tmp.path())
        .args(["model", "list", "--catalogue"])
        .output()
        .expect("run");
    // Stale is still *used*: the model lists, and the warning names the age.
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("a/b"));
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(stderr.contains("days old"), "stderr: {stderr}");
    assert!(stderr.contains("stale"), "stderr: {stderr}");
}

#[test]
fn model_add_writes_a_prefilled_entry_and_refuses_a_duplicate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");
    seed_catalogue_cache(
        tmp.path(),
        "2099-01-01T00:00:00Z",
        serde_json::json!([catalogue_model("test/model-a", 100000, 0.5, 1.5)]),
    );

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["model", "add", "test/model-a"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = std::fs::read_to_string(project.join(".forge/config.toml")).expect("config");
    assert!(config.contains("[models.\"test/model-a\"]"), "{config}");
    assert!(
        config.contains("key_env = \"OPENROUTER_API_KEY\""),
        "{config}"
    );
    assert!(config.contains("max_context = 100000"), "{config}");
    assert!(config.contains("cost_input_per_mtok = 0.5"), "{config}");
    assert!(config.contains("cost_output_per_mtok = 1.5"), "{config}");
    assert!(
        !config.contains("tools"),
        "tool support must be left to the operator: {config}"
    );
    // The note says why.
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(stdout.contains("tools"), "{stdout}");

    // A second add must not clobber the entry it just wrote.
    let again = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["model", "add", "test/model-a"])
        .output()
        .expect("run");
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already declared"),
        "{:?}",
        String::from_utf8_lossy(&again.stderr)
    );
}

#[test]
fn model_add_without_a_cache_fails_naming_the_fix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).expect("mkdir");

    let output = forge(tmp.path())
        .args(["--project"])
        .arg(&project)
        .args(["model", "add", "test/model-a"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(stderr.contains("forge model refresh"), "{stderr}");
    assert!(!project.join(".forge/config.toml").exists());
}

#[tokio::test]
async fn model_refresh_fetches_offline_and_the_cache_then_lists() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {
                    "id": "anthropic/claude-sonnet-4.5",
                    "context_length": 200000,
                    "pricing": { "prompt": "0.000003", "completion": "0.000015" }
                },
                {
                    "id": "free/model-no-pricing",
                    "context_length": 8192
                }
            ]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let output = forge(tmp.path())
        .args(["model", "refresh"])
        .env("FORGE_OPENROUTER_BASE_URL", server.uri())
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(
        stdout.contains("refreshed OpenRouter catalogue: 2 models"),
        "{stdout}"
    );
    assert!(
        tmp.path()
            .join("home/.cache/forge/openrouter/models.json")
            .is_file(),
        "the cache file was written"
    );

    // …and what was fetched is what `--catalogue` shows (per-token strings
    // converted to per-million-token prices).
    let list = forge(tmp.path())
        .args(["model", "list", "--catalogue"])
        .output()
        .expect("run");
    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).expect("utf8");
    assert!(stdout.contains("cost_in=$3/1M"), "{stdout}");
    assert!(
        stdout.contains("free/model-no-pricing — unpriced"),
        "{stdout}"
    );
}

#[test]
fn model_refresh_refuses_under_local_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = forge(tmp.path())
        .args(["--local-only", "model", "refresh"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(stderr.contains("local_only"), "{stderr}");
    assert!(
        !tmp.path().join("home/.cache/forge/openrouter").exists(),
        "no fetch, no cache directory"
    );
}

#[test]
fn learning_proposals_are_local_reviewable_explicit_and_measured() {
    use forge_core::{Event, EventKind, SessionStore};

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let init = forge(tmp.path())
        .arg("--project")
        .arg(&project)
        .arg("init")
        .output()
        .expect("init");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let sessions = forge_session::JsonlSessionStore::new(project.join(".forge/sessions"));
    for (session, run) in [("session-a", "run-a"), ("session-b", "run-b")] {
        sessions
            .append(Event::new(
                run,
                session,
                EventKind::InputReceived {
                    message: "Always keep sk-abcdefgh12345678 out of output".into(),
                },
            ))
            .unwrap();
        sessions
            .append(Event::new(
                run,
                session,
                EventKind::Error {
                    message: "compiler unavailable".into(),
                },
            ))
            .unwrap();
    }

    for args in [
        vec!["learn", "propose", "--session", "session-a"],
        vec!["learn", "propose", "--since", "2999-01-01T00:00:00Z"],
    ] {
        let scoped = forge(tmp.path())
            .arg("--project")
            .arg(&project)
            .args(args)
            .output()
            .expect("scoped proposal");
        assert!(scoped.status.success());
        let report: serde_json::Value = serde_json::from_slice(&scoped.stdout).unwrap();
        assert!(report["proposals"].as_array().unwrap().is_empty());
    }
    let unknown = forge(tmp.path())
        .arg("--project")
        .arg(&project)
        .args(["learn", "propose", "--session", "unknown"])
        .output()
        .expect("unknown session");
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown session"));

    let proposed = forge(tmp.path())
        .arg("--project")
        .arg(&project)
        .args(["learn", "propose"])
        .output()
        .expect("propose");
    assert!(
        proposed.status.success(),
        "{}",
        String::from_utf8_lossy(&proposed.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&proposed.stdout).unwrap();
    assert_eq!(report["storage"], ".forge/context/learning/index.json");
    assert_eq!(report["proposals"].as_array().unwrap().len(), 2);
    let proposals = report["proposals"].as_array().unwrap();
    let accepted_id = proposals
        .iter()
        .find(|proposal| proposal["proposal"]["kind"] == "correction")
        .unwrap()["proposal"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let rejected_id = proposals
        .iter()
        .find(|proposal| proposal["proposal"]["kind"] == "failure")
        .unwrap()["proposal"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stdout = String::from_utf8(proposed.stdout).unwrap();
    assert!(!stdout.contains("sk-abcdefgh"), "{stdout}");
    assert!(stdout.contains("[REDACTED]"), "{stdout}");
    assert!(stdout.contains("session-a"), "{stdout}");
    assert!(stdout.contains("run-b"), "{stdout}");

    let refused = forge(tmp.path())
        .arg("--project")
        .arg(&project)
        .args(["learn", "apply", &accepted_id])
        .output()
        .expect("refuse unconfirmed apply");
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("requires --yes"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    for args in [
        vec!["learn", "apply", &accepted_id, "--yes"],
        vec!["learn", "reject", &rejected_id],
    ] {
        let output = forge(tmp.path())
            .arg("--project")
            .arg(&project)
            .args(args)
            .output()
            .expect("decide");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    sessions
        .append(Event::new(
            "run-c",
            "session-c",
            EventKind::InputReceived {
                message: "Always keep sk-abcdefgh12345678 out of output".into(),
            },
        ))
        .unwrap();
    let metrics = forge(tmp.path())
        .arg("--project")
        .arg(&project)
        .args(["learn", "metrics"])
        .output()
        .expect("metrics");
    assert!(metrics.status.success());
    let metrics: serde_json::Value = serde_json::from_slice(&metrics.stdout).unwrap();
    assert_eq!(metrics["accepted"], 1);
    assert_eq!(metrics["rejected"], 1);
    assert_eq!(metrics["recurrence_after_acceptance"], 1);

    let gitignore = std::fs::read_to_string(project.join(".gitignore")).unwrap();
    assert!(gitignore.lines().any(|line| line.trim() == ".forge/"));
    assert!(project.join(".forge/context/learning/index.json").is_file());
}
