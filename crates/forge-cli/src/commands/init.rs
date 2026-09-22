use std::path::{Path, PathBuf};

use forge_core::{ForgeError, find_project_root};
use tracing::info;

use crate::commands::Context;

const STARTER_CONFIG: &str = "\
# Forge project configuration.
# Values here override user config and defaults; environment variables and
# CLI flags override this file. See `forge config explain <key>`.
model = \"mock-local\"
router = \"static\"
execution = \"native\"
approval = \"prompt\"
";

const GITIGNORE_ENTRY: &str = ".forge/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemStatus {
    Created,
    Updated,
    Unchanged,
}

impl ItemStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Created => "created  ",
            Self::Updated => "updated  ",
            Self::Unchanged => "unchanged",
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

    report(&root, &items, ctx.global.json)
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
