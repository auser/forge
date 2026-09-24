//! `forge mcp` end to end: a real MCP client handshake over the spawned
//! binary's stdio.
//!
//! Hermetic exactly like `cli.rs`: temp HOME/XDG, FORGE_* scrubbed,
//! autofetch off, offline providers only. stdout purity matters double
//! here — it is the protocol channel.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

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
];

/// The current MCP revision, used for the modern (per-request metadata)
/// half of the handshake tests.
const MODERN_VERSION: &str = "2026-07-28";
/// A handshake-based revision, for the legacy half.
const LEGACY_VERSION: &str = "2025-06-18";

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
    cmd.env("NO_COLOR", "1");
    cmd
}

/// A project with two source files, a skill, a scripted-mock script and a
/// built graph.
fn scaffold(tmp: &Path, approval: &str) -> std::path::PathBuf {
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(project.join("alpha.rs"), "fn parse_config() {}\n").expect("alpha");
    std::fs::write(project.join("beta.rs"), "fn serve_http() {}\n").expect("beta");

    let skill = project.join(".forge").join("skills").join("reviewing");
    std::fs::create_dir_all(&skill).expect("skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: reviewing\ndescription: Review a diff carefully\n---\n\nRead the diff first.\n",
    )
    .expect("skill");

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

/// A minimal MCP client speaking newline-delimited JSON-RPC to a spawned
/// `forge mcp`.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
    /// Every line the server wrote, for the stdout-purity assertion.
    received: Vec<String>,
}

impl McpClient {
    fn spawn(tmp: &Path, project: &Path) -> Self {
        let mut child = forge(tmp, project)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn forge mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            received: Vec::new(),
        }
    }

    fn send(&mut self, message: &serde_json::Value) {
        let line = serde_json::to_string(message).expect("serialize");
        assert!(!line.contains('\n'), "stdio messages are one line each");
        writeln!(self.stdin, "{line}").expect("write to server stdin");
        self.stdin.flush().expect("flush");
    }

    /// Send a request and read until its response arrives.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        loop {
            let message = self.read_message();
            if message["id"] == serde_json::json!(id) {
                return message;
            }
        }
    }

    /// A modern (2026-07-28) request: the protocol version, client
    /// identity and capabilities travel in `_meta` instead of a handshake.
    fn modern_request(&mut self, method: &str, mut params: serde_json::Value) -> serde_json::Value {
        params["_meta"] = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": MODERN_VERSION,
            "io.modelcontextprotocol/clientInfo": { "name": "forge-tests", "version": "1.0.0" },
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        self.request(method, params)
    }

    fn read_message(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("read server stdout");
        assert!(read > 0, "server closed stdout unexpectedly");
        self.received.push(line.trim_end().to_string());
        serde_json::from_str(line.trim_end())
            .unwrap_or_else(|e| panic!("non-JSON on stdout: {e}: {line:?}"))
    }

    fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
    }

    /// The JSON payload a tool returned, read out of the content array
    /// (the text item) — and cross-checked against `structuredContent`.
    fn tool_json(result: &serde_json::Value) -> serde_json::Value {
        let content = result["result"]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("no content array in {result}"));
        let text = content[0]["text"].as_str().expect("text content");
        let parsed: serde_json::Value = serde_json::from_str(text).expect("tool text is JSON");
        assert_eq!(
            result["result"]["structuredContent"], parsed,
            "structuredContent must mirror the text item"
        );
        parsed
    }

    /// Close stdin (the graceful-shutdown signal) and wait for exit.
    fn shutdown(mut self) -> std::process::Output {
        drop(self.stdin);
        let status = self.child.wait().expect("wait for forge mcp");
        let mut stderr = Vec::new();
        if let Some(mut handle) = self.child.stderr.take() {
            use std::io::Read;
            let _ = handle.read_to_end(&mut stderr);
        }
        assert!(
            status.success(),
            "forge mcp exited with {status}: {}",
            String::from_utf8_lossy(&stderr)
        );
        std::process::Output {
            status,
            stdout: self.received.join("\n").into_bytes(),
            stderr,
        }
    }

    fn initialize(&mut self) -> serde_json::Value {
        let response = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": LEGACY_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "forge-tests", "version": "1.0.0" },
            }),
        );
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
        response
    }
}

#[test]
fn legacy_handshake_lists_the_whole_tool_surface() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = McpClient::spawn(tmp.path(), &project);

    let init = client.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "forge");
    assert_eq!(
        init["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "the tools capability must be advertised: {init}"
    );
    assert_eq!(
        init["result"]["protocolVersion"], LEGACY_VERSION,
        "a legacy client's version should be echoed back"
    );

    let listed = client.request("tools/list", serde_json::json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
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
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    for tool in tools {
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "{}: inputSchema must be an object schema",
            tool["name"]
        );
        assert!(tool["description"].is_string());
    }

    client.shutdown();
}

#[test]
fn a_modern_client_can_discover_and_call_without_an_initialize_handshake() {
    // MCP 2026-07-28 replaced the handshake with per-request `_meta` plus
    // a mandatory `server/discover`. Serving both eras is the whole reason
    // this adapter uses the official SDK.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = McpClient::spawn(tmp.path(), &project);

    let discovered = client.modern_request("server/discover", serde_json::json!({}));
    let versions = discovered["result"]["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("no supportedVersions in {discovered}"));
    assert!(
        versions.iter().any(|v| v == MODERN_VERSION),
        "server must support the current revision: {versions:?}"
    );
    assert!(discovered["result"]["capabilities"]["tools"].is_object());

    let listed = client.modern_request("tools/list", serde_json::json!({}));
    assert!(
        !listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .is_empty()
    );

    let called = client.modern_request(
        "tools/call",
        serde_json::json!({ "name": "forge_graph_map", "arguments": {} }),
    );
    assert!(
        McpClient::tool_json(&called)["directories"].is_array(),
        "{called}"
    );

    client.shutdown();
}

#[test]
fn graph_and_skill_tools_answer_from_the_real_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = McpClient::spawn(tmp.path(), &project);
    client.initialize();

    let map = client.call_tool("forge_graph_map", serde_json::json!({}));
    let dirs = McpClient::tool_json(&map);
    assert!(
        !dirs["directories"]
            .as_array()
            .expect("directories")
            .is_empty(),
        "{dirs}"
    );

    let context = client.call_tool(
        "forge_graph_context",
        serde_json::json!({ "query": "parse_config" }),
    );
    let hits = McpClient::tool_json(&context);
    assert_eq!(hits["hits"][0]["path"], "alpha.rs", "{hits}");

    let grep = client.call_tool(
        "forge_graph_grep",
        serde_json::json!({ "pattern": "serve_http" }),
    );
    assert!(
        !McpClient::tool_json(&grep)["matches"]
            .as_array()
            .expect("matches")
            .is_empty()
    );

    // Semantic search has no weights in a hermetic run: a tool error that
    // names the fix, never a protocol error.
    let semantic = client.call_tool(
        "forge_graph_grep",
        serde_json::json!({ "pattern": "http", "semantic": true }),
    );
    assert_eq!(semantic["result"]["isError"], true, "{semantic}");
    let message = McpClient::tool_json(&semantic)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(message.contains("forge init"), "unhelpful: {message}");

    let skills = client.call_tool("forge_skill_list", serde_json::json!({}));
    let listed = McpClient::tool_json(&skills);
    assert!(
        listed["skills"]
            .as_array()
            .expect("skills")
            .iter()
            .any(|s| s["name"] == "reviewing"),
        "{listed}"
    );

    let shown = client.call_tool(
        "forge_skill_show",
        serde_json::json!({ "name": "reviewing" }),
    );
    assert!(
        McpClient::tool_json(&shown)["instructions"]
            .as_str()
            .unwrap_or_default()
            .contains("Read the diff first")
    );

    let doctor = client.call_tool("forge_doctor", serde_json::json!({}));
    let report = McpClient::tool_json(&doctor);
    assert!(report["checks"].is_array(), "{report}");
    assert!(
        report["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .any(|c| c["check"] == "project graph"),
        "doctor must report the same checks as the CLI: {report}"
    );

    client.shutdown();
}

#[test]
fn an_unknown_tool_is_a_json_rpc_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = McpClient::spawn(tmp.path(), &project);
    client.initialize();

    let response = client.call_tool("forge_not_a_tool", serde_json::json!({}));
    assert!(response["error"].is_object(), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("forge_not_a_tool")
    );

    client.shutdown();
}

#[test]
fn run_executes_the_agent_loop_and_everything_on_stdout_is_protocol() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut client = McpClient::spawn(tmp.path(), &project);
    client.initialize();

    let run = client.call_tool(
        "forge_run",
        serde_json::json!({ "prompt": "write the notes" }),
    );
    let outcome = McpClient::tool_json(&run);
    assert_eq!(outcome["status"], "completed", "{outcome}");
    assert_eq!(outcome["text"], "all done", "{outcome}");
    assert!(
        project.join("notes.txt").is_file(),
        "the scripted tool call should have written notes.txt"
    );

    let status = client.call_tool(
        "forge_run_status",
        serde_json::json!({ "run_id": outcome["run_id"] }),
    );
    let status_json = McpClient::tool_json(&status);
    assert_eq!(status_json["status"], "completed");
    assert!(
        !status_json["last_events"]
            .as_array()
            .expect("events")
            .is_empty()
    );

    let output = client.shutdown();
    // stdout purity: every single line the server emitted is a JSON-RPC
    // message. A stray println! anywhere in forge would break MCP.
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(!stdout.trim().is_empty(), "expected protocol traffic");
    for line in stdout.lines() {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("non-JSON line on stdout: {e}: {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "not a JSON-RPC message: {line}");
    }
}

#[test]
fn an_approval_pause_is_answered_with_run_input() {
    // stdin is the protocol channel, so the loop cannot prompt on it:
    // `NativeExecution` sees a non-terminal stdin, parks the run with
    // ApprovalRequired, and the MCP client approves out of band.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let mut client = McpClient::spawn(tmp.path(), &project);
    client.initialize();

    // Short budget: the run parks on approval and will never finish on
    // its own, so waiting the full default would just stall the test.
    let run = client.call_tool(
        "forge_run",
        serde_json::json!({ "prompt": "write the notes", "timeout_ms": 1500 }),
    );
    let started = McpClient::tool_json(&run);
    let run_id = started["run_id"].as_str().expect("run id").to_string();

    // Poll until the run reports itself parked.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = client.call_tool("forge_run_status", serde_json::json!({ "run_id": run_id }));
        let json = McpClient::tool_json(&status);
        if json["status"] == "waiting_for_approval" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never parked for approval: {json}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let delivered = client.call_tool(
        "forge_run_input",
        serde_json::json!({ "run_id": run_id, "input": "y" }),
    );
    assert_eq!(McpClient::tool_json(&delivered)["delivered"], true);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = client.call_tool("forge_run_status", serde_json::json!({ "run_id": run_id }));
        let json = McpClient::tool_json(&status);
        if json["status"] == "completed" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "approved run never completed: {json}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        project.join("notes.txt").is_file(),
        "the approved write should have happened"
    );

    client.shutdown();
}
