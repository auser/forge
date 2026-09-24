//! Step definitions matching `tests/features/*.feature` verbatim.

use cucumber::{given, then, when};

use crate::world::BddWorld;

// ---------------------------------------------------------------------------
// configuration.feature
// ---------------------------------------------------------------------------

#[given(expr = "a project config sets the model to {string}")]
fn project_config_sets_model(world: &mut BddWorld, model: String) {
    world.write_file(".forge/config.toml", &format!("model = \"{model}\"\n"));
}

#[given(expr = "the environment sets the model to {string}")]
fn env_sets_model(world: &mut BddWorld, model: String) {
    world.env.insert("FORGE_MODEL".to_string(), model);
}

#[when(expr = "Forge runs with the model flag {string}")]
async fn forge_runs_with_model_flag(world: &mut BddWorld, model: String) {
    world
        .run_forge(&["--model", &model, "config", "explain", "model"])
        .await;
}

#[when("Forge loads configuration")]
async fn forge_loads_configuration(world: &mut BddWorld) {
    world.run_forge(&["config", "explain", "model"]).await;
}

#[then(expr = "the effective model is {string}")]
fn effective_model_is(world: &mut BddWorld, model: String) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains(&format!("model = \"{model}\"")),
        "stdout: {}",
        world.last_stdout
    );
}

// ---------------------------------------------------------------------------
// execution.feature
// ---------------------------------------------------------------------------

fn write_skill_with_test(world: &mut BddWorld) {
    world.write_file(
        ".forge/skills/demo/SKILL.md",
        "---\nname: demo\ndescription: Demo skill\n---\n# Demo\n\nDo demo things.\n",
    );
    world.write_file(".forge/skills/demo/test.sh", "echo demo-ok\n");
}

#[given("the mock execution provider is configured")]
fn mock_execution_configured(world: &mut BddWorld) {
    world.write_file(
        ".forge/config.toml",
        "execution = \"mock\"\napproval = \"auto\"\n",
    );
}

#[when("the agent requests a command")]
async fn agent_requests_command(world: &mut BddWorld) {
    write_skill_with_test(world);
    world.run_forge(&["skill", "test", "demo"]).await;
}

#[then("the mock provider receives it")]
fn mock_provider_receives_it(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains("mock execution recorded: sh"),
        "stdout: {}",
        world.last_stdout
    );
}

#[given("risky execution requires approval")]
fn risky_execution_requires_approval(world: &mut BddWorld) {
    world.write_file(
        ".forge/config.toml",
        "execution = \"native\"\napproval = \"prompt\"\n",
    );
}

#[when("the agent requests a destructive command")]
async fn agent_requests_destructive_command(world: &mut BddWorld) {
    // Skill test scripts run as RiskLevel::Risky through the execution
    // provider; with stdin not a TTY this must pause for approval.
    write_skill_with_test(world);
    world.run_forge(&["skill", "test", "demo"]).await;
}

#[then("execution pauses for approval")]
fn execution_pauses_for_approval(world: &mut BddWorld) {
    assert_ne!(
        world.last_code,
        Some(0),
        "expected failure, stdout: {}",
        world.last_stdout
    );
    assert!(
        world.last_stderr.contains("approval required"),
        "stderr: {}",
        world.last_stderr
    );
}

// ---------------------------------------------------------------------------
// init.feature
// ---------------------------------------------------------------------------

#[given("a project with source files and a .gitignore")]
fn project_with_sources_and_gitignore(world: &mut BddWorld) {
    world.write_file("src/main.rs", "fn main() {}\n");
    world.write_file(".gitignore", "target/\n");
}

#[when(expr = "I run {string}")]
async fn i_run(world: &mut BddWorld, command: String) {
    let args: Vec<&str> = command
        .split_whitespace()
        .skip(1) // drop the leading "forge"
        .collect();
    world.run_forge(&args).await;
}

#[when(expr = "I run {string} again")]
async fn i_run_again(world: &mut BddWorld, command: String) {
    i_run(world, command).await;
}

#[then(".forge and the project graph exist")]
fn forge_dirs_and_graph_exist(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let root = world.project();
    assert!(root.join(".forge").is_dir());
    assert!(root.join(".forge/sessions").is_dir());
    assert!(root.join(".forge/graph/graph.json").is_file());
}

#[then("generated paths are ignored")]
fn generated_paths_ignored(world: &mut BddWorld) {
    let gitignore = std::fs::read_to_string(world.project().join(".gitignore")).expect("gitignore");
    assert!(
        gitignore.lines().any(|l| l == ".forge/"),
        "gitignore: {gitignore}"
    );
}

#[then("no duplicate ignore entries are created")]
fn no_duplicate_ignore_entries(world: &mut BddWorld) {
    let gitignore = std::fs::read_to_string(world.project().join(".gitignore")).expect("gitignore");
    assert_eq!(
        gitignore.lines().filter(|l| *l == ".forge/").count(),
        1,
        "gitignore: {gitignore}"
    );
    assert!(
        gitignore.lines().any(|l| l == "target/"),
        "pre-existing entry preserved, gitignore: {gitignore}"
    );
}

// ---------------------------------------------------------------------------
// routing.feature
// ---------------------------------------------------------------------------

#[given("a local System One-compatible router")]
async fn local_system_one_router(world: &mut BddWorld) {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/route"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "selected_model": "routed-model",
            "confidence": 0.9,
            "reason": "bdd test router"
        })))
        .mount(&server)
        .await;
    // The routed model resolves through its `[models]` entry, served by
    // the same mock server (OpenAI-compatible completion).
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": "routed answer" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;
    world.write_file(
        ".forge/config.toml",
        &format!(
            "router = \"http\"\nrouter_url = \"{}/route\"\nrouter_timeout_ms = 5000\n\n[models.routed-model]\nbase_url = \"{}\"\n",
            server.uri(),
            server.uri()
        ),
    );
    world.router_mock = Some(server);
}

#[given("the configured router is unavailable")]
fn configured_router_unavailable(world: &mut BddWorld) {
    // Port 9 (discard) is closed: connections fail fast.
    world.write_file(
        ".forge/config.toml",
        "router = \"http\"\nrouter_url = \"http://127.0.0.1:9/route\"\nrouter_timeout_ms = 300\n",
    );
}

#[given(expr = "static routing selects {string}")]
fn static_routing_selects(world: &mut BddWorld, model: String) {
    // The static fallback router routes to the configured default model.
    world.write_file(
        ".forge/config.toml",
        &format!(
            "model = \"{model}\"\nrouter = \"http\"\nrouter_url = \"http://127.0.0.1:9/route\"\nrouter_timeout_ms = 300\n"
        ),
    );
}

#[when("Forge routes a coding task")]
async fn forge_routes_task(world: &mut BddWorld) {
    world
        .run_forge(&["--json", "run", "implement the thing"])
        .await;
}

#[then("it returns a selected model and confidence")]
async fn returns_selected_model_and_confidence(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);

    // The router actually received the routing request.
    let received = world
        .router_mock
        .as_ref()
        .expect("router mock")
        .received_requests()
        .await
        .expect("received requests");
    assert!(!received.is_empty(), "router received no request");
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("router request json");
    assert_eq!(body["task"], "implement the thing");

    // The run outcome carries the routed model + confidence.
    let outcome: serde_json::Value =
        serde_json::from_str(&world.last_stdout).expect("run --json output");
    let decision = outcome["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|e| e["type"] == "routing_decision_made")
        .expect("routing decision event");
    assert_eq!(decision["selected_model"], "routed-model");
    assert_eq!(decision["confidence"], 0.9);
    assert_eq!(decision["fallback_used"], false);
}

#[then("records the routing decision")]
fn records_routing_decision(world: &mut BddWorld) {
    let log = world.session_log();
    let decision = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "routing_decision_made")
        .unwrap_or_else(|| panic!("no routing decision in session log: {log}"));
    assert_eq!(decision["selected_model"], "routed-model");
    assert_eq!(decision["router"], "http");
    let confidence = decision["confidence"].as_f64().expect("confidence");
    assert!((confidence - 0.9).abs() < 0.001, "confidence: {confidence}");
}

#[then(expr = "the selected model is {string}")]
fn selected_model_is(world: &mut BddWorld, model: String) {
    // The fallback path selects the static model; the (unreachable) model
    // endpoint fails afterwards, which the scenario tolerates — the
    // routing decision is what is asserted.
    let log = world.session_log();
    let decision = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "routing_decision_made")
        .unwrap_or_else(|| panic!("no routing decision in session log: {log}"));
    assert_eq!(decision["selected_model"], model);
    assert_eq!(decision["fallback_used"], true);
}

// ---------------------------------------------------------------------------
// server.feature
// ---------------------------------------------------------------------------

#[given("Forge runs with a mock model")]
async fn forge_runs_with_mock_model(world: &mut BddWorld) {
    // Mock is opt-in (the default model is a real local endpoint).
    world.set_config("model", "\"mock-local\"");
    world.flush_config();
    world.start_server().await;
}

#[when("I request health and create a REST run")]
async fn request_health_and_create_run(world: &mut BddWorld) {
    let client = reqwest::Client::new();

    let health = client
        .get(format!("{}/health", world.base_url))
        .send()
        .await
        .expect("health response");
    world.health_status = health.status().as_u16();
    world.health_body = health.text().await.expect("health body");

    let run = client
        .post(format!("{}/v1/runs", world.base_url))
        .json(&serde_json::json!({"prompt": "bdd task"}))
        .send()
        .await
        .expect("run response");
    world.run_status = run.status().as_u16();
    let body: serde_json::Value = run.json().await.expect("run body");
    world.run_id = body["run_id"].as_str().expect("run id").to_string();
}

#[then("health succeeds")]
fn health_succeeds(world: &mut BddWorld) {
    assert_eq!(world.health_status, 200);
    let body: serde_json::Value = serde_json::from_str(&world.health_body).expect("health json");
    assert_eq!(body["status"], "ok");
    assert_eq!(world.run_status, 202);
    assert!(!world.run_id.is_empty());
}

#[then("run events are available through SSE")]
async fn run_events_via_sse(world: &mut BddWorld) {
    let client = reqwest::Client::new();
    let response = client
        .get(format!(
            "{}/v1/runs/{}/events",
            world.base_url, world.run_id
        ))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .expect("sse response");
    world.sse_content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Reading the body to completion also proves the stream ends.
    world.sse_body = response.text().await.expect("sse body");

    assert_eq!(world.sse_content_type, "text/event-stream");
    assert!(
        world.sse_body.contains("\"type\":\"run_started\""),
        "sse: {}",
        world.sse_body
    );
    assert!(
        world
            .sse_body
            .contains("\"type\":\"routing_decision_made\""),
        "sse: {}",
        world.sse_body
    );
}

#[then("the stream ends with completion")]
fn stream_ends_with_completion(world: &mut BddWorld) {
    // sse_body was read to end (no hang); it must contain the terminal
    // completed event as the last data line.
    let last = world
        .sse_body
        .lines()
        .rfind(|l| l.starts_with("data:"))
        .expect("at least one data line");
    assert!(
        last.contains("\"type\":\"completed\""),
        "last event: {last}"
    );
}

// ---------------------------------------------------------------------------
// skills.feature
// ---------------------------------------------------------------------------

#[given("a project contains a SKILL.md skill")]
fn project_contains_skill(world: &mut BddWorld) {
    world.write_file(
        ".forge/skills/demo/SKILL.md",
        "---\nname: demo\ndescription: Demo skill description\n---\n# Demo\n\nDo the demo thing.\n",
    );
}

#[when("I list skills")]
async fn i_list_skills(world: &mut BddWorld) {
    world.run_forge(&["skill", "list"]).await;
}

#[then("the skill description is shown")]
fn skill_description_shown(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains("Demo skill description"),
        "stdout: {}",
        world.last_stdout
    );
}

#[when("a task matches the skill")]
async fn task_matches_skill(world: &mut BddWorld) {
    world.set_config("model", "\"mock-local\"");
    // The mock's reply is clean by default (the plain `forge run` first
    // impression must not be a wall of internals); this scenario is
    // specifically about the instructions reaching the model, so it opts
    // into the echo rather than asserting less.
    world
        .env
        .insert("FORGE_MOCK_VERBOSE".to_string(), "1".to_string());
    world.run_forge(&["run", "please run the demo"]).await;
}

#[then("its instructions are activated and logged")]
fn instructions_activated_and_logged(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    // The activated skill's instructions were injected into the model
    // request (the mock echoes system context).
    assert!(
        world.last_stdout.contains("system context"),
        "stdout: {}",
        world.last_stdout
    );
    assert!(
        world.last_stdout.contains("demo"),
        "stdout: {}",
        world.last_stdout
    );
    // Activation was logged as a skill_activated event.
    let log = world.session_log();
    let event = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "skill_activated")
        .unwrap_or_else(|| panic!("no skill_activated in session log: {log}"));
    assert_eq!(event["name"], "demo");
}

#[when("a task matches the skill with default mock output")]
async fn task_matches_skill_default_output(world: &mut BddWorld) {
    world.set_config("model", "\"mock-local\"");
    world.run_forge(&["run", "please run the demo"]).await;
}

#[then("the reply is exactly the mock response to the prompt")]
fn reply_is_exactly_the_mock_response(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert_eq!(
        world.last_stdout.trim(),
        "mock response to: please run the demo",
        "the mock reply must not carry system context: {}",
        world.last_stdout
    );
    // The skill still activated — it is only the *reply* that stays clean.
    let log = world.session_log();
    assert!(
        log.contains("skill_activated"),
        "skill must still activate: {log}"
    );
}

// ---------------------------------------------------------------------------
// tracing.feature
// ---------------------------------------------------------------------------

#[when(expr = "I run Forge with {string}")]
async fn i_run_forge_with_verbosity(world: &mut BddWorld, flag: String) {
    match flag.as_str() {
        "-v" => world.run_forge(&["-v", "init"]).await,
        "-vvv" => world.run_forge(&["-vvv", "doctor"]).await,
        other => panic!("unexpected verbosity flag {other:?}"),
    }
}

#[then("informational diagnostics are enabled")]
fn informational_diagnostics_enabled(world: &mut BddWorld) {
    assert!(
        world.last_stderr.contains("INFO"),
        "stderr: {}",
        world.last_stderr
    );
}

#[then("trace diagnostics are enabled")]
fn trace_diagnostics_enabled(world: &mut BddWorld) {
    assert!(
        world.last_stderr.contains("TRACE"),
        "stderr: {}",
        world.last_stderr
    );
}

#[when("I run Forge with JSON output and verbose tracing")]
async fn json_output_and_verbose_tracing(world: &mut BddWorld) {
    world.run_forge(&["--json", "-vvv", "config", "show"]).await;
}

#[then("stdout contains only JSON")]
fn stdout_contains_only_json(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let value: serde_json::Value =
        serde_json::from_str(world.last_stdout.trim()).expect("stdout is one JSON value");
    assert_eq!(value["config"]["model"], "qwen3-coder");
}

#[then("diagnostics are written to stderr")]
fn diagnostics_written_to_stderr(world: &mut BddWorld) {
    assert!(!world.last_stderr.is_empty());
    assert!(
        world.last_stderr.contains("TRACE"),
        "stderr: {}",
        world.last_stderr
    );
}

// ---------------------------------------------------------------------------
// graph.feature
// ---------------------------------------------------------------------------

#[given("a project with Rust source files")]
fn project_with_rust_sources(world: &mut BddWorld) {
    world.write_file(
        "src/main.rs",
        "use crate::util::help;\nfn main() {\n    help();\n}\n",
    );
    world.write_file("src/util.rs", "pub fn help() {}\n");
}

#[when("I build the project graph")]
async fn i_build_the_project_graph(world: &mut BddWorld) {
    world.run_forge(&["--json", "graph", "build"]).await;
}

#[then("the graph records symbols and files")]
fn graph_records_symbols_and_files(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let stats: serde_json::Value =
        serde_json::from_str(world.last_stdout.trim()).expect("graph build json");
    assert!(stats["files"].as_u64().expect("files") >= 2, "{stats}");
    assert!(stats["symbols"].as_u64().expect("symbols") >= 2, "{stats}");
    assert!(world.project().join(".forge/graph/graph.json").is_file());
}

#[given("a project with a built graph")]
async fn project_with_a_built_graph(world: &mut BddWorld) {
    project_with_rust_sources(world);
    world.run_forge(&["graph", "build"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[when("I modify a source file")]
fn i_modify_a_source_file(world: &mut BddWorld) {
    world.write_file(
        "src/util.rs",
        "pub fn help() {\n    println!(\"changed\");\n}\n",
    );
}

#[then("the graph reports that it is stale")]
async fn graph_reports_stale(world: &mut BddWorld) {
    world.run_forge(&["graph", "check"]).await;
    assert_ne!(
        world.last_code,
        Some(0),
        "graph check must fail on stale graph"
    );
    assert!(
        world.last_stdout.contains("stale"),
        "stdout: {}",
        world.last_stdout
    );
    assert!(
        world.last_stdout.contains("src/util.rs"),
        "stdout: {}",
        world.last_stdout
    );
}

#[when("I rebuild the project graph")]
async fn i_rebuild_the_project_graph(world: &mut BddWorld) {
    world.run_forge(&["graph", "build"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[then("the graph reports that it is fresh")]
async fn graph_reports_fresh(world: &mut BddWorld) {
    world.run_forge(&["graph", "check"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains("fresh"),
        "stdout: {}",
        world.last_stdout
    );
}

// ---------------------------------------------------------------------------
// privacy.feature
// ---------------------------------------------------------------------------

#[when(expr = "I run a prompt containing the secret {string}")]
async fn i_run_prompt_with_secret(world: &mut BddWorld, secret: String) {
    world.set_config("model", "\"mock-local\"");
    let prompt = format!("please use key {secret} here");
    world.secret = secret;
    world.run_forge(&["run", &prompt]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[then(expr = "the session log does not contain {string}")]
fn session_log_omits_secret(world: &mut BddWorld, secret: String) {
    let log = world.session_log();
    assert!(!log.is_empty(), "expected a session log");
    assert!(
        !log.contains(&secret),
        "secret leaked into session log: {log}"
    );
}

#[then("the session log marks the value as redacted")]
fn session_log_marks_redacted(world: &mut BddWorld) {
    let log = world.session_log();
    assert!(
        log.contains("[REDACTED]"),
        "session log missing redaction marker: {log}"
    );
}

// ---------------------------------------------------------------------------
// sessions.feature
// ---------------------------------------------------------------------------

#[when("I run a prompt with the mock model")]
async fn i_run_a_prompt_with_mock_model(world: &mut BddWorld) {
    world.set_config("model", "\"mock-local\"");
    world.run_forge(&["--json", "run", "session test"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let outcome: serde_json::Value =
        serde_json::from_str(world.last_stdout.trim()).expect("run json");
    world.run_id = outcome["run_id"].as_str().expect("run id").to_string();
    world.session_id = outcome["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
}

#[then("the session is listed")]
async fn the_session_is_listed(world: &mut BddWorld) {
    let session_id = world.session_id.clone();
    world.run_forge(&["session", "list"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains(&session_id),
        "stdout: {}",
        world.last_stdout
    );
}

#[then("the session events include run started and completed")]
async fn session_events_include_run_started_and_completed(world: &mut BddWorld) {
    let session_id = world.session_id.clone();
    world.run_forge(&["session", "show", &session_id]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains("run_started"),
        "stdout: {}",
        world.last_stdout
    );
    assert!(
        world.last_stdout.contains("completed"),
        "stdout: {}",
        world.last_stdout
    );
}

#[given("a completed run with the mock model")]
async fn a_completed_run_with_mock_model(world: &mut BddWorld) {
    i_run_a_prompt_with_mock_model(world).await;
    // The mock completes synchronously; the run is finished by now.
}

#[when("I cancel the run")]
async fn i_cancel_the_run(world: &mut BddWorld) {
    let run_id = world.run_id.clone();
    world.run_forge(&["cancel", &run_id]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[then("a cancellation event is recorded for the run")]
fn cancellation_event_recorded(world: &mut BddWorld) {
    let log = world.session_log();
    let cancelled = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "cancelled" && e["run_id"] == world.run_id);
    assert!(
        cancelled.is_some(),
        "no cancelled event for {} in: {log}",
        world.run_id
    );
}

// ---------------------------------------------------------------------------
// agent-loop.feature
// ---------------------------------------------------------------------------

#[given(expr = "a scripted mock model that edits {string}")]
async fn scripted_mock_edits(world: &mut BddWorld, path: String) {
    world.write_file(&path, "fn main() {}\n");
    world.write_file(
        "script.json",
        r#"[
            {"tool_calls": [{"id": "call_1", "name": "edit_file", "arguments": {"path": "main.rs", "old": "fn main() {}", "new": "fn main() { hello(); }\n\nfn hello() { println!(\"hello\"); } // scripted content"}}]},
            {"text": "added hello() to main.rs"}
        ]"#,
    );
    world.set_config("model", "\"scripted-mock\"");
    world.set_config("mock_script", "\"script.json\"");
    world.set_config("approval", "\"auto\"");
    world.flush_config();
}

#[when("I run an agent task")]
async fn i_run_an_agent_task(world: &mut BddWorld) {
    world.flush_config();
    world.run_forge(&["run", "do the agent task"]).await;
}

#[then(expr = "the file {string} contains the scripted content")]
fn file_contains_scripted_content(world: &mut BddWorld, path: String) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let content = std::fs::read_to_string(world.project().join(&path)).expect("file exists");
    assert!(
        content.contains("scripted content"),
        "expected scripted content in {path}: {content}"
    );
}

#[then("the session events include tool calls and a file change")]
fn session_events_include_tool_calls_and_file_change(world: &mut BddWorld) {
    let log = world.session_log();
    for needle in [
        "tool_call_requested",
        "tool_started",
        "file_changed",
        "tool_completed",
        "turn_completed",
        "completed",
    ] {
        assert!(log.contains(needle), "missing {needle} in: {log}");
    }
}

#[given("a scripted mock model that always requests a tool call")]
fn scripted_mock_always_tool_calls(world: &mut BddWorld) {
    let replies: Vec<String> = (0..6)
        .map(|i| {
            format!(
                r#"{{"tool_calls": [{{"id": "c{i}", "name": "read_file", "arguments": {{"path": "f{i}.txt"}}}}]}}"#
            )
        })
        .collect();
    world.write_file("script.json", &format!("[{}]", replies.join(",")));
    world.set_config("model", "\"scripted-mock\"");
    world.set_config("mock_script", "\"script.json\"");
    world.set_config("approval", "\"auto\"");
}

#[when(expr = "I run an agent task with max turns {int}")]
async fn i_run_agent_task_with_max_turns(world: &mut BddWorld, max_turns: u32) {
    world.flush_config();
    world
        .run_forge(&["run", "--max-turns", &max_turns.to_string(), "spin"])
        .await;
}

#[then("the run fails with a turn budget error")]
fn run_fails_with_turn_budget_error(world: &mut BddWorld) {
    assert_ne!(world.last_code, Some(0), "run must fail");
    assert!(
        world.last_stderr.contains("max turns (3) exhausted"),
        "stderr: {}",
        world.last_stderr
    );
    let log = world.session_log();
    let error_event = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("no error event in: {log}"));
    assert!(
        error_event["message"]
            .as_str()
            .expect("message")
            .contains("max turns (3)"),
        "error event: {error_event}"
    );
}

// ---------------------------------------------------------------------------
// approval.feature
// ---------------------------------------------------------------------------

#[given(expr = "approval mode {string}")]
fn approval_mode(world: &mut BddWorld, mode: String) {
    world.set_config("approval", &format!("\"{mode}\""));
}

#[given(expr = "a scripted mock model that writes {string}")]
fn scripted_mock_writes(world: &mut BddWorld, path: String) {
    let script = format!(
        r#"[
            {{"tool_calls": [{{"id": "call_1", "name": "write_file", "arguments": {{"path": "{path}", "content": "scripted content"}}}}]}},
            {{"text": "all done"}}
        ]"#
    );
    world.write_file("script.json", &script);
    world.set_config("model", "\"scripted-mock\"");
    world.set_config("mock_script", "\"script.json\"");
}

#[then(expr = "the file {string} does not exist")]
fn file_does_not_exist(world: &mut BddWorld, path: String) {
    assert!(
        !world.project().join(&path).exists(),
        "{path} should not exist"
    );
}

#[then("the session events include a tool error")]
fn session_events_include_tool_error(world: &mut BddWorld) {
    let log = world.session_log();
    let tool_error = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "tool_completed" && e["success"] == false);
    assert!(
        tool_error.is_some(),
        "no failed tool_completed event in: {log}"
    );
}

// ---------------------------------------------------------------------------
// resume.feature
// ---------------------------------------------------------------------------

#[given("a completed scripted-mock run")]
async fn completed_scripted_mock_run(world: &mut BddWorld) {
    world.write_file("script.json", r#"[{"text": "first"}, {"text": "second"}]"#);
    world.set_config("model", "\"scripted-mock\"");
    world.set_config("mock_script", "\"script.json\"");
    world.flush_config();
    world.run_forge(&["--json", "run", "original task"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let outcome: serde_json::Value =
        serde_json::from_str(world.last_stdout.trim()).expect("run json");
    world.run_id = outcome["run_id"].as_str().expect("run id").to_string();
    world.session_id = outcome["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
}

#[when("I resume the run")]
async fn i_resume_the_run(world: &mut BddWorld) {
    let run_id = world.run_id.clone();
    world.run_forge(&["resume", &run_id]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[then("a new run continues in the same session")]
fn new_run_continues_in_same_session(world: &mut BddWorld) {
    let log = world.session_log();
    let events: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let run_ids: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "run_started")
        .filter_map(|e| e["run_id"].as_str())
        .collect();
    assert_eq!(run_ids.len(), 2, "expected two runs in log: {log}");
    assert_ne!(run_ids[0], run_ids[1], "resume must start a NEW run");
    assert_eq!(run_ids[0], world.run_id);
    // Same session file, resume marker linking the runs.
    assert!(
        events.iter().all(|e| e["session_id"] == world.session_id),
        "all events in the same session"
    );
    let marker = events
        .iter()
        .find(|e| e["type"] == "input_received")
        .expect("resume marker");
    assert!(
        marker["message"]
            .as_str()
            .expect("message")
            .contains(&world.run_id),
        "marker: {marker}"
    );
}

// ---------------------------------------------------------------------------
// server-input.feature / cancellation.feature
// ---------------------------------------------------------------------------

#[given(expr = "a served project with approval mode {string}")]
async fn served_project_with_approval(world: &mut BddWorld, mode: String) {
    world.set_config("approval", &format!("\"{mode}\""));
    // Config is flushed in the When step, right before the server starts
    // (the model/script Givens may add more keys first).
}

#[when("a run pauses for approval via REST")]
async fn run_pauses_for_approval_via_rest(world: &mut BddWorld) {
    world.flush_config();
    if world.server.is_none() {
        world.start_server().await;
    }
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/v1/runs", world.base_url))
        .json(&serde_json::json!({"prompt": "write the notes"}))
        .send()
        .await
        .expect("run response");
    assert_eq!(response.status(), 202);
    let body: serde_json::Value = response.json().await.expect("run body");
    world.run_id = body["run_id"].as_str().expect("run id").to_string();

    // Poll until the run parks at the approval wait.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let run: serde_json::Value = client
            .get(format!("{}/v1/runs/{}", world.base_url, world.run_id))
            .send()
            .await
            .expect("get run")
            .json()
            .await
            .expect("run json");
        if run["status"] == "waiting_for_approval" {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never parked; last status: {}",
            run["status"]
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Poll the run until terminal; return its final JSON.
async fn wait_terminal(world: &BddWorld) -> serde_json::Value {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let run: serde_json::Value = client
            .get(format!("{}/v1/runs/{}", world.base_url, world.run_id))
            .send()
            .await
            .expect("get run")
            .json()
            .await
            .expect("run json");
        let status = run["status"].as_str().unwrap_or("");
        if status != "running" && status != "waiting_for_approval" {
            return run;
        }
        assert!(std::time::Instant::now() < deadline, "run never terminated");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[when("I send approval input via REST")]
async fn i_send_approval_input_via_rest(world: &mut BddWorld) {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/v1/runs/{}/input", world.base_url, world.run_id))
        .json(&serde_json::json!({"input": "y"}))
        .send()
        .await
        .expect("input response");
    assert_eq!(response.status(), 202);
}

#[then("the run completes and the file exists")]
async fn run_completes_and_file_exists(world: &mut BddWorld) {
    let run = wait_terminal(world).await;
    assert_eq!(run["status"], "completed", "run: {run}");
    let content = std::fs::read_to_string(world.project().join("notes.txt")).expect("file exists");
    assert_eq!(content, "scripted content");
    let types: Vec<&str> = run["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|e| e["type"].as_str())
        .collect();
    for needle in [
        "approval_requested",
        "input_received",
        "approval_decided",
        "file_changed",
        "completed",
    ] {
        assert!(types.contains(&needle), "missing {needle} in {types:?}");
    }
}

#[when("I cancel the run via REST")]
async fn i_cancel_the_run_via_rest(world: &mut BddWorld) {
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/v1/runs/{}/cancel",
            world.base_url, world.run_id
        ))
        .send()
        .await
        .expect("cancel response");
    assert_eq!(response.status(), 200);
}

#[then("the run ends with a cancelled event")]
async fn run_ends_with_cancelled_event(world: &mut BddWorld) {
    let run = wait_terminal(world).await;
    assert_eq!(run["status"], "cancelled", "run: {run}");
    let events = run["events"].as_array().expect("events");
    assert!(
        events.iter().any(|e| e["type"] == "cancelled"),
        "no cancelled event: {events:?}"
    );
    assert!(
        !world.project().join("notes.txt").exists(),
        "cancelled run must not have written the file"
    );
}

// ---------------------------------------------------------------------------
// event-schema.feature
// ---------------------------------------------------------------------------

#[given("a session log written in the v1 event format")]
fn v1_session_log(world: &mut BddWorld) {
    // Literal v1 lines: v:1, no `seq`, no `prompt` on run_started,
    // f32-widened confidence value.
    world.write_file(
        ".forge/sessions/v1sess.jsonl",
        concat!(
            "{\"v\":1,\"ts\":\"2026-09-01T10:00:00Z\",\"run_id\":\"old-run\",\"session_id\":\"v1sess\",\"type\":\"run_started\",\"provider\":\"mock-local\",\"model\":\"mock-local\"}\n",
            "{\"v\":1,\"ts\":\"2026-09-01T10:00:01Z\",\"run_id\":\"old-run\",\"session_id\":\"v1sess\",\"type\":\"routing_decision_made\",\"router\":\"static\",\"selected_model\":\"mock-local\",\"confidence\":0.8999999761581421,\"fallback_used\":false}\n",
            "{\"v\":1,\"ts\":\"2026-09-01T10:00:02Z\",\"run_id\":\"old-run\",\"session_id\":\"v1sess\",\"type\":\"completed\",\"summary\":\"done\"}\n",
        ),
    );
}

#[when("I inspect the session")]
async fn i_inspect_the_session(world: &mut BddWorld) {
    world.run_forge(&["session", "show", "v1sess"]).await;
}

#[then("the v1 events are shown")]
fn v1_events_are_shown(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains("run_started"),
        "stdout: {}",
        world.last_stdout
    );
    assert!(
        world.last_stdout.contains("routing_decision"),
        "stdout: {}",
        world.last_stdout
    );
    assert!(
        world.last_stdout.contains("completed"),
        "stdout: {}",
        world.last_stdout
    );
}

// ---------------------------------------------------------------------------
// cost-routing.feature
// ---------------------------------------------------------------------------

#[given(expr = "models {string} costing {float} and {string} costing {float}")]
async fn models_with_costs(
    world: &mut BddWorld,
    cheap: String,
    cheap_cost: f64,
    pricey: String,
    pricey_cost: f64,
) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // The cheap model is served by a local OpenAI-compatible mock so the
    // run completes offline; the pricey one points at a closed port.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": "cheap answer" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;
    let chat_url = server.uri();
    world.chat_mock = Some(server);
    // The active model is part of the table (otherwise the built-in free
    // mock would always win cheapest).
    world.set_config("model", &format!("\"{cheap}\""));
    world.add_config_block(format!(
        "[models.{cheap}]\ncost_input_per_mtok = {cheap_cost}\ndescription = \"cheap test model\"\nbase_url = \"{chat_url}\""
    ));
    world.add_config_block(format!(
        "[models.{pricey}]\ncost_input_per_mtok = {pricey_cost}\ndescription = \"pricey test model\"\nbase_url = \"http://127.0.0.1:9\""
    ));
    // The built-in local model is free ($0) and would win "cheapest";
    // reprice it so the fixture's own entries decide the ranking.
    world.add_config_block(
        "[models.qwen3-coder]\ncost_input_per_mtok = 99.9\ncost_output_per_mtok = 99.9".to_string(),
    );
    // claude-sonnet is also free by default; reprice it out of the way.
    world.add_config_block(
        "[models.claude-sonnet]\ncost_input_per_mtok = 99.9\ncost_output_per_mtok = 99.9"
            .to_string(),
    );
}

#[given(expr = "router mode {string}")]
fn router_mode(world: &mut BddWorld, mode: String) {
    world.set_config("router", &format!("\"{mode}\""));
}

#[then(expr = "the cheapest model {string} is selected with a cost reason")]
fn cheapest_model_selected(world: &mut BddWorld, model: String) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let outcome: serde_json::Value =
        serde_json::from_str(world.last_stdout.trim()).expect("run json");
    let decision = outcome["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|e| e["type"] == "routing_decision_made")
        .expect("routing decision");
    assert_eq!(decision["selected_model"], model);
    assert_eq!(decision["router"], "cheapest");
    let reason = decision["reason"].as_str().expect("reason");
    assert!(reason.contains("cheapest"), "reason: {reason}");
    assert!(reason.contains("$0.1"), "reason: {reason}");
    // The cheap endpoint actually served the completion.
    assert_eq!(outcome["text"], "cheap answer");
}

#[given(expr = "a local Laya router answering {string} with confidence {float}")]
async fn laya_router_answering(world: &mut BddWorld, choice: String, confidence: f64) {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "answers": { "model": { "choice": choice, "confidence": confidence } },
            "routing": { "backend": "laya" }
        })))
        .mount(&server)
        .await;
    world.set_config("router", "\"laya\"");
    world.set_config("router_url", &format!("\"{}\"", server.uri()));
    // The answered model must be a candidate and resolvable offline.
    if choice == "mock-local" {
        world.set_config("model", "\"mock-local\"");
    }
    world.router_mock = Some(server);
}

#[then(expr = "the model {string} is selected with recorded confidence {float}")]
fn model_selected_with_confidence(world: &mut BddWorld, model: String, confidence: f64) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let log = world.session_log();
    let decision = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "routing_decision_made")
        .unwrap_or_else(|| panic!("no routing decision in: {log}"));
    assert_eq!(decision["selected_model"], model);
    assert_eq!(decision["router"], "laya");
    assert_eq!(decision["fallback_used"], false);
    let recorded = decision["confidence"].as_f64().expect("confidence");
    assert!(
        (recorded - confidence).abs() < 0.001,
        "confidence: {recorded}"
    );
}

#[given("the Laya router is unavailable")]
fn laya_router_unavailable(world: &mut BddWorld) {
    world.set_config("router", "\"laya\"");
    world.set_config("router_url", "\"http://127.0.0.1:9/decide\"");
    world.set_config("router_timeout_ms", "300");
}

#[given(expr = "the static fallback selects {string}")]
fn static_fallback_selects(world: &mut BddWorld, model: String) {
    world.set_config("model", &format!("\"{model}\""));
    world.set_config("router_fallback", "\"static\"");
}

#[given(expr = "the confidence threshold is {float}")]
fn confidence_threshold(world: &mut BddWorld, threshold: f64) {
    world.set_config("router_confidence_threshold", &threshold.to_string());
}

#[then("the routing decision fell back to static routing")]
fn routing_decision_fell_back(world: &mut BddWorld) {
    let log = world.session_log();
    let decision = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "routing_decision_made")
        .unwrap_or_else(|| panic!("no routing decision in: {log}"));
    assert_eq!(decision["fallback_used"], true);
    assert_eq!(decision["router"], "static");
    // The low-confidence reason is logged on stderr (the event schema
    // carries the decision, not the reason text).
    assert!(
        world.last_stderr.contains("below threshold"),
        "stderr: {}",
        world.last_stderr
    );
}

// ---------------------------------------------------------------------------
// env-files.feature
// ---------------------------------------------------------------------------

#[given(expr = "a project with a .env file setting the model to {string}")]
fn env_file_sets_model(world: &mut BddWorld, model: String) {
    world.write_file(".env", &format!("FORGE_MODEL={model}\n"));
}

#[given(expr = "a .env.local file setting the model to {string}")]
fn env_local_file_sets_model(world: &mut BddWorld, model: String) {
    world.write_file(".env.local", &format!("FORGE_MODEL={model}\n"));
}

// ---------------------------------------------------------------------------
// auth.feature
// ---------------------------------------------------------------------------

#[given("a Claude Code credentials file with a dummy token")]
fn claude_credentials_file(world: &mut BddWorld) {
    // The harness sets HOME to <project>/home for each forge invocation.
    world.write_file(
        "home/.claude/.credentials.json",
        r#"{"claudeOauth": {"accessToken": "sk-ant-oat01-bdd-dummy-token", "expiresAt": 1}}"#,
    );
}

#[given(expr = "the environment sets {string} to a dummy key")]
fn environment_sets_dummy_key(world: &mut BddWorld, name: String) {
    world.env.insert(name, "sk-bdd-dummy-env-key".to_string());
}

#[when("I check auth status")]
async fn i_check_auth_status(world: &mut BddWorld) {
    world.run_forge(&["auth", "status"]).await;
}

#[then("anthropic is reported detected without leaking the token")]
fn anthropic_detected_without_leak(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let anthropic = world
        .last_stdout
        .lines()
        .find(|l| l.starts_with("anthropic"))
        .unwrap_or_else(|| panic!("no anthropic row: {}", world.last_stdout));
    assert!(anthropic.contains("claude-sonnet"), "row: {anthropic}");
    assert!(
        anthropic.contains(".claude/.credentials.json"),
        "row: {anthropic}"
    );
    assert!(anthropic.contains("oauth"), "row: {anthropic}");
    for output in [&world.last_stdout, &world.last_stderr] {
        assert!(
            !output.contains("sk-ant-oat01-bdd-dummy-token"),
            "token leaked: {output}"
        );
    }
}

#[then("deepseek is reported detected via environment")]
fn deepseek_detected_via_env(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    let row = world
        .last_stdout
        .lines()
        .find(|l| l.starts_with("deepseek"))
        .unwrap_or_else(|| panic!("no deepseek row: {}", world.last_stdout));
    assert!(row.contains("deepseek-chat"), "row: {row}");
    assert!(row.contains("DEEPSEEK_API_KEY"), "row: {row}");
    assert!(row.contains("api-key"), "row: {row}");
    assert!(
        !world.last_stdout.contains("sk-bdd-dummy-env-key"),
        "key leaked: {}",
        world.last_stdout
    );
}

#[then("missing credentials are reported without failing")]
fn missing_credentials_reported(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains("not found"),
        "stdout: {}",
        world.last_stdout
    );
}

// ---------------------------------------------------------------------------
// needle_routing.feature
// ---------------------------------------------------------------------------

#[given("an initialized project with no needle weights")]
async fn initialized_project_no_needle_weights(world: &mut BddWorld) {
    // `run_forge` already injects `FORGE_NEEDLE_AUTOFETCH=false` and leaves
    // `FORGE_NEEDLE_BACKEND` unset for every invocation, so the starter
    // config's default `router = "needle"` ends up with no usable engine:
    // no weights fetched, no hash test backend selected, no `ffi` feature.
    world.run_forge(&["init"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[given("an initialized project with the hash needle backend")]
async fn initialized_project_hash_needle_backend(world: &mut BddWorld) {
    world
        .env
        .insert("FORGE_NEEDLE_BACKEND".to_string(), "hash".to_string());
    world.run_forge(&["init"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[when(expr = "I run forge with prompt {string} and model {string}")]
async fn run_forge_with_prompt_and_model(world: &mut BddWorld, prompt: String, model: String) {
    world.run_forge(&["--model", &model, "run", &prompt]).await;
}

#[then("the run completes successfully")]
fn the_run_completes_successfully(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[then("the session events contain a routing decision with fallback_used true")]
fn session_events_routing_decision_fallback_used(world: &mut BddWorld) {
    let log = world.session_log();
    let decision = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "routing_decision_made")
        .unwrap_or_else(|| panic!("no routing decision in session log: {log}"));
    assert_eq!(decision["fallback_used"], true, "decision: {decision}");
}

#[then(expr = "the session events contain a routing decision from router {string}")]
fn session_events_routing_decision_from_router(world: &mut BddWorld, router: String) {
    let log = world.session_log();
    let decision = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "routing_decision_made")
        .unwrap_or_else(|| panic!("no routing decision in session log: {log}"));
    assert_eq!(decision["router"], router, "decision: {decision}");
}

#[when("I run forge doctor")]
async fn run_forge_doctor(world: &mut BddWorld) {
    world.run_forge(&["doctor"]).await;
}

#[then(expr = "the doctor output mentions {string}")]
fn doctor_output_mentions(world: &mut BddWorld, needle: String) {
    assert!(
        world.last_stdout.contains(&needle),
        "stdout: {}",
        world.last_stdout
    );
}

// ---------------------------------------------------------------------------
// first-run.feature
// ---------------------------------------------------------------------------

#[given(expr = "a project config sets the router to {string}")]
fn project_config_sets_router(world: &mut BddWorld, router: String) {
    world.set_config("router", &format!("\"{router}\""));
}

#[given(expr = "a project config names the model key env var {string}")]
fn project_config_names_model_key_env(world: &mut BddWorld, env_name: String) {
    // Mocks never authenticate, so the mismatch check ignores them: name a
    // real (unreachable) endpoint model instead.
    world.set_config("model", "\"qwen3-coder\"");
    world.set_config("model_key_env", &format!("\"{env_name}\""));
}

#[given("a fresh project directory")]
fn fresh_project_directory(world: &mut BddWorld) {
    world.project();
}

#[when("I run forge init with --local-only")]
async fn run_forge_init_local_only(world: &mut BddWorld) {
    world.run_forge(&["init", "--local-only"]).await;
}

#[then(expr = "the init output mentions {string}")]
fn init_output_mentions(world: &mut BddWorld, text: String) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        world.last_stdout.contains(&text),
        "stdout: {}",
        world.last_stdout
    );
}

#[then("no file exists under the forge cache models directory")]
fn no_file_under_cache_models_dir(world: &mut BddWorld) {
    // `weights_path`/`cache_dir` resolve against `std::env::home_dir()`,
    // which `run_forge` points at `<scenario>/home` — the same hermetic
    // HOME every other scenario uses.
    let models_dir = world
        .project()
        .join("home")
        .join(".cache")
        .join("forge")
        .join("models");
    let has_files = models_dir.is_dir()
        && std::fs::read_dir(&models_dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
    assert!(
        !has_files,
        "unexpected file(s) under {}",
        models_dir.display()
    );
}

#[given("an initialized project with the hash needle backend and a built graph")]
async fn initialized_project_hash_backend_and_built_graph(world: &mut BddWorld) {
    world
        .env
        .insert("FORGE_NEEDLE_BACKEND".to_string(), "hash".to_string());
    world.write_file(
        "src/parser.rs",
        "pub fn parse_document(input: &str) -> usize {\n    input.len()\n}\n",
    );
    world.run_forge(&["init"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    // `forge init` builds the graph's structure but never embeds (no
    // model calls from `init`, ever); an explicit `graph build` with the
    // hash backend available produces the semantic index.
    world.run_forge(&["graph", "build"]).await;
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
}

#[when(expr = "I run forge graph grep --semantic {string}")]
async fn run_forge_graph_grep_semantic(world: &mut BddWorld, query: String) {
    world
        .run_forge(&["graph", "grep", "--semantic", &query])
        .await;
}

#[then("the output lists at least one symbol")]
fn output_lists_at_least_one_symbol(world: &mut BddWorld) {
    assert_eq!(world.last_code, Some(0), "stderr: {}", world.last_stderr);
    assert!(
        !world.last_stdout.contains("no semantic matches"),
        "stdout: {}",
        world.last_stdout
    );
    assert!(
        world.last_stdout.contains("::"),
        "expected at least one `file::symbol` result, stdout: {}",
        world.last_stdout
    );
}

// ---------------------------------------------------------------------------
// jev_escalation.feature
// ---------------------------------------------------------------------------

#[given("a Jev credential is set to a dummy key")]
fn jev_credential_dummy_key(world: &mut BddWorld) {
    world
        .env
        .insert("TYPESAFE_API_KEY".to_string(), "dummy-jev-key".to_string());
}

#[then(expr = "the routing decision reason mentions {string}")]
fn routing_decision_reason_mentions(world: &mut BddWorld, needle: String) {
    // Discriminates the escalation path from a plain needle->static run:
    // both satisfy "fallback_used true" alone, but only a run that actually
    // tried jev has the jev error folded into the FallbackRouter reason
    // chain (see `FallbackRouter::route`'s `"primary router failed ({err});
    // {reason}"` composition — the jev leg's error text names "jev router
    // request to ... failed/timed out").
    let log = world.session_log();
    let decision = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["type"] == "routing_decision_made")
        .unwrap_or_else(|| panic!("no routing decision in session log: {log}"));
    let reason = decision["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains(&needle),
        "reason {reason:?} does not mention {needle:?}"
    );
}

#[given("the Jev router endpoint is unreachable")]
fn jev_router_endpoint_unreachable(world: &mut BddWorld) {
    // `jev_url`, not `router_url`: the escalation tier resolves its
    // endpoint from `jev_url` only (never the generic `router_url`, which
    // in `router = "needle"` mode belongs to no router at all — see
    // `resolved_jev_url` in forge-providers). Port 9 (discard) is closed on
    // loopback in every CI/dev environment this suite runs in, so the
    // connection is refused immediately instead of hanging — the same
    // convention `configured_router_unavailable` uses for http/laya above.
    world.set_config("jev_url", "\"http://127.0.0.1:9/systemone\"");
}

// ---------------------------------------------------------------------------
// mcp.feature
// ---------------------------------------------------------------------------

#[when("an MCP client handshakes over stdio")]
async fn mcp_client_handshakes(world: &mut BddWorld) {
    world.start_mcp().await;
}

#[then("the tool list includes the forge graph, skill and run tools")]
async fn mcp_tool_list_includes_the_surface(world: &mut BddWorld) {
    let listed = world.mcp_request("tools/list", serde_json::json!({})).await;
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {listed}"))
        .clone();
    world.mcp_tools = tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    for expected in [
        "forge_graph_context",
        "forge_graph_map",
        "forge_skill_list",
        "forge_run",
        "forge_run_input",
    ] {
        assert!(
            world.mcp_tools.iter().any(|name| name == expected),
            "{expected} missing from {:?}",
            world.mcp_tools
        );
    }
}

#[then(expr = "calling {string} over MCP returns the project structure")]
async fn mcp_tool_call_returns_structure(world: &mut BddWorld, tool: String) {
    let called = world
        .mcp_request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": {} }),
        )
        .await;
    let text = called["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {called}"))
        .to_string();
    let payload: serde_json::Value =
        serde_json::from_str(&text).expect("tool result text is compact JSON");
    assert!(
        !payload["directories"]
            .as_array()
            .unwrap_or_else(|| panic!("no directories in {payload}"))
            .is_empty(),
        "the graph map should list at least one directory: {payload}"
    );
}

#[then("nothing but JSON-RPC reached stdout")]
fn mcp_stdout_is_pure_protocol(world: &mut BddWorld) {
    assert!(
        !world.mcp_lines.is_empty(),
        "expected protocol traffic on stdout"
    );
    for line in &world.mcp_lines {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("non-JSON line on stdout: {e}: {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "not a JSON-RPC message: {line}");
    }
}

// ---------------------------------------------------------------------------
// acp.feature
// ---------------------------------------------------------------------------

#[when("an ACP client starts a session over stdio")]
async fn acp_client_starts_a_session(world: &mut BddWorld) {
    world.start_acp().await;
    assert!(
        !world.acp_session.is_empty(),
        "session/new should have returned a session id"
    );
}

#[when(expr = "the ACP client prompts {string}")]
async fn acp_client_prompts(world: &mut BddWorld, prompt: String) {
    world.acp_prompt(&prompt).await;
}

#[then("the ACP client was asked for permission in the editor")]
fn acp_client_was_asked_for_permission(world: &mut BddWorld) {
    // The distinctive ACP affordance: because stdio is the protocol
    // channel the loop cannot prompt on the terminal, so the risky write
    // becomes a `session/request_permission` the editor renders itself.
    assert_eq!(
        world.acp_permissions, 1,
        "expected exactly one session/request_permission"
    );
}

#[then(expr = "the ACP turn ends with stop reason {string}")]
fn acp_turn_ends_with_stop_reason(world: &mut BddWorld, expected: String) {
    assert_eq!(world.acp_stop_reason, expected);
}

#[then("the ACP client saw the tool call and the agent's final message")]
fn acp_client_saw_tool_call_and_message(world: &mut BddWorld) {
    let kinds: Vec<&str> = world
        .acp_updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    assert!(
        kinds.contains(&"tool_call"),
        "expected a tool_call update, saw {kinds:?}"
    );
    assert!(
        world
            .acp_updates
            .iter()
            .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed"),
        "expected the tool call to reach completed, saw {:?}",
        world.acp_updates
    );
    assert!(
        world
            .acp_updates
            .iter()
            .any(|u| u["sessionUpdate"] == "agent_message_chunk"
                && u["content"]["text"] == "all done"),
        "expected the final text as an agent_message_chunk, saw {:?}",
        world.acp_updates
    );
}

#[then("nothing but JSON-RPC reached the ACP stdout")]
fn acp_stdout_is_pure_protocol(world: &mut BddWorld) {
    assert!(
        !world.acp_lines.is_empty(),
        "expected protocol traffic on stdout"
    );
    for line in &world.acp_lines {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("non-JSON line on stdout: {e}: {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "not a JSON-RPC message: {line}");
    }
}
