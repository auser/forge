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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "selected_model": "routed-model",
            "confidence": 0.9,
            "reason": "bdd test router"
        })))
        .mount(&server)
        .await;
    world.write_file(
        ".forge/config.toml",
        &format!(
            "router = \"http\"\nrouter_url = \"{}/route\"\nrouter_timeout_ms = 5000\n",
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
    // Default config is model = "mock-local"; just start the server.
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
    assert_eq!(value["config"]["model"], "mock-local");
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
