use std::path::{Path, PathBuf};

use forge_core::{ForgeError, find_project_root};
use tracing::{info, warn};

use crate::commands::Context;

const STARTER_CONFIG: &str = "\
# Forge project configuration.
# Defaults: local oMLX model + embedded Needle 3 decision router.
# Precedence: user config -> this file -> FORGE_* env -> CLI flags.
# See `forge config explain <key>`.
model = \"qwen3-coder\"        # served by oMLX at model_base_url
router = \"needle\"            # on-device decisions; falls back to static when unavailable
execution = \"native\"
approval = \"prompt\"
";

const GITIGNORE_ENTRY: &str = ".forge/";

/// Known provider API keys and the built-in model entry they unlock
/// (None = no built-in entry; still worth reporting).
const KNOWN_PROVIDER_KEYS: &[(&str, Option<&str>)] = &[
    ("DEEPSEEK_API_KEY", Some("deepseek-chat")),
    ("MOONSHOT_API_KEY", Some("kimi-k2.7-code")),
    ("OPENAI_API_KEY", None),
    ("ANTHROPIC_API_KEY", None),
    ("OPENROUTER_API_KEY", None),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemStatus {
    Created,
    Updated,
    Unchanged,
    /// Informational: existing environment/file detected, nothing changed.
    Detected,
}

impl ItemStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Created => "created  ",
            Self::Updated => "updated  ",
            Self::Unchanged => "unchanged",
            Self::Detected => "detected ",
        }
    }
}

struct InitItem {
    status: ItemStatus,
    path: PathBuf,
    note: Option<String>,
}

/// Idempotent project initialization: `.forge/` directories, starter config
/// (only when absent), and a single `.forge/` line in `.gitignore`.
pub fn run(ctx: &Context) -> Result<(), ForgeError> {
    let start = match &ctx.global.project {
        Some(p) => p.clone(),
        None => std::env::current_dir().map_err(ForgeError::Io)?,
    };
    let root = find_project_root(&start);
    info!(root = %root.display(), "initializing forge project");

    let mut items: Vec<InitItem> = Vec::new();

    let forge_dir = root.join(".forge");
    for dir in [
        &forge_dir,
        &forge_dir.join("graph"),
        &forge_dir.join("sessions"),
    ] {
        let status = if dir.is_dir() {
            ItemStatus::Unchanged
        } else {
            std::fs::create_dir_all(dir).map_err(ForgeError::Io)?;
            ItemStatus::Created
        };
        items.push(InitItem {
            status,
            path: dir.clone(),
            note: None,
        });
    }

    let config_path = forge_dir.join("config.toml");
    if config_path.exists() {
        items.push(InitItem {
            status: ItemStatus::Unchanged,
            path: config_path,
            note: Some("existing config preserved".to_string()),
        });
    } else {
        std::fs::write(&config_path, STARTER_CONFIG).map_err(ForgeError::Io)?;
        items.push(InitItem {
            status: ItemStatus::Created,
            path: config_path,
            note: None,
        });
    }

    items.push(update_gitignore(&root)?);
    items.push(build_graph(&root)?);

    let resolved = ctx.resolve_config()?;
    items.extend(legacy_config_item(&root, &resolved.config));
    items.push(needle_weights_item(&root, &resolved.config));

    items.extend(detect_environment(&root));

    report(&root, &items, ctx.global.json)
}

/// Pre-needle settings that a config written against an older Forge still
/// carries. `init` is where someone comes back to a project, so it is the
/// right place to say that the default moved: `router = "laya"` reads as
/// deliberate, but usually it only means "this file predates the embedded
/// brain" — and it costs a working default plus a separate process.
/// Read-only and informational; nothing is rewritten.
fn legacy_config_item(root: &Path, config: &forge_config::Config) -> Option<InitItem> {
    if config.router != "laya" {
        return None;
    }
    Some(InitItem {
        status: ItemStatus::Detected,
        path: forge_config::Config::project_config_path(root),
        note: Some(
            "note: router = \"laya\" is set; the built-in default is now the embedded \
             needle brain — delete the router line to use it (or keep laya and run \
             `forge router serve`)"
                .to_string(),
        ),
    })
}

/// `forge init`'s weights step: fetch/verify `[needle]` weights when the
/// resolved config actually wants them — `needle.autofetch` on, not
/// `--local-only`, and `router = "needle"` so a fetch wouldn't be wasted on
/// a router that never runs. Never fails `forge init`: every outcome
/// (including "no pinned artifact for this variant" and network failure)
/// degrades to an informational item, matching the design's guarantee that
/// forge stays fully functional on static routing without weights.
///
/// Feature-gated first: the `needle-ffi` feature (off by default — see
/// `forge-cli/Cargo.toml`) is what actually lets anything *use* fetched
/// weights (`FfiBackend` vs. `UnavailableBackend`). Without it, fetching the
/// ~35 MB `full` artifact would only ever sit on disk unused, so this skips
/// the fetch entirely rather than downloading bytes no build here can act on
/// — regardless of `local_only`/`autofetch`, since the more fundamental
/// reason to skip is "this binary has no inference backend to feed", not the
/// config.
///
/// That skip note carries [`forge_needle::ENGINE_REMEDY`] — the same single
/// command the router's error and `forge doctor` name. It has to: the
/// original note said "build with --features needle-ffi" while the router,
/// later in the same session, said "weights missing — run `forge init` to
/// fetch". Both sentences were true and together they described a loop with
/// no exit. Whatever a reader sees first must now point at the one command
/// that ends it.
fn needle_weights_item(root: &Path, config: &forge_config::Config) -> InitItem {
    if !cfg!(feature = "needle-ffi") {
        return InitItem {
            status: ItemStatus::Detected,
            path: root.to_path_buf(),
            note: Some(format!(
                "needle weights: skipped — this build has no embedded inference backend \
                 (`needle-ffi`), so weights would sit unused and routing stays static. \
                 To fix, {}, then re-run `forge init` and it fetches them.",
                forge_needle::ENGINE_REMEDY
            )),
        };
    }
    if config.local_only {
        return InitItem {
            status: ItemStatus::Detected,
            path: root.to_path_buf(),
            note: Some(
                "needle weights: skipped (--local-only); routing falls back to static until weights exist"
                    .to_string(),
            ),
        };
    }
    if !config.needle.autofetch || config.router != "needle" {
        return InitItem {
            status: ItemStatus::Detected,
            path: root.to_path_buf(),
            note: Some(
                "needle weights: autofetch disabled (needle.autofetch = false or router != \"needle\")"
                    .to_string(),
            ),
        };
    }

    let needle = config.needle.clone();
    match run_async(async move { forge_needle::ensure_weights(&needle).await }) {
        Ok(Ok(forge_needle::WeightsStatus::Present(path))) => InitItem {
            status: ItemStatus::Unchanged,
            path,
            note: Some("needle weights present (verified)".to_string()),
        },
        Ok(Ok(forge_needle::WeightsStatus::Fetched(path))) => InitItem {
            status: ItemStatus::Created,
            path,
            note: Some(format!(
                "fetched needle weights (variant: {})",
                config.needle.variant
            )),
        },
        Ok(Ok(forge_needle::WeightsStatus::Missing { path, reason })) => {
            warn!(reason = %reason, "needle weights unavailable; static routing continues");
            InitItem {
                status: ItemStatus::Detected,
                path,
                note: Some(format!(
                    "needle weights unavailable: {reason} (static routing continues)"
                )),
            }
        }
        Ok(Err(e)) => {
            warn!(error = %e, "needle weights fetch returned an error");
            InitItem {
                status: ItemStatus::Detected,
                path: root.to_path_buf(),
                note: Some(format!(
                    "needle weights: error resolving weights ({e}); static routing continues"
                )),
            }
        }
        Err(e) => {
            warn!(error = %e, "could not run the needle weights fetch");
            InitItem {
                status: ItemStatus::Detected,
                path: root.to_path_buf(),
                note: Some(format!(
                    "needle weights: could not run fetch ({e}); static routing continues"
                )),
            }
        }
    }
}

/// Bridge a sync call site (`forge init`'s `run()` is sync — see the
/// `Command::Init => init::run(&ctx)` line in `commands::dispatch`, called
/// without `.await`) into `forge-needle`'s async weights I/O. Runs the
/// future on a dedicated OS thread with its own single-purpose Tokio
/// runtime: `forge-cli`'s top-level `runtime.block_on(dispatch(cli))`
/// already drives this call from inside a multi-thread Tokio runtime, and
/// nesting `Handle::block_on` there would deadlock/panic; a fresh thread
/// sidesteps that regardless of whether a runtime happens to be running
/// already (e.g. a plain unit test calling this directly would have none).
fn run_async<F, T>(fut: F) -> Result<T, ForgeError>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name("forge-init-async".to_string())
        .spawn(move || {
            tokio::runtime::Runtime::new()
                .map(|rt| rt.block_on(fut))
                .map_err(|e| ForgeError::config(format!("starting async runtime: {e}")))
        })
        .map_err(|e| ForgeError::config(format!("spawning async thread: {e}")))?
        .join()
        .map_err(|_| ForgeError::config("async thread panicked"))?
}

/// Drop-in conveniences: report `.env`/`.env.local` files (loaded at
/// startup) and known provider API keys (names only, never values).
/// Read-only: nothing is written or modified.
fn detect_environment(root: &Path) -> Vec<InitItem> {
    let mut items = Vec::new();
    for name in [".env.local", ".env"] {
        let path = root.join(name);
        if path.is_file() {
            items.push(InitItem {
                status: ItemStatus::Detected,
                path: path.clone(),
                note: Some("loaded at startup".to_string()),
            });
        }
    }
    for (key, model) in KNOWN_PROVIDER_KEYS {
        let present = std::env::var(key).is_ok_and(|v| !v.is_empty());
        if present {
            let note = match model {
                Some(entry) => format!("{key} → {entry} routable"),
                None => format!("{key} present (no built-in model entry)"),
            };
            items.push(InitItem {
                status: ItemStatus::Detected,
                path: root.to_path_buf(),
                note: Some(note),
            });
        }
    }
    items
}

/// Build the structural graph unless the stored one is already fresh.
fn build_graph(root: &Path) -> Result<InitItem, ForgeError> {
    use forge_core::ProjectGraph;

    let graph_path = root.join(".forge").join("graph").join("graph.json");
    let existed = graph_path.is_file();
    let mut graph = forge_graph::LocalGraph::open(root)?;
    if existed && graph.is_fresh() {
        return Ok(InitItem {
            status: ItemStatus::Unchanged,
            path: graph_path,
            note: Some("graph: unchanged (fresh)".to_string()),
        });
    }
    let (stats, _) = graph.build_report()?;
    Ok(InitItem {
        status: if existed {
            ItemStatus::Updated
        } else {
            ItemStatus::Created
        },
        path: graph_path,
        note: Some(format!(
            "graph: {} files, {} symbols, {} imports, {} tests",
            stats.files, stats.symbols, stats.imports, stats.tests
        )),
    })
}

fn update_gitignore(root: &Path) -> Result<InitItem, ForgeError> {
    let path = root.join(".gitignore");
    if path.exists() {
        let existing = std::fs::read_to_string(&path).map_err(ForgeError::Io)?;
        if existing.lines().any(|line| line.trim() == GITIGNORE_ENTRY) {
            return Ok(InitItem {
                status: ItemStatus::Unchanged,
                path,
                note: Some(format!("already contains {GITIGNORE_ENTRY}")),
            });
        }
        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(GITIGNORE_ENTRY);
        updated.push('\n');
        std::fs::write(&path, updated).map_err(ForgeError::Io)?;
        Ok(InitItem {
            status: ItemStatus::Updated,
            path,
            note: Some(format!("added {GITIGNORE_ENTRY}")),
        })
    } else {
        std::fs::write(&path, format!("{GITIGNORE_ENTRY}\n")).map_err(ForgeError::Io)?;
        Ok(InitItem {
            status: ItemStatus::Created,
            path,
            note: Some(format!("added {GITIGNORE_ENTRY}")),
        })
    }
}

fn report(root: &Path, items: &[InitItem], json: bool) -> Result<(), ForgeError> {
    if json {
        let entries: Vec<serde_json::Value> = items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "status": item.status.label().trim(),
                    "path": item.path,
                    "note": item.note,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "root": root,
                "items": entries,
            }))
            .map_err(|e| ForgeError::config(format!("serializing init report: {e}")))?
        );
        return Ok(());
    }

    println!("Forge project at {}", root.display());
    for item in items {
        match &item.note {
            Some(note) => println!(
                "  {} {} ({})",
                item.status.label(),
                item.path.display(),
                note
            ),
            None => println!("  {} {}", item.status.label(), item.path.display()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_item_reports_configured_laya_and_names_both_fixes() {
        let config = forge_config::Config {
            router: "laya".to_string(),
            ..forge_config::Config::default()
        };
        let item = legacy_config_item(Path::new("/proj"), &config).expect("laya is reported");
        assert_eq!(item.status, ItemStatus::Detected);
        let note = item.note.expect("note");
        assert!(note.contains("router = \"laya\" is set"), "note: {note}");
        assert!(note.contains("embedded needle brain"), "note: {note}");
        assert!(note.contains("delete the router line"), "note: {note}");
        // The note points at the file that has to be edited.
        assert!(item.path.ends_with(".forge/config.toml"), "{:?}", item.path);
    }

    #[test]
    fn legacy_config_item_is_silent_for_the_default_router() {
        let config = forge_config::Config::default();
        assert_eq!(config.router, "needle");
        assert!(legacy_config_item(Path::new("/proj"), &config).is_none());
    }

    /// Regression test for the fix that made this path feature-aware: a
    /// default build (no `needle-ffi` — this is exactly how `cargo test`
    /// compiles this crate; see `forge-cli/Cargo.toml`'s `default = []`)
    /// must skip the fetch entirely rather than downloading ~35 MB it has
    /// no backend to use, and must say so. Guarded with `cfg!` rather than
    /// `#[cfg(not(feature = "needle-ffi"))]` so `cargo test --features
    /// needle-ffi` still compiles this test (it just returns early instead
    /// of asserting the wrong branch).
    #[test]
    fn needle_weights_item_skips_fetch_without_needle_ffi_feature() {
        if cfg!(feature = "needle-ffi") {
            return;
        }

        let dir = tempfile::tempdir().expect("tmp");
        let weights_path = dir.path().join("cache").join("w.cact");
        let mut config = forge_config::Config::default();
        // Would autofetch if anything tried: proves the skip happens
        // regardless of config, not because autofetch/local_only already
        // said no.
        config.needle.autofetch = true;
        config.needle.weights_path = weights_path.display().to_string();
        config.router = "needle".to_string();
        config.local_only = false;

        let item = needle_weights_item(dir.path(), &config);

        let note = item.note.expect("note present");
        assert!(note.contains("skipped"), "note: {note}");
        assert!(note.contains("needle-ffi"), "note: {note}");
        assert!(
            !weights_path.exists(),
            "no fetch should have happened, but a file exists at {}",
            weights_path.display()
        );
    }

    /// The skip note must name the *one* command that gets a working brain —
    /// the same string the router's error and `forge doctor` use. This is the
    /// first half of the contradiction the user hit: init said "build with
    /// --features needle-ffi", the router then said "run `forge init` to
    /// fetch", and neither told them where the loop ends.
    #[test]
    fn the_skip_note_names_the_one_command_that_gets_a_working_brain() {
        if cfg!(feature = "needle-ffi") {
            return;
        }

        let dir = tempfile::tempdir().expect("tmp");
        let config = forge_config::Config::default();
        let note = needle_weights_item(dir.path(), &config)
            .note
            .expect("note present");

        assert!(
            note.contains(forge_needle::ENGINE_REMEDY),
            "the note must carry the shared remedy verbatim, so init, the router and doctor \
             cannot drift apart: {note}"
        );
        assert!(
            note.contains("cargo install"),
            "the remedy must be runnable as written: {note}"
        );
        assert!(
            note.contains("static"),
            "it must also say what happens meanwhile: {note}"
        );
    }
}
