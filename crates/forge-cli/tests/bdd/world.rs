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
    "FORGE_ROUTER",
    "FORGE_ROUTER_URL",
    "FORGE_ROUTER_KEY_ENV",
    "FORGE_EXECUTION",
    "FORGE_APPROVAL",
    "FORGE_LOCAL_ONLY",
    "FORGE_SERVER_HOST",
    "FORGE_SERVER_PORT",
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
    pub base_url: String,
    /// Wiremock server standing in for a System One-compatible router.
    pub router_mock: Option<wiremock::MockServer>,
    // Server scenario state.
    pub health_status: u16,
    pub health_body: String,
    pub run_id: String,
    pub run_status: u16,
    pub sse_content_type: String,
    pub sse_body: String,
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
        let root = self.project();
        let home = root.join("home");
        let xdg = root.join("xdg");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&xdg).expect("xdg");

        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge"));
        cmd.arg("--project")
            .arg(&root)
            .args(args)
            .stdin(Stdio::null())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("NO_COLOR", "1");
        for var in FORGE_ENV_VARS {
            cmd.env_remove(var);
        }
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
    }
}
