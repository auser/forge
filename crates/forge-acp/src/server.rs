//! The stdio server loop and the prompt-turn driver: the only module here
//! that does I/O.
//!
//! Three long-lived pieces cooperate:
//!
//! * **the reader** ([`serve_stdio`]) owns stdin. It parses one message per
//!   line and never blocks on work: `session/prompt` is spawned so that
//!   `session/cancel` and permission answers can arrive *during* a turn,
//!   which is the whole point of the protocol.
//! * **the writer** owns stdout. Every outgoing message goes through one
//!   channel, so nothing can interleave mid-line and the ordering a client
//!   relies on (the final `agent_message_chunk` before the `session/prompt`
//!   response) is guaranteed by construction.
//! * **the turn driver** ([`ForgeAcpServer::run_turn`]) subscribes to a
//!   run's events, translates them through [`dispatch`], and answers the
//!   request when the run settles.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_core::{Event, EventKind, ForgeError};
use forge_runtime::{AgentService, RunOptions};
use forge_session::{new_run_id, new_session_id};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::dispatch::{
    self, ApprovalDecision, TurnAction, TurnEnd, initialize, parse_line, permission_options,
    prompt_text, session_root, turn_end,
};
use crate::protocol::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, NewSessionResponse,
    Outgoing, PromptRequest, PromptResponse, RequestPermissionRequest, RequestPermissionResponse,
    RpcError, SessionNotification, SessionUpdate, ToolCallUpdate, client_method, method,
};

/// Builds an [`AgentService`] rooted at an ACP session's `cwd`.
///
/// The client picks the project directory per session, and forge's
/// configuration, project graph, session store and risk classification are
/// all rooted there — so the service is built per session rather than once
/// per process. `forge-cli` implements this over the same
/// `service::build_run_service` path every other subcommand uses, which is
/// how this crate stays free of any dependency on the CLI.
#[async_trait]
pub trait ServiceFactory: Send + Sync {
    async fn build(&self, root: &Path) -> Result<Arc<AgentService>, ForgeError>;
}

/// One ACP session: a project root, the runtime rooted there, and whatever
/// turn is currently in flight.
struct Session {
    service: Arc<AgentService>,
    root: PathBuf,
    turn: Mutex<TurnSlot>,
}

/// The mutable per-session bits. `cancel_seen` is sticky for the duration
/// of a turn: a `session/cancel` that lands while the loop is unwinding
/// must still turn into the `cancelled` stop reason.
#[derive(Default)]
struct TurnSlot {
    active_run: Option<String>,
    cancel_seen: bool,
}

/// ACP over stdio, on top of the shared agent runtime.
pub struct ForgeAcpServer {
    factory: Arc<dyn ServiceFactory>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    outgoing: mpsc::Sender<Outgoing>,
    /// Ids for requests *we* make of the client. Separate counter from the
    /// client's ids: the two id spaces are independent in JSON-RPC.
    next_request_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
}

impl ForgeAcpServer {
    fn new(factory: Arc<dyn ServiceFactory>, outgoing: mpsc::Sender<Outgoing>) -> Self {
        Self {
            factory,
            sessions: Mutex::new(HashMap::new()),
            outgoing,
            next_request_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// A poisoned lock here carries no invariant worth aborting the
    /// protocol loop for — these maps are bookkeeping — so recover the
    /// data instead of panicking mid-turn.
    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn send(&self, message: Outgoing) {
        if self.outgoing.send(message).await.is_err() {
            // The writer is gone, which means stdout closed: there is no
            // way to talk to the client any more and nothing to recover.
            tracing::debug!("stdout writer has shut down; dropping outgoing message");
        }
    }

    async fn notify_update(&self, session_id: &str, update: SessionUpdate) {
        let params = match serde_json::to_value(SessionNotification {
            session_id: session_id.to_string(),
            update,
        }) {
            Ok(params) => params,
            Err(e) => {
                tracing::error!(error = %e, "could not serialize a session update");
                return;
            }
        };
        self.send(Outgoing::notification(
            client_method::SESSION_UPDATE,
            params,
        ))
        .await;
    }

    // --- request handling ------------------------------------------------

    async fn handle_request(self: &Arc<Self>, id: Value, method_name: &str, params: Value) {
        match method_name {
            method::INITIALIZE => {
                let response = match serde_json::from_value::<InitializeRequest>(params) {
                    Ok(request) => request,
                    Err(e) => {
                        self.send(Outgoing::error(
                            id,
                            RpcError::invalid_params(format!("invalid initialize params: {e}")),
                        ))
                        .await;
                        return;
                    }
                };
                if let Some(info) = response.client_info.as_ref() {
                    tracing::info!(
                        client = %info.name,
                        version = %info.version,
                        protocol = response.protocol_version,
                        "ACP client connected"
                    );
                }
                match serde_json::to_value(initialize(&response)) {
                    Ok(result) => self.send(Outgoing::response(id, result)).await,
                    Err(e) => {
                        self.send(Outgoing::error(id, RpcError::internal(e.to_string())))
                            .await;
                    }
                }
            }

            method::SESSION_NEW => {
                let result = self.new_session(params).await;
                self.respond(id, result).await;
            }

            method::SESSION_PROMPT => {
                let request = match serde_json::from_value::<PromptRequest>(params) {
                    Ok(request) => request,
                    Err(e) => {
                        self.send(Outgoing::error(
                            id,
                            RpcError::invalid_params(format!("invalid session/prompt params: {e}")),
                        ))
                        .await;
                        return;
                    }
                };
                // Spawned: a turn takes as long as the agent loop takes,
                // and the reader must stay free to deliver session/cancel
                // and permission answers to it.
                let server = Arc::clone(self);
                tokio::spawn(async move {
                    let result = server.prompt(request).await;
                    server.respond(id, result).await;
                });
            }

            other => {
                self.send(Outgoing::error(id, RpcError::method_not_found(other)))
                    .await;
            }
        }
    }

    async fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        match result {
            Ok(value) => self.send(Outgoing::response(id, value)).await,
            Err(error) => {
                tracing::debug!(code = error.code, message = %error.message, "request failed");
                self.send(Outgoing::error(id, error)).await;
            }
        }
    }

    async fn handle_notification(&self, method_name: &str, params: Value) {
        match method_name {
            method::SESSION_CANCEL => {
                let request: CancelNotification = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(e) => {
                        tracing::warn!(error = %e, "invalid session/cancel params");
                        return;
                    }
                };
                let Some(session_id) = request.session_id else {
                    tracing::warn!("session/cancel without a sessionId");
                    return;
                };
                self.cancel(&session_id);
            }
            // Notifications get no response, so an unknown one is logged
            // and ignored rather than answered with method_not_found.
            other => tracing::debug!(method = other, "ignoring unknown notification"),
        }
    }

    /// `session/new`: validate the root, build a runtime for it, and mint
    /// the session id.
    async fn new_session(&self, params: Value) -> Result<Value, RpcError> {
        let request: NewSessionRequest = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("invalid session/new params: {e}")))?;
        if !request.mcp_servers.is_empty() {
            // We did not advertise any mcpCapabilities, so a client should
            // not be sending these; say so rather than silently ignoring
            // servers the user expected to be connected.
            tracing::warn!(
                count = request.mcp_servers.len(),
                "ignoring mcpServers: this agent does not connect to client-supplied MCP servers"
            );
        }
        let root = session_root(&request)?;

        let service = self.factory.build(&root).await.map_err(|e| {
            RpcError::internal(format!(
                "could not start a forge session in {}: {e}",
                root.display()
            ))
        })?;

        // The ACP session id *is* the forge session id, so everything the
        // turn records is inspectable afterwards with `forge session show
        // <id>` and resumable with `forge resume <id>`.
        let session_id = new_session_id();
        self.sessions().insert(
            session_id.clone(),
            Arc::new(Session {
                service,
                root: root.clone(),
                turn: Mutex::new(TurnSlot::default()),
            }),
        );
        tracing::info!(session = %session_id, root = %root.display(), "ACP session created");

        serde_json::to_value(NewSessionResponse {
            session_id: session_id.clone(),
        })
        .map_err(|e| RpcError::internal(e.to_string()))
    }

    fn session(&self, session_id: &str) -> Result<Arc<Session>, RpcError> {
        self.sessions()
            .get(session_id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params(format!("unknown session: {session_id}")))
    }

    /// `session/cancel`: stop the session's in-flight run. The turn's own
    /// driver reports the `cancelled` stop reason once the loop unwinds, so
    /// there is nothing to answer here (notifications get no response).
    fn cancel(&self, session_id: &str) {
        let Ok(session) = self.session(session_id) else {
            tracing::debug!(session = session_id, "cancel for an unknown session");
            return;
        };
        let run_id = {
            let mut turn = session.turn.lock().unwrap_or_else(|e| e.into_inner());
            // Set the flag even with no run in flight: a cancel that races
            // ahead of the run still has to make the turn report
            // `cancelled`.
            turn.cancel_seen = true;
            turn.active_run.clone()
        };
        let Some(run_id) = run_id else {
            tracing::debug!(session = session_id, "cancel with no run in flight");
            return;
        };
        match session.service.cancel(&run_id) {
            Ok(()) => tracing::info!(session = session_id, run = %run_id, "cancelled"),
            Err(e) => {
                tracing::warn!(session = session_id, run = %run_id, error = %e, "cancel failed")
            }
        }
    }

    // --- the prompt turn -------------------------------------------------

    async fn prompt(self: &Arc<Self>, request: PromptRequest) -> Result<Value, RpcError> {
        let session_id = request
            .session_id
            .ok_or_else(|| RpcError::invalid_params("session/prompt requires a `sessionId`"))?;
        let session = self.session(&session_id)?;
        let prompt = prompt_text(&request.prompt)?;

        // One turn per session at a time. The agent loop is a conversation,
        // and two concurrent runs writing into one session would interleave
        // their events and their history.
        //
        // The id is generated here so that claiming the slot and naming its
        // occupant happen in the *same* critical section. Checking here and
        // filling `active_run` inside `run_turn` would leave a window for a
        // second spawned `session/prompt` task to pass the check, and for a
        // `session/cancel` to find no run to cancel while a turn is in fact
        // starting.
        let run_id = new_run_id();
        {
            let mut turn = session.turn.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = turn.active_run.as_ref() {
                return Err(RpcError::invalid_request(format!(
                    "session {session_id} is already running a turn (run {active}); \
                     cancel it or wait for it to finish"
                )));
            }
            turn.active_run = Some(run_id.clone());
            turn.cancel_seen = false;
        }

        let result = self.run_turn(&session_id, &session, &run_id, prompt).await;

        {
            let mut turn = session.turn.lock().unwrap_or_else(|e| e.into_inner());
            turn.active_run = None;
        }
        result
    }

    async fn run_turn(
        self: &Arc<Self>,
        session_id: &str,
        session: &Arc<Session>,
        run_id: &str,
        prompt: String,
    ) -> Result<Value, RpcError> {
        // Subscribe *before* the run task exists. `subscribe` creates the
        // broadcast channel on demand, so there is no window in which an
        // early event — an approval request from a fast first tool call —
        // could be emitted with nobody listening.
        let mut events = session.service.subscribe(run_id);

        let (_, _, mut handle) = session.service.start_run_with_options(
            prompt,
            RunOptions {
                run_id: Some(run_id.to_string()),
                // The ACP session id is the forge session id, so every turn
                // in this session continues the same conversation.
                session_id: Some(session_id.to_string()),
                ..RunOptions::default()
            },
        );
        tracing::info!(session = session_id, run = %run_id, root = %session.root.display(), "turn started");

        let mut state = dispatch::TurnState::default();
        let run_result: Result<String, String> = loop {
            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) => {
                        self.apply(session_id, session, run_id, &mut state, &event).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        // Still live, just behind: keep going. Some updates
                        // are lost, which is why the turn's final text comes
                        // from the run outcome and not from the event log.
                        tracing::warn!(missed, "event stream lagged during an ACP turn");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // No more events can arrive; the run result is the
                        // only thing left to wait for.
                        break join(&mut handle).await;
                    }
                },
                joined = &mut handle => break settle(joined),
            }
        };

        // Flush whatever was already queued when the run settled, so the
        // last tool call's `completed` update reaches the client before the
        // final message.
        while let Ok(event) = events.try_recv() {
            self.apply(session_id, session, run_id, &mut state, &event)
                .await;
        }

        let cancel_seen = {
            let turn = session.turn.lock().unwrap_or_else(|e| e.into_inner());
            turn.cancel_seen
        };

        // The model's answer, as one chunk. forge's loop produces final
        // text rather than a token stream, so streaming it token by token
        // would be theatre; one honest chunk is what the protocol gets.
        if let Ok(text) = run_result.as_ref()
            && !text.trim().is_empty()
        {
            self.notify_update(
                session_id,
                SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::text(text.clone()),
                },
            )
            .await;
        }

        match turn_end(run_result, cancel_seen) {
            TurnEnd::Stop(stop_reason) => {
                tracing::info!(session = session_id, run = %run_id, ?stop_reason, "turn finished");
                serde_json::to_value(PromptResponse { stop_reason })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }
            TurnEnd::Failed(error) => Err(error),
        }
    }

    /// Translate one run event into protocol traffic.
    async fn apply(
        self: &Arc<Self>,
        session_id: &str,
        session: &Arc<Session>,
        run_id: &str,
        state: &mut dispatch::TurnState,
        event: &Event,
    ) {
        if matches!(event.kind, EventKind::Cancelled { .. }) {
            let mut turn = session.turn.lock().unwrap_or_else(|e| e.into_inner());
            turn.cancel_seen = true;
        }
        for action in state.on_event(event) {
            match action {
                TurnAction::Notify(update) => self.notify_update(session_id, update).await,
                TurnAction::AskPermission { tool_call, title } => {
                    // Spawned on purpose: the run is parked inside the loop
                    // waiting for input, and this driver has to keep
                    // draining events (and stay ready to be cancelled)
                    // while the user decides.
                    let server = Arc::clone(self);
                    let service = Arc::clone(&session.service);
                    let session_id = session_id.to_string();
                    let run_id = run_id.to_string();
                    tokio::spawn(async move {
                        server
                            .resolve_permission(&session_id, &service, &run_id, tool_call, title)
                            .await;
                    });
                }
            }
        }
    }

    /// Ask the client for permission and unblock the parked run with the
    /// answer.
    async fn resolve_permission(
        &self,
        session_id: &str,
        service: &Arc<AgentService>,
        run_id: &str,
        tool_call: ToolCallUpdate,
        title: String,
    ) {
        let decision = self.ask_permission(session_id, tool_call, title).await;
        tracing::info!(session = session_id, run = %run_id, ?decision, "permission decided");
        // The run is blocked on this input. Failing to deliver it would
        // hang the turn, so a send error is worth a loud log: the loop's
        // own closed-channel handling is the only remaining safety net.
        if let Err(e) = service.send_input(run_id, decision.input()) {
            tracing::error!(run = %run_id, error = %e, "could not deliver the permission decision");
        }
    }

    async fn ask_permission(
        &self,
        session_id: &str,
        tool_call: ToolCallUpdate,
        title: String,
    ) -> ApprovalDecision {
        let request = RequestPermissionRequest {
            session_id: session_id.to_string(),
            tool_call,
            options: permission_options(),
        };
        let params = match serde_json::to_value(&request) {
            Ok(params) => params,
            Err(e) => {
                tracing::error!(error = %e, "could not serialize a permission request");
                return ApprovalDecision::Deny;
            }
        };
        tracing::info!(session = session_id, %title, "asking the client for permission");

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id, tx);
        self.send(Outgoing::request(
            id,
            client_method::SESSION_REQUEST_PERMISSION,
            params,
        ))
        .await;

        match rx.await {
            Ok(Ok(result)) => match serde_json::from_value::<RequestPermissionResponse>(result) {
                Ok(response) => dispatch::approval_decision(&response.outcome),
                Err(e) => {
                    tracing::warn!(error = %e, "unreadable permission response; denying");
                    ApprovalDecision::Deny
                }
            },
            Ok(Err(error)) => {
                tracing::warn!(%error, "the client refused the permission request; denying");
                ApprovalDecision::Deny
            }
            // The reader shut down (stdin EOF) with the request outstanding.
            // Denying is the only safe answer, and the run must not be left
            // parked forever.
            Err(_) => {
                tracing::warn!("no answer to the permission request; denying");
                ApprovalDecision::Deny
            }
        }
    }

    /// The client is gone (stdin EOF). Wind everything down so this process
    /// can actually exit.
    ///
    /// Both halves are load-bearing. The writer task only ends once every
    /// clone of the outgoing sender is dropped, and those clones live inside
    /// the `Arc<Self>` held by spawned turn and permission tasks — so any
    /// task still waiting here would keep the process alive indefinitely:
    ///
    /// * **Outstanding client requests are failed.** A permission task
    ///   waiting on an answer that can no longer arrive would wait forever.
    ///   Dropping the responder makes its `rx.await` return `Err`, which the
    ///   caller already treats as a denial.
    /// * **In-flight turns are cancelled.** An abandoned turn that kept
    ///   running would go on editing files for an editor that is no longer
    ///   watching, and would hold the process open until it finished.
    fn shutdown(&self) {
        let outstanding: Vec<u64> = self.pending().keys().copied().collect();
        for id in outstanding {
            // Dropping the sender is the signal; there is no answer to send.
            if self.pending().remove(&id).is_some() {
                tracing::debug!(id, "failing an outstanding client request at shutdown");
            }
        }
        // Collect first: `cancel` takes the sessions lock itself.
        let session_ids: Vec<String> = self.sessions().keys().cloned().collect();
        for session_id in session_ids {
            let has_run = self
                .sessions()
                .get(&session_id)
                .map(|session| {
                    session
                        .turn
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .active_run
                        .is_some()
                })
                .unwrap_or(false);
            if has_run {
                tracing::info!(session = %session_id, "cancelling an in-flight turn at shutdown");
                self.cancel(&session_id);
            }
        }
    }

    /// Route a response to whichever request is waiting for it.
    fn resolve_response(&self, id: &Value, result: Option<Value>, error: Option<Value>) {
        let Some(key) = id.as_u64() else {
            tracing::warn!(%id, "response with an id we never issued");
            return;
        };
        let Some(waiting) = self.pending().remove(&key) else {
            tracing::warn!(
                id = key,
                "response to an unknown or already-answered request"
            );
            return;
        };
        let payload = match (result, error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(serde_json::from_value::<RpcError>(error.clone())
                .unwrap_or_else(|_| RpcError::internal(error.to_string()))),
            (None, None) => Err(RpcError::internal(
                "response carried neither result nor error",
            )),
        };
        let _ = waiting.send(payload);
    }
}

/// Await a run's task handle.
async fn join(
    handle: &mut tokio::task::JoinHandle<Result<forge_runtime::RunOutcome, ForgeError>>,
) -> Result<String, String> {
    settle(handle.await)
}

/// Reduce a joined run to "final text" or "why it failed".
fn settle(
    joined: Result<Result<forge_runtime::RunOutcome, ForgeError>, tokio::task::JoinError>,
) -> Result<String, String> {
    match joined {
        Ok(Ok(outcome)) => Ok(outcome.text),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) if e.is_cancelled() => Err("run cancelled".to_string()),
        Err(e) => Err(format!("run task did not finish: {e}")),
    }
}

/// Serve ACP on stdin/stdout until the client closes stdin (EOF).
///
/// Nothing in this process may write to stdout: stdout is the protocol
/// channel, and a single stray `println!` anywhere in forge would corrupt
/// the stream. Diagnostics go to stderr.
pub async fn serve_stdio(factory: Arc<dyn ServiceFactory>) -> Result<(), ForgeError> {
    // A bounded queue: if a client stops reading, we apply backpressure to
    // the turn rather than buffering an unbounded transcript.
    let (tx, mut rx) = mpsc::channel::<Outgoing>(256);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = rx.recv().await {
            let line = match serde_json::to_string(&message) {
                Ok(line) => line,
                Err(e) => {
                    tracing::error!(error = %e, "could not serialize an outgoing message");
                    continue;
                }
            };
            if let Err(e) = stdout.write_all(line.as_bytes()).await {
                tracing::error!(error = %e, "stdout write failed");
                break;
            }
            if let Err(e) = stdout.write_all(b"\n").await {
                tracing::error!(error = %e, "stdout write failed");
                break;
            }
            if let Err(e) = stdout.flush().await {
                tracing::error!(error = %e, "stdout flush failed");
                break;
            }
        }
    });

    let server = Arc::new(ForgeAcpServer::new(factory, tx));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // EOF: the client closed stdin, which is the graceful-shutdown
            // signal for a stdio agent.
            Ok(None) => break,
            Err(e) => {
                tracing::error!(error = %e, "stdin read failed");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        match parse_line(&line) {
            Ok(incoming) => {
                if incoming.is_response() {
                    if let Some(id) = incoming.id.as_ref() {
                        server.resolve_response(id, incoming.result, incoming.error);
                    }
                    continue;
                }
                let params = incoming.params.unwrap_or(Value::Null);
                let Some(method_name) = incoming.method else {
                    continue; // parse_line already rejected this shape
                };
                match incoming.id {
                    Some(id) => server.handle_request(id, &method_name, params).await,
                    None => server.handle_notification(&method_name, params).await,
                }
            }
            // A malformed line must not take the server down. Answer it
            // when it carried an id, log it when it did not, and keep
            // reading either way.
            Err(error) => {
                tracing::warn!(%error, line = %truncate(&line), "malformed message");
                if let Some(id) = salvage_id(&line) {
                    server.send(Outgoing::error(id, error)).await;
                }
            }
        }
    }

    tracing::info!("ACP client closed stdin; shutting down");
    // Unblock anything still holding an `Arc<ForgeAcpServer>` (see
    // `shutdown`), then drop our own reference: the writer task ends when
    // the last outgoing sender goes with it, having flushed what was queued.
    server.shutdown();
    drop(server);
    // A bounded wait, because failing to exit is worse than losing a few
    // trailing notifications: `shutdown` releases every task we know how to
    // release, and this covers anything it cannot reach.
    if tokio::time::timeout(SHUTDOWN_FLUSH, writer).await.is_err() {
        tracing::warn!("stdout writer did not finish within the shutdown window");
    }
    Ok(())
}

/// How long to let the writer drain after the client disconnects.
const SHUTDOWN_FLUSH: std::time::Duration = std::time::Duration::from_secs(5);

/// Best-effort id recovery from a line we could not dispatch, so a client
/// that sent a syntactically-broken *request* still gets an error response
/// instead of waiting forever.
fn salvage_id(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    let id = value.get("id")?;
    (id.is_number() || id.is_string()).then(|| id.clone())
}

/// Keep a malformed line out of the logs at full length.
fn truncate(line: &str) -> String {
    line.chars().take(200).collect()
}

/// A convenience wrapper for building a session's service from a plain
/// async closure, used by the tests in this crate.
#[cfg(test)]
pub(crate) struct FnFactory<F>(pub F);

#[cfg(test)]
#[async_trait]
impl<F> ServiceFactory for FnFactory<F>
where
    F: Fn(&Path) -> Result<Arc<AgentService>, ForgeError> + Send + Sync,
{
    async fn build(&self, root: &Path) -> Result<Arc<AgentService>, ForgeError> {
        (self.0)(root)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn service(root: &Path) -> Arc<AgentService> {
        Arc::new(AgentService::new(
            Arc::new(forge_providers::MockModel::new()),
            Arc::new(forge_providers::MockRouter::selecting("mock-local")),
            Arc::new(forge_execution::MockExecution::new(root)),
            Arc::new(forge_skills::FsSkillRegistry::with_roots(vec![], None)),
            Arc::new(forge_session::JsonlSessionStore::new(
                root.join(".forge").join("sessions"),
            )),
            forge_config::Config::default(),
        ))
    }

    fn server() -> (Arc<ForgeAcpServer>, mpsc::Receiver<Outgoing>) {
        let (tx, rx) = mpsc::channel(64);
        let factory = Arc::new(FnFactory(|root: &Path| Ok(service(root))));
        (Arc::new(ForgeAcpServer::new(factory, tx)), rx)
    }

    /// The `params` of the next outgoing message, as JSON.
    fn next(rx: &mut mpsc::Receiver<Outgoing>) -> Value {
        let message = rx.try_recv().expect("an outgoing message");
        serde_json::to_value(&message).expect("serialize")
    }

    #[tokio::test]
    async fn session_new_then_prompt_drives_a_run_and_ends_the_turn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (server, mut rx) = server();

        let created = server
            .new_session(json!({ "cwd": tmp.path(), "mcpServers": [] }))
            .await
            .expect("session created");
        let session_id = created["sessionId"]
            .as_str()
            .expect("session id")
            .to_string();

        let response = server
            .prompt(
                serde_json::from_value(json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "hello" }],
                }))
                .expect("prompt request"),
            )
            .await
            .expect("turn completed");
        assert_eq!(response["stopReason"], "end_turn", "{response}");

        // The mock model answers with text, so the client must have been
        // sent at least one agent_message_chunk for this session.
        let mut chunks = 0;
        while let Ok(message) = rx.try_recv() {
            let value = serde_json::to_value(&message).expect("serialize");
            if value["method"] == client_method::SESSION_UPDATE {
                assert_eq!(value["params"]["sessionId"], session_id.as_str());
                if value["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
                    chunks += 1;
                }
            }
        }
        assert!(chunks >= 1, "expected the final text as a message chunk");
    }

    #[tokio::test]
    async fn a_prompt_for_an_unknown_session_is_an_invalid_params_error() {
        let (server, _rx) = server();
        let error = server
            .prompt(
                serde_json::from_value(json!({
                    "sessionId": "never-created",
                    "prompt": [{ "type": "text", "text": "hi" }],
                }))
                .expect("prompt request"),
            )
            .await
            .expect_err("unknown session");
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("never-created"), "{error}");
    }

    #[tokio::test]
    async fn session_new_with_a_bad_cwd_is_an_invalid_params_error() {
        let (server, _rx) = server();
        let error = server
            .new_session(json!({ "cwd": "not/absolute", "mcpServers": [] }))
            .await
            .expect_err("bad cwd");
        assert_eq!(error.code, -32602);
    }

    #[tokio::test]
    async fn an_unknown_method_gets_method_not_found() {
        let (server, mut rx) = server();
        server
            .handle_request(json!(1), "session/teleport", Value::Null)
            .await;
        let message = next(&mut rx);
        assert_eq!(message["error"]["code"], -32601);
        assert_eq!(message["id"], 1);
    }

    #[tokio::test]
    async fn initialize_responds_over_the_wire_with_agent_info() {
        let (server, mut rx) = server();
        server
            .handle_request(
                json!(0),
                method::INITIALIZE,
                json!({ "protocolVersion": 1, "clientCapabilities": {} }),
            )
            .await;
        let message = next(&mut rx);
        assert_eq!(message["id"], 0);
        assert_eq!(message["result"]["protocolVersion"], 1);
        assert_eq!(message["result"]["agentInfo"]["name"], "forge");
    }

    #[tokio::test]
    async fn a_second_concurrent_turn_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (server, _rx) = server();
        let created = server
            .new_session(json!({ "cwd": tmp.path(), "mcpServers": [] }))
            .await
            .expect("session created");
        let session_id = created["sessionId"].as_str().expect("id").to_string();

        // Occupy the turn slot the way a running turn would.
        {
            let session = server.session(&session_id).expect("session");
            let mut turn = session.turn.lock().expect("lock");
            turn.active_run = Some("run-in-flight".to_string());
        }

        let error = server
            .prompt(
                serde_json::from_value(json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "hi" }],
                }))
                .expect("prompt request"),
            )
            .await
            .expect_err("a concurrent turn must be refused");
        assert_eq!(error.code, -32600);
        assert!(error.message.contains("run-in-flight"), "{error}");
    }

    #[tokio::test]
    async fn cancelling_an_unknown_session_is_ignored() {
        let (server, _rx) = server();
        server.cancel("never-created"); // must not panic
    }

    #[tokio::test]
    async fn a_response_to_an_unissued_request_is_ignored() {
        let (server, _rx) = server();
        server.resolve_response(&json!(999), Some(json!({})), None); // must not panic
    }

    #[tokio::test]
    async fn shutdown_releases_a_task_waiting_on_a_permission_answer() {
        // The failure this guards against is a hang, not a wrong value: a
        // permission task waiting on an answer that can never arrive keeps
        // its `Arc<ForgeAcpServer>` — and therefore the outgoing sender, and
        // therefore the writer task — alive forever, so `forge acp` would
        // never exit after the editor disconnects.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (server, mut rx) = server();
        let created = server
            .new_session(json!({ "cwd": tmp.path(), "mcpServers": [] }))
            .await
            .expect("session created");
        let session_id = created["sessionId"].as_str().expect("id").to_string();

        let asking = Arc::clone(&server);
        let waiting = tokio::spawn(async move {
            asking
                .ask_permission(
                    &session_id,
                    ToolCallUpdate::new("call_1"),
                    "Run rm -rf build (destructive risk)".to_string(),
                )
                .await
        });

        // Let the request reach the wire, so `pending` really has an entry.
        let sent = rx
            .recv()
            .await
            .expect("the permission request should reach the wire");
        let request = serde_json::to_value(&sent).expect("serialize");
        assert_eq!(request["method"], client_method::SESSION_REQUEST_PERMISSION);

        server.shutdown();

        let decision = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("shutdown must release the waiting task")
            .expect("task did not panic");
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "an unanswerable permission request must deny, never approve"
        );
    }

    #[test]
    fn an_id_is_salvaged_from_an_undispatchable_request() {
        assert_eq!(salvage_id(r#"{"id":4,"method":42}"#), Some(json!(4)));
        assert_eq!(salvage_id(r#"{"id":"abc"}"#), Some(json!("abc")));
        assert_eq!(salvage_id("not json"), None);
        assert_eq!(salvage_id(r#"{"method":"x"}"#), None);
    }
}
