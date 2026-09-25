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

    // Independent of what is configured: an exported FORGE_TEST_MOCKS
    // makes every mock selectable for the whole shell, long after whatever
    // test run needed it. Say so once, always.
    if forge_config::test_mocks_allowed() {
        checks.push(Check {
            level: Level::Warn,
            label: "test mocks".into(),
            detail: format!(
                "{}=1 is set — test-only mock providers are selectable in this environment",
                forge_config::TEST_MOCKS_ENV
            ),
        });
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
        if let Some(check) = local_only_check(config) {
            checks.push(check);
        }
        // Model provider: a test-only mock is reported as whatever it
        // actually is right now — usable under the gate, broken without it
        // (every run would fail at provider construction, and "why does
        // forge say mock response to:" is exactly the confusion the gate
        // exists to prevent). Anything else gets a reachability probe of
        // its endpoint (warn, never fail).
        // One list of mock names, owned by the crate that enforces the gate.
        let mock_model = forge_providers::is_mock_model(&config.model);
        let model_detail = if mock_model {
            Some(if forge_config::test_mocks_allowed() {
                (
                    Level::Warn,
                    format!(
                        "{} is a test-only mock, unlocked by {}",
                        config.model,
                        forge_config::TEST_MOCKS_ENV
                    ),
                )
            } else {
                (
                    Level::Fail,
                    format!(
                        "{} is a test-only mock and will not load; pick a real model \
                         (see the README's \"Pick your model\"), or set {}=1 if you \
                         are running forge's own tests",
                        config.model,
                        forge_config::TEST_MOCKS_ENV
                    ),
                )
            })
        } else {
            None
        };
        match model_detail {
            Some((level, detail)) => checks.push(Check {
                level,
                label: "model provider".into(),
                detail,
            }),
            None => {
                // The endpoint the provider will actually dial, straight
                // from `forge-providers`, so this check can never drift from
                // the resolution order a run uses.
                let url = forge_providers::model_endpoint(config);
                let check = match url {
                    // `local_only` refuses this provider at construction, so
                    // every run fails: Fail, and no network probe — reaching
                    // out to the very host the setting forbids would be the
                    // check contradicting the guarantee it reports on.
                    // The provider crate refuses this at construction, so
                    // every run fails: Fail, and no probe — reaching out to
                    // the host the setting forbids would be this check
                    // contradicting the guarantee it reports on. The wording
                    // comes from the same place as the refusal so doctor
                    // cannot give a different instruction than the error
                    // does.
                    Some(url) if local_only_refuses(config, &url) => (
                        Level::Fail,
                        // The provider crate's own wording, so doctor and the
                        // run cannot give different instructions. Its
                        // `ForgeError` type prefix is dropped: the label and
                        // the Fail level already say what kind of problem this
                        // is, and "will not load: configuration error: …"
                        // reads as two labels for one thing.
                        forge_providers::model_from_config(config, &root)
                            .err()
                            .map(|e| strip_error_kind(&e.to_string()))
                            .unwrap_or_else(|| {
                                format!("local_only is set and {url} is not a local endpoint")
                            }),
                    ),
                    None => (
                        Level::Warn,
                        format!("{} (no base URL configured)", config.model),
                    ),
                    Some(url) => {
                        let probe = forge_providers::EgressPolicy::from_config(config)
                            .client(std::time::Duration::from_millis(1_500))
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
        // The router that will actually run, not the one the file names: under
        // `local_only` a refused router degrades to `static`, and reporting the
        // configured name here while the next line says "local_only forces
        // static routing" is one report giving two answers.
        let effective_router = forge_providers::effective_router_name(&config.router, config);
        let router_detail = if effective_router == config.router {
            router_note(&effective_router).to_string()
        } else {
            format!(
                "{} — local_only replaced router = {:?}",
                router_note(&effective_router),
                config.router
            )
        };
        checks.push(mock_aware_check(
            "decision router",
            &effective_router,
            effective_router == "mock",
            &router_detail,
        ));

        checks.extend(needle_checks(config).await);
        checks.push(jev_check(config));
        checks.extend(credential_env_checks(config));
        if let Some(check) = legacy_router_check(config) {
            checks.push(check);
        }

        // Reachability of the active router's endpoint (warn, never fail),
        // resolved by `forge-providers` so this cannot drift from the URL the
        // router will dial — and `None` for a router that dials nothing, or
        // for `http` with no `router_url` (which cannot be built at all, so
        // probing laya's default in its place would just be wrong).
        if let Some(url) = forge_providers::router_endpoint(&config.router, config) {
            // A router `local_only` prunes is never contacted at all (see
            // `router_from_config`), so probing it would be both pointless
            // and a request to exactly the host the setting forbids.
            if local_only_refuses(config, &url) {
                checks.push(Check {
                    level: Level::Warn,
                    label: "router endpoint".into(),
                    detail: format!(
                        "{url} is not a local endpoint; local_only forces static routing \
                         instead of router = {:?}",
                        config.router
                    ),
                });
            } else {
                let client = forge_providers::EgressPolicy::from_config(config)
                    .client(std::time::Duration::from_millis(1_500))
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
        }
        checks.push(mock_aware_check(
            "execution provider",
            &config.execution,
            config.execution == "mock",
            execution_note(&config.execution),
        ));
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

/// Probe the embedded Needle brain and report it as **one line-pair**:
///
/// ```text
/// [warn] needle engine: backend not in this build; weights not fetched (init skips them without a backend)
/// [warn] needle brain: inactive — falling back to static routing; install a build with the brain: `cargo install …`
/// ```
///
/// The pair is the fix for a specific failure: the single line this replaced
/// could only report one cause at a time, so a user with no backend *and* no
/// weights was told "weights missing at …; run `forge init` to fetch them" —
/// a hint their `forge init` would refuse to act on, since it skips the fetch
/// when there is no backend to feed. Line one now always states both
/// preconditions; line two states the consequence and the remedy that matches
/// whichever precondition actually failed.
///
/// Only meaningful when `router = "needle"` (else purely informational, and a
/// single line). Never returns `Level::Fail` — a broken or missing brain
/// degrades to the configured fallback, so forge stays usable either way;
/// these checks exist to surface *why* it degraded.
///
/// Filesystem/checksum only: this never fetches weights over the network
/// (that's `forge init`'s job), so `forge doctor` stays fast and offline.
async fn needle_checks(config: &forge_config::Config) -> Vec<Check> {
    const ENGINE_LABEL: &str = "needle engine";
    const BRAIN_LABEL: &str = "needle brain";

    if config.router != "needle" {
        return vec![Check {
            level: Level::Ok,
            label: BRAIN_LABEL.into(),
            detail: "not the active router".into(),
        }];
    }

    let using_hash_backend = std::env::var("FORGE_NEEDLE_BACKEND").as_deref() == Ok("hash");
    let backend = backend_state(using_hash_backend);
    let weights = weights_state(config, using_hash_backend, backend.usable);

    let engine = Check {
        level: if backend.usable && weights.usable {
            Level::Ok
        } else {
            Level::Warn
        },
        label: ENGINE_LABEL.into(),
        detail: format!("{}; {}", backend.detail, weights.detail),
    };

    // Whichever precondition is missing decides the remedy, so the pair never
    // points two ways at once. Backend first: without it, nothing about the
    // weights is actionable.
    let blocked_remedy = if !backend.usable {
        Some(forge_needle::ENGINE_REMEDY.to_string())
    } else {
        weights.remedy.clone()
    };
    if let Some(remedy) = blocked_remedy {
        return vec![
            engine,
            Check {
                level: Level::Warn,
                label: BRAIN_LABEL.into(),
                detail: format!(
                    "inactive — falling back to {} routing; {remedy}",
                    config.router_fallback
                ),
            },
        ];
    }

    // Both preconditions hold, so actually ask the brain something.
    let brain = probe_brain(config, using_hash_backend).await;
    vec![engine, brain]
}

/// Is there an inference engine in this binary at all?
struct BackendState {
    usable: bool,
    detail: String,
}

/// Where the weights stand, and — if they are the thing blocking the brain —
/// what fixes them.
struct WeightsState {
    usable: bool,
    detail: String,
    remedy: Option<String>,
}

/// The `needle-ffi` feature is an exact proxy for "this binary can run
/// inference": with it, `engine_from_config` builds `FfiBackend` and the build
/// linked a real `libneedle` (a build with the feature and no engine fails at
/// link time, so a running binary that has the feature has the engine);
/// without it, it builds `UnavailableBackend`, which cannot load anything.
fn backend_state(using_hash_backend: bool) -> BackendState {
    if using_hash_backend {
        return BackendState {
            usable: true,
            detail: "backend overridden to the deterministic hash backend \
                     (FORGE_NEEDLE_BACKEND=hash)"
                .to_string(),
        };
    }
    if cfg!(feature = "needle-ffi") {
        BackendState {
            usable: true,
            detail: "backend built in (libneedle linked)".to_string(),
        }
    } else {
        BackendState {
            usable: false,
            detail: "backend not in this build (`needle-ffi` off)".to_string(),
        }
    }
}

/// Resolve and checksum the weights on disk. Never touches the network.
///
/// When there is no backend this deliberately does **not** say "run `forge
/// init`": that binary's `forge init` skips the weights fetch on purpose, so
/// the hint would be a dead end. It reports the situation instead and leaves
/// the remedy to the backend half of the pair.
fn weights_state(
    config: &forge_config::Config,
    using_hash_backend: bool,
    backend_usable: bool,
) -> WeightsState {
    if using_hash_backend {
        return WeightsState {
            usable: true,
            detail: "weights not needed (hash backend)".to_string(),
            remedy: None,
        };
    }

    let path = match forge_needle::weights_path(&config.needle) {
        Ok(path) => path,
        Err(e) => {
            // Typically an unpinned variant (e.g. "small"/"medium") — name the
            // situation rather than pretending it is fixable with `forge init`.
            return WeightsState {
                usable: false,
                detail: format!("weights unresolvable: {e}"),
                remedy: Some(
                    "set `[needle] variant` to one with a pinned artifact (\"full\"), \
                     or point `weights_path` at your own and pin `weights_sha256`"
                        .to_string(),
                ),
            };
        }
    };

    if !path.is_file() {
        return WeightsState {
            usable: false,
            detail: if backend_usable {
                format!("weights not on disk ({})", path.display())
            } else {
                // Says *why* they were never fetched without naming `forge
                // init` — naming it is what created the loop, and a reader
                // who has just been told the backend is missing does not need
                // a second, conflicting instruction.
                format!(
                    "weights not fetched ({}) — nothing here could use them",
                    path.display()
                )
            },
            remedy: if backend_usable {
                Some("run `forge init` to fetch them".to_string())
            } else {
                // The backend half carries the only useful remedy.
                None
            },
        };
    }

    let expected_sha256 = if !config.needle.weights_sha256.trim().is_empty() {
        config.needle.weights_sha256.clone()
    } else {
        match forge_needle::spec_for(&config.needle.variant) {
            Ok(spec) => spec.sha256.to_string(),
            Err(e) => {
                return WeightsState {
                    usable: false,
                    detail: format!("weights checksum unknown: {e}"),
                    remedy: Some(
                        "pin `[needle] weights_sha256` for these weights, or use a pinned variant"
                            .to_string(),
                    ),
                };
            }
        }
    };

    match forge_needle::verify(&path, &expected_sha256) {
        Ok(true) => WeightsState {
            usable: true,
            detail: format!("weights present and verified ({})", path.display()),
            remedy: None,
        },
        Ok(false) => WeightsState {
            usable: false,
            detail: format!("weights at {} failed checksum verification", path.display()),
            remedy: Some("run `forge init` to refetch them".to_string()),
        },
        Err(e) => WeightsState {
            usable: false,
            detail: format!("weights at {} could not be read: {e}", path.display()),
            remedy: Some(format!(
                "fix permissions on {} or delete it and run `forge init`",
                path.display()
            )),
        },
    }
}

/// Ask the brain one real question and time it. Only called once both
/// preconditions hold, so anything that fails here is a genuine inference
/// problem rather than a setup problem — which is why none of these messages
/// suggests `forge init` or a rebuild.
async fn probe_brain(config: &forge_config::Config, using_hash_backend: bool) -> Check {
    const LABEL: &str = "needle brain";

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
                detail: format!(
                    "inactive — engine would not start ({e}); falling back to {} routing",
                    config.router_fallback
                ),
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
                detail: format!("active (model {model_id}, decide {elapsed_ms} ms)"),
            },
            Err(e) => Check {
                level: Level::Warn,
                label: LABEL.into(),
                detail: format!("decide succeeded but model info failed: {e}"),
            },
        },
        Ok(Err(e)) => Check {
            level: Level::Warn,
            label: LABEL.into(),
            detail: format!(
                "inactive — decide failed: {e}; falling back to {} routing",
                config.router_fallback
            ),
        },
        Err(_) => Check {
            level: Level::Warn,
            label: LABEL.into(),
            detail: format!(
                "inactive — decide timed out after {} ms; falling back to {} routing",
                timeout.as_millis(),
                config.router_fallback
            ),
        },
    }
}

/// Drop a `ForgeError`'s `"<kind> error: "` prefix for use inside a check
/// detail, where the label and level already carry that information.
/// Anything without a recognised prefix is returned unchanged — the wording
/// still has to be the provider crate's, not a paraphrase.
fn strip_error_kind(message: &str) -> String {
    const PREFIXES: &[&str] = &["configuration error: ", "model provider error: "];
    for prefix in PREFIXES {
        if let Some(rest) = message.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    message.to_string()
}

/// Whether `local_only` will refuse this endpoint — the same predicate
/// `forge-providers` enforces with, so a check can never promise a
/// restriction the code does not apply (or report one it does).
fn local_only_refuses(config: &forge_config::Config, url: &str) -> bool {
    config.local_only && !forge_providers::endpoint_is_local(url)
}

/// What `local_only` is actually doing, for the run this configuration
/// describes — reported only when it is on, and only in terms of what the
/// code enforces: providers refuse a non-local endpoint at construction
/// (redirects included), network decision routers are pruned, `forge init`
/// skips the weights fetch.
///
/// Deliberately `Ok` even when the configured model contradicts the setting:
/// the `model provider` check below owns that verdict, and two `Fail` lines
/// for one defect reads as if something else is also broken. What it does
/// carry are the two limits a user cannot see from the setting's name — that
/// tools and hooks are not sandboxed, and that a router may still *select* a
/// hosted entry, whose construction then fails the run.
fn local_only_check(config: &forge_config::Config) -> Option<Check> {
    if !config.local_only {
        return None;
    }
    Some(Check {
        level: Level::Ok,
        label: "local only".into(),
        detail: "enforced at provider construction (endpoint and every redirect must be \
                 loopback, localhost or a socket path); jev and off-device decision routers \
                 are pruned. Not a sandbox: tools and hooks you run are unrestricted, and a \
                 router that selects a hosted [models] entry fails that run rather than \
                 silently picking another."
            .into(),
    })
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
    // Mocks never authenticate, so naming a var for them is meaningless. The
    // list of mock names lives in the crate that enforces the gate — this was
    // the second copy.
    if !forge_providers::is_mock_model(&config.model) {
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
        "mock" => "test-only mock router",
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
        // Not "available offline": it reports commands as run and files as
        // written while doing neither.
        "mock" => "test-only mock execution; records operations instead of performing them",
        _ => "unrecognized execution provider",
    }
}

/// A check for a config value that may name a test-only mock.
///
/// Mirrors the model-provider check: a configured mock is a **failing**
/// check when the gate is closed (nothing will build, and the user has no
/// idea why), a warning when it is open (it works, but it is not real), and
/// an ordinary Ok line otherwise.
fn mock_aware_check(label: &str, value: &str, is_mock: bool, note: &str) -> Check {
    if !is_mock {
        return Check {
            level: Level::Ok,
            label: label.into(),
            detail: format!("{value} ({note})"),
        };
    }
    if forge_config::test_mocks_allowed() {
        Check {
            level: Level::Warn,
            label: label.into(),
            detail: format!(
                "{value} ({note}), unlocked by {}",
                forge_config::TEST_MOCKS_ENV
            ),
        }
    } else {
        Check {
            level: Level::Fail,
            label: label.into(),
            detail: format!(
                "{value} is a test-only mock and will not load; \
                 use a real one, or set {}=1 if you are running forge's own tests",
                forge_config::TEST_MOCKS_ENV
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    /// Find one check by label, for the line-pair assertions below.
    fn find<'a>(checks: &'a [Check], label: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("no {label:?} check in {:?}", labels(checks)))
    }

    fn labels(checks: &[Check]) -> Vec<&str> {
        checks.iter().map(|c| c.label.as_str()).collect()
    }

    /// Both halves joined, which is what a reader actually sees.
    fn pair_text(checks: &[Check]) -> String {
        checks
            .iter()
            .map(|c| format!("{}: {}", c.label, c.detail))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    #[serial]
    async fn needle_checks_when_router_is_not_needle_is_ok_and_informational() {
        let config = forge_config::Config {
            router: "static".to_string(),
            ..forge_config::Config::default()
        };
        let checks = needle_checks(&config).await;
        assert_eq!(checks.len(), 1, "no line-pair when needle is not in play");
        assert_eq!(checks[0].level, Level::Ok);
        assert!(checks[0].detail.contains("not the active router"));
    }

    /// The whole point of the pair: every needle report states both
    /// preconditions and the consequence, so no reader ever gets half the
    /// story and has to guess the other half.
    #[tokio::test]
    #[serial]
    async fn the_needle_report_is_always_a_line_pair_covering_backend_and_weights() {
        let mut config = forge_config::Config::default();
        config.needle.weights_path = "/nonexistent/needle.cact".to_string();

        let checks = needle_checks(&config).await;

        assert_eq!(labels(&checks), vec!["needle engine", "needle brain"]);
        let engine = find(&checks, "needle engine");
        assert!(
            engine.detail.contains("backend"),
            "line one must state the backend: {}",
            engine.detail
        );
        assert!(
            engine.detail.contains("weights"),
            "line one must state the weights: {}",
            engine.detail
        );
        let brain = find(&checks, "needle brain");
        assert!(
            brain.detail.contains("active") || brain.detail.contains("inactive"),
            "line two must state the verdict: {}",
            brain.detail
        );
    }

    /// The exact contradiction the user reported, from doctor's side: a build
    /// with no backend must never be told to run `forge init`, because that
    /// binary's `forge init` skips the weights fetch precisely because there
    /// is no backend. It gets the one remedy that ends the loop instead.
    #[tokio::test]
    #[serial]
    async fn a_backend_less_build_is_never_told_to_run_forge_init() {
        if cfg!(feature = "needle-ffi") {
            return; // this binary *has* a backend; nothing to assert
        }
        let mut config = forge_config::Config::default();
        config.needle.weights_path = "/nonexistent/needle.cact".to_string();

        let checks = needle_checks(&config).await;
        let text = pair_text(&checks);

        assert!(
            !text.contains("forge init"),
            "no half of the pair may point at `forge init` here: {text}"
        );
        assert!(
            text.contains(forge_needle::ENGINE_REMEDY),
            "the pair must carry the one shared remedy: {text}"
        );
        assert!(
            find(&checks, "needle engine")
                .detail
                .contains("backend not in this build"),
            "{text}"
        );
        assert!(
            find(&checks, "needle brain").detail.contains("inactive"),
            "{text}"
        );
        assert!(
            find(&checks, "needle brain")
                .detail
                .contains(&config.router_fallback),
            "the verdict must name what routes instead: {text}"
        );
        for check in &checks {
            assert_ne!(check.level, Level::Fail, "a missing brain is never fatal");
        }
    }

    /// The mirror image: with a backend present, missing weights *are* a
    /// `forge init` job, and the hint must survive. (Only assertable in an
    /// `ffi` build; the two tests together cover both branches, so neither
    /// build configuration loses the coverage.)
    #[tokio::test]
    #[serial]
    async fn missing_weights_with_a_backend_present_do_point_at_forge_init() {
        if !cfg!(feature = "needle-ffi") {
            return;
        }
        let mut config = forge_config::Config::default();
        config.needle.weights_path = "/nonexistent/needle.cact".to_string();

        let checks = needle_checks(&config).await;
        let text = pair_text(&checks);

        assert!(
            text.contains("forge init"),
            "with a backend, a refetch is exactly the remedy: {text}"
        );
        assert!(
            !text.contains(forge_needle::ENGINE_REMEDY),
            "and a reinstall is not: {text}"
        );
    }

    /// Weights present and verified but no backend: line one must say so
    /// *positively* about the weights (they are fine) and negatively about the
    /// backend, and the verdict must not blame the weights.
    #[tokio::test]
    #[serial]
    async fn verified_weights_without_a_backend_blame_the_backend_not_the_weights() {
        if cfg!(feature = "needle-ffi") {
            return;
        }
        use sha2::{Digest, Sha256};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weights.bin");
        let bytes = b"arbitrary-bytes-standing-in-for-real-needle-weights";
        std::fs::write(&path, bytes).expect("write fake weights");

        let config = forge_config::Config {
            needle: forge_config::NeedleConfig {
                variant: "full".to_string(),
                weights_path: path.display().to_string(),
                autofetch: true,
                // Operator-supplied override bypasses the pinned-spec
                // checksum lookup entirely, so an arbitrary payload can
                // verify cleanly.
                weights_sha256: format!("{:x}", Sha256::digest(bytes)),
            },
            ..forge_config::Config::default()
        };

        let checks = needle_checks(&config).await;
        let engine = find(&checks, "needle engine");

        assert_eq!(engine.level, Level::Warn);
        assert!(
            engine.detail.contains("weights present and verified"),
            "the weights are fine and must be reported as fine: {}",
            engine.detail
        );
        assert!(
            engine.detail.contains("backend not in this build"),
            "and the backend is what is missing: {}",
            engine.detail
        );
        let text = pair_text(&checks);
        assert!(!text.contains("forge init"), "{text}");
        assert!(text.contains(forge_needle::ENGINE_REMEDY), "{text}");
    }

    #[tokio::test]
    #[serial]
    async fn needle_checks_warn_for_unpinned_variant_without_panicking() {
        // "medium" is config-valid but has no pinned artifact yet (see
        // forge-needle's weights module doc) — must degrade to Warn, never
        // panic or Fail.
        let mut config = forge_config::Config::default();
        config.needle.variant = "medium".to_string();
        let checks = needle_checks(&config).await;
        let text = pair_text(&checks);
        assert!(text.contains("medium"), "{text}");
        for check in &checks {
            assert_ne!(check.level, Level::Fail, "{text}");
        }
        assert_eq!(find(&checks, "needle engine").level, Level::Warn);
    }

    #[tokio::test]
    #[serial]
    async fn needle_checks_with_hash_backend_report_active_and_latency() {
        // SAFETY: test-only env mutation, serialized via #[serial] against
        // any other test touching FORGE_NEEDLE_BACKEND in this crate.
        unsafe {
            std::env::set_var("FORGE_NEEDLE_BACKEND", "hash");
        }
        let checks = needle_checks(&forge_config::Config::default()).await;
        unsafe {
            std::env::remove_var("FORGE_NEEDLE_BACKEND");
        }
        assert_eq!(find(&checks, "needle engine").level, Level::Ok);
        let brain = find(&checks, "needle brain");
        assert_eq!(brain.level, Level::Ok);
        assert!(brain.detail.contains("active"), "{}", brain.detail);
        assert!(brain.detail.contains("ms"), "{}", brain.detail); // measured decide() latency
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

    /// `local_only` off: no check at all, so the report doesn't grow a line
    /// about a setting nobody turned on.
    #[test]
    fn local_only_check_is_absent_when_the_setting_is_off() {
        assert!(local_only_check(&forge_config::Config::default()).is_none());
    }

    /// The detail may only claim what the code enforces — provider
    /// construction including redirects, and router pruning — and must state
    /// the two limits the setting's name hides: tools and hooks are not
    /// sandboxed, and a router selecting a hosted entry fails the run.
    #[test]
    fn local_only_check_reports_what_is_actually_enforced() {
        let config = forge_config::Config {
            local_only: true,
            model: "qwen3-coder".to_string(),
            model_base_url: Some("http://127.0.0.1:8080/v1".to_string()),
            ..forge_config::Config::default()
        };
        let check = local_only_check(&config).expect("reported when on");
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("redirect"), "{}", check.detail);
        assert!(check.detail.contains("Not a sandbox"), "{}", check.detail);
        assert!(
            check.detail.contains("fails that run"),
            "the candidate-selection limit must be stated: {}",
            check.detail
        );
    }

    /// One defect, one failure: a model that contradicts `local_only` is
    /// reported by the `model provider` check, so this line stays `Ok`
    /// instead of making the report look like two things are broken.
    #[test]
    fn local_only_check_does_not_duplicate_the_model_provider_verdict() {
        let config = forge_config::Config {
            local_only: true,
            model: "claude-sonnet".to_string(),
            ..forge_config::Config::default()
        };
        let check = local_only_check(&config).expect("reported when on");
        assert_eq!(check.level, Level::Ok);
        assert!(
            !check.detail.contains("api.anthropic.com"),
            "the model verdict belongs to the model provider check: {}",
            check.detail
        );
        // …and that check does refuse it, with the provider crate's own
        // wording (so doctor cannot give a different instruction).
        assert!(local_only_refuses(&config, "https://api.anthropic.com"));
    }

    /// One report, one answer: the `decision router` line must name the
    /// router that will run, not the one the config file names, or it
    /// contradicts the `router endpoint` warning two lines below it.
    #[tokio::test]
    async fn decision_router_check_reports_the_effective_router_under_local_only() {
        let config = forge_config::Config {
            local_only: true,
            router: "laya".to_string(),
            router_url: Some("https://laya.example.com/decide".to_string()),
            ..forge_config::Config::default()
        };
        let effective = forge_providers::effective_router_name(&config.router, &config);
        assert_eq!(effective, "static");

        // …and with nothing to degrade, the configured name is reported as-is.
        let plain = forge_config::Config::default();
        assert_eq!(
            forge_providers::effective_router_name(&plain.router, &plain),
            plain.router
        );
    }

    /// A check detail must not label the error kind twice: the label and the
    /// Fail level already say it is the model provider's configuration.
    #[test]
    fn a_check_detail_drops_the_forge_error_type_prefix() {
        assert_eq!(
            strip_error_kind("configuration error: local_only is set, but model \"x\" …"),
            "local_only is set, but model \"x\" …"
        );
        // Unrecognised prefixes are left exactly as the provider crate wrote
        // them — the point is to reuse its wording, not to rewrite it.
        assert_eq!(
            strip_error_kind("something else: boom"),
            "something else: boom"
        );
    }

    /// The shared predicate: doctor must judge locality exactly the way
    /// `forge-providers` enforces it, or a check would promise something the
    /// code doesn't do.
    #[test]
    fn local_only_refuses_matches_the_provider_crate() {
        let on = forge_config::Config {
            local_only: true,
            ..forge_config::Config::default()
        };
        assert!(local_only_refuses(&on, "https://api.anthropic.com"));
        assert!(local_only_refuses(&on, "http://192.168.1.4:8080/v1"));
        assert!(!local_only_refuses(&on, "http://127.0.0.1:8788/decide"));
        assert!(!local_only_refuses(&on, "http://localhost:8080/v1"));

        let off = forge_config::Config::default();
        assert!(!local_only_refuses(&off, "https://api.anthropic.com"));
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

    /// A mock model authenticates against nothing, so naming a credential
    /// variable for it is meaningless — and the set of mock names comes from
    /// the crate that enforces the gate, not from a second list here.
    #[test]
    #[serial]
    fn credential_envs_are_not_reported_for_mock_models() {
        unsafe { std::env::set_var("SOME_MODEL_KEY", "x") };
        for model in ["mock", "mock-local", "scripted-mock"] {
            let config = forge_config::Config {
                model: model.to_string(),
                model_key_env: Some("SOME_MODEL_KEY".to_string()),
                ..forge_config::Config::default()
            };
            assert!(
                named_credential_envs(&config).is_empty(),
                "{model} authenticates against nothing"
            );
        }
        // A real model with the same setting is reported.
        let config = forge_config::Config {
            model: "qwen3-coder".to_string(),
            model_key_env: Some("SOME_MODEL_KEY".to_string()),
            ..forge_config::Config::default()
        };
        assert_eq!(
            named_credential_envs(&config),
            vec![("model_key_env", "SOME_MODEL_KEY".to_string())]
        );
        unsafe { std::env::remove_var("SOME_MODEL_KEY") };
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
