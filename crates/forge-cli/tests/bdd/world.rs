//! Shared world state for the cucumber BDD harness. Every scenario runs
//! the compiled `forge` binary in a fresh tempdir with hermetic HOME /
//! XDG_CONFIG_HOME / FORGE_* env.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

pub const FORGE_ENV_VARS: &[&str] = &[
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
    // Jev's default credential env var is a conventional provider-style
    // name, not a FORGE_* one — scrubbed here too so a developer's shell
    // can't make the default `router = "needle"`, `router_escalate =
    // "auto"` stack silently try a real network escalation to
    // api.typesafe.ai during an otherwise-hermetic scenario run.
    "TYPESAFE_API_KEY",
    "FORGE_EXECUTION",
    "FORGE_APPROVAL",
    "FORGE_LOCAL_ONLY",
    "FORGE_MAX_TURNS",
    "FORGE_SERVER_HOST",
    "FORGE_SERVER_PORT",
    // needle config/env knobs (see forge-config's ENV_KEYS and
    // forge-needle's weights.rs/lib.rs): removed so a developer's shell
    // can't perturb a supposedly-hermetic scenario run. `FORGE_NEEDLE_AUTOFETCH`
    // is set back to `"false"` explicitly below, after this removal runs.
    "FORGE_NEEDLE_VARIANT",
    "FORGE_NEEDLE_AUTOFETCH",
    "FORGE_NEEDLE_WEIGHTS_SHA256",
    "FORGE_NEEDLE_BACKEND",
    "FORGE_NEEDLE_WEIGHTS_BASE_URL",
    "FORGE_NEEDLE_TEST_SHA256",
    // Mock providers are test-only and gated (see forge-providers'
    // `test_mocks`). Scrubbed then set back to "1" below, so a scenario
    // runs with mocks unlocked no matter what the developer's shell says.
    "FORGE_TEST_MOCKS",
    // The mock's system-context echo is opt-in (see `MockModel`'s docs);
    // scrubbed so a developer's shell can neither switch it on for
    // scenarios that assert clean output nor off for the one that needs it.
    "FORGE_MOCK_VERBOSE",
];

#[derive(Debug, Default, cucumber::World)]
pub struct BddWorld {
    /// Scenario tempdir; the project root is the tempdir itself.
    pub dir: Option<tempfile::TempDir>,
    /// Extra env vars for the next command (e.g. FORGE_MODEL).
    pub env: HashMap<String, String>,
    pub last_stdout: String,
    pub last_stderr: String,
    pub last_code: Option<i32>,
    /// Spawned `forge serve` child process.
    pub server: Option<tokio::process::Child>,
    /// Spawned `forge mcp` child process and its protocol pipes.
    pub mcp: Option<McpChild>,
    /// Every line `forge mcp` wrote to stdout (the protocol channel).
    pub mcp_lines: Vec<String>,
    /// Tool names from the last `tools/list`.
    pub mcp_tools: Vec<String>,
    /// Spawned `forge acp` child process and its protocol pipes.
    pub acp: Option<AcpChild>,
    /// Every line `forge acp` wrote to stdout (the protocol channel).
    pub acp_lines: Vec<String>,
    /// The `update` object of every `session/update` notification, in order.
    pub acp_updates: Vec<serde_json::Value>,
    /// The ACP session created by `start_acp`.
    pub acp_session: String,
    /// `stopReason` from the last `session/prompt` response.
    pub acp_stop_reason: String,
    /// How many `session/request_permission` requests the agent made.
    pub acp_permissions: usize,
    pub base_url: String,
    /// Wiremock server standing in for a System One-compatible router.
    pub router_mock: Option<wiremock::MockServer>,
    /// Wiremock server standing in for an OpenAI-compatible model endpoint.
    pub chat_mock: Option<wiremock::MockServer>,
    // Server scenario state.
    pub health_status: u16,
    pub health_body: String,
    pub run_id: String,
    pub run_status: u16,
    pub session_id: String,
    /// The session `forge session fork` created.
    pub fork_session_id: String,
    /// The source session's log, captured before forking.
    pub source_log_before: String,
    pub secret: String,
    pub sse_content_type: String,
    pub sse_body: String,
    /// Pending `.forge/config.toml` lines, flushed before the next
    /// command or server start (lets several Given steps each set keys).
    pub config_lines: Vec<String>,
    /// Pending `[table]` config blocks (e.g. `[models.x]`).
    pub config_blocks: Vec<String>,
}

/// A running `forge mcp` with its stdio pipes and JSON-RPC id counter.
#[derive(Debug)]
pub struct McpChild {
    pub child: tokio::process::Child,
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    pub next_id: u64,
}

/// A running `forge acp` with its stdio pipes and JSON-RPC id counter.
#[derive(Debug)]
pub struct AcpChild {
    pub child: tokio::process::Child,
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    pub next_id: u64,
}

impl BddWorld {
    pub fn project(&mut self) -> PathBuf {
        if self.dir.is_none() {
            self.dir = Some(tempfile::tempdir().expect("create scenario tempdir"));
        }
        self.dir.as_ref().expect("tempdir").path().to_path_buf()
    }

    pub fn write_file(&mut self, rel: &str, content: &str) {
        let path = self.project().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent dir")).expect("mkdir");
        std::fs::write(path, content).expect("write file");
    }

    /// Run the compiled forge binary against the scenario project with a
    /// hermetic environment.
    pub async fn run_forge(&mut self, args: &[&str]) {
        self.flush_config();
        let root = self.project();
        let home = root.join("home");
        let xdg = root.join("xdg");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&xdg).expect("xdg");

        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge"));
        cmd.arg("--project")
            .arg(&root)
            .args(args)
            .stdin(Stdio::null());
        // Remove first, then set defaults: FORGE_NEEDLE_AUTOFETCH is one of
        // FORGE_ENV_VARS now, so setting it before removing would have the
        // removal undo it.
        for var in FORGE_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("NO_COLOR", "1")
            // Hermetic default: BDD never depends on the network.
            // `needle.variant` now defaults to "full" (the only variant
            // with a hosted, pinned artifact), so an un-overridden `forge
            // init` would otherwise download ~35 MB from Hugging Face on
            // every scenario run. A scenario that wants to exercise real
            // autofetch can still opt in via `world.env`, which is applied
            // after this and wins.
            .env("FORGE_NEEDLE_AUTOFETCH", "false")
            // Scenarios drive the agent loop with mock/scripted models,
            // which configuration refuses to resolve without this.
            .env("FORGE_TEST_MOCKS", "1");
        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        let output = cmd.output().await.expect("forge executes");
        self.last_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        self.last_stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        self.last_code = output.status.code();
    }

    /// Spawn `forge serve` on a free loopback port and wait for /health.
    pub async fn start_server(&mut self) {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
            let port = listener.local_addr().expect("addr").port();
            drop(listener);
            port
        };
        let root = self.project();
        let home = root.join("home");
        let xdg = root.join("xdg");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&xdg).expect("xdg");

        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge"));
        cmd.arg("--project")
            .arg(&root)
            .args(["serve", "--host", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("NO_COLOR", "1");
        for var in FORGE_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.env("FORGE_TEST_MOCKS", "1");
        let child = cmd.spawn().expect("spawn forge serve");
        self.server = Some(child);
        self.base_url = format!("http://127.0.0.1:{port}");

        let client = reqwest::Client::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match client.get(format!("{}/health", self.base_url)).send().await {
                Ok(response) if response.status().is_success() => return,
                _ => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "forge serve did not become healthy within 10s"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Spawn `forge mcp` and complete the MCP handshake.
    ///
    /// stdio is the protocol channel here, so stdin/stdout are pipes and
    /// stderr is left inherited-free for diagnostics only.
    pub async fn start_mcp(&mut self) {
        use tokio::io::BufReader;

        self.flush_config();
        let root = self.project();
        let home = root.join("home");
        let xdg = root.join("xdg");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&xdg).expect("xdg");

        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge"));
        cmd.arg("--project")
            .arg(&root)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("NO_COLOR", "1");
        for var in FORGE_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.env("FORGE_NEEDLE_AUTOFETCH", "false");
        cmd.env("FORGE_TEST_MOCKS", "1");
        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().expect("spawn forge mcp");
        let stdin = child.stdin.take().expect("mcp stdin");
        let stdout = BufReader::new(child.stdout.take().expect("mcp stdout"));
        self.mcp = Some(McpChild {
            child,
            stdin,
            stdout,
            next_id: 1,
        });

        let init = self
            .mcp_request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "forge-bdd", "version": "1.0.0" },
                }),
            )
            .await;
        assert_eq!(
            init["result"]["serverInfo"]["name"], "forge",
            "unexpected initialize result: {init}"
        );
        self.mcp_notify("notifications/initialized", serde_json::json!({}))
            .await;
    }

    /// Send a JSON-RPC notification to `forge mcp` (no response expected).
    pub async fn mcp_notify(&mut self, method: &str, params: serde_json::Value) {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .expect("serialize");
        let mcp = self.mcp.as_mut().expect("forge mcp running");
        mcp.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write to mcp stdin");
        mcp.stdin.flush().await.expect("flush mcp stdin");
    }

    /// Send a request and read newline-delimited messages until its
    /// response arrives.
    pub async fn mcp_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let id = {
            let mcp = self.mcp.as_mut().expect("forge mcp running");
            let id = mcp.next_id;
            mcp.next_id += 1;
            let line = serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .expect("serialize");
            mcp.stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .expect("write to mcp stdin");
            mcp.stdin.flush().await.expect("flush mcp stdin");
            id
        };

        loop {
            let mut line = String::new();
            {
                let mcp = self.mcp.as_mut().expect("forge mcp running");
                let read = mcp
                    .stdout
                    .read_line(&mut line)
                    .await
                    .expect("read mcp stdout");
                assert!(read > 0, "forge mcp closed stdout unexpectedly");
            }
            let line = line.trim_end().to_string();
            self.mcp_lines.push(line.clone());
            let message: serde_json::Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON on mcp stdout: {e}: {line:?}"));
            if message["id"] == serde_json::json!(id) {
                return message;
            }
        }
    }

    /// Spawn `forge acp`, complete the ACP handshake and open a session
    /// rooted at the scenario project.
    ///
    /// stdio is the protocol channel here, so stdin/stdout are pipes.
    pub async fn start_acp(&mut self) {
        use tokio::io::BufReader;

        self.flush_config();
        let root = self.project();
        let home = root.join("home");
        let xdg = root.join("xdg");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&xdg).expect("xdg");

        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge"));
        cmd.arg("--project")
            .arg(&root)
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("NO_COLOR", "1");
        for var in FORGE_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.env("FORGE_NEEDLE_AUTOFETCH", "false");
        cmd.env("FORGE_TEST_MOCKS", "1");
        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().expect("spawn forge acp");
        let stdin = child.stdin.take().expect("acp stdin");
        let stdout = BufReader::new(child.stdout.take().expect("acp stdout"));
        self.acp = Some(AcpChild {
            child,
            stdin,
            stdout,
            next_id: 1,
        });

        let init = self
            .acp_request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": { "name": "forge-bdd", "version": "1.0.0" },
                }),
            )
            .await;
        assert_eq!(
            init["result"]["agentInfo"]["name"], "forge",
            "unexpected initialize result: {init}"
        );

        let created = self
            .acp_request(
                "session/new",
                serde_json::json!({ "cwd": root, "mcpServers": [] }),
            )
            .await;
        self.acp_session = created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("no sessionId in {created}"))
            .to_string();
    }

    /// Send a prompt and drive the turn to its response.
    pub async fn acp_prompt(&mut self, text: &str) {
        let session_id = self.acp_session.clone();
        let response = self
            .acp_request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await;
        self.acp_stop_reason = response["result"]["stopReason"]
            .as_str()
            .unwrap_or_else(|| panic!("no stopReason in {response}"))
            .to_string();
    }

    /// Send a request and read until its response arrives, handling what
    /// interleaves: `session/update` notifications are collected, and
    /// `session/request_permission` requests are approved (the scenario
    /// plays a user who says yes).
    pub async fn acp_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let id = {
            let acp = self.acp.as_mut().expect("forge acp running");
            let id = acp.next_id;
            acp.next_id += 1;
            id
        };
        self.acp_send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;

        loop {
            let message = self.acp_read().await;
            if message["id"] == serde_json::json!(id) && message.get("method").is_none() {
                return message;
            }
            match message["method"].as_str() {
                Some("session/update") => {
                    self.acp_updates.push(message["params"]["update"].clone())
                }
                Some("session/request_permission") => self.acp_approve(&message).await,
                other => panic!("unexpected message from forge acp: {other:?}: {message}"),
            }
        }
    }

    async fn acp_approve(&mut self, message: &serde_json::Value) {
        self.acp_permissions += 1;
        let options = message["params"]["options"]
            .as_array()
            .unwrap_or_else(|| panic!("no options in {message}"));
        let allow = options
            .iter()
            .find(|o| o["kind"] == "allow_once")
            .unwrap_or_else(|| panic!("no allow_once option in {message}"));
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": message["id"].clone(),
            "result": { "outcome": { "outcome": "selected", "optionId": allow["optionId"] } },
        });
        self.acp_send(response).await;
    }

    async fn acp_send(&mut self, message: serde_json::Value) {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(&message).expect("serialize");
        let acp = self.acp.as_mut().expect("forge acp running");
        acp.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write to acp stdin");
        acp.stdin.flush().await.expect("flush acp stdin");
    }

    async fn acp_read(&mut self) -> serde_json::Value {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        {
            let acp = self.acp.as_mut().expect("forge acp running");
            let read = acp
                .stdout
                .read_line(&mut line)
                .await
                .expect("read acp stdout");
            assert!(read > 0, "forge acp closed stdout unexpectedly");
        }
        let line = line.trim_end().to_string();
        self.acp_lines.push(line.clone());
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("non-JSON on acp stdout: {e}: {line:?}"))
    }

    /// Set/replace a config key line (TOML `key = value`); flushed to
    /// `.forge/config.toml` by `flush_config`.
    pub fn set_config(&mut self, key: &str, value: &str) {
        let prefix = format!("{key} =");
        self.config_lines.retain(|l| !l.starts_with(&prefix));
        self.config_lines.push(format!("{prefix} {value}"));
    }

    /// Append a `[table ...]` block (replaces a block with the same
    /// header line).
    pub fn add_config_block(&mut self, block: String) {
        let header = block.lines().next().expect("block header").to_string();
        self.config_blocks.retain(|b| !b.starts_with(&header));
        self.config_blocks.push(block);
    }

    pub fn flush_config(&mut self) {
        if self.config_lines.is_empty() && self.config_blocks.is_empty() {
            return;
        }
        let mut content = self.config_lines.join("\n");
        if !self.config_blocks.is_empty() {
            content.push('\n');
            content.push_str(&self.config_blocks.join("\n\n"));
        }
        content.push('\n');
        self.write_file(".forge/config.toml", &content);
    }

    /// One session's JSONL content ("" when there is no such file).
    pub fn session_file(&mut self, session_id: &str) -> String {
        let path = self
            .project()
            .join(".forge")
            .join("sessions")
            .join(format!("{session_id}.jsonl"));
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// All session JSONL content under .forge/sessions, concatenated.
    pub fn session_log(&mut self) -> String {
        let dir = self.project().join(".forge").join("sessions");
        let mut out = String::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                out.push_str(&text);
            }
        }
        out
    }
}

impl Drop for BddWorld {
    fn drop(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = child.start_kill();
        }
        if let Some(mut mcp) = self.mcp.take() {
            let _ = mcp.child.start_kill();
        }
        if let Some(mut acp) = self.acp.take() {
            let _ = acp.child.start_kill();
        }
    }
}
