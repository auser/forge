//! Protocol decisions as pure functions.
//!
//! Everything in this module is synchronous and side-effect free: given a
//! client's JSON (or a run's [`Event`]s) it returns the ACP values to send.
//! `server.rs` owns all the I/O, all the state that outlives a turn, and
//! the `AgentService` calls. That split is what lets the whole
//! forge-→-ACP mapping be tested without spawning a process or a runtime.

use std::path::{Path, PathBuf};

use forge_core::{Event, EventKind};
use serde_json::Value;

use crate::protocol::{
    AgentCapabilities, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
    NewSessionRequest, PROTOCOL_VERSION, PermissionOption, PermissionOptionKind,
    PromptCapabilities, RequestPermissionOutcome, RpcError, SessionCapabilities, SessionUpdate,
    StopReason, Supported, ToolCall, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolKind,
};

#[cfg(test)]
mod tests;

/// How we identify ourselves in `initialize`.
pub const AGENT_NAME: &str = "forge";

/// Option ids for the two permission choices we offer. They travel to the
/// client and come back in `RequestPermissionResponse`, so they are
/// constants rather than generated strings.
const OPTION_ALLOW: &str = "allow-once";
const OPTION_REJECT: &str = "reject-once";

// --- initialize ---------------------------------------------------------

/// Negotiate the connection.
///
/// We only speak v1, so we always answer [`PROTOCOL_VERSION`]: the spec
/// says to echo the client's version when we support it and otherwise
/// reply with our own latest, leaving the client to disconnect if it
/// cannot live with that.
///
/// The client's `fs`/`terminal` capabilities are deliberately ignored.
/// forge runs every tool through its own `ExecutionProvider`, rooted at the
/// session's project directory, so it never asks the editor to read or
/// write files on its behalf. That keeps one execution path (with one set
/// of risk classification and approval rules) instead of two.
pub fn initialize(request: &InitializeRequest) -> InitializeResponse {
    if request.protocol_version != PROTOCOL_VERSION {
        tracing::debug!(
            client = request.protocol_version,
            ours = PROTOCOL_VERSION,
            "client asked for a protocol version we do not speak; answering with ours"
        );
    }
    InitializeResponse {
        protocol_version: PROTOCOL_VERSION,
        agent_capabilities: AgentCapabilities {
            // No `session/load`: forge sessions are replayable on disk but
            // this adapter does not yet rebuild an ACP transcript from
            // them.
            load_session: false,
            // Text only, and we mean it — the model providers behind this
            // adapter take text prompts.
            prompt_capabilities: PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: false,
            },
            // `session/close` *is* supported, and advertising it matters:
            // this is a long-lived process, and a client that never tells us
            // a conversation is over leaves us holding its runtime for the
            // life of the editor.
            session_capabilities: SessionCapabilities {
                close: Some(Supported {}),
            },
        },
        auth_methods: Vec::new(),
        agent_info: Implementation {
            name: AGENT_NAME.to_string(),
            title: Some("Forge".to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    }
}

// --- session/new --------------------------------------------------------

/// Validate the `cwd` a client wants a session in, and return it as the
/// session's project root.
///
/// The spec requires an absolute path; we additionally require it to exist
/// and be a directory, because everything downstream (config discovery,
/// the graph, risk classification for file operations) is relative to it.
/// Failing here with a clear message beats starting a session that cannot
/// run anything.
pub fn session_root(request: &NewSessionRequest) -> Result<PathBuf, RpcError> {
    let Some(cwd) = request.cwd.as_ref() else {
        return Err(RpcError::invalid_params(
            "session/new requires a `cwd` (an absolute path to the project directory)",
        ));
    };
    if !cwd.is_absolute() {
        return Err(RpcError::invalid_params(format!(
            "session/new `cwd` must be an absolute path, got {}",
            cwd.display()
        )));
    }
    if !cwd.exists() {
        return Err(RpcError::invalid_params(format!(
            "session/new `cwd` does not exist: {}",
            cwd.display()
        )));
    }
    if !cwd.is_dir() {
        return Err(RpcError::invalid_params(format!(
            "session/new `cwd` must be a directory: {}",
            cwd.display()
        )));
    }
    Ok(cwd.clone())
}

// --- session/prompt content --------------------------------------------

/// Flatten a prompt's content blocks into the text forge's agent loop
/// takes.
///
/// `text` and `resource_link` are the two block types every ACP agent must
/// accept. A resource link becomes a path mention rather than being
/// dropped — the model can then ask to read it with `read_file`, which is
/// how forge sees files.
///
/// An embedded `resource` is **degraded rather than refused**, even though we
/// advertise `embeddedContext: false`. The spec puts the obligation on the
/// client ("MUST adapt its interface according to `PromptCapabilities`"), so
/// strictly this is the client's mistake — but the content is *right there*
/// and readable, and the failure we would cause is an editor @-mention
/// answering with `-32602` instead of doing the work. Its text is used when
/// it has any, and its uri as a mention when it does not (a blob we cannot
/// read is exactly a link we can name).
///
/// `image` and `audio` stay hard errors: there is no text in them to
/// degrade to, and silently dropping them would answer a question the user
/// did not ask.
pub fn prompt_text(blocks: &[ContentBlock]) -> Result<String, RpcError> {
    if blocks.is_empty() {
        return Err(RpcError::invalid_params(
            "session/prompt requires a non-empty `prompt`",
        ));
    }
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(text.clone()),
            ContentBlock::ResourceLink { uri, name } => {
                parts.push(name.clone().unwrap_or_else(|| uri.clone()));
            }
            ContentBlock::Resource { resource } => {
                let uri = resource.get("uri").and_then(Value::as_str);
                let text = resource
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty());
                tracing::debug!(
                    uri = uri.unwrap_or("<none>"),
                    embedded_text = text.is_some(),
                    "degrading an embedded resource block we did not advertise support for"
                );
                match (uri, text) {
                    (Some(uri), Some(text)) => parts.push(format!("{uri}:\n{text}")),
                    (None, Some(text)) => parts.push(text.to_string()),
                    (Some(uri), None) => parts.push(uri.to_string()),
                    (None, None) => {
                        return Err(RpcError::invalid_params(
                            "session/prompt carried a `resource` block with neither `uri` nor \
                             `text` (this agent advertises embeddedContext: false)",
                        ));
                    }
                }
            }
            other => {
                return Err(RpcError::invalid_params(format!(
                    "this agent accepts text and resource_link prompt content only, got {:?} \
                     (see the promptCapabilities in our initialize response)",
                    other.type_name()
                )));
            }
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return Err(RpcError::invalid_params(
            "session/prompt `prompt` contained no text",
        ));
    }
    Ok(text)
}

// --- events → updates ---------------------------------------------------

/// One thing to do in response to a run event.
#[derive(Debug, Clone)]
pub enum TurnAction {
    /// Send a `session/update` notification.
    Notify(SessionUpdate),
    /// Ask the client for permission, then unblock the parked run with the
    /// answer. `tool_call` points at a tool call the client has already
    /// been told about.
    AskPermission {
        tool_call: ToolCallUpdate,
        title: String,
    },
}

/// Per-turn state for translating forge events into ACP updates.
///
/// forge's events name tools but carry no call ids, and the agent loop
/// dispatches one call at a time (`ToolCallRequested` → `ToolStarted` →
/// `ToolCompleted`, then the next call), so this assigns the ids ACP needs
/// and remembers which call is in flight. Every event kind that has no
/// honest ACP slot maps to nothing: an editor transcript full of invented
/// updates would be worse than a quiet one.
///
/// The sequencing is an observation about the loop, not a guarantee we can
/// enforce from here — so [`TurnState::transition`] also checks the tool
/// *name* before attributing a status change, and opens a fresh tool call
/// rather than relabelling someone else's if the two ever disagree. A
/// mislabelled tool call is a lie about what the agent did.
#[derive(Debug)]
pub struct TurnState {
    /// Prefix making every id unique for the whole *session*, not just this
    /// turn — see [`TurnState::for_run`].
    prefix: String,
    /// Absolute project root, for turning the model's project-relative path
    /// arguments into the absolute paths `ToolCallLocation` requires.
    root: PathBuf,
    next_id: u64,
    current: Option<CurrentCall>,
}

/// The tool call we are currently narrating. `tool` is `None` for calls we
/// synthesized without a name to go on.
#[derive(Debug, Clone)]
struct CurrentCall {
    id: String,
    tool: Option<String>,
}

impl TurnState {
    /// State for one turn of `run_id`, in a project rooted at `root`.
    ///
    /// The run id becomes the tool-call id prefix because **ACP requires
    /// `toolCallId` to be unique within the _session_, not the turn**. A
    /// per-turn counter starting at 1 would re-issue `call_1` on the second
    /// prompt of every conversation, and a client that upserts tool calls by
    /// id (Zed does) would silently mutate the first turn's entry instead of
    /// adding a new one. Run ids are fresh per turn, so prefixing with one
    /// makes collisions impossible without any cross-turn bookkeeping.
    pub fn for_run(run_id: &str, root: impl Into<PathBuf>) -> Self {
        Self {
            prefix: run_id.to_string(),
            root: root.into(),
            next_id: 0,
            current: None,
        }
    }

    fn allocate(&mut self, tool: Option<&str>) -> String {
        self.next_id += 1;
        let id = format!("{}/call_{}", self.prefix, self.next_id);
        self.current = Some(CurrentCall {
            id: id.clone(),
            tool: tool.map(str::to_string),
        });
        id
    }

    /// Resolve a path the agent loop reported (project-relative, as the
    /// model wrote it) against the session root.
    ///
    /// The schema is explicit that `ToolCallLocation.path` is "the absolute
    /// file path", and it has to be: the client resolves it to open a file,
    /// and it does not know forge's project root. `Path::join` leaves an
    /// already-absolute path alone, so this is safe either way.
    fn locate(&self, path: &Path) -> ToolCallLocation {
        ToolCallLocation::new(self.root.join(path))
    }

    /// Translate one run event into zero or more actions.
    pub fn on_event(&mut self, event: &Event) -> Vec<TurnAction> {
        match &event.kind {
            EventKind::RoutingDecisionMade {
                router,
                selected_model,
                confidence,
                fallback_used,
                reason,
            } => {
                let mut text =
                    format!("Routing via {router}: {selected_model} (confidence {confidence:.2})");
                if *fallback_used {
                    text.push_str(" [fallback]");
                }
                if !reason.trim().is_empty() {
                    text.push_str(&format!(" — {reason}"));
                }
                vec![thought(text)]
            }

            EventKind::SkillActivated { name, .. } => vec![thought(format!("Skill: {name}"))],

            EventKind::ToolCallRequested { tool, args_summary } => {
                let args = Args::new(args_summary);
                let locations = args
                    .path()
                    .map(|path| vec![self.locate(&path)])
                    .unwrap_or_default();
                let id = self.allocate(Some(tool));
                vec![TurnAction::Notify(SessionUpdate::ToolCall(ToolCall {
                    tool_call_id: id,
                    title: title_for(tool, &args),
                    name: Some(tool.clone()),
                    kind: tool_kind(tool),
                    status: ToolCallStatus::Pending,
                    locations,
                    raw_input: Some(args.raw_input()),
                }))]
            }

            EventKind::ToolStarted { name } => {
                vec![self.transition(name, ToolCallStatus::InProgress)]
            }

            EventKind::ToolCompleted { name, success } => {
                let status = if *success {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                let action = self.transition(name, status);
                // The call is over; a later FileChanged belongs to
                // whatever comes next, not to this one.
                self.current = None;
                vec![action]
            }

            EventKind::FileChanged { path } => match self.current.clone() {
                Some(current) => {
                    let mut update = ToolCallUpdate::new(current.id);
                    update.locations = vec![self.locate(path)];
                    vec![TurnAction::Notify(SessionUpdate::ToolCallUpdate(update))]
                }
                // No call in flight (the needle fast path writes without
                // announcing a tool call): synthesize a finished edit so
                // the editor can still follow along to the file.
                None => {
                    let location = self.locate(path);
                    let id = self.allocate(None);
                    self.current = None;
                    vec![TurnAction::Notify(SessionUpdate::ToolCall(ToolCall {
                        tool_call_id: id,
                        title: format!("Edited {}", path.display()),
                        name: None,
                        kind: ToolKind::Edit,
                        status: ToolCallStatus::Completed,
                        locations: vec![location],
                        raw_input: None,
                    }))]
                }
            },

            EventKind::ApprovalRequested { command, risk } => {
                let title = format!("Run {command} ({} risk)", risk_word(*risk));
                let mut actions = Vec::new();
                let id = match self.current.clone() {
                    Some(current) => current.id,
                    // Nothing on screen to attach the prompt to, so put
                    // the operation there first: a permission request
                    // referencing an unknown tool call would leave the
                    // user approving a blank.
                    None => {
                        let id = self.allocate(None);
                        actions.push(TurnAction::Notify(SessionUpdate::ToolCall(ToolCall {
                            tool_call_id: id.clone(),
                            title: title.clone(),
                            name: None,
                            kind: ToolKind::Execute,
                            status: ToolCallStatus::Pending,
                            locations: Vec::new(),
                            raw_input: None,
                        })));
                        id
                    }
                };
                actions.push(TurnAction::AskPermission {
                    tool_call: ToolCallUpdate::new(id),
                    title,
                });
                actions
            }

            // Deliberately unmapped. `Completed`'s summary is truncated to
            // 80 characters by the session store, so the turn's final text
            // comes from the run outcome instead (see `server.rs`); the
            // rest is bookkeeping the editor has no use for.
            //
            // The v3 replay kinds (`AssistantMessage`, `ToolResult`,
            // `SessionForked`) are deliberately silent too: the editor
            // already gets the turn's text as one `agent_message_chunk`
            // and its tool calls as `tool_call` updates, so narrating the
            // replay records as well would duplicate the transcript.
            EventKind::AssistantMessage { .. }
            | EventKind::ToolResult { .. }
            | EventKind::SessionForked { .. }
            | EventKind::RunStarted { .. }
            | EventKind::ApprovalDecided { .. }
            | EventKind::TurnCompleted { .. }
            | EventKind::Note { .. }
            | EventKind::InputReceived { .. }
            | EventKind::Error { .. }
            | EventKind::Cancelled { .. }
            | EventKind::Completed { .. } => Vec::new(),
        }
    }

    /// Move a named tool call to a new status.
    ///
    /// The status is attributed to the call in flight only when the names
    /// agree (or when ours has no name to compare). Otherwise — a call we
    /// never saw requested, or events that arrived out of the order the loop
    /// produces them in — a fresh tool call is opened instead, because
    /// reporting "completed" against the wrong call would misdescribe what
    /// the agent actually did.
    fn transition(&mut self, name: &str, status: ToolCallStatus) -> TurnAction {
        let mine = self
            .current
            .clone()
            .filter(|current| current.tool.as_deref().is_none_or(|tool| tool == name));
        match mine {
            Some(current) => TurnAction::Notify(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(current.id).status(status),
            )),
            None => {
                let id = self.allocate(Some(name));
                TurnAction::Notify(SessionUpdate::ToolCall(ToolCall {
                    tool_call_id: id,
                    title: humanize(name),
                    name: Some(name.to_string()),
                    kind: tool_kind(name),
                    status,
                    locations: Vec::new(),
                    raw_input: None,
                }))
            }
        }
    }
}

fn thought(text: String) -> TurnAction {
    TurnAction::Notify(SessionUpdate::AgentThoughtChunk {
        content: ContentBlock::text(text),
    })
}

fn risk_word(risk: forge_core::execution::RiskLevel) -> &'static str {
    match risk {
        forge_core::execution::RiskLevel::Safe => "safe",
        forge_core::execution::RiskLevel::Risky => "risky",
        forge_core::execution::RiskLevel::Destructive => "destructive",
    }
}

/// Map a forge tool name onto the schema's `ToolKind` vocabulary, which is
/// what picks the icon and UI treatment in the editor.
pub fn tool_kind(tool: &str) -> ToolKind {
    match tool {
        "read_file" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "run_command" => ToolKind::Execute,
        "graph_context" | "graph_grep" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// A tool call's arguments as the event stream carries them: a JSON string
/// truncated to 120 characters.
///
/// Truncation is why this is not just `serde_json::from_str`: the most
/// interesting call (`write_file` with real content) is exactly the one
/// whose summary gets cut mid-string. When the JSON does not parse we scan
/// the text for the fields we care about, so a file edit still contributes
/// a title and a location.
struct Args<'a> {
    raw: &'a str,
    parsed: Option<Value>,
}

impl<'a> Args<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            raw,
            parsed: serde_json::from_str::<Value>(raw)
                .ok()
                .filter(Value::is_object),
        }
    }

    /// A string-valued argument, from the parsed JSON when it parsed and
    /// from a textual scan of the truncated remains when it did not.
    fn field(&self, key: &str) -> Option<String> {
        if let Some(value) = self.parsed.as_ref() {
            return value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty());
        }
        let needle = format!("\"{key}\":\"");
        let start = self.raw.find(&needle)? + needle.len();
        let rest = self.raw.get(start..)?;
        // Stop at the closing quote, honouring backslash escapes so a
        // path containing `\"` is not cut short.
        let mut out = String::new();
        let mut escaped = false;
        for ch in rest.chars() {
            match ch {
                _ if escaped => {
                    out.push(ch);
                    escaped = false;
                }
                '\\' => escaped = true,
                '"' => return Some(out).filter(|s| !s.is_empty()),
                _ => out.push(ch),
            }
        }
        // Truncated before the closing quote: an incomplete value is worse
        // than none, since it would point at a path that does not exist.
        None
    }

    /// The file this call touches, as the model wrote it (project-relative).
    /// [`TurnState::locate`] makes it absolute for the wire.
    fn path(&self) -> Option<PathBuf> {
        self.field("path").map(PathBuf::from)
    }

    /// What to show the user as the call's raw input. Structured when we
    /// have it; otherwise the summary verbatim, labelled as such rather
    /// than passed off as the real arguments.
    fn raw_input(&self) -> Value {
        match self.parsed.clone() {
            Some(value) => value,
            None => serde_json::json!({ "summary": self.raw }),
        }
    }
}

/// A human-readable one-liner for a tool call, in the imperative mood the
/// schema's examples use ("Reading configuration file").
fn title_for(tool: &str, args: &Args<'_>) -> String {
    let path = || args.field("path");
    match tool {
        "read_file" => path().map(|p| format!("Read {p}")),
        "write_file" => path().map(|p| format!("Write {p}")),
        "edit_file" => path().map(|p| format!("Edit {p}")),
        "delete_file" => path().map(|p| format!("Delete {p}")),
        "run_command" => args.field("command").map(|c| format!("Run {c}")),
        "graph_context" => args.field("query").map(|q| format!("Find files for {q:?}")),
        "graph_grep" => args.field("pattern").map(|p| format!("Search for {p:?}")),
        _ => None,
    }
    .unwrap_or_else(|| humanize(tool))
}

/// `write_file` → `Write file`: a readable fallback title when we could
/// not recover the arguments.
fn humanize(tool: &str) -> String {
    let spaced = tool.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Tool call".to_string(),
    }
}

// --- permissions --------------------------------------------------------

/// The choices we offer for a risky operation.
///
/// Only "once" variants: forge's approval gate is evaluated per operation
/// and it has nowhere to persist a standing decision, so advertising
/// `allow_always` would promise something we cannot honour.
pub fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: OPTION_ALLOW.to_string(),
            name: "Allow".to_string(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            option_id: OPTION_REJECT.to_string(),
            name: "Reject".to_string(),
            kind: PermissionOptionKind::RejectOnce,
        },
    ]
}

/// What the client's answer means for the parked run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
    /// The client cancelled the request instead of answering.
    Cancelled,
}

impl ApprovalDecision {
    /// The word to feed the run's input channel. The agent loop reads
    /// `y`/`yes`/`approve` as approval and treats everything else as a
    /// denial, so a denial only has to be "not one of those" — but it does
    /// have to be *sent*, or the run stays blocked.
    pub fn input(self) -> &'static str {
        match self {
            Self::Approve => "y",
            Self::Deny | Self::Cancelled => "n",
        }
    }
}

/// Map a permission outcome onto a decision.
///
/// An option id we never offered denies rather than guessing: approving on
/// an unrecognised answer would be the one failure mode with consequences.
pub fn approval_decision(outcome: &RequestPermissionOutcome) -> ApprovalDecision {
    match outcome {
        RequestPermissionOutcome::Cancelled => ApprovalDecision::Cancelled,
        RequestPermissionOutcome::Selected { option_id } if option_id == OPTION_ALLOW => {
            ApprovalDecision::Approve
        }
        RequestPermissionOutcome::Selected { option_id } => {
            if option_id != OPTION_REJECT {
                tracing::warn!(
                    option_id,
                    "client selected a permission option we never offered; denying"
                );
            }
            ApprovalDecision::Deny
        }
    }
}

// --- end of turn --------------------------------------------------------

/// How a finished run ends the ACP turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// Respond to `session/prompt` with this stop reason.
    Stop(StopReason),
    /// Respond with a JSON-RPC error: the turn did not run.
    Failed(RpcError),
}

/// Decide the turn's ending from the run's result.
///
/// `cancel_seen` is set when this session was cancelled (a `session/cancel`
/// notification, or a `Cancelled` event on the stream). It wins over
/// whatever the loop returned, because the spec requires `cancelled` to be
/// the stop reason after a `session/cancel` "even if the cancellation
/// causes exceptions in underlying operations".
pub fn turn_end(result: Result<String, String>, cancel_seen: bool) -> TurnEnd {
    if cancel_seen {
        return TurnEnd::Stop(StopReason::Cancelled);
    }
    match result {
        Ok(_) => TurnEnd::Stop(StopReason::EndTurn),
        Err(message) if message.contains("cancelled") => TurnEnd::Stop(StopReason::Cancelled),
        // `ApprovalRequired` escaped the loop: nobody answered the
        // permission request, so the risky operation did not happen and
        // the turn stopped short. That is a refusal, not a crash.
        Err(message) if message.contains("approval required") => {
            tracing::info!(%message, "turn stopped on an unanswered approval");
            TurnEnd::Stop(StopReason::Refusal)
        }
        Err(message) => TurnEnd::Failed(RpcError::internal(message)),
    }
}

// --- framing ------------------------------------------------------------

/// Parse one line of stdin into an incoming message.
///
/// A malformed line must never take the server down: every failure here
/// becomes a JSON-RPC error the caller can either reply with (when the
/// line had an id) or log (when it did not).
pub fn parse_line(line: &str) -> Result<crate::protocol::Incoming, RpcError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| RpcError::parse_error(format!("could not parse JSON: {e}")))?;
    if !value.is_object() {
        return Err(RpcError::invalid_request(
            "a JSON-RPC message must be an object",
        ));
    }
    let incoming: crate::protocol::Incoming = serde_json::from_value(value)
        .map_err(|e| RpcError::invalid_request(format!("not a valid JSON-RPC message: {e}")))?;
    if incoming.method.is_none() && !incoming.is_response() {
        return Err(RpcError::invalid_request(
            "a JSON-RPC message needs a `method`, or an `id` with a `result`/`error`",
        ));
    }
    Ok(incoming)
}
