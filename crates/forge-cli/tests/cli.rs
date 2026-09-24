use std::path::Path;
use std::process::Command;

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
];

/// A `forge` invocation isolated from the developer's real user config and
/// environment: XDG_CONFIG_HOME points at a temp dir, FORGE_* vars removed.
fn forge(tmp: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
    for var in FORGE_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env("XDG_CONFIG_HOME", tmp.join("xdg"));
    cmd.env("NO_COLOR", "1");
    cmd
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
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("timeout");
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

    // The session was persisted under .forge/sessions/.
    let sessions_dir = project.join(".forge").join("sessions");
    assert!(sessions_dir.is_dir());
    let entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("read sessions")
        .collect();
    assert_eq!(entries.len(), 1);
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
    assert!(stdout.contains("3 events"), "list output: {stdout}");

    // resume continues the completed run: a NEW run in the same session,
    // seeded with the original prompt, printing the new run's output.
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
    assert!(
        stdout.contains("mock response to: hi"),
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

    // And the full tool trail is in the session log.
    let log = std::fs::read_to_string(
        std::fs::read_dir(project.join(".forge/sessions"))
            .expect("sessions")
            .next()
            .expect("one session")
            .expect("entry")
            .path(),
    )
    .expect("log");
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
