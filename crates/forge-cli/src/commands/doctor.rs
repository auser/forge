use std::path::Path;

use forge_core::ForgeError;

use crate::commands::Context;

#[derive(Debug, PartialEq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Check {
    level: Level,
    label: String,
    detail: String,
}

/// Environment and configuration health report. Exits non-zero (via
/// `ForgeError`) only when something is actually broken.
pub async fn run(ctx: &Context) -> Result<(), ForgeError> {
    let mut checks: Vec<Check> = Vec::new();

    let root = ctx.project_root()?;
    checks.push(Check {
        level: Level::Ok,
        label: "project root".into(),
        detail: root.display().to_string(),
    });

    check_config_file(
        &mut checks,
        "user config",
        &forge_config::Config::user_config_path(),
    );
    check_config_file(
        &mut checks,
        "project config",
        &forge_config::Config::project_config_path(&root),
    );

    let resolved = ctx.resolve_config();
    match &resolved {
        Ok(_) => checks.push(Check {
            level: Level::Ok,
            label: "merged config".into(),
            detail: "loads with full precedence chain".into(),
        }),
        Err(e) => checks.push(Check {
            level: Level::Fail,
            label: "merged config".into(),
            detail: e.to_string(),
        }),
    }

    let forge_dir = root.join(".forge");
    if forge_dir.is_dir() {
        let mut missing = Vec::new();
        for sub in ["graph", "sessions"] {
            if !forge_dir.join(sub).is_dir() {
                missing.push(sub);
            }
        }
        checks.push(if missing.is_empty() {
            Check {
                level: Level::Ok,
                label: ".forge/".into(),
                detail: "initialized (graph/, sessions/ present)".into(),
            }
        } else {
            Check {
                level: Level::Warn,
                label: ".forge/".into(),
                detail: format!("missing subdirectories: {}", missing.join(", ")),
            }
        });
    } else {
        checks.push(Check {
            level: Level::Warn,
            label: ".forge/".into(),
            detail: "not initialized; run `forge init`".into(),
        });
    }

    // Project graph state.
    let graph_path = root.join(".forge").join("graph").join("graph.json");
    if graph_path.is_file() {
        match forge_graph::LocalGraph::open(&root).and_then(|g| {
            let stats = g.stats();
            g.freshness().map(|f| (stats, f))
        }) {
            Ok((stats, freshness)) if freshness.fresh => checks.push(Check {
                level: Level::Ok,
                label: "project graph".into(),
                detail: format!("fresh ({} files, {} symbols)", stats.files, stats.symbols),
            }),
            Ok((_, freshness)) => checks.push(Check {
                level: Level::Warn,
                label: "project graph".into(),
                detail: format!(
                    "stale ({} added, {} modified, {} removed); run `forge graph build`",
                    freshness.added.len(),
                    freshness.modified.len(),
                    freshness.removed.len()
                ),
            }),
            Err(e) => checks.push(Check {
                level: Level::Fail,
                label: "project graph".into(),
                detail: e.to_string(),
            }),
        }
    } else if forge_dir.is_dir() {
        checks.push(Check {
            level: Level::Warn,
            label: "project graph".into(),
            detail: "not built; run `forge graph build`".into(),
        });
    }

    // Credential detection summary (sources only, never values).
    {
        let probes = forge_providers::probe_auth();
        let detected: Vec<String> = probes
            .iter()
            .filter(|p| p.detected)
            .filter_map(|p| p.source.as_ref().map(|s| format!("{} ({s})", p.provider)))
            .collect();
        let codex_note = probes.iter().any(|p| p.note.is_some());
        let (level, detail) = if !detected.is_empty() {
            let suffix = if codex_note {
                "; codex subscription OAuth unsupported (set OPENAI_API_KEY)"
            } else {
                ""
            };
            (
                Level::Ok,
                format!("{} detected{}", detected.join(", "), suffix),
            )
        } else if codex_note {
            (
                Level::Warn,
                "codex subscription OAuth detected but unsupported; set OPENAI_API_KEY".to_string(),
            )
        } else {
            (
                Level::Warn,
                "no provider credentials detected (env keys or CLI stores)".to_string(),
            )
        };
        checks.push(Check {
            level,
            label: "credentials".into(),
            detail,
        });
    }

    // Skill discovery.
    {
        use forge_core::SkillRegistry;
        let count = forge_skills::FsSkillRegistry::new(&root, None).list().len();
        checks.push(Check {
            level: Level::Ok,
            label: "skills".into(),
            detail: format!("{count} discovered"),
        });
    }

    if let Ok(resolved) = &resolved {
        let config = &resolved.config;
        // Model provider: mock/scripted are offline; anything else gets a
        // reachability probe of its endpoint (warn, never fail).
        let model_detail = match config.model.as_str() {
            "mock-local" | "mock" => Some("mock-local (built-in mock, available offline)".into()),
            "scripted-mock" => Some("scripted-mock (offline script)".into()),
            _ => None,
        };
        match model_detail {
            Some(detail) => checks.push(Check {
                level: Level::Ok,
                label: "model provider".into(),
                detail,
            }),
            None => {
                let url = config
                    .models
                    .get(&config.model)
                    .and_then(|e| e.base_url.clone())
                    .or_else(|| config.model_base_url.clone());
                let check = match url {
                    None => (
                        Level::Warn,
                        format!("{} (no base URL configured)", config.model),
                    ),
                    Some(url) => {
                        let probe = reqwest::Client::builder()
                            .timeout(std::time::Duration::from_millis(1_500))
                            .build()
                            .ok();
                        match probe {
                            Some(client) => {
                                let probe_url = format!("{}/models", url.trim_end_matches('/'));
                                match client.get(&probe_url).send().await {
                                    Ok(_) => (
                                        Level::Ok,
                                        format!("{} (reachable at {url})", config.model),
                                    ),
                                    Err(_) => (
                                        Level::Warn,
                                        format!(
                                            "{} (no OpenAI-compatible server at {url}; start oMLX or set model_base_url)",
                                            config.model
                                        ),
                                    ),
                                }
                            }
                            None => (
                                Level::Warn,
                                format!("{} (could not build probe client)", config.model),
                            ),
                        }
                    }
                };
                checks.push(Check {
                    level: check.0,
                    label: "model provider".into(),
                    detail: check.1,
                });
            }
        }
        checks.push(Check {
            level: Level::Ok,
            label: "decision router".into(),
            detail: format!("{} ({})", config.router, router_note(&config.router)),
        });

        checks.push(needle_check(config).await);

        // Reachability of http/laya routers (warn, never fail).
        if matches!(config.router.as_str(), "http" | "laya") {
            let url = config
                .router_url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:8788/decide".to_string());
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(1_500))
                .build()
                .ok();
            let check = match client {
                Some(client) => match client.get(&url).send().await {
                    Ok(_) => Some(format!("{url} is reachable")),
                    Err(e) if e.is_connect() || e.is_timeout() => None,
                    Err(_) => Some(format!("{url} responded")),
                },
                None => None,
            };
            checks.push(match check {
                Some(detail) => Check {
                    level: Level::Ok,
                    label: "router endpoint".into(),
                    detail,
                },
                None => {
                    let hint = if config.router == "laya" {
                        "; start the local adapter with `forge router serve`"
                    } else {
                        ""
                    };
                    Check {
                        level: Level::Warn,
                        label: "router endpoint".into(),
                        detail: format!("{url} unreachable; fallback routing will apply{hint}"),
                    }
                }
            });
        }
        checks.push(Check {
            level: Level::Ok,
            label: "execution provider".into(),
            detail: format!(
                "{} ({})",
                config.execution,
                execution_note(&config.execution)
            ),
        });
        match forge_core::ApprovalPolicy::parse(&config.approval) {
            Ok(policy) => checks.push(Check {
                level: Level::Ok,
                label: "approval mode".into(),
                detail: format!("{} ({policy:?})", config.approval),
            }),
            Err(e) => checks.push(Check {
                level: Level::Fail,
                label: "approval mode".into(),
                detail: e.to_string(),
            }),
        }
    }

    let mut failures = 0usize;
    let mut report: Vec<serde_json::Value> = Vec::new();
    for check in &checks {
        let tag = match check.level {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => {
                failures += 1;
                "fail"
            }
        };
        if ctx.global.json {
            report.push(serde_json::json!({
                "status": tag,
                "check": check.label,
                "detail": check.detail,
            }));
        } else {
            println!("[{tag:>4}] {}: {}", check.label, check.detail);
        }
    }

    let healthy = failures == 0;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "healthy": healthy,
                "checks": report,
            }))
            .map_err(|e| ForgeError::config(format!("serializing doctor report: {e}")))?
        );
    } else if healthy {
        println!("doctor: healthy");
    }

    if healthy {
        Ok(())
    } else {
        Err(ForgeError::config(format!(
            "doctor found {failures} failing check(s)"
        )))
    }
}

fn check_config_file(checks: &mut Vec<Check>, label: &str, path: &Path) {
    if !path.exists() {
        checks.push(Check {
            level: Level::Ok,
            label: label.into(),
            detail: format!("{} (absent, defaults/env apply)", path.display()),
        });
        return;
    }
    match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| text.parse::<toml::Table>().map_err(|e| e.to_string()))
    {
        Ok(_) => checks.push(Check {
            level: Level::Ok,
            label: label.into(),
            detail: format!("{} (parses)", path.display()),
        }),
        Err(e) => checks.push(Check {
            level: Level::Fail,
            label: label.into(),
            detail: format!("{} ({e})", path.display()),
        }),
    }
}

/// Probe the embedded Needle brain: only meaningful when `router = "needle"`
/// (else purely informational). Never returns `Level::Fail` — a broken or
/// missing brain degrades to the configured static fallback, so forge stays
/// usable either way; this check exists to surface *why* it degraded.
///
/// Filesystem/checksum only: this never fetches weights over the network
/// (that's `forge init`'s job), so `forge doctor` stays fast and offline.
async fn needle_check(config: &forge_config::Config) -> Check {
    const LABEL: &str = "needle brain";

    if config.router != "needle" {
        return Check {
            level: Level::Ok,
            label: LABEL.into(),
            detail: "not the active router".into(),
        };
    }

    let using_hash_backend = std::env::var("FORGE_NEEDLE_BACKEND").as_deref() == Ok("hash");
    // Set once this function's own checksum step confirms weights are
    // present and verified on disk — used below to give an honest message
    // when `engine_from_config` still fails with a weights-missing-shaped
    // error (today it always does: no `ffi` backend exists yet, so it
    // always spawns `UnavailableBackend`, whose `load()` always reports
    // `WeightsMissing` regardless of what's actually on disk). Without this
    // flag the probe would print a `forge init` hint that's actively wrong
    // — the weights *are* fetched and verified; the binary just can't load
    // them yet.
    let mut weights_verified = false;

    if !using_hash_backend {
        let path = match forge_needle::weights_path(&config.needle) {
            Ok(path) => path,
            Err(e) => {
                // Typically an unpinned variant (e.g. "small"/"medium") —
                // name the situation rather than pretending it's fixable
                // with `forge init`.
                return Check {
                    level: Level::Warn,
                    label: LABEL.into(),
                    detail: format!("{e} (falling back to {} routing)", config.router_fallback),
                };
            }
        };
        if !path.is_file() {
            return Check {
                level: Level::Warn,
                label: LABEL.into(),
                detail: format!(
                    "weights missing at {}; run `forge init` to fetch them",
                    path.display()
                ),
            };
        }
        let expected_sha256 = if !config.needle.weights_sha256.trim().is_empty() {
            config.needle.weights_sha256.clone()
        } else {
            match forge_needle::spec_for(&config.needle.variant) {
                Ok(spec) => spec.sha256.to_string(),
                Err(e) => {
                    return Check {
                        level: Level::Warn,
                        label: LABEL.into(),
                        detail: e.to_string(),
                    };
                }
            }
        };
        match forge_needle::verify(&path, &expected_sha256) {
            Ok(true) => weights_verified = true,
            Ok(false) => {
                return Check {
                    level: Level::Warn,
                    label: LABEL.into(),
                    detail: format!(
                        "weights at {} failed checksum verification; run `forge init` to refetch them",
                        path.display()
                    ),
                };
            }
            Err(e) => {
                return Check {
                    level: Level::Warn,
                    label: LABEL.into(),
                    detail: format!(
                        "could not verify weights at {}: {e}; run `forge init`",
                        path.display()
                    ),
                };
            }
        }
    }

    let engine_result = if using_hash_backend {
        Ok(forge_needle::NeedleEngine::spawn(
            forge_needle::HashBackend::new(),
        ))
    } else {
        forge_needle::engine_from_config(&config.needle)
    };
    let engine = match engine_result {
        Ok(engine) => engine,
        Err(e) => {
            return Check {
                level: Level::Warn,
                label: LABEL.into(),
                detail: format!("engine unavailable: {e}"),
            };
        }
    };

    let timeout = std::time::Duration::from_millis(config.router_timeout_ms);
    let started = std::time::Instant::now();
    let decide_result = tokio::time::timeout(
        timeout,
        engine.decide("doctor smoke test".to_string(), vec!["ok".to_string()]),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis();

    match decide_result {
        Ok(Ok(_decision)) => match engine.info().await {
            Ok((model_id, _dims)) => Check {
                level: Level::Ok,
                label: LABEL.into(),
                detail: format!("ok (model {model_id}, decide {elapsed_ms} ms)"),
            },
            Err(e) => Check {
                level: Level::Warn,
                label: LABEL.into(),
                detail: format!("decide succeeded but model info failed: {e}"),
            },
        },
        Ok(Err(e)) if weights_verified && is_weights_missing_shaped(&e) => Check {
            // This function's own checksum check above just confirmed the
            // weights ARE present and verified — a `forge init` hint here
            // would be actively wrong. What's actually true: no `ffi`
            // backend is built into this binary yet (Task 8), so
            // `engine_from_config` always yields a stub that can't load
            // any weights, verified or not.
            level: Level::Warn,
            label: LABEL.into(),
            detail: format!(
                "weights present and verified, but the embedded inference backend is not built into this binary yet; falls back to {} routing",
                config.router_fallback
            ),
        },
        Ok(Err(e)) => Check {
            level: Level::Warn,
            label: LABEL.into(),
            detail: format!("decide failed: {e}"),
        },
        Err(_) => Check {
            level: Level::Warn,
            label: LABEL.into(),
            detail: format!("decide timed out after {} ms", timeout.as_millis()),
        },
    }
}

/// `NeedleEngine::decide`'s error is a stringly-typed `ForgeError::Router`
/// by the time it reaches doctor — the engine layer collapses
/// `BackendError` into a message rather than preserving the variant. This
/// matches on the exact wording `BackendError::WeightsMissing`'s `Display`
/// impl produces (`forge-needle/src/backend.rs`) so `needle_check` can tell
/// "no ffi backend built in" apart from a genuine inference failure.
fn is_weights_missing_shaped(err: &ForgeError) -> bool {
    err.to_string().contains("weights missing at")
}

fn router_note(router: &str) -> &'static str {
    match router {
        "needle" => "embedded on-device Needle 3 decisions, available offline",
        "static" => "deterministic rules, available offline",
        "mock" => "deterministic mock, available offline",
        "cheapest" => "lowest-cost capable candidate, available offline",
        "http" => "System One-compatible HTTP router (uses router_url)",
        "laya" => "Laya typed-questions router (uses router_url, default 127.0.0.1:8788)",
        _ => "unrecognized router name",
    }
}

fn execution_note(execution: &str) -> &'static str {
    match execution {
        "native" => "local process execution, available",
        "mock" => "recorded mock execution, available offline",
        _ => "unrecognized execution provider",
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[tokio::test]
    #[serial]
    async fn needle_check_when_router_is_not_needle_is_ok_and_informational() {
        let config = forge_config::Config {
            router: "static".to_string(),
            ..forge_config::Config::default()
        };
        let check = needle_check(&config).await;
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("not the active router"));
    }

    #[tokio::test]
    #[serial]
    async fn needle_check_reports_missing_weights_as_warn_not_fail() {
        let mut config = forge_config::Config::default();
        config.needle.weights_path = "/nonexistent/needle.bin".to_string();
        let check = needle_check(&config).await;
        assert_eq!(check.level, Level::Warn);
        assert!(check.detail.contains("forge init"));
    }

    #[tokio::test]
    #[serial]
    async fn needle_check_gives_honest_message_when_weights_verified_but_no_ffi_backend() {
        // Weights genuinely present and checksum-verified on disk, but
        // `engine_from_config` still can't load them (no `ffi` backend
        // built into this binary until Task 8). The probe must not blame
        // this on missing weights or suggest `forge init` — that would be
        // actively wrong, since the checksum step just succeeded.
        use sha2::{Digest, Sha256};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weights.bin");
        let bytes = b"arbitrary-bytes-standing-in-for-real-needle-weights";
        std::fs::write(&path, bytes).expect("write fake weights");
        let sha256 = format!("{:x}", Sha256::digest(bytes));

        let config = forge_config::Config {
            needle: forge_config::NeedleConfig {
                variant: "full".to_string(),
                weights_path: path.display().to_string(),
                autofetch: true,
                // Operator-supplied override bypasses the pinned-spec
                // checksum lookup entirely, so an arbitrary payload can
                // verify cleanly.
                weights_sha256: sha256,
            },
            ..forge_config::Config::default()
        };

        let check = needle_check(&config).await;

        assert_eq!(check.level, Level::Warn);
        assert!(
            !check.detail.contains("forge init"),
            "must not suggest `forge init` once weights are already verified: {}",
            check.detail
        );
        assert!(
            check.detail.contains("not built into this binary"),
            "detail: {}",
            check.detail
        );
    }

    #[tokio::test]
    #[serial]
    async fn needle_check_warns_for_unpinned_variant_without_panicking() {
        // "medium" is config-valid but has no pinned artifact yet (see
        // forge-needle's weights module doc) — must degrade to Warn, never
        // panic or Fail.
        let mut config = forge_config::Config::default();
        config.needle.variant = "medium".to_string();
        let check = needle_check(&config).await;
        assert_eq!(check.level, Level::Warn);
        assert!(check.detail.contains("medium"), "detail: {}", check.detail);
    }

    #[tokio::test]
    #[serial]
    async fn needle_check_with_hash_backend_reports_ok_and_latency() {
        // SAFETY: test-only env mutation, serialized via #[serial] against
        // any other test touching FORGE_NEEDLE_BACKEND in this crate.
        unsafe {
            std::env::set_var("FORGE_NEEDLE_BACKEND", "hash");
        }
        let check = needle_check(&forge_config::Config::default()).await;
        unsafe {
            std::env::remove_var("FORGE_NEEDLE_BACKEND");
        }
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("ms")); // measured decide() latency
    }
}
