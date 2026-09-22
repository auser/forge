use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_config::Config;
use forge_execution::MockExecution;
use forge_graph::LocalGraph;
use forge_providers::{MockModel, MockRouter};
use forge_runtime::AgentService;
use forge_session::JsonlSessionStore;
use forge_skills::FsSkillRegistry;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Compose a fully offline test app: mock model/router/execution, tempdir
/// session store, empty skill registry, graph over the temp project.
fn test_app(project: &std::path::Path) -> Router {
    let service = Arc::new(AgentService::new(
        Arc::new(MockModel::new()),
        Arc::new(MockRouter::selecting("mock-local")),
        Arc::new(MockExecution::new()),
        Arc::new(FsSkillRegistry::with_roots(vec![], None)),
        Arc::new(JsonlSessionStore::new(
            project.join(".forge").join("sessions"),
        )),
        Config::default(),
    ));
    let skills = service.skills().clone();
    let graph = Arc::new(LocalGraph::open(project).expect("open graph"));
    forge_server::build_router(service, skills, Some(graph), Config::default())
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).expect("json"))
}

async fn post_json(
    app: &Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).expect("json"))
}

/// Poll GET /v1/runs/:id until the run reaches a terminal status.
async fn wait_for_terminal(app: &Router, run_id: &str) -> serde_json::Value {
    for _ in 0..100 {
        let (status, body) = get_json(app, &format!("/v1/runs/{run_id}")).await;
        assert_eq!(status, StatusCode::OK);
        if body["status"] != "running" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not finish in time");
}

#[tokio::test]
async fn health_capabilities_models() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());

    let (status, body) = get_json(&app, "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());

    let (status, body) = get_json(&app, "/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["model"]["name"], "mock-local");
    assert_eq!(body["model"]["capabilities"]["tools"], true);
    assert_eq!(body["router"], "static");
    assert_eq!(body["execution"], "native");

    let (status, body) = get_json(&app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    let models = body["models"].as_array().expect("models array");
    assert_eq!(models[0]["name"], "mock-local");
    assert_eq!(models[0]["active"], true);
}

#[tokio::test]
async fn run_lifecycle_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());

    let (status, body) = post_json(&app, "/v1/runs", serde_json::json!({"prompt": "hi"})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let run_id = body["run_id"].as_str().expect("run_id").to_string();
    assert!(body["session_id"].is_string());

    let run = wait_for_terminal(&app, &run_id).await;
    assert_eq!(run["status"], "completed");
    let events = run["events"].as_array().expect("events");
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["type"].as_str().expect("type"))
        .collect();
    assert_eq!(types, ["run_started", "routing_decision_made", "completed"]);
}

#[tokio::test]
async fn run_input_records_note_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());

    let (_, body) = post_json(&app, "/v1/runs", serde_json::json!({"prompt": "hi"})).await;
    let run_id = body["run_id"].as_str().expect("run_id").to_string();
    wait_for_terminal(&app, &run_id).await;

    let (status, _) = post_json(
        &app,
        &format!("/v1/runs/{run_id}/input"),
        serde_json::json!({"input": "please also check tests"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (_, run) = get_json(&app, &format!("/v1/runs/{run_id}")).await;
    let events = run["events"].as_array().expect("events");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "note" && e["message"] == "please also check tests"),
        "events: {events:?}"
    );

    // Unknown run → 404.
    let (status, _) = post_json(
        &app,
        "/v1/runs/nope/input",
        serde_json::json!({"input": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cancel_unknown_run_is_404() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());
    let (status, body) = post_json(&app, "/v1/runs/nope/cancel", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().expect("error").contains("nope"));
}

#[tokio::test]
async fn cancel_completed_run_records_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());
    let (_, body) = post_json(&app, "/v1/runs", serde_json::json!({"prompt": "hi"})).await;
    let run_id = body["run_id"].as_str().expect("run_id").to_string();
    wait_for_terminal(&app, &run_id).await;

    let (status, _) = post_json(
        &app,
        &format!("/v1/runs/{run_id}/cancel"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, run) = get_json(&app, &format!("/v1/runs/{run_id}")).await;
    let events = run["events"].as_array().expect("events");
    assert!(events.iter().any(|e| e["type"] == "cancelled"));
}

#[tokio::test]
async fn sse_replays_and_terminates_after_completion() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());

    let (_, body) = post_json(&app, "/v1/runs", serde_json::json!({"prompt": "hi"})).await;
    let run_id = body["run_id"].as_str().expect("run_id").to_string();
    wait_for_terminal(&app, &run_id).await;

    // Subscribe after finish: the stream must replay and end.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{run_id}/events"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .expect("content-type"),
        "text/event-stream"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("sse body")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert!(text.contains("\"type\":\"run_started\""), "sse: {text}");
    assert!(
        text.contains("\"type\":\"routing_decision_made\""),
        "sse: {text}"
    );
    assert!(text.contains("\"type\":\"completed\""), "sse: {text}");
    assert!(text.lines().all(|l| l.is_empty() || l.starts_with("data:")));
}

#[tokio::test]
async fn sse_streams_live_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());

    // Open the SSE stream while the run is (almost certainly) in flight:
    // create the run and immediately attach. Even if the run finishes
    // first, replay guarantees the events arrive — assert the terminal
    // event either way.
    let (_, body) = post_json(&app, "/v1/runs", serde_json::json!({"prompt": "hi"})).await;
    let run_id = body["run_id"].as_str().expect("run_id").to_string();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{run_id}/events"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("sse body")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert!(text.contains("\"type\":\"run_started\""), "sse: {text}");
    assert!(text.contains("\"type\":\"completed\""), "sse: {text}");
    // Exactly one run_started — replay/live dedup works.
    assert_eq!(text.matches("\"type\":\"run_started\"").count(), 1);
}

#[tokio::test]
async fn skills_graph_and_context_endpoints() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A small project so the graph and context have content.
    std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
    std::fs::write(
        tmp.path().join("src/main.rs"),
        "fn main() {\n    compute();\n}\nfn compute() -> i32 { 1 }\n",
    )
    .expect("write");
    std::fs::create_dir_all(tmp.path().join(".forge/graph")).expect("mkdir");

    let app = test_app(tmp.path());

    let (status, body) = get_json(&app, "/v1/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"], serde_json::json!([]));

    // Graph not built yet.
    let (status, body) = get_json(&app, "/v1/project/graph").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["built"], false);

    let (status, _) = post_json(
        &app,
        "/v1/project/context",
        serde_json::json!({"query": "compute"}),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    // Build the graph, reopen, rebuild the app.
    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    forge_core::ProjectGraph::build(&mut graph).expect("build");
    let app = test_app(tmp.path());

    let (status, body) = get_json(&app, "/v1/project/graph").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["built"], true);
    assert_eq!(body["fresh"], true);
    assert!(body["stats"]["files"].as_u64().expect("files") >= 1);

    let (status, body) = post_json(
        &app,
        "/v1/project/context",
        serde_json::json!({"query": "compute", "limit": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results");
    assert_eq!(results[0]["path"], "src/main.rs");
}

#[tokio::test]
async fn serves_on_a_real_ephemeral_port() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = test_app(tmp.path());
    let (addr, server) = forge_server::bind(app, "127.0.0.1", 0).await.expect("bind");
    let task = tokio::spawn(server);

    let response = reqwest::get(format!("http://{addr}/health"))
        .await
        .expect("http get");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["status"], "ok");

    task.abort();
}
