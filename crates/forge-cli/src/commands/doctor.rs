use std::path::Path;

use forge_core::ForgeError;

use crate::commands::Context;

#[derive(Debug, PartialEq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    /// Wire tag, shared by the human output, `--json` and the MCP tool.
    fn tag(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

pub struct Check {
    level: Level,
    label: String,
    detail: String,
}

/// Every environment/configuration check, in report order.
///
/// This is the one definition of forge's health: `forge doctor` renders it
/// and the MCP `forge_doctor` tool serves the same values (via
/// [`crate::commands::mcp_cmd::CliDiagnostics`]) — never by shelling out to
/// the CLI.
pub async fn collect_checks(ctx: &Context) -> Result<Vec<Check>, ForgeError> {
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
        checks.push(jev_check(config));
        checks.extend(credential_env_checks(config));
        if let Some(check) = legacy_router_check(config) {
            checks.push(check);
        }

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

    Ok(checks)
}

/// How many checks are outright broken (warnings do not count).
pub fn failures(checks: &[Check]) -> usize {
    checks.iter().filter(|c| c.level == Level::Fail).count()
}

/// The machine-readable report — exactly what `forge doctor --json` prints
/// and what the MCP `forge_doctor` tool returns.
pub fn report_json(checks: &[Check]) -> serde_json::Value {
    serde_json::json!({
        "healthy": failures(checks) == 0,
        "checks": checks
            .iter()
            .map(|check| serde_json::json!({
                "status": check.level.tag(),
                "check": check.label,
                "detail": check.detail,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Environment and configuration health report. Exits non-zero (via
/// `ForgeError`) only when something is actually broken.
pub async fn run(ctx: &Context) -> Result<(), ForgeError> {
    let checks = collect_checks(ctx).await?;
    let failures = failures(&checks);
    let healthy = failures == 0;

    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(&checks))
                .map_err(|e| ForgeError::config(format!("serializing doctor report: {e}")))?
        );
    } else {
        for check in &checks {
            println!(
                "[{:>4}] {}: {}",
                check.level.tag(),
                check.label,
                check.detail
            );
        }
        if healthy {
            println!("doctor: healthy");
        }
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
    // The probe has to be a task that genuinely maps to one of the options.
    // Needle refuses to guess by design: asked to choose "ok" for a "doctor
    // smoke test" it declines — correctly — and the probe then reports a
    // healthy brain as broken. So ask something real (this mirrors the
    // options a routing decision actually sees) and only check that a
    // decision came back at all, not which one.
    let decide_result = tokio::time::timeout(
        timeout,
        engine.decide(
            "run the project's test suite".to_string(),
            vec!["test-runner".to_string(), "chat-model".to_string()],
        ),
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
            // would be actively wrong. What's actually true: this binary was
            // built without the `ffi` inference backend, so
            // `engine_from_config` yields `UnavailableBackend`, which cannot
            // load any weights, verified or not.
            level: Level::Warn,
            label: LABEL.into(),
            detail: format!(
                "weights present and verified, but this binary was built without the embedded \
                 inference backend (rebuild with `--features needle-ffi`); falls back to {} routing",
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

/// Probe the Jev escalation tier: credential + endpoint only, no network
/// probe (consistent with `needle_check` — this stays fast and offline).
/// Meaningful whenever jev could actually be contacted: as the primary
/// router (`router = "jev"`), or as needle's escalation tier
/// (`router = "needle"`, `router_escalate = "auto"`). Never returns
/// `Level::Fail` — an absent credential just means jev stays out of the
/// stack, which `router_from_config` already handles gracefully.
fn jev_check(config: &forge_config::Config) -> Check {
    const LABEL: &str = "jev";
    let is_primary = config.router == "jev";
    let escalation_configured = config.router == "needle" && config.router_escalate == "auto";

    if !is_primary && !escalation_configured {
        return Check {
            level: Level::Ok,
            label: LABEL.into(),
            detail: "not active (router is neither \"jev\" nor escalating from \"needle\")".into(),
        };
    }

    if config.local_only {
        // `--local-only` prunes jev in both roles at construction time
        // (see `router_from_config`); this mirrors why it never even gets
        // asked about a credential. Primary always degrades straight to
        // "static" (a literal, hardcoded target in `router_from_config`,
        // not `router_fallback` — name the actual behavior, not a
        // plausible-looking but wrong guess).
        let detail = if is_primary {
            "not active (--local-only forces static routing)".to_string()
        } else {
            "escalation disabled (--local-only)".to_string()
        };
        return Check {
            level: Level::Ok,
            label: LABEL.into(),
            detail,
        };
    }

    // Role-scoped resolution, shared with `router_from_config` so this
    // check can never drift from what the router actually does: escalation
    // never falls back to the generic `router_url`/`router_key_env` (which
    // could belong to an unrelated http/laya setup), but the primary role
    // does, for backwards compatibility.
    let escalation = !is_primary;
    let key_env = forge_providers::resolved_jev_key_env(config, escalation);
    let endpoint = forge_providers::resolved_jev_url(config, escalation)
        .unwrap_or_else(|| forge_providers::JevRouter::DEFAULT_URL.to_string());
    let key_present = forge_providers::jev_credential_present(config, escalation);

    if key_present {
        Check {
            level: Level::Ok,
            label: LABEL.into(),
            detail: format!("credential detected ({key_env}); endpoint {endpoint}"),
        }
    } else if is_primary {
        Check {
            level: Level::Warn,
            label: LABEL.into(),
            detail: format!(
                "no credential ({key_env}); requests to {endpoint} will fail (falls back to {})",
                config.router_fallback
            ),
        }
    } else {
        Check {
            level: Level::Ok,
            label: LABEL.into(),
            detail: format!("jev escalation: no credential ({key_env}) — on-device only"),
        }
    }
}

/// Every credential env var the *effective* configuration actually names,
/// paired with the config key that named it. Only roles that can really be
/// contacted are included: the active model's `key_env` (or the global
/// `model_key_env`), and `router_key_env` when an `http`/`laya` router uses
/// it. Names only — no value ever leaves this function.
///
/// **One owner per root cause.** Anything in a jev role — `router = "jev"`
/// (which also resolves `router_key_env`) and needle's escalation tier — is
/// [`jev_check`]'s alone: it already reports that credential with the
/// endpoint and the `--local-only`/escalation semantics attached, and a
/// second warn about the same unset variable is exactly the noise this
/// check exists to remove.
fn named_credential_envs(config: &forge_config::Config) -> Vec<(&'static str, String)> {
    let mut named: Vec<(&'static str, String)> = Vec::new();

    // The active model's env var: the entry's `key_env` wins over the
    // global `model_key_env`, exactly as `model_from_config` resolves it.
    // Mocks never authenticate, so naming a var for them is meaningless.
    if !matches!(
        config.model.as_str(),
        "mock" | "mock-local" | "scripted-mock"
    ) {
        let entry_key_env = config
            .models
            .get(&config.model)
            .and_then(|e| e.key_env.clone());
        match entry_key_env {
            Some(name) => named.push(("[models] key_env", name)),
            None => {
                if let Some(name) = config.model_key_env.clone() {
                    named.push(("model_key_env", name));
                }
            }
        }
    }

    // `router = "jev"` deliberately absent: that role's credential (which
    // may itself be `router_key_env`) belongs to `jev_check`.
    if matches!(config.router.as_str(), "http" | "laya")
        && let Some(name) = config.router_key_env.clone()
    {
        named.push(("router_key_env", name));
    }
    named
}

/// Config-vs-environment mismatch: a credential env var the configuration
/// names but the environment does not provide. This is the exact shape of
/// the reported first-run failure — `model_key_env = "OMLX_API_KEY"` with
/// `OMLX_API_KEY` unset — where every individual check passed and nothing
/// said which two edits would fix it. Warn, never Fail: an unauthenticated
/// request may well succeed against a local server.
fn credential_env_checks(config: &forge_config::Config) -> Vec<Check> {
    named_credential_envs(config)
        .into_iter()
        .map(|(config_key, env_name)| {
            let set = std::env::var(&env_name).is_ok_and(|v| !v.trim().is_empty());
            if set {
                Check {
                    level: Level::Ok,
                    label: "credential env".into(),
                    detail: format!("{config_key} names {env_name} and it is set"),
                }
            } else {
                // Both halves, for every key: a self-hosted endpoint can be
                // keyless whether it serves models (oMLX with auth off) or
                // routing decisions (a local laya adapter, a self-hosted
                // OpenJev), so "delete the line" is a real fix in every
                // case, not just for model credentials.
                Check {
                    level: Level::Warn,
                    label: "credential env".into(),
                    detail: format!(
                        "{config_key} names {env_name} but it is not set — \
                         export {env_name}=... or remove {config_key} if the \
                         endpoint needs no key"
                    ),
                }
            }
        })
        .collect()
}

/// Settings that used to be right and now silently cost the user the
/// built-in default. Config-only (no network): the `router endpoint` check
/// above already probes reachability when it applies.
fn legacy_router_check(config: &forge_config::Config) -> Option<Check> {
    if config.router != "laya" {
        return None;
    }
    Some(Check {
        level: Level::Warn,
        label: "legacy config".into(),
        detail: "laya is no longer the default; the embedded needle brain is \
                 (delete the router line to use it) — keep laya by running: \
                 forge router serve"
            .into(),
    })
}

fn router_note(router: &str) -> &'static str {
    match router {
        "needle" => "embedded on-device Needle 3 decisions, available offline",
        "static" => "deterministic rules, available offline",
        "mock" => "deterministic mock, available offline",
        "cheapest" => "lowest-cost capable candidate, available offline",
        "http" => "System One-compatible HTTP router (uses router_url)",
        "laya" => "Laya typed-questions router (uses router_url, default 127.0.0.1:8788)",
        "jev" => {
            "Jev/OpenJev System One router (TYPESAFE_API_KEY or router_url for self-hosted OpenJev)"
        }
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
            check
                .detail
                .contains("without the embedded inference backend"),
            "detail: {}",
            check.detail
        );
        // The message has to tell the operator how to fix it, which is a
        // rebuild with the feature — not a refetch.
        assert!(
            check.detail.contains("needle-ffi"),
            "should name the feature that turns the backend on: {}",
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

    // --- jev ---

    #[test]
    #[serial]
    fn jev_check_inactive_when_router_is_neither_jev_nor_escalating() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        let config = forge_config::Config {
            router: "static".to_string(),
            ..forge_config::Config::default()
        };
        let check = jev_check(&config);
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("not active"), "{}", check.detail);
    }

    #[test]
    #[serial]
    fn jev_check_reports_credential_detected_for_escalation() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "dummy-value-for-test") };
        let config = forge_config::Config::default(); // router = "needle", escalate = "auto"
        let check = jev_check(&config);
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert_eq!(check.level, Level::Ok);
        assert!(
            check
                .detail
                .contains("credential detected (TYPESAFE_API_KEY)"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("api.typesafe.ai"), "{}", check.detail);
    }

    #[test]
    #[serial]
    fn jev_check_reports_informational_message_when_escalation_has_no_credential() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        let config = forge_config::Config::default(); // router = "needle", escalate = "auto"
        let check = jev_check(&config);
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            check.detail,
            "jev escalation: no credential (TYPESAFE_API_KEY) — on-device only"
        );
    }

    #[test]
    #[serial]
    fn jev_check_warns_when_primary_router_has_no_credential() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        let config = forge_config::Config {
            router: "jev".to_string(),
            ..forge_config::Config::default()
        };
        let check = jev_check(&config);
        assert_eq!(check.level, Level::Warn);
        assert!(check.detail.contains("no credential"), "{}", check.detail);
        assert!(check.detail.contains("static"), "{}", check.detail);
    }

    #[test]
    #[serial]
    fn jev_check_local_only_prunes_both_roles() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "dummy-value-for-test") };

        let escalation = forge_config::Config {
            local_only: true,
            ..forge_config::Config::default() // router = "needle", escalate = "auto"
        };
        let check = jev_check(&escalation);
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("--local-only"), "{}", check.detail);

        // `router_fallback` deliberately set to something other than
        // "static": `router_from_config` always forces "static" under
        // --local-only for a jev primary (a literal, not `router_fallback`
        // substituted in) — the message must say "static", not "cheapest",
        // or it would describe behavior that never happens.
        let primary = forge_config::Config {
            router: "jev".to_string(),
            router_fallback: "cheapest".to_string(),
            local_only: true,
            ..forge_config::Config::default()
        };
        let check = jev_check(&primary);
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("--local-only"), "{}", check.detail);
        assert!(
            check.detail.contains("static") && !check.detail.contains("cheapest"),
            "must name the router_from_config's actual hardcoded fallback (static), not \
             router_fallback: {}",
            check.detail
        );
    }

    #[test]
    #[serial]
    fn jev_check_primary_falls_back_to_generic_router_key_env_and_router_url() {
        // Primary role backwards-compat: with no jev_url/jev_key_env set,
        // `router = "jev"` reuses the generic fields, same as http/laya do.
        unsafe { std::env::set_var("MY_JEV_KEY", "dummy-value-for-test") };
        let config = forge_config::Config {
            router: "jev".to_string(),
            router_key_env: Some("MY_JEV_KEY".to_string()),
            router_url: Some("https://openjev.example.internal/v1/systemone".to_string()),
            ..forge_config::Config::default()
        };
        let check = jev_check(&config);
        unsafe { std::env::remove_var("MY_JEV_KEY") };
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("MY_JEV_KEY"), "{}", check.detail);
        assert!(
            check.detail.contains("openjev.example.internal"),
            "{}",
            check.detail
        );
    }

    #[test]
    #[serial]
    fn jev_check_escalation_ignores_generic_router_key_env_and_router_url() {
        // Escalation role must NOT fall back to router_url/router_key_env
        // (a leftover setting from an unrelated http/laya config): with no
        // jev_url/jev_key_env, escalation reports the compiled-in defaults
        // regardless of what router_url/router_key_env say.
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        let config = forge_config::Config {
            router: "needle".to_string(),
            router_key_env: Some("SOME_OTHER_ROUTERS_KEY".to_string()),
            router_url: Some("https://poisoned.example.internal/route".to_string()),
            ..forge_config::Config::default() // escalate = "auto"
        };
        let check = jev_check(&config);
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            check.detail,
            "jev escalation: no credential (TYPESAFE_API_KEY) — on-device only"
        );
        assert!(
            !check.detail.contains("poisoned.example.internal")
                && !check.detail.contains("SOME_OTHER_ROUTERS_KEY"),
            "escalation must never surface the generic router_url/router_key_env: {}",
            check.detail
        );
    }

    // --- credential env mismatches ---

    /// The reported failure, verbatim shape: `model_key_env` names a var
    /// the shell does not have. The message has to carry both fixes.
    #[test]
    #[serial]
    fn credential_env_warns_when_model_key_env_is_unset() {
        unsafe { std::env::remove_var("OMLX_API_KEY") };
        let config = forge_config::Config {
            model: "Qwen3-Coder-Next-4bit".to_string(),
            model_key_env: Some("OMLX_API_KEY".to_string()),
            ..forge_config::Config::default()
        };
        let checks = credential_env_checks(&config);
        assert_eq!(checks.len(), 1, "{:?}", checks[0].detail);
        assert_eq!(checks[0].level, Level::Warn);
        assert_eq!(
            checks[0].detail,
            "model_key_env names OMLX_API_KEY but it is not set — \
             export OMLX_API_KEY=... or remove model_key_env if the endpoint needs no key"
        );
    }

    #[test]
    #[serial]
    fn credential_env_is_ok_when_the_named_var_is_set() {
        unsafe { std::env::set_var("OMLX_API_KEY", "dummy-value-for-test") };
        let config = forge_config::Config {
            model: "Qwen3-Coder-Next-4bit".to_string(),
            model_key_env: Some("OMLX_API_KEY".to_string()),
            ..forge_config::Config::default()
        };
        let checks = credential_env_checks(&config);
        unsafe { std::env::remove_var("OMLX_API_KEY") };
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].level, Level::Ok);
        assert!(
            checks[0].detail.contains("it is set"),
            "{}",
            checks[0].detail
        );
        // Never the value, only the name.
        assert!(!checks[0].detail.contains("dummy-value-for-test"));
    }

    /// An empty value is as broken as an unset one (`export FOO=` is a
    /// common way to end up here) and must not read as healthy.
    #[test]
    #[serial]
    fn credential_env_treats_empty_value_as_unset() {
        unsafe { std::env::set_var("OMLX_API_KEY", "   ") };
        let config = forge_config::Config {
            model_key_env: Some("OMLX_API_KEY".to_string()),
            ..forge_config::Config::default()
        };
        let checks = credential_env_checks(&config);
        unsafe { std::env::remove_var("OMLX_API_KEY") };
        assert_eq!(checks[0].level, Level::Warn);
    }

    /// The active `[models]` entry's own `key_env` wins over the global
    /// `model_key_env` — same resolution order `model_from_config` uses, so
    /// doctor reports the var that will actually be read.
    #[test]
    #[serial]
    fn credential_env_prefers_the_active_model_entrys_key_env() {
        unsafe { std::env::remove_var("ENTRY_KEY") };
        unsafe { std::env::remove_var("GLOBAL_KEY") };
        let mut config = forge_config::Config {
            model: "deepseek-chat".to_string(),
            model_key_env: Some("GLOBAL_KEY".to_string()),
            ..forge_config::Config::default()
        };
        if let Some(entry) = config.models.get_mut("deepseek-chat") {
            entry.key_env = Some("ENTRY_KEY".to_string());
        }
        let checks = credential_env_checks(&config);
        assert_eq!(checks.len(), 1, "{:?}", checks[0].detail);
        assert!(
            checks[0].detail.contains("ENTRY_KEY"),
            "{}",
            checks[0].detail
        );
        assert!(
            !checks[0].detail.contains("GLOBAL_KEY"),
            "{}",
            checks[0].detail
        );
    }

    /// Only *active* roles are reported: a leftover `router_key_env` from
    /// an http/laya setup names no credential the default needle stack will
    /// ever read, so warning about it would be noise.
    #[test]
    #[serial]
    fn credential_env_ignores_router_key_env_when_that_router_is_inactive() {
        unsafe { std::env::remove_var("LEFTOVER_ROUTER_KEY") };
        let config = forge_config::Config {
            router: "needle".to_string(),
            router_key_env: Some("LEFTOVER_ROUTER_KEY".to_string()),
            ..forge_config::Config::default()
        };
        assert!(credential_env_checks(&config).is_empty());

        // ...and reports it once that router is the active one.
        let config = forge_config::Config {
            router: "laya".to_string(),
            router_key_env: Some("LEFTOVER_ROUTER_KEY".to_string()),
            ..forge_config::Config::default()
        };
        let checks = credential_env_checks(&config);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].level, Level::Warn);
        // A self-hosted laya adapter can be keyless too, so the router key
        // gets the same "or delete the line" alternative as a model key.
        assert_eq!(
            checks[0].detail,
            "router_key_env names LEFTOVER_ROUTER_KEY but it is not set — \
             export LEFTOVER_ROUTER_KEY=... or remove router_key_env if the endpoint needs no key"
        );
    }

    /// One owner per root cause: `jev_check` reports jev's credential in
    /// both roles (with endpoint and escalation semantics attached), so
    /// `credential_env_checks` must stay out of it — otherwise one unset
    /// variable produces two warns, which is the noise this batch fights.
    #[test]
    #[serial]
    fn jev_credentials_are_reported_once_by_jev_check_only() {
        unsafe { std::env::remove_var("MY_JEV_KEY") };

        // Primary role: jev_check warns, and it is the *only* warn.
        let primary = forge_config::Config {
            router: "jev".to_string(),
            jev_key_env: Some("MY_JEV_KEY".to_string()),
            // A primary jev also resolves `router_key_env`; that is
            // jev_check's business too, so it must not double up here.
            router_key_env: Some("MY_JEV_KEY".to_string()),
            ..forge_config::Config::default()
        };
        let mut checks = vec![jev_check(&primary)];
        checks.extend(credential_env_checks(&primary));
        let warns: Vec<&Check> = checks
            .iter()
            .filter(|c| c.level == Level::Warn && c.detail.contains("MY_JEV_KEY"))
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "exactly one warn per root cause: {:?}",
            checks.iter().map(|c| &c.detail).collect::<Vec<_>>()
        );
        assert_eq!(warns[0].label, "jev");

        // Escalation role: jev_check reports it informationally (Ok), and
        // credential_env_checks still adds nothing.
        let escalating = forge_config::Config {
            jev_key_env: Some("MY_JEV_KEY".to_string()),
            ..forge_config::Config::default() // router = needle, escalate = auto
        };
        assert_eq!(jev_check(&escalating).level, Level::Ok);
        assert!(
            credential_env_checks(&escalating).is_empty(),
            "jev's credential is jev_check's to report, in either role"
        );
    }

    /// Mocks never authenticate: naming a credential var for them is not a
    /// mismatch worth a warning.
    #[test]
    #[serial]
    fn credential_env_skips_mock_models() {
        unsafe { std::env::remove_var("OMLX_API_KEY") };
        let config = forge_config::Config {
            model: "mock-local".to_string(),
            model_key_env: Some("OMLX_API_KEY".to_string()),
            ..forge_config::Config::default()
        };
        assert!(credential_env_checks(&config).is_empty());
    }

    // --- legacy router setting ---

    #[test]
    fn legacy_router_check_flags_configured_laya_with_both_fixes() {
        let config = forge_config::Config {
            router: "laya".to_string(),
            ..forge_config::Config::default()
        };
        let check = legacy_router_check(&config).expect("laya is flagged");
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.detail.contains("no longer the default"),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains("delete the router line"),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains("forge router serve"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn legacy_router_check_is_silent_for_every_current_router() {
        for router in ["needle", "static", "cheapest", "mock", "http", "jev"] {
            let config = forge_config::Config {
                router: router.to_string(),
                ..forge_config::Config::default()
            };
            assert!(
                legacy_router_check(&config).is_none(),
                "{router} must not be flagged as legacy"
            );
        }
    }

    #[test]
    #[serial]
    fn jev_check_honors_jev_url_and_jev_key_env_for_escalation() {
        unsafe { std::env::set_var("MY_JEV_KEY", "dummy-value-for-test") };
        let config = forge_config::Config {
            router: "needle".to_string(),
            jev_key_env: Some("MY_JEV_KEY".to_string()),
            jev_url: Some("https://openjev.example.internal/v1/systemone".to_string()),
            // A poisoned generic router_url must be ignored too.
            router_url: Some("https://poisoned.example.internal/route".to_string()),
            ..forge_config::Config::default() // escalate = "auto"
        };
        let check = jev_check(&config);
        unsafe { std::env::remove_var("MY_JEV_KEY") };
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("MY_JEV_KEY"), "{}", check.detail);
        assert!(
            check.detail.contains("openjev.example.internal"),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains("poisoned.example.internal"));
    }
}
