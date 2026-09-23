//! CLI bootstrap: `.env` / `.env.local` loading, run before config
//! resolution and before any provider/session-store construction (the
//! session store snapshots the environment for secret redaction — keys
//! loaded here are covered by it).

use std::path::Path;

use forge_core::find_project_root;

use crate::cli::GlobalOpts;

/// Load environment files from the resolved project root. Precedence:
/// real shell env > `.env.local` > `.env` (we never override existing
/// vars, and load `.env.local` first). Missing files are fine; parse
/// errors warn but never abort startup. Values are never logged.
pub fn load_dotenv(global: &GlobalOpts) {
    let start = match &global.project {
        Some(p) => p.clone(),
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(_) => return,
        },
    };
    let root = find_project_root(&start);
    load_from_root(&root);
}

fn load_from_root(root: &Path) {
    for name in [".env.local", ".env"] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        match dotenvy::from_path_iter(&path) {
            Ok(iter) => {
                let mut loaded = 0usize;
                for item in iter {
                    match item {
                        Ok((key, value)) => {
                            // Shell env always wins.
                            if std::env::var_os(&key).is_none() {
                                // Single-threaded bootstrap before any
                                // provider/runtime spawns.
                                unsafe { std::env::set_var(&key, &value) };
                                loaded += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "parse error in environment file");
                            break;
                        }
                    }
                }
                tracing::debug!(path = %path.display(), loaded, "loaded environment file");
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read environment file");
            }
        }
    }
}
