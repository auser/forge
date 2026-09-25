use std::collections::HashSet;
use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use forge_core::{Event, ForgeError, ProjectGraph};
use serde::Deserialize;

use crate::state::{AppState, RunStatus};

/// HTTP error with a status code and a JSON `{"error": ...}` body.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn not_found(message: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message,
        }
    }
}

impl From<ForgeError> for ApiError {
    fn from(err: ForgeError) -> Self {
        let status = match &err {
            ForgeError::Session(_) => StatusCode::NOT_FOUND,
            // The session exists and is fine; it is busy. A retry after the
            // in-flight run finishes succeeds, which is exactly 409.
            ForgeError::SessionBusy { .. } => StatusCode::CONFLICT,
            ForgeError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            ForgeError::Config(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

pub async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

pub async fn capabilities(State(state): State<AppState>) -> Json<serde_json::Value> {
    let model = state.service.model();
    Json(serde_json::json!({
        "server": {
            "name": "forge-server",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "model": {
            "name": model.name(),
            "capabilities": model.capabilities(),
        },
        "router": state.config.router,
        "execution": state.config.execution,
        "local_only": state.config.local_only,
    }))
}

pub async fn models(State(state): State<AppState>) -> Json<serde_json::Value> {
    let active = state.service.model();
    let mut models = vec![serde_json::json!({
        "name": active.name(),
        "capabilities": active.capabilities(),
        "active": true,
    })];
    // `mock-local` is a test-only provider (`forge_config::test_mocks`), so
    // it is only advertised when the gate that would let a client actually
    // select it is open. Otherwise this endpoint would offer a model that
    // configuration refuses to build.
    if active.name() != "mock-local" && forge_config::test_mocks_allowed() {
        models.push(serde_json::json!({
            "name": "mock-local",
            "capabilities": forge_core::ModelCapabilities {
                streaming: true,
                tools: true,
                structured_output: true,
                vision: false,
                max_context: 32_768,
            },
            "active": false,
            "note": "test-only mock",
        }));
    }
    Json(serde_json::json!({ "models": models }))
}

#[derive(Debug, Deserialize)]
pub struct CreateRun {
    prompt: String,
    #[serde(default)]
    session_id: Option<String>,
}

/// Start a run on a tokio task; respond immediately with 202 + ids.
///
/// Naming a `session_id` whose run is still in flight is **409 Conflict**:
/// the caller may not put two runs in one session, because their events would
/// interleave in the session log and corrupt its replay. Waiting for the run
/// (or cancelling it) and retrying is the fix, which is what 409 tells a
/// client.
pub async fn create_run(
    State(state): State<AppState>,
    Json(body): Json<CreateRun>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if body.prompt.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "prompt must not be empty".to_string(),
        });
    }
    let forge_runtime::StartedRun {
        run_id,
        session_id,
        handle,
    } = state.service.start_run(body.prompt, body.session_id)?;

    state.insert_run(
        run_id.clone(),
        crate::state::RunEntry {
            status: RunStatus::Running,
            abort: handle.abort_handle(),
        },
    );

    // Monitor: update the status map when the task finishes.
    let monitor = state.clone();
    let monitor_id = run_id.clone();
    tokio::spawn(async move {
        let status = match handle.await {
            Ok(Ok(_)) => RunStatus::Completed,
            Ok(Err(e)) => RunStatus::Failed(e.to_string()),
            Err(e) if e.is_cancelled() => RunStatus::Cancelled,
            Err(e) => RunStatus::Failed(format!("task join error: {e}")),
        };
        monitor.set_status(&monitor_id, status);
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "run_id": run_id,
            "session_id": session_id,
        })),
    ))
}

/// Derive a status from stored events for runs this server did not start
/// (e.g. after a restart, or CLI-created runs).
fn status_from_events(events: &[Event]) -> RunStatus {
    match events.last().map(|e| &e.kind) {
        Some(forge_core::EventKind::Completed { .. }) => RunStatus::Completed,
        Some(forge_core::EventKind::Cancelled { .. }) => RunStatus::Cancelled,
        Some(forge_core::EventKind::Error { message }) => RunStatus::Failed(message.clone()),
        // Parked in an approval wait.
        Some(forge_core::EventKind::ApprovalRequested { .. }) => RunStatus::WaitingForApproval,
        _ => RunStatus::Running,
    }
}

pub async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let in_flight = state.status_of(&id);
    let events = state.service.events(&id).unwrap_or_default();

    let status = match (in_flight, events.is_empty()) {
        // Known to this server: trust the task map even before the first
        // event lands in the store.
        (Some(status), _) => status,
        // Not ours, but has stored events (e.g. after a restart).
        (None, false) => status_from_events(&events),
        (None, true) => return Err(ApiError::not_found(format!("unknown run: {id}"))),
    };
    // A "running" run whose latest event is an unanswered approval request
    // is actually parked.
    let status = if status == RunStatus::Running
        && matches!(
            events.last().map(|e| &e.kind),
            Some(forge_core::EventKind::ApprovalRequested { .. })
        ) {
        RunStatus::WaitingForApproval
    } else {
        status
    };
    let session_id = events
        .first()
        .map(|e| e.session_id.clone())
        .unwrap_or_default();
    Ok(Json(serde_json::json!({
        "run_id": id,
        "session_id": session_id,
        "status": status,
        "events": events,
    })))
}

#[derive(Debug, Deserialize)]
pub struct RunInput {
    input: String,
}

/// Deliver user input to a run: `AgentService::send_input` feeds the
/// run's input channel (approval pauses consume it) and records an
/// `InputReceived` event. 404 for unknown runs; 409 for terminal runs or
/// closed input channels. Input for a run owned by another process is
/// recorded as an event but not consumed by that loop (the input channel
/// is in-process).
pub async fn run_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RunInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let in_flight = state.status_of(&id);
    let events = state.service.events(&id).unwrap_or_default();
    if in_flight.is_none() && events.is_empty() {
        return Err(ApiError::not_found(format!("unknown run: {id}")));
    }
    let status = in_flight.unwrap_or_else(|| status_from_events(&events));
    if status.is_terminal() {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!("run {id} is {status:?}; not accepting input"),
        });
    }
    state
        .service
        .send_input(&id, body.input)
        .map_err(|e| ApiError {
            status: StatusCode::CONFLICT,
            message: e.to_string(),
        })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "delivered": true, "run_id": id })),
    ))
}

pub async fn cancel_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Abort the in-flight task when this server started it (the runtime's
    // cancellation token also unwinds the loop; the abort is the prompt
    // fallback for a task blocked elsewhere).
    let abort = {
        let mut runs = state.runs.lock().unwrap_or_else(|e| e.into_inner());
        runs.get_mut(&id).and_then(|entry| {
            (!entry.status.is_terminal()).then(|| {
                entry.status = RunStatus::Cancelled;
                entry.abort.clone()
            })
        })
    };
    if let Some(abort) = abort {
        abort.abort();
    }
    // Record the Cancelled event (also validates the run exists).
    state
        .service
        .cancel(&id)
        .map_err(|_| ApiError::not_found(format!("unknown run: {id}")))?;
    Ok(Json(serde_json::json!({ "cancelled": id })))
}

/// SSE stream: replay stored events for the run, then stream live events
/// from the broadcast channel until a terminal event, then end. A stream
/// opened after the run finished replays and ends immediately.
pub async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    // Subscribe BEFORE reading stored events so nothing emitted in between
    // is lost (the broadcast channel buffers); replayed/live overlap is
    // deduplicated by exact event JSON below.
    let mut rx = state.service.subscribe(&id);
    let in_flight = state.status_of(&id).is_some();
    let replay = state.service.events(&id).unwrap_or_default();
    if replay.is_empty() && !in_flight {
        return Err(ApiError::not_found(format!("unknown run: {id}")));
    }

    let (tx, stream_rx) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(64);
    tokio::spawn(async move {
        let mut seen: HashSet<String> = HashSet::new();
        let mut terminal_reached = false;
        for event in &replay {
            if let Ok(line) = serde_json::to_string(event) {
                seen.insert(line.clone());
                if tx.send(Ok(SseEvent::default().data(line))).await.is_err() {
                    return; // client disconnected
                }
            }
            if event.kind.is_terminal() {
                terminal_reached = true;
            }
        }
        if terminal_reached {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let Ok(line) = serde_json::to_string(&event) else {
                        continue;
                    };
                    if !seen.insert(line.clone()) {
                        continue; // already replayed
                    }
                    if tx.send(Ok(SseEvent::default().data(line))).await.is_err() {
                        return;
                    }
                    if event.kind.is_terminal() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "SSE receiver lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    Ok(Sse::new(tokio_stream::wrappers::ReceiverStream::new(
        stream_rx,
    )))
}

pub async fn list_skills(State(state): State<AppState>) -> Json<serde_json::Value> {
    let skills: Vec<serde_json::Value> = state
        .skills
        .list()
        .iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "description": m.description,
                "path": m.path,
            })
        })
        .collect();
    Json(serde_json::json!({ "skills": skills }))
}

/// Report stored graph state and freshness; never builds.
pub async fn graph_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let Some(graph) = &state.graph else {
        return Json(serde_json::json!({ "built": false }));
    };
    if !graph.graph_file().is_file() {
        return Json(serde_json::json!({ "built": false }));
    }
    let stats = graph.stats();
    let freshness = graph.freshness();
    Json(serde_json::json!({
        "built": true,
        "stats": {
            "files": stats.files,
            "directories": stats.directories,
            "symbols": stats.symbols,
            "imports": stats.imports,
            "tests": stats.tests,
        },
        "fresh": freshness.as_ref().map(|f| f.fresh).unwrap_or(false),
        "added": freshness.as_ref().map(|f| f.added.clone()).unwrap_or_default(),
        "modified": freshness.as_ref().map(|f| f.modified.clone()).unwrap_or_default(),
        "removed": freshness.as_ref().map(|f| f.removed.clone()).unwrap_or_default(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct ContextRequest {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

pub async fn project_context(
    State(state): State<AppState>,
    Json(body): Json<ContextRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let graph = state.graph.as_ref().ok_or_else(|| ApiError {
        status: StatusCode::PRECONDITION_FAILED,
        message: "project graph not available; run `forge graph build`".to_string(),
    })?;
    if !graph.graph_file().is_file() {
        return Err(ApiError {
            status: StatusCode::PRECONDITION_FAILED,
            message: "project graph not built; run `forge graph build`".to_string(),
        });
    }
    let hits = graph.context(&body.query, body.limit.unwrap_or(10));
    let out: Vec<serde_json::Value> = hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "path": h.path,
                "score": h.score,
                "reasons": h.reasons,
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "query": body.query, "results": out }),
    ))
}
