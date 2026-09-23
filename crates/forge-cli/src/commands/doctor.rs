use std::path::Path;

use forge_core::ForgeError;

use crate::commands::Context;

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

fn router_note(router: &str) -> &'static str {
    match router {
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
