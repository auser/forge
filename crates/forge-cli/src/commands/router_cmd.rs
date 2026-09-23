use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_core::{ExecRequest, ExecutionProvider, ForgeError, RiskLevel};

use crate::commands::Context;
use crate::commands::service::build_execution;

/// The adapter script, embedded so release binaries work without a repo
/// checkout. Source of truth: `adapters/laya-http.py`.
pub const ADAPTER_SCRIPT: &str = include_str!("../../../../adapters/laya-http.py");

/// Cache location for the materialized adapter script.
fn adapter_cache_path() -> PathBuf {
    let base = if let Some(dir) = std::env::var_os("XDG_CACHE_HOME")
        && !dir.is_empty()
    {
        PathBuf::from(dir)
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache")
    };
    base.join("forge").join("laya-http.py")
}

/// Write the embedded adapter to the cache path, only when the content
/// differs. Returns true when the file was (re)written.
fn materialize_adapter(path: &Path) -> Result<bool, ForgeError> {
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == ADAPTER_SCRIPT
    {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
    }
    std::fs::write(path, ADAPTER_SCRIPT).map_err(ForgeError::Io)?;
    Ok(true)
}

/// Prerequisite checks, run through the configured ExecutionProvider
/// (all execution goes through the trait — that's the architecture rule).
async fn check_prerequisites(exec: &Arc<dyn ExecutionProvider>) -> Result<(), ForgeError> {
    let python = exec
        .execute(ExecRequest {
            command: "python3".to_string(),
            args: vec!["--version".to_string()],
            cwd: None,
            risk: RiskLevel::Safe,
            inherit_stdio: false,
        })
        .await;
    match python {
        Ok(result) if result.success() => {}
        _ => {
            return Err(ForgeError::execution(
                "python3 not found on PATH; install Python 3.10+",
            ));
        }
    }

    let import = exec
        .execute(ExecRequest {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), "import laya".to_string()],
            cwd: None,
            risk: RiskLevel::Safe,
            inherit_stdio: false,
        })
        .await;
    match import {
        Ok(result) if result.success() => Ok(()),
        _ => Err(ForgeError::execution(
            "the `laya` package is not installed; install with: pip install laya",
        )),
    }
}

/// `forge router serve [--host --port]` — run the embedded Laya adapter in
/// the foreground (like `forge serve`), blocking until Ctrl-C.
pub async fn serve(ctx: &Context, host: String, port: u16) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    let root = ctx.project_root()?;
    let exec = build_execution(&resolved.config, &root)?;

    check_prerequisites(&exec).await?;

    let script = adapter_cache_path();
    materialize_adapter(&script)?;

    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({ "listening": format!("{host}:{port}"), "adapter": script })
        );
    } else {
        println!("laya router adapter listening on http://{host}:{port} (Ctrl-C to stop)");
    }

    // Foreground: inherit stdio so the adapter's own output/lifecycle is
    // the user's; Ctrl-C reaches the whole process group.
    let result = exec
        .execute(ExecRequest {
            command: "python3".to_string(),
            args: vec![
                script.to_string_lossy().to_string(),
                port.to_string(),
                "--host".to_string(),
                host.clone(),
            ],
            cwd: None,
            risk: RiskLevel::Safe,
            inherit_stdio: true,
        })
        .await?;
    if result.success() {
        Ok(())
    } else {
        Err(ForgeError::execution(format!(
            "laya adapter exited with code {}",
            result.exit_code
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_adapter_is_present() {
        assert!(!ADAPTER_SCRIPT.is_empty());
        assert!(ADAPTER_SCRIPT.contains("laya"));
        assert!(ADAPTER_SCRIPT.contains("ThreadingHTTPServer"));
    }

    #[test]
    fn materialize_writes_only_when_content_differs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested").join("laya-http.py");
        assert!(materialize_adapter(&path).expect("first write"));
        assert!(!materialize_adapter(&path).expect("unchanged"));
        std::fs::write(&path, "stale").expect("corrupt");
        assert!(materialize_adapter(&path).expect("rewritten"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            ADAPTER_SCRIPT
        );
    }
}
