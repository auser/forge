//! Tool-surface tests. Everything here runs against a real
//! [`AgentService`] built from offline providers in a tempdir — no
//! process, no JSON-RPC, no network.

use std::sync::Arc;

use forge_config::Config;
use forge_core::ProjectGraph;
use forge_execution::MockExecution;
use forge_graph::LocalGraph;
use forge_providers::{MockModel, MockRouter};
use forge_runtime::AgentService;
use forge_session::JsonlSessionStore;
use forge_skills::{FsSkillRegistry, SkillSource};

use super::*;

/// A project with two source files and one skill, plus a built graph.
fn project(dir: &std::path::Path) {
    std::fs::write(
        dir.join("alpha.rs"),
        "fn parse_config() {}\nfn render_report() {}\n",
    )
    .expect("write alpha");
    std::fs::write(dir.join("beta.rs"), "fn serve_http() {}\n").expect("write beta");

    let skill_dir = dir.join(".forge").join("skills").join("reviewing");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: reviewing\ndescription: Review a diff carefully\n---\n\nStep one: read the diff.\n",
    )
    .expect("write skill");
}

fn build_graph(dir: &std::path::Path) {
    let mut graph = LocalGraph::open(dir).expect("open graph");
    graph.build().expect("build graph");
}

fn tools_for(dir: &std::path::Path, config: Config) -> ForgeTools {
    tools_with_model(dir, config, Arc::new(MockModel::new()))
}

fn tools_with_model(
    dir: &std::path::Path,
    config: Config,
    model: Arc<dyn forge_core::ModelProvider>,
) -> ForgeTools {
    let service = Arc::new(AgentService::new(
        model,
        Arc::new(MockRouter::selecting("mock-local")),
        Arc::new(MockExecution::new(dir)),
        // Explicit roots, not `FsSkillRegistry::new`: the real
        // constructor also scans the developer's `$HOME` skill
        // directories, which would make these assertions depend on the
        // machine running them.
        Arc::new(FsSkillRegistry::with_roots(
            vec![(SkillSource::ProjectForge, dir.join(".forge").join("skills"))],
            None,
        )),
        Arc::new(JsonlSessionStore::new(dir.join(".forge").join("sessions"))),
        config,
    ));
    ForgeTools::new(service, dir)
}

/// A project + built graph + tools, all in one tempdir.
fn fixture() -> (tempfile::TempDir, ForgeTools) {
    let tmp = tempfile::tempdir().expect("tempdir");
    project(tmp.path());
    build_graph(tmp.path());
    let tools = tools_for(tmp.path(), Config::default());
    (tmp, tools)
}

/// A model that takes a known, non-trivial amount of time to answer, so
/// the `forge_run` timeout branch is reached deterministically instead of
/// racing the mock.
struct SlowModel;

#[async_trait]
impl forge_core::ModelProvider for SlowModel {
    fn name(&self) -> &str {
        "slow-test"
    }

    fn capabilities(&self) -> forge_core::ModelCapabilities {
        forge_core::ModelCapabilities {
            streaming: false,
            tools: true,
            structured_output: false,
            vision: false,
            max_context: 8_192,
        }
    }

    async fn complete(
        &self,
        _request: forge_core::CompletionRequest,
    ) -> Result<forge_core::CompletionResponse, ForgeError> {
        tokio::time::sleep(Duration::from_millis(600)).await;
        Ok(forge_core::CompletionResponse {
            model: "slow-test".to_string(),
            content: "slow but finished".to_string(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".to_string()),
            usage: None,
        })
    }
}

struct FakeDoctor;

#[async_trait]
impl Diagnostics for FakeDoctor {
    async fn report(&self) -> Result<Value, ForgeError> {
        Ok(json!({ "healthy": true, "checks": [{ "status": "ok", "check": "stub" }] }))
    }
}

// --- registry / schemas -------------------------------------------------

#[test]
fn every_tool_has_a_valid_object_schema() {
    for def in definitions() {
        let schema = (def.input_schema)();
        assert_eq!(
            schema["type"], "object",
            "{}: MCP requires an object inputSchema",
            def.name
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{}: properties must be an object", def.name));
        for required in schema["required"].as_array().unwrap_or(&Vec::new()) {
            let key = required.as_str().unwrap_or_default();
            assert!(
                properties.contains_key(key),
                "{}: required {key:?} is not declared in properties",
                def.name
            );
        }
        for (name, property) in properties {
            assert!(
                property["type"].is_string(),
                "{}: property {name:?} has no type",
                def.name
            );
            assert!(
                property["description"].is_string(),
                "{}: property {name:?} has no description (clients render it)",
                def.name
            );
        }
        assert!(
            !def.description.is_empty(),
            "{}: tools need a description",
            def.name
        );
    }
}

#[test]
fn the_tool_surface_is_the_documented_one_in_a_stable_order() {
    let names: Vec<&str> = definitions().iter().map(|d| d.name).collect();
    assert_eq!(
        names,
        vec![
            "forge_graph_context",
            "forge_graph_grep",
            "forge_graph_map",
            "forge_skill_list",
            "forge_skill_show",
            "forge_doctor",
            "forge_run",
            "forge_run_status",
            "forge_run_input",
            "forge_run_cancel",
        ]
    );
}

#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error_not_a_tool_error() {
    let (_tmp, tools) = fixture();
    let err = tools
        .call("forge_nope", &json!({}))
        .await
        .expect_err("unknown tool");
    assert_eq!(err, ToolError::UnknownTool("forge_nope".to_string()));
}

// --- argument validation ------------------------------------------------

#[tokio::test]
async fn missing_required_arguments_are_tool_errors_naming_the_argument() {
    let (_tmp, tools) = fixture();
    for (tool, arg) in [
        ("forge_graph_context", "query"),
        ("forge_graph_grep", "pattern"),
        ("forge_skill_show", "name"),
        ("forge_run", "prompt"),
        ("forge_run_status", "run_id"),
        ("forge_run_input", "run_id"),
        ("forge_run_cancel", "run_id"),
    ] {
        let outcome = tools.call(tool, &json!({})).await.expect("dispatch");
        assert!(outcome.is_error, "{tool}: should be a tool error");
        assert_eq!(outcome.value["code"], "invalid_params", "{tool}");
        let message = outcome.value["error"].as_str().unwrap_or_default();
        assert!(message.contains(arg), "{tool}: {message}");
    }
}

#[tokio::test]
async fn wrongly_typed_arguments_are_rejected_with_the_expected_type() {
    let (_tmp, tools) = fixture();

    let outcome = tools
        .call("forge_graph_context", &json!({ "query": 7 }))
        .await
        .expect("dispatch");
    assert!(outcome.is_error);
    assert!(
        outcome.value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("must be a string")
    );

    let outcome = tools
        .call(
            "forge_graph_context",
            &json!({ "query": "x", "limit": "ten" }),
        )
        .await
        .expect("dispatch");
    assert!(outcome.is_error);
    assert!(
        outcome.value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("must be an integer")
    );

    let outcome = tools
        .call(
            "forge_graph_grep",
            &json!({ "pattern": "x", "semantic": "yes" }),
        )
        .await
        .expect("dispatch");
    assert!(outcome.is_error);
    assert!(
        outcome.value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("must be a boolean")
    );
}

#[tokio::test]
async fn an_empty_prompt_is_rejected_before_a_run_starts() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_run", &json!({ "prompt": "   " }))
        .await
        .expect("dispatch");
    assert!(outcome.is_error);
    assert_eq!(outcome.value["code"], "invalid_params");
}

// --- graph tools --------------------------------------------------------

#[tokio::test]
async fn graph_map_summarizes_the_project() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_graph_map", &json!({}))
        .await
        .expect("map");
    assert!(!outcome.is_error, "{:?}", outcome.value);
    let dirs = outcome.value["directories"]
        .as_array()
        .expect("directories");
    assert!(!dirs.is_empty());
    assert!(dirs.iter().any(|d| d["symbols"].as_u64().unwrap_or(0) > 0));
}

#[tokio::test]
async fn graph_tools_on_an_unbuilt_project_name_the_fix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    project(tmp.path()); // deliberately not built
    let tools = tools_for(tmp.path(), Config::default());

    let outcome = tools
        .call("forge_graph_map", &json!({}))
        .await
        .expect("map");
    assert!(outcome.is_error);
    assert_eq!(outcome.value["code"], "graph_not_built");
    assert!(
        outcome.value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("forge graph build")
    );
}

#[tokio::test]
async fn graph_context_ranks_files_and_honours_the_limit() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_graph_context", &json!({ "query": "parse_config" }))
        .await
        .expect("context");
    assert!(!outcome.is_error, "{:?}", outcome.value);
    let hits = outcome.value["hits"].as_array().expect("hits");
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["path"], "alpha.rs");

    let limited = tools
        .call("forge_graph_context", &json!({ "query": "fn", "limit": 1 }))
        .await
        .expect("context");
    assert!(limited.value["hits"].as_array().expect("hits").len() <= 1);
}

#[tokio::test]
async fn graph_grep_matches_symbols() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_graph_grep", &json!({ "pattern": "serve_http" }))
        .await
        .expect("grep");
    assert!(!outcome.is_error, "{:?}", outcome.value);
    let matches = outcome.value["matches"].as_array().expect("matches");
    assert!(!matches.is_empty(), "expected a match for serve_http");
}

#[tokio::test]
async fn semantic_grep_without_an_engine_is_a_tool_error_naming_the_fix() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call(
            "forge_graph_grep",
            &json!({ "pattern": "http server", "semantic": true }),
        )
        .await
        .expect("grep");
    assert!(
        outcome.is_error,
        "semantic without weights must not succeed"
    );
    assert_eq!(outcome.value["code"], "semantic_unavailable");
    let message = outcome.value["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("weights") && message.contains("forge init"),
        "unhelpful message: {message}"
    );
}

// --- skills -------------------------------------------------------------

#[tokio::test]
async fn skill_list_is_metadata_only() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_skill_list", &json!({}))
        .await
        .expect("list");
    assert!(!outcome.is_error);
    let skills = outcome.value["skills"].as_array().expect("skills");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "reviewing");
    assert_eq!(skills[0]["description"], "Review a diff carefully");
    assert!(
        skills[0].get("instructions").is_none(),
        "progressive disclosure: no instructions in the list"
    );
}

#[tokio::test]
async fn skill_show_returns_full_instructions() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_skill_show", &json!({ "name": "reviewing" }))
        .await
        .expect("show");
    assert!(!outcome.is_error, "{:?}", outcome.value);
    assert!(
        outcome.value["instructions"]
            .as_str()
            .unwrap_or_default()
            .contains("Step one")
    );
}

#[tokio::test]
async fn skill_show_for_an_unknown_skill_is_a_tool_error() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_skill_show", &json!({ "name": "nope" }))
        .await
        .expect("show");
    assert!(outcome.is_error);
    assert_eq!(outcome.value["code"], "unknown_skill");
}

// --- doctor -------------------------------------------------------------

#[tokio::test]
async fn doctor_without_a_provider_reports_unavailable() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_doctor", &json!({}))
        .await
        .expect("doctor");
    assert!(outcome.is_error);
    assert_eq!(outcome.value["code"], "unavailable");
}

#[tokio::test]
async fn doctor_returns_the_shared_check_report() {
    let tmp = tempfile::tempdir().expect("tempdir");
    project(tmp.path());
    build_graph(tmp.path());
    let tools = tools_for(tmp.path(), Config::default()).with_diagnostics(Arc::new(FakeDoctor));

    let outcome = tools
        .call("forge_doctor", &json!({}))
        .await
        .expect("doctor");
    assert!(!outcome.is_error);
    assert_eq!(outcome.value["healthy"], true);
    assert_eq!(outcome.value["checks"][0]["check"], "stub");
}

// --- runs ---------------------------------------------------------------

#[tokio::test]
async fn run_completes_and_reports_text_and_router() {
    let (_tmp, tools) = fixture();
    let outcome = tools
        .call("forge_run", &json!({ "prompt": "say hello" }))
        .await
        .expect("run");
    assert!(!outcome.is_error, "{:?}", outcome.value);
    assert_eq!(outcome.value["status"], "completed");
    assert!(outcome.value["run_id"].as_str().is_some());
    assert!(
        !outcome.value["text"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "a completed run must carry its final text"
    );
    assert_eq!(outcome.value["router"], "mock");
}

#[tokio::test]
async fn run_status_reports_a_completed_run_with_its_events() {
    let (_tmp, tools) = fixture();
    let run = tools
        .call("forge_run", &json!({ "prompt": "say hello" }))
        .await
        .expect("run");
    let run_id = run.value["run_id"].as_str().expect("run id");

    let status = tools
        .call("forge_run_status", &json!({ "run_id": run_id }))
        .await
        .expect("status");
    assert!(!status.is_error, "{:?}", status.value);
    assert_eq!(status.value["status"], "completed");
    assert_eq!(status.value["text"], run.value["text"]);
    let events = status.value["last_events"].as_array().expect("events");
    assert!(!events.is_empty());
    assert!(events.len() <= STATUS_EVENT_WINDOW);
}

#[tokio::test]
async fn a_timed_out_run_keeps_going_and_is_pollable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    project(tmp.path());
    build_graph(tmp.path());
    // The model takes 600 ms; 50 ms of patience guarantees the timeout
    // branch without depending on machine speed.
    let tools = tools_with_model(tmp.path(), Config::default(), Arc::new(SlowModel));

    let outcome = tools
        .call(
            "forge_run",
            &json!({ "prompt": "say hello", "timeout_ms": 50 }),
        )
        .await
        .expect("run");
    assert!(!outcome.is_error, "a timeout is not an error: {outcome:?}");
    let run_id = outcome.value["run_id"]
        .as_str()
        .expect("run id")
        .to_string();
    // Derived from the event log rather than hardcoded, and carrying what
    // the client needs to follow up.
    assert_eq!(outcome.value["status"], "running", "{:?}", outcome.value);
    assert!(outcome.value["session_id"].as_str().is_some());
    assert!(
        outcome.value["note"]
            .as_str()
            .unwrap_or_default()
            .contains("forge_run_status"),
        "{:?}",
        outcome.value
    );

    // The monitor settles it shortly after; polling must converge.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = tools
            .call("forge_run_status", &json!({ "run_id": run_id }))
            .await
            .expect("status");
        if status.value["status"] == "completed" {
            assert!(
                !status.value["text"].as_str().unwrap_or_default().is_empty(),
                "the full text survives a timed-out call"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never settled: {:?}",
            status.value
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn run_tools_reject_unknown_run_ids() {
    let (_tmp, tools) = fixture();
    for (tool, args) in [
        ("forge_run_status", json!({ "run_id": "nope" })),
        ("forge_run_input", json!({ "run_id": "nope", "input": "y" })),
        ("forge_run_cancel", json!({ "run_id": "nope" })),
    ] {
        let outcome = tools.call(tool, &args).await.expect("dispatch");
        assert!(outcome.is_error, "{tool} should reject unknown runs");
        assert_eq!(outcome.value["code"], "unknown_run", "{tool}");
    }
}

#[tokio::test]
async fn run_input_for_a_finished_run_says_so() {
    let (_tmp, tools) = fixture();
    let run = tools
        .call("forge_run", &json!({ "prompt": "say hello" }))
        .await
        .expect("run");
    let run_id = run.value["run_id"].as_str().expect("run id");

    let outcome = tools
        .call(
            "forge_run_input",
            &json!({ "run_id": run_id, "input": "y" }),
        )
        .await
        .expect("input");
    assert!(outcome.is_error);
    assert_eq!(outcome.value["code"], "run_finished");
    assert!(
        outcome.value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("completed")
    );
}

#[tokio::test]
async fn cancelling_a_finished_run_records_the_cancellation() {
    let (_tmp, tools) = fixture();
    let run = tools
        .call("forge_run", &json!({ "prompt": "say hello" }))
        .await
        .expect("run");
    let run_id = run.value["run_id"].as_str().expect("run id");

    let outcome = tools
        .call("forge_run_cancel", &json!({ "run_id": run_id }))
        .await
        .expect("cancel");
    assert!(!outcome.is_error, "{:?}", outcome.value);
    assert_eq!(outcome.value["cancelled"], run_id);
}

#[tokio::test]
async fn run_input_refuses_a_run_another_process_already_finished() {
    // Not in our registry, terminal in the event log: the REST adapter
    // answers 409 here, so this must not look like a live run either.
    let (tmp, tools) = fixture();
    let run = tools
        .call("forge_run", &json!({ "prompt": "say hello" }))
        .await
        .expect("run");
    let run_id = run.value["run_id"].as_str().expect("run id").to_string();

    // A fresh host over the same session store has no memory of the run.
    let fresh = tools_for(tmp.path(), Config::default());
    let outcome = fresh
        .call(
            "forge_run_input",
            &json!({ "run_id": run_id, "input": "y" }),
        )
        .await
        .expect("input");
    assert!(outcome.is_error);
    assert_eq!(outcome.value["code"], "run_finished", "{:?}", outcome.value);
}

/// A tool host whose agent loop actually parks: a scripted model that asks
/// to write a file, and `NativeExecution` under `approval = "prompt"`.
/// Tests run with a non-terminal stdin, so the write pauses the run with
/// `ApprovalRequired` instead of prompting.
fn approval_fixture() -> (tempfile::TempDir, ForgeTools) {
    let tmp = tempfile::tempdir().expect("tempdir");
    project(tmp.path());
    build_graph(tmp.path());

    let script = vec![
        forge_providers::ScriptedReply {
            text: None,
            tool_calls: vec![forge_core::ToolCall {
                id: "call_1".to_string(),
                name: "write_file".to_string(),
                arguments: json!({ "path": "notes.txt", "content": "scripted" }),
            }],
        },
        forge_providers::ScriptedReply {
            text: Some("all done".to_string()),
            tool_calls: Vec::new(),
        },
    ];
    let service = Arc::new(AgentService::new(
        Arc::new(forge_providers::ScriptedMockModel::new(script)),
        Arc::new(MockRouter::selecting("scripted-mock")),
        Arc::new(forge_execution::NativeExecution::new(
            forge_core::ApprovalPolicy::Prompt,
            tmp.path(),
        )),
        Arc::new(FsSkillRegistry::with_roots(vec![], None)),
        Arc::new(JsonlSessionStore::new(
            tmp.path().join(".forge").join("sessions"),
        )),
        Config::default(),
    ));
    let tools = ForgeTools::new(service, tmp.path());
    (tmp, tools)
}

/// The regression this exists for: a parked run is blocked *inside* the
/// loop, so nothing will ever complete it. `forge_run` must say
/// `waiting_for_approval` as soon as the request is emitted — not sit out
/// the whole timeout and then guess "running".
#[tokio::test]
async fn run_reports_waiting_for_approval_without_waiting_out_the_timeout() {
    let (tmp, tools) = approval_fixture();

    // A budget far longer than the test may take: if the approval arm did
    // not fire, this would block for 30 s and the elapsed assertion below
    // would fail rather than the test hanging forever.
    let started = std::time::Instant::now();
    let outcome = tools
        .call(
            "forge_run",
            &json!({ "prompt": "write the notes", "timeout_ms": 30_000 }),
        )
        .await
        .expect("run");
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.value["status"], "waiting_for_approval",
        "{:?}",
        outcome.value
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "should return as soon as the approval lands, took {elapsed:?}"
    );
    assert!(
        outcome.value["note"]
            .as_str()
            .unwrap_or_default()
            .contains("forge_run_input"),
        "the client must be told how to answer: {:?}",
        outcome.value
    );
    assert!(outcome.value["session_id"].as_str().is_some());
    assert!(
        !tmp.path().join("notes.txt").exists(),
        "the write must not have happened yet"
    );

    // And the run is answerable: approving completes it.
    let run_id = outcome.value["run_id"]
        .as_str()
        .expect("run id")
        .to_string();
    let delivered = tools
        .call(
            "forge_run_input",
            &json!({ "run_id": run_id, "input": "y" }),
        )
        .await
        .expect("input");
    assert!(!delivered.is_error, "{:?}", delivered.value);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = tools
            .call("forge_run_status", &json!({ "run_id": run_id }))
            .await
            .expect("status");
        if status.value["status"] == "completed" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "approved run never completed: {:?}",
            status.value
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(tmp.path().join("notes.txt").is_file());
}
