//! `forge acp` end to end: a real ACP client handshake, prompt turn,
//! permission round trip and cancellation over the spawned binary's stdio.
//!
//! Hermetic exactly like `cli.rs` and `mcp.rs`: temp HOME/XDG, FORGE_*
//! scrubbed, autofetch off, offline providers only. stdout purity matters
//! double here — it is the protocol channel.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

use serde_json::{Value, json};

const FORGE_ENV_VARS: &[&str] = &[
    "FORGE_MODEL",
    "FORGE_MODEL_BASE_URL",
    "FORGE_MODEL_KEY_ENV",
    "FORGE_MOCK_SCRIPT",
    "FORGE_ROUTER",
    "FORGE_ROUTER_URL",
    "FORGE_ROUTER_KEY_ENV",
    "FORGE_ROUTER_ESCALATE",
    "FORGE_JEV_URL",
    "FORGE_JEV_KEY_ENV",
    "TYPESAFE_API_KEY",
    "FORGE_EXECUTION",
    "FORGE_APPROVAL",
    "FORGE_LOCAL_ONLY",
    "FORGE_MAX_TURNS",
    "FORGE_SERVER_HOST",
    "FORGE_SERVER_PORT",
    "FORGE_NEEDLE_VARIANT",
    "FORGE_NEEDLE_AUTOFETCH",
    "FORGE_NEEDLE_WEIGHTS_SHA256",
    "FORGE_NEEDLE_BACKEND",
    "FORGE_NEEDLE_WEIGHTS_BASE_URL",
    "FORGE_NEEDLE_TEST_SHA256",
    "FORGE_MOCK_VERBOSE",
    // Test-only mocks are gated; scrubbed then set by `forge()` below.
    "FORGE_TEST_MOCKS",
];

/// The ACP protocol version this adapter speaks.
const PROTOCOL_VERSION: i64 = 1;

/// A hermetic `forge` invocation (see `cli.rs::forge` for why each piece
/// is here).
fn forge(tmp: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
    for var in FORGE_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.arg("--project").arg(project);
    cmd.env("HOME", tmp.join("home"));
    cmd.env("XDG_CONFIG_HOME", tmp.join("xdg"));
    cmd.env("FORGE_NEEDLE_AUTOFETCH", "false");
    // The scaffolded project uses `model = "scripted-mock"`.
    cmd.env("FORGE_TEST_MOCKS", "1");
    cmd.env("NO_COLOR", "1");
    cmd
}

/// A project whose scripted model writes a file and then answers, plus a
/// built graph.
fn scaffold(tmp: &Path, approval: &str) -> std::path::PathBuf {
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(project.join("alpha.rs"), "fn parse_config() {}\n").expect("alpha");

    std::fs::write(
        project.join("script.json"),
        r#"[
            {"tool_calls": [{"id": "call_1", "name": "write_file", "arguments": {"path": "notes.txt", "content": "scripted content"}}]},
            {"text": "all done"}
        ]"#,
    )
    .expect("script");

    std::fs::create_dir_all(project.join(".forge")).expect(".forge");
    std::fs::write(
        project.join(".forge").join("config.toml"),
        format!(
            "model = \"scripted-mock\"\nmock_script = \"script.json\"\nrouter = \"static\"\napproval = \"{approval}\"\n"
        ),
    )
    .expect("config");

    let built = forge(tmp, &project)
        .args(["graph", "build"])
        .output()
        .expect("graph build");
    assert!(
        built.status.success(),
        "graph build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    project
}

/// How the test client answers a `session/request_permission` request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Allow,
    Reject,
    /// What a client sends when it is cancelling the turn instead of
    /// answering (the spec requires this for every pending request).
    Cancelled,
}

/// A minimal ACP client speaking newline-delimited JSON-RPC to a spawned
/// `forge acp`.
struct AcpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
    /// Every line the agent wrote, for the stdout-purity assertion.
    received: Vec<String>,
    /// Every `session/update` notification, in order.
    updates: Vec<Value>,
    /// How to answer permission requests, and how many arrived.
    answer: Answer,
    permission_requests: usize,
    /// Set when the client should send `session/cancel` on the first
    /// permission request — the deterministic way to cancel mid-turn.
    cancel_on_permission: Option<String>,
}

impl AcpClient {
    fn spawn(tmp: &Path, project: &Path) -> Self {
        let mut child = forge(tmp, project)
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn forge acp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            received: Vec::new(),
            updates: Vec::new(),
            answer: Answer::Allow,
            permission_requests: 0,
            cancel_on_permission: None,
        }
    }

    fn send(&mut self, message: &Value) {
        let line = serde_json::to_string(message).expect("serialize");
        assert!(!line.contains('\n'), "stdio messages are one line each");
        writeln!(self.stdin, "{line}").expect("write to agent stdin");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn read_message(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read agent stdout");
        assert!(read > 0, "agent closed stdout unexpectedly");
        self.received.push(line.trim_end().to_string());
        serde_json::from_str(line.trim_end())
            .unwrap_or_else(|e| panic!("non-JSON on stdout: {e}: {line:?}"))
    }

    /// Send a request and read until its response arrives, handling
    /// everything that interleaves: `session/update` notifications get
    /// collected, and `session/request_permission` requests get answered.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        loop {
            let message = self.read_message();
            if message["id"] == json!(id) && message.get("method").is_none() {
                return message;
            }
            match message["method"].as_str() {
                Some("session/update") => self.updates.push(message["params"].clone()),
                Some("session/request_permission") => self.answer_permission(&message),
                Some(other) => panic!("unexpected request from the agent: {other}"),
                None => panic!("unexpected message: {message}"),
            }
        }
    }

    fn answer_permission(&mut self, message: &Value) {
        self.permission_requests += 1;

        let options = message["params"]["options"]
            .as_array()
            .unwrap_or_else(|| panic!("no options in {message}"));
        assert!(
            !message["params"]["toolCall"]["toolCallId"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "a permission request must name the tool call it is about: {message}"
        );

        // Cancel first when asked to: the run is parked here, so this is a
        // deterministic mid-turn cancellation.
        if let Some(session_id) = self.cancel_on_permission.clone() {
            self.notify("session/cancel", json!({ "sessionId": session_id }));
        }

        let outcome = match self.answer {
            Answer::Cancelled => json!({ "outcome": "cancelled" }),
            Answer::Allow | Answer::Reject => {
                let wanted = if self.answer == Answer::Allow {
                    "allow_once"
                } else {
                    "reject_once"
                };
                let option = options
                    .iter()
                    .find(|o| o["kind"] == wanted)
                    .unwrap_or_else(|| panic!("no {wanted} option in {message}"));
                json!({ "outcome": "selected", "optionId": option["optionId"] })
            }
        };

        let id = message["id"].clone();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "outcome": outcome },
        }));
    }

    fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } },
                "clientInfo": { "name": "forge-tests", "version": "1.0.0" },
            }),
        )
    }

    fn new_session(&mut self, cwd: &Path) -> String {
        let created = self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }));
        created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("no sessionId in {created}"))
            .to_string()
    }

    fn prompt(&mut self, session_id: &str, text: &str) -> Value {
        self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": text }],
            }),
        )
    }

    /// Every id announced by a `tool_call` update so far, in order.
    fn tool_call_ids(&self) -> Vec<String> {
        self.updates_of("tool_call")
            .iter()
            .filter_map(|update| update["toolCallId"].as_str().map(str::to_string))
            .collect()
    }

    /// The `update` objects of one `sessionUpdate` kind, in order.
    fn updates_of(&self, kind: &str) -> Vec<&Value> {
        self.updates
            .iter()
            .map(|params| &params["update"])
            .filter(|update| update["sessionUpdate"] == kind)
            .collect()
    }

    /// Close stdin and wait for exit, but never longer than `within`.
    ///
    /// `wait()` would hang the whole test run if the agent failed to exit, so
    /// this polls: the assertion we want is "it exited", and a hang is the
    /// failure mode being tested for.
    fn expect_exit_within(mut self, within: std::time::Duration) {
        drop(self.stdin);
        let deadline = std::time::Instant::now() + within;
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => {
                    assert!(status.success(), "forge acp exited with {status}");
                    return;
                }
                None => {
                    if std::time::Instant::now() >= deadline {
                        let _ = self.child.kill();
                        panic!("forge acp did not exit within {within:?} after stdin closed");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }

    /// Close stdin (the graceful-shutdown signal) and wait for exit.
    fn shutdown(mut self) -> std::process::Output {
        drop(self.stdin);
        let status = self.child.wait().expect("wait for forge acp");
        let mut stderr = Vec::new();
        if let Some(mut handle) = self.child.stderr.take() {
            use std::io::Read;
            let _ = handle.read_to_end(&mut stderr);
        }
        assert!(
            status.success(),
            "forge acp exited with {status}: {}",
            String::from_utf8_lossy(&stderr)
        );
        std::process::Output {
            status,
            stdout: self.received.join("\n").into_bytes(),
            stderr,
        }
    }
}

#[test]
fn the_handshake_advertises_forge_and_protocol_v1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);

    let init = client.initialize();
    assert_eq!(
        init["result"]["protocolVersion"], PROTOCOL_VERSION,
        "{init}"
    );
    assert_eq!(init["result"]["agentInfo"]["name"], "forge");
    assert_eq!(
        init["result"]["agentInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    // Advertised honestly: no session/load, text-only prompts.
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], false);
    let prompt_caps = &init["result"]["agentCapabilities"]["promptCapabilities"];
    assert_eq!(prompt_caps["image"], false);
    assert_eq!(prompt_caps["audio"], false);
    assert_eq!(prompt_caps["embeddedContext"], false);

    client.shutdown();
}

#[test]
fn a_prompt_turn_streams_tool_calls_and_a_final_message() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();
    let session_id = client.new_session(&project);

    let response = client.prompt(&session_id, "write the notes");
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        project.join("notes.txt").is_file(),
        "the scripted tool call should have written notes.txt"
    );

    // Every update belongs to this session.
    for params in &client.updates {
        assert_eq!(params["sessionId"], session_id.as_str(), "{params}");
    }

    // The tool call is announced, then progresses to completed.
    let calls = client.updates_of("tool_call");
    let write = calls
        .iter()
        .find(|c| c["name"] == "write_file")
        .unwrap_or_else(|| panic!("no write_file tool_call in {:?}", client.updates));
    assert_eq!(write["kind"], "edit", "{write}");
    assert!(
        write["title"]
            .as_str()
            .unwrap_or_default()
            .contains("notes.txt"),
        "{write}"
    );
    // The schema is explicit that a location path is "the absolute file
    // path", and it has to be: the editor resolves it to open the file and
    // has no idea what forge's project root is. Emitting the model's
    // project-relative argument would make follow-along resolve nothing.
    assert_eq!(
        write["locations"][0]["path"],
        project.join("notes.txt").to_string_lossy().as_ref(),
        "locations must be absolute for Zed's follow-along: {write}"
    );
    let tool_call_id = write["toolCallId"].as_str().expect("toolCallId");

    let completed = client.updates_of("tool_call_update");
    assert!(
        completed
            .iter()
            .any(|u| u["toolCallId"] == tool_call_id && u["status"] == "completed"),
        "the tool call should reach completed: {completed:?}"
    );

    // The model's answer arrives as an agent message chunk.
    let chunks = client.updates_of("agent_message_chunk");
    assert!(
        chunks
            .iter()
            .any(|c| c["content"]["text"] == "all done" && c["content"]["type"] == "text"),
        "expected the final text as a message chunk: {chunks:?}"
    );

    // The routing decision shows up as a thought, which is how the needle
    // fast path becomes visible in the editor.
    assert!(
        !client.updates_of("agent_thought_chunk").is_empty(),
        "expected a routing thought: {:?}",
        client.updates
    );

    let output = client.shutdown();
    // stdout purity: every line the agent emitted is a JSON-RPC message.
    // A stray println! anywhere in forge would break ACP.
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(!stdout.trim().is_empty(), "expected protocol traffic");
    for line in stdout.lines() {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("non-JSON line on stdout: {e}: {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "not a JSON-RPC message: {line}");
    }
}

#[test]
fn the_acp_session_id_is_a_forge_session_id() {
    // The ids are deliberately the same, so a turn run from the editor is
    // inspectable and resumable from the CLI afterwards.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();
    let session_id = client.new_session(&project);
    client.prompt(&session_id, "write the notes");
    client.shutdown();

    let shown = forge(tmp.path(), &project)
        .args(["session", "show", &session_id])
        .output()
        .expect("session show");
    assert!(
        shown.status.success(),
        "forge session show {session_id} failed: {}",
        String::from_utf8_lossy(&shown.stderr)
    );
}

#[test]
fn a_risky_operation_asks_the_editor_for_permission_and_proceeds_when_allowed() {
    // stdin is the protocol channel, so the loop cannot prompt on it:
    // `NativeExecution` sees a non-terminal stdin, parks the run, and the
    // ACP client approves through session/request_permission.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.answer = Answer::Allow;
    client.initialize();
    let session_id = client.new_session(&project);

    let response = client.prompt(&session_id, "write the notes");
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        client.permission_requests, 1,
        "the parked run must produce exactly one permission request"
    );
    assert!(
        project.join("notes.txt").is_file(),
        "the approved write should have happened"
    );

    client.shutdown();
}

#[test]
fn rejecting_permission_stops_the_operation_without_failing_the_turn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.answer = Answer::Reject;
    client.initialize();
    let session_id = client.new_session(&project);

    let response = client.prompt(&session_id, "write the notes");
    assert_eq!(client.permission_requests, 1);
    // The tool was refused, the model was told, and the turn finished.
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        !project.join("notes.txt").exists(),
        "a rejected write must not happen"
    );
    assert!(
        client
            .updates_of("tool_call_update")
            .iter()
            .any(|u| u["status"] == "failed"),
        "the refused tool call should be reported failed: {:?}",
        client.updates
    );

    client.shutdown();
}

#[test]
fn cancelling_mid_turn_ends_the_turn_with_the_cancelled_stop_reason() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();
    let session_id = client.new_session(&project);

    // Cancel while the run is parked on the approval, then answer the
    // pending permission request with `cancelled`, which is what the spec
    // requires of a cancelling client.
    client.answer = Answer::Cancelled;
    client.cancel_on_permission = Some(session_id.clone());

    let response = client.prompt(&session_id, "write the notes");
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert!(
        !project.join("notes.txt").exists(),
        "a cancelled turn must not perform the risky write"
    );

    client.shutdown();
}

#[test]
fn the_agent_exits_when_the_editor_disconnects_mid_permission() {
    // The editor is closed while a permission prompt is on screen. The turn
    // is parked inside the loop and a task is waiting on an answer that can
    // never arrive — so this is the one path where a naive shutdown (wait for
    // everything holding the outgoing channel) never terminates.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();
    let session_id = client.new_session(&project);

    client.send(&json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "write the notes" }],
        },
    }));

    // Read until the permission request arrives, then walk away without
    // answering it.
    loop {
        let message = client.read_message();
        if message["method"] == "session/request_permission" {
            break;
        }
    }

    client.expect_exit_within(std::time::Duration::from_secs(30));
}

#[test]
fn a_prompt_for_an_unknown_session_is_a_json_rpc_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();

    let response = client.prompt("sess-does-not-exist", "hello");
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("sess-does-not-exist"),
        "{response}"
    );

    client.shutdown();
}

#[test]
fn session_new_rejects_a_cwd_that_does_not_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();

    let response = client.request(
        "session/new",
        json!({ "cwd": project.join("no-such-dir"), "mcpServers": [] }),
    );
    assert_eq!(response["error"]["code"], -32602, "{response}");

    client.shutdown();
}

#[test]
fn an_unknown_method_is_answered_with_method_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();

    let response = client.request("session/teleport", json!({}));
    assert_eq!(response["error"]["code"], -32601, "{response}");

    client.shutdown();
}

#[test]
fn the_server_survives_malformed_input_and_keeps_serving() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();

    // Garbage with no id: logged to stderr, no response, connection lives.
    client.send(&json!("this is not a jsonrpc object"));
    writeln!(client.stdin, "not json at all").expect("write");
    client.stdin.flush().expect("flush");

    // A broken request that still carries an id gets an error response
    // rather than silence.
    client.send(&json!({ "jsonrpc": "2.0", "id": 9001, "method": 42 }));
    let response = loop {
        let message = client.read_message();
        if message["id"] == json!(9001) {
            break message;
        }
    };
    assert_eq!(response["error"]["code"], -32600, "{response}");

    // Still fully functional afterwards.
    let session_id = client.new_session(&project);
    let done = client.prompt(&session_id, "write the notes");
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");

    client.shutdown();
}

#[test]
fn a_second_prompt_continues_the_same_session_with_fresh_tool_call_ids() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();
    let session_id = client.new_session(&project);

    let first = client.prompt(&session_id, "write the notes");
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let after_first: Vec<String> = client.tool_call_ids();
    assert!(!after_first.is_empty(), "the first turn ran a tool call");

    let second = client.prompt(&session_id, "and again");
    assert_eq!(second["result"]["stopReason"], "end_turn", "{second}");

    // ACP requires `toolCallId` to be unique within the SESSION. A client
    // that upserts tool calls by id (Zed does) would mutate the first
    // turn's entry if the second turn reused an id.
    let all = client.tool_call_ids();
    let mut unique = all.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        all.len(),
        unique.len(),
        "tool call ids repeated across turns of one session: {all:?}"
    );

    client.shutdown();
}

#[test]
fn a_cancel_arriving_right_after_a_prompt_is_not_lost() {
    // Both messages are written before the agent has necessarily started
    // the turn. The turn slot is claimed synchronously when the prompt is
    // read, so the cancel that follows always finds a run to cancel; if it
    // did not, the turn would answer `end_turn` for work the user stopped.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let mut client = AcpClient::spawn(tmp.path(), &project);
    client.initialize();
    let session_id = client.new_session(&project);

    // One write, two messages, no reads in between.
    let batch = format!(
        "{}\n{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "write the notes" }],
            },
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id },
        }),
    );
    client.stdin.write_all(batch.as_bytes()).expect("write");
    client.stdin.flush().expect("flush");

    // Answer any permission request with `cancelled`, per the spec's rule
    // for a cancelling client.
    client.answer = Answer::Cancelled;
    let response = loop {
        let message = client.read_message();
        if message["id"] == json!(77) && message.get("method").is_none() {
            break message;
        }
        if message["method"] == "session/request_permission" {
            client.answer_permission(&message);
        }
    };

    assert_eq!(
        response["result"]["stopReason"], "cancelled",
        "a cancel racing the prompt must not be erased: {response}"
    );
    assert!(
        !project.join("notes.txt").exists(),
        "a cancelled turn must not perform the write"
    );

    client.shutdown();
}

#[test]
fn session_close_ends_a_conversation_and_is_advertised() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = AcpClient::spawn(tmp.path(), &project);

    // A client only sends session/close if we advertise it.
    let init = client.initialize();
    assert_eq!(
        init["result"]["agentCapabilities"]["sessionCapabilities"]["close"],
        json!({}),
        "{init}"
    );

    let session_id = client.new_session(&project);
    let closed = client.request("session/close", json!({ "sessionId": session_id }));
    assert!(closed["result"].is_object(), "{closed}");

    // The session is really gone: prompting it now is an error.
    let after = client.prompt(&session_id, "still there?");
    assert_eq!(after["error"]["code"], -32602, "{after}");

    // And the connection is still usable for a fresh conversation.
    let fresh = client.new_session(&project);
    assert_ne!(fresh, session_id);
    let done = client.prompt(&fresh, "write the notes");
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");

    client.shutdown();
}
