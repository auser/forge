//! The tool registry: names, descriptions, hand-written JSON Schemas, and
//! dispatch into the shared runtime.
//!
//! Dispatch is deliberately protocol-free — [`ForgeTools::call`] takes a
//! tool name and a `serde_json::Value` of arguments and returns a
//! [`ToolOutcome`], so the whole tool surface is unit-testable without
//! spawning a process or speaking JSON-RPC. `server.rs` is the only place
//! that knows about MCP types.
//!
//! Schemas are hand-written rather than derived: clients render them to
//! the model, so their wording is part of the product.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_core::ProjectGraph;
use forge_core::{Event, EventKind, ForgeError, RunState, SessionStore};
use forge_graph::LocalGraph;
use forge_needle::EngineEmbedder;
use forge_runtime::{AgentService, RunOptions};
use forge_session::new_run_id;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::runs::{Final, RunRegistry};

/// Default synchronous budget for `forge_run` before it reports the run's
/// current status and leaves it going. Sits comfortably inside typical MCP
/// client request timeouts. A run that parks for approval returns
/// immediately rather than waiting this out.
const DEFAULT_RUN_TIMEOUT_MS: u64 = 120_000;

/// Upper bound a caller may request. Beyond this an MCP client's own
/// request timeout would fire first anyway, and a wedged tool call is
/// worse than a poll.
const MAX_RUN_TIMEOUT_MS: u64 = 600_000;

/// How many trailing events `forge_run_status` reports.
const STATUS_EVENT_WINDOW: usize = 10;

/// Session id used for adapter-side lifecycle events such as skill
/// activation (kept out of per-run sessions, mirroring the CLI's `cli`).
const MCP_SESSION: &str = "mcp";

/// Result of a tool call: a JSON value plus whether it is a tool
/// *execution* error (MCP `isError: true`) rather than a success.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub value: Value,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(value: Value) -> Self {
        Self {
            value,
            is_error: false,
        }
    }

    /// A tool execution error: actionable text the model can act on.
    /// `code` is a short machine tag (`invalid_params`, `unavailable`, …).
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self {
            value: json!({ "error": message.into(), "code": code }),
            is_error: true,
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self::error("invalid_params", message)
    }
}

/// The only *protocol*-level failure this layer can produce. Per the MCP
/// tools spec, an unknown tool name is a JSON-RPC error, not an
/// `isError` result.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolError {
    UnknownTool(String),
}

/// One entry in the registry. `input_schema` is a JSON Schema object
/// (2020-12 by default, per the MCP tools spec).
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: fn() -> Value,
}

/// Schema for a tool that takes no arguments. The spec recommends
/// `additionalProperties: false` over a bare `{"type": "object"}` so
/// clients know nothing is accepted.
fn no_args() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn graph_context_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Natural-language or keyword description of what you are looking for."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "default": 10,
                "description": "Maximum number of files to return."
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

fn graph_grep_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regular expression matched against symbol names and signatures."
            },
            "semantic": {
                "type": "boolean",
                "default": false,
                "description": "Search the local embedding index by meaning instead of by regex. Requires needle weights and a built semantic index."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

fn skill_show_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Skill name as reported by forge_skill_list." }
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

fn run_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string", "description": "The task for the forge agent." },
            "max_turns": {
                "type": "integer",
                "minimum": 1,
                "description": "Agent-loop turn budget (defaults to the project's max_turns config)."
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_RUN_TIMEOUT_MS,
                "default": DEFAULT_RUN_TIMEOUT_MS,
                "description": "How long to wait synchronously. Returns sooner if the run needs an approval decision. If the budget runs out the run keeps going — poll forge_run_status with the returned run_id."
            }
        },
        "required": ["prompt"],
        "additionalProperties": false
    })
}

fn run_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": { "type": "string", "description": "Run id returned by forge_run." }
        },
        "required": ["run_id"],
        "additionalProperties": false
    })
}

fn run_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": { "type": "string", "description": "Run id returned by forge_run." },
            "input": {
                "type": "string",
                "description": "Text delivered to the waiting run. For an approval request, \"y\" approves and anything else denies."
            }
        },
        "required": ["run_id", "input"],
        "additionalProperties": false
    })
}

/// The v1 tool surface, in a stable order (the tools spec asks servers to
/// return tools deterministically so clients can cache the list).
pub fn definitions() -> &'static [ToolDef] {
    &[
        ToolDef {
            name: "forge_graph_context",
            title: "Project context for a task",
            description: "Rank the files most relevant to a task from forge's project graph, blending lexical structure with on-device semantic search when a needle index exists. Use this first to find where to work.",
            input_schema: graph_context_schema,
        },
        ToolDef {
            name: "forge_graph_grep",
            title: "Search symbols",
            description: "Find symbols by regular expression, or by meaning with semantic=true. Returns file, line and the matching text.",
            input_schema: graph_grep_schema,
        },
        ToolDef {
            name: "forge_graph_map",
            title: "Repository structure",
            description: "Per-directory summary of the project: file counts by kind and symbol counts. A fast way to orient in an unfamiliar repository.",
            input_schema: no_args,
        },
        ToolDef {
            name: "forge_skill_list",
            title: "List skills",
            description: "List the skills available in this project: names and one-line descriptions only. Call forge_skill_show to read one.",
            input_schema: no_args,
        },
        ToolDef {
            name: "forge_skill_show",
            title: "Read a skill",
            description: "Return a skill's full instructions.",
            input_schema: skill_show_schema,
        },
        ToolDef {
            name: "forge_doctor",
            title: "Environment health check",
            description: "Report forge's configuration and environment health: config files, project graph, credentials, model provider, router and approval mode.",
            input_schema: no_args,
        },
        ToolDef {
            name: "forge_run",
            title: "Run a forge agent task",
            description: "Run a prompt through the forge agent loop (routing, tools, approvals, session recording) and return its final text. If the run outlives timeout_ms it keeps going and you poll forge_run_status.",
            input_schema: run_schema,
        },
        ToolDef {
            name: "forge_run_status",
            title: "Check a run",
            description: "Status of a run: completed, failed, cancelled, running, or waiting_for_approval — with its final text when finished and its most recent events.",
            input_schema: run_id_schema,
        },
        ToolDef {
            name: "forge_run_input",
            title: "Answer a waiting run",
            description: "Deliver input to a run that is waiting. A run parked with status waiting_for_approval is asking permission for a risky operation: send \"y\" to approve, anything else to deny.",
            input_schema: run_input_schema,
        },
        ToolDef {
            name: "forge_run_cancel",
            title: "Cancel a run",
            description: "Cancel a running forge run.",
            input_schema: run_id_schema,
        },
    ]
}

/// Async seam for `forge_doctor`.
///
/// Doctor's checks probe config files, credentials, the model endpoint,
/// needle weights and the jev tier — a combination only `forge-cli` can
/// see (it is the one crate depending on providers, graph, skills and
/// needle at once), and moving them down would drag `forge-providers` into
/// a lower layer for no other reason. So the CLI keeps ownership of the
/// checks and hands them to this adapter, which only renders them. The
/// report is the same value `forge doctor --json` prints — one definition
/// of "healthy", never a subprocess.
#[async_trait]
pub trait Diagnostics: Send + Sync {
    async fn report(&self) -> Result<Value, ForgeError>;
}

/// Tool dispatch over a constructed runtime.
pub struct ForgeTools {
    service: Arc<AgentService>,
    root: PathBuf,
    diagnostics: Option<Arc<dyn Diagnostics>>,
    runs: Arc<RunRegistry>,
}

impl ForgeTools {
    pub fn new(service: Arc<AgentService>, root: impl Into<PathBuf>) -> Self {
        Self {
            service,
            root: root.into(),
            diagnostics: None,
            runs: Arc::new(RunRegistry::default()),
        }
    }

    /// Attach the doctor checks (see [`Diagnostics`]). Without them
    /// `forge_doctor` reports itself unavailable rather than lying.
    pub fn with_diagnostics(mut self, diagnostics: Arc<dyn Diagnostics>) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    /// Dispatch one tool call. `Err` is reserved for protocol-level
    /// failures (an unknown tool name); everything a model could fix —
    /// missing arguments, an unbuilt graph, an unknown run — comes back
    /// as a [`ToolOutcome`] with `is_error` set.
    pub async fn call(&self, name: &str, args: &Value) -> Result<ToolOutcome, ToolError> {
        let outcome = match name {
            "forge_graph_context" => self.graph_context(args).await,
            "forge_graph_grep" => self.graph_grep(args).await,
            "forge_graph_map" => self.graph_map(),
            "forge_skill_list" => self.skill_list(),
            "forge_skill_show" => self.skill_show(args),
            "forge_doctor" => self.doctor().await,
            "forge_run" => self.run(args).await,
            "forge_run_status" => self.run_status(args),
            "forge_run_input" => self.run_input(args),
            "forge_run_cancel" => self.run_cancel(args),
            other => return Err(ToolError::UnknownTool(other.to_string())),
        };
        Ok(outcome)
    }

    // --- graph ----------------------------------------------------------

    /// Open the stored graph, or explain how to build it.
    fn graph(&self) -> Result<LocalGraph, ToolOutcome> {
        let graph = LocalGraph::open(&self.root)
            .map_err(|e| ToolOutcome::error("graph_unavailable", e.to_string()))?;
        if !graph.graph_file().is_file() {
            return Err(ToolOutcome::error(
                "graph_not_built",
                "project graph not built yet; run `forge graph build` (or `forge init`) in this project",
            ));
        }
        Ok(graph)
    }

    /// The on-device embedder, when one is genuinely usable. `None` is
    /// normal (no weights fetched, no `ffi` feature) and never an error on
    /// its own — `forge_graph_context` simply stays lexical.
    async fn embedder(&self) -> Option<EngineEmbedder> {
        let engine = forge_needle::engine_if_available(self.service.config()).await?;
        match EngineEmbedder::new(engine).await {
            Ok(embedder) => Some(embedder),
            Err(e) => {
                tracing::debug!(error = %e, "needle engine present but not usable as an embedder");
                None
            }
        }
    }

    async fn graph_context(&self, args: &Value) -> ToolOutcome {
        let query = match require_str(args, "query") {
            Ok(q) => q,
            Err(outcome) => return outcome,
        };
        let limit = match opt_u64(args, "limit") {
            Ok(limit) => limit.unwrap_or(10).clamp(1, 100) as usize,
            Err(outcome) => return outcome,
        };
        let graph = match self.graph() {
            Ok(graph) => graph,
            Err(outcome) => return outcome,
        };

        let embedder = self.embedder().await;
        let hits = match forge_graph::blended_context(
            &graph,
            embedder
                .as_ref()
                .map(|e| e as &dyn forge_core::embed::Embedder),
            query,
            limit,
        )
        .await
        {
            Ok(hits) => hits,
            Err(e) => return ToolOutcome::error("graph_error", e.to_string()),
        };

        ToolOutcome::ok(json!({
            "query": query,
            "semantic": embedder.is_some(),
            "hits": hits
                .iter()
                .map(|h| json!({ "path": h.path, "score": h.score, "reasons": h.reasons }))
                .collect::<Vec<_>>(),
        }))
    }

    async fn graph_grep(&self, args: &Value) -> ToolOutcome {
        let pattern = match require_str(args, "pattern") {
            Ok(p) => p,
            Err(outcome) => return outcome,
        };
        let semantic = match opt_bool(args, "semantic") {
            Ok(value) => value.unwrap_or(false),
            Err(outcome) => return outcome,
        };
        let graph = match self.graph() {
            Ok(graph) => graph,
            Err(outcome) => return outcome,
        };

        if !semantic {
            return match graph.grep(pattern) {
                Ok(matches) => ToolOutcome::ok(json!({
                    "pattern": pattern,
                    "semantic": false,
                    "matches": matches,
                })),
                Err(e) => ToolOutcome::error("graph_error", e.to_string()),
            };
        }

        // Semantic search without an engine/index is a *tool* error with
        // the fix in it, never a protocol error: the client should be able
        // to retry with semantic=false.
        let embedder = self.embedder().await;
        match forge_graph::semantic_grep(
            &graph,
            embedder
                .as_ref()
                .map(|e| e as &dyn forge_core::embed::Embedder),
            pattern,
            20,
        )
        .await
        {
            Ok(matches) => ToolOutcome::ok(json!({
                "pattern": pattern,
                "semantic": true,
                "matches": matches
                    .iter()
                    .map(|(key, score)| json!({ "key": key, "score": score }))
                    .collect::<Vec<_>>(),
            })),
            Err(e) => ToolOutcome::error("semantic_unavailable", e.to_string()),
        }
    }

    fn graph_map(&self) -> ToolOutcome {
        let graph = match self.graph() {
            Ok(graph) => graph,
            Err(outcome) => return outcome,
        };
        let dirs = graph.map();
        ToolOutcome::ok(json!({
            "directories": dirs
                .iter()
                .map(|d| json!({ "dir": d.dir, "files": d.files, "symbols": d.symbols }))
                .collect::<Vec<_>>(),
        }))
    }

    // --- skills ---------------------------------------------------------

    fn skill_list(&self) -> ToolOutcome {
        // Metadata only: progressive disclosure is the point of skills,
        // so instructions arrive via forge_skill_show.
        let skills = self.service.skills().list();
        ToolOutcome::ok(json!({
            "skills": skills
                .iter()
                .map(|m| json!({
                    "name": m.name,
                    "description": m.description,
                    "path": m.path,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    fn skill_show(&self, args: &Value) -> ToolOutcome {
        let name = match require_str(args, "name") {
            Ok(name) => name,
            Err(outcome) => return outcome,
        };
        let skill = match self.service.skills().activate(name) {
            Ok(skill) => skill,
            Err(e) => return ToolOutcome::error("unknown_skill", e.to_string()),
        };
        // Activation is a session event everywhere else in forge; a
        // failure to log it must not fail the tool call.
        if let Err(e) = self.service.sessions().append(Event::new(
            new_run_id(),
            MCP_SESSION,
            EventKind::SkillActivated {
                name: skill.meta.name.clone(),
                path: skill.meta.path.clone(),
            },
        )) {
            tracing::warn!(error = %e, "could not record skill activation");
        }
        ToolOutcome::ok(json!({
            "name": skill.meta.name,
            "description": skill.meta.description,
            "path": skill.meta.path,
            "instructions": skill.instructions,
        }))
    }

    // --- doctor ---------------------------------------------------------

    async fn doctor(&self) -> ToolOutcome {
        let Some(diagnostics) = &self.diagnostics else {
            return ToolOutcome::error(
                "unavailable",
                "doctor checks are not wired into this server build",
            );
        };
        match diagnostics.report().await {
            Ok(report) => ToolOutcome::ok(report),
            Err(e) => ToolOutcome::error("doctor_failed", e.to_string()),
        }
    }

    // --- runs -----------------------------------------------------------

    async fn run(&self, args: &Value) -> ToolOutcome {
        let prompt = match require_str(args, "prompt") {
            Ok(prompt) => prompt,
            Err(outcome) => return outcome,
        };
        if prompt.trim().is_empty() {
            return ToolOutcome::invalid_params("prompt must not be empty");
        }
        let max_turns = match opt_u64(args, "max_turns") {
            Ok(value) => value.map(|v| v.clamp(1, u32::MAX as u64) as u32),
            Err(outcome) => return outcome,
        };
        let timeout_ms = match opt_u64(args, "timeout_ms") {
            Ok(value) => value
                .unwrap_or(DEFAULT_RUN_TIMEOUT_MS)
                .clamp(1, MAX_RUN_TIMEOUT_MS),
            Err(outcome) => return outcome,
        };

        // Generate the run id here so we can subscribe to its event stream
        // *before* the run task exists. `subscribe` creates the broadcast
        // channel on demand, so there is no window in which an early event
        // — an approval request from a fast first tool call — could be
        // emitted before anyone is listening.
        let run_id = new_run_id();
        let mut events = self.service.subscribe(&run_id);

        let (run_id, session_id, handle) = self.service.start_run_with_options(
            prompt,
            RunOptions {
                run_id: Some(run_id),
                max_turns,
                ..RunOptions::default()
            },
        );
        self.runs.start(&run_id);

        // The monitor owns the join handle, so a timed-out call still
        // records the full outcome for forge_run_status to return later.
        // It outlives this tool call on purpose: the run continues.
        let registry = Arc::clone(&self.runs);
        let monitored = run_id.clone();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let state = Final::from_join(handle.await);
            registry.settle(&monitored, state);
            let _ = finished_tx.send(());
        });

        // Three ways this call can end. The approval arm is the load-bearing
        // one: a run parked on an approval request is *blocked* inside the
        // loop, so it will never finish on its own — waiting out the full
        // timeout before saying so would strand the client for two minutes
        // on a run that needs one word from it.
        tokio::select! {
            biased;
            _ = finished_rx => self.settled_or_running(&run_id, &session_id),
            () = wait_for_approval_request(&mut events) => {
                self.paused_or_current(&run_id, &session_id)
            }
            () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                // Never hardcode "running": by now the run may have parked
                // for approval (with the event missed), failed, or finished
                // between the timer firing and this line. The event log is
                // the authority.
                let mut outcome = self.status_value(&run_id);
                if let Some(obj) = outcome.value.as_object_mut() {
                    obj.insert("session_id".to_string(), Value::String(session_id.clone()));
                    obj.insert(
                        "note".to_string(),
                        Value::String(format!(
                            "still going after {timeout_ms} ms; the run continues — \
                             poll forge_run_status with this run_id"
                        )),
                    );
                }
                outcome
            }
        }
    }

    /// Report a run that just emitted an approval request. Reads the event
    /// log rather than asserting the status, so a run that raced past the
    /// pause (approval auto-granted, or answered by another client between
    /// the event and this call) is still described accurately.
    fn paused_or_current(&self, run_id: &str, session_id: &str) -> ToolOutcome {
        let mut outcome = self.status_value(run_id);
        if let Some(obj) = outcome.value.as_object_mut() {
            obj.insert(
                "session_id".to_string(),
                Value::String(session_id.to_string()),
            );
            if obj.get("status").and_then(Value::as_str) == Some("waiting_for_approval") {
                obj.insert(
                    "note".to_string(),
                    Value::String(
                        "the run is waiting for permission to perform a risky operation — \
                         answer it with forge_run_input (\"y\" approves, anything else denies)"
                            .to_string(),
                    ),
                );
            }
        }
        outcome
    }

    /// Render a run that the monitor has (or should have) settled.
    fn settled_or_running(&self, run_id: &str, session_id: &str) -> ToolOutcome {
        match self.runs.state(run_id) {
            Some(Some(Final::Completed(outcome))) => ToolOutcome::ok(json!({
                "run_id": run_id,
                "session_id": session_id,
                "status": wire_status(RunState::Completed),
                "text": outcome.text,
                "turns": outcome.turns,
                "tool_calls": outcome.tool_calls,
                "router": router_of(&outcome.events),
            })),
            Some(Some(Final::Failed(message))) => ToolOutcome {
                value: json!({
                    "run_id": run_id,
                    "session_id": session_id,
                    "status": wire_status(RunState::Failed),
                    "error": message,
                }),
                is_error: true,
            },
            Some(Some(Final::Cancelled)) => ToolOutcome::ok(json!({
                "run_id": run_id,
                "session_id": session_id,
                "status": wire_status(RunState::Cancelled),
            })),
            // The oneshot fired, so the monitor settled the run; anything
            // else means it was evicted under load. Events still answer.
            _ => self.status_value(run_id),
        }
    }

    fn run_status(&self, args: &Value) -> ToolOutcome {
        let run_id = match require_str(args, "run_id") {
            Ok(id) => id,
            Err(outcome) => return outcome,
        };
        self.status_value(run_id)
    }

    /// Status of any run — ours or one started by `forge run` in another
    /// process. Mirrors the REST adapter's semantics: a run whose latest
    /// event is an unanswered approval request is parked, not running.
    fn status_value(&self, run_id: &str) -> ToolOutcome {
        let events = self.service.events(run_id).unwrap_or_default();
        let tracked = self.runs.state(run_id);
        if events.is_empty() && tracked.is_none() {
            return ToolOutcome::error("unknown_run", format!("unknown run: {run_id}"));
        }

        let (state, text, error) = match tracked {
            Some(Some(Final::Completed(outcome))) => {
                (RunState::Completed, Some(outcome.text.clone()), None)
            }
            Some(Some(Final::Failed(message))) => (RunState::Failed, None, Some(message)),
            Some(Some(Final::Cancelled)) => (RunState::Cancelled, None, None),
            // In flight here, or not ours: the event log decides. A
            // completed run's summary is its text, as before.
            _ => match events.last().map(|e| &e.kind) {
                Some(EventKind::Completed { summary }) => {
                    (RunState::Completed, Some(summary.clone()), None)
                }
                Some(EventKind::Error { message }) => {
                    (RunState::Failed, None, Some(message.clone()))
                }
                _ => (RunState::of_events(&events), None, None),
            },
        };
        let status = wire_status(state);

        let last_events: Vec<&Event> = events
            .iter()
            .rev()
            .take(STATUS_EVENT_WINDOW)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        let mut value = json!({
            "run_id": run_id,
            "status": status,
            "last_events": last_events,
        });
        if let (Some(text), Some(obj)) = (text, value.as_object_mut()) {
            obj.insert("text".to_string(), Value::String(text));
        }
        if let (Some(error), Some(obj)) = (error, value.as_object_mut()) {
            obj.insert("error".to_string(), Value::String(error));
        }
        ToolOutcome::ok(value)
    }

    fn run_input(&self, args: &Value) -> ToolOutcome {
        let run_id = match require_str(args, "run_id") {
            Ok(id) => id,
            Err(outcome) => return outcome,
        };
        let input = match require_str(args, "input") {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };

        let events = self.service.events(run_id).unwrap_or_default();
        let tracked = self.runs.state(run_id);
        if events.is_empty() && tracked.is_none() {
            return ToolOutcome::error("unknown_run", format!("unknown run: {run_id}"));
        }
        // Terminal runs do not take input — mirroring the REST adapter's
        // 409. Ours settle in the registry; a run from another process
        // (`forge run`, `forge serve`) is judged by its event log.
        let finished = match &tracked {
            Some(Some(settled)) => Some(settled.state()),
            Some(None) => None,
            None => {
                let state = RunState::of_events(&events);
                state.is_terminal().then_some(state)
            }
        };
        if let Some(state) = finished {
            return ToolOutcome::error(
                "run_finished",
                format!(
                    "run {run_id} is {}; not accepting input",
                    wire_status(state)
                ),
            );
        }

        match self.service.send_input(run_id, input) {
            Ok(()) => ToolOutcome::ok(json!({ "delivered": true, "run_id": run_id })),
            Err(e) => ToolOutcome::error("input_rejected", e.to_string()),
        }
    }

    fn run_cancel(&self, args: &Value) -> ToolOutcome {
        let run_id = match require_str(args, "run_id") {
            Ok(id) => id,
            Err(outcome) => return outcome,
        };
        match self.service.cancel(run_id) {
            Ok(()) => ToolOutcome::ok(json!({ "cancelled": run_id })),
            Err(e) => ToolOutcome::error("unknown_run", e.to_string()),
        }
    }
}

/// Wire spelling of a run state for this adapter.
///
/// Identical to [`RunState::as_str`] except for `AwaitingApproval`: the
/// `forge_run*` tool schemas document five statuses, and an unanswered
/// approval escaping the loop has always been reported here as `failed`
/// (with the approval error in `error`). Naming a sixth status would change
/// the tool contract, so it is folded in explicitly rather than by accident.
fn wire_status(state: RunState) -> &'static str {
    match state {
        RunState::AwaitingApproval => RunState::Failed.as_str(),
        other => other.as_str(),
    }
}

/// Resolve as soon as the run emits an approval request.
///
/// A `Lagged` receiver has missed events but is still live, so it keeps
/// watching; `Closed` means no further events can arrive, which the caller
/// handles by reading the event log. Every arm therefore ends in a state
/// the caller can describe truthfully.
async fn wait_for_approval_request(events: &mut broadcast::Receiver<Event>) {
    loop {
        match events.recv().await {
            Ok(event) if matches!(event.kind, EventKind::ApprovalRequested { .. }) => return,
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                tracing::debug!(missed, "event stream lagged while watching for approvals");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The router that decided this run, when it recorded a decision — the
/// fast path reports `needle-dispatch`.
fn router_of(events: &[Event]) -> Option<String> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::RoutingDecisionMade { router, .. } => Some(router.clone()),
        _ => None,
    })
}

// --- argument helpers ---------------------------------------------------

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(ToolOutcome::invalid_params(format!(
            "argument {key:?} must be a string (got {})",
            type_name(other)
        ))),
        None => Err(ToolOutcome::invalid_params(format!(
            "missing required argument {key:?} (expected a string)"
        ))),
    }
}

fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
            ToolOutcome::invalid_params(format!("argument {key:?} must be a positive integer"))
        }),
        Some(other) => Err(ToolOutcome::invalid_params(format!(
            "argument {key:?} must be an integer (got {})",
            type_name(other)
        ))),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(ToolOutcome::invalid_params(format!(
            "argument {key:?} must be a boolean (got {})",
            type_name(other)
        ))),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests;
