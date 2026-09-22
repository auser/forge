use forge_core::{ForgeError, ProjectGraph};
use forge_graph::LocalGraph;

use crate::cli::GraphCommand;
use crate::commands::Context;

pub fn run(ctx: &Context, command: GraphCommand) -> Result<(), ForgeError> {
    match command {
        GraphCommand::Build => build(ctx),
        GraphCommand::Check => check(ctx),
        GraphCommand::Map => map(ctx),
        GraphCommand::Grep { pattern } => grep(ctx, &pattern),
        GraphCommand::Callers { symbol } => callers(ctx, &symbol),
        GraphCommand::Blast { path } => blast(ctx, &path.to_string_lossy()),
        GraphCommand::Context { query } => context(ctx, &query),
    }
}

/// Open the stored graph for read-only queries; errors when never built.
fn open_built(ctx: &Context) -> Result<LocalGraph, ForgeError> {
    let graph = LocalGraph::open(ctx.project_root()?)?;
    if !graph.graph_file().is_file() {
        return Err(ForgeError::graph(
            "graph not built yet; run `forge graph build` (or `forge init`)",
        ));
    }
    Ok(graph)
}

fn build(ctx: &Context) -> Result<(), ForgeError> {
    let mut graph = LocalGraph::open(ctx.project_root()?)?;
    let (stats, report) = graph.build_report()?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "files": stats.files,
                "directories": stats.directories,
                "symbols": stats.symbols,
                "imports": stats.imports,
                "tests": stats.tests,
                "duration_ms": stats.duration_ms,
                "parsed": report.parsed,
                "reused": report.reused,
                "removed": report.removed,
            }))
            .map_err(|e| ForgeError::graph(format!("serializing stats: {e}")))?
        );
    } else {
        println!(
            "graph built: {} files, {} symbols, {} imports, {} tests ({} ms; {} parsed, {} reused, {} removed)",
            stats.files,
            stats.symbols,
            stats.imports,
            stats.tests,
            stats.duration_ms,
            report.parsed.len(),
            report.reused.len(),
            report.removed.len()
        );
    }
    Ok(())
}

/// Exit 0 when fresh, 1 when stale (with a summary of what changed).
fn check(ctx: &Context) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    let report = graph.freshness()?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "fresh": report.fresh,
                "added": report.added,
                "removed": report.removed,
                "modified": report.modified,
            }))
            .map_err(|e| ForgeError::graph(format!("serializing freshness: {e}")))?
        );
    } else if report.fresh {
        println!("graph: fresh");
    } else {
        println!("graph: stale");
        for path in &report.added {
            println!("  added    {path}");
        }
        for path in &report.modified {
            println!("  modified {path}");
        }
        for path in &report.removed {
            println!("  removed  {path}");
        }
    }
    if report.fresh {
        Ok(())
    } else {
        Err(ForgeError::graph(format!(
            "graph is stale ({} added, {} modified, {} removed); run `forge graph build`",
            report.added.len(),
            report.modified.len(),
            report.removed.len()
        )))
    }
}

fn map(ctx: &Context) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    let dirs = graph.map();
    if ctx.global.json {
        let out: Vec<serde_json::Value> = dirs
            .iter()
            .map(|d| {
                serde_json::json!({
                    "dir": d.dir,
                    "files": d.files,
                    "symbols": d.symbols,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::graph(format!("serializing map: {e}")))?
        );
    } else {
        for d in &dirs {
            let kinds = d
                .files
                .iter()
                .map(|(kind, count)| format!("{count} {kind}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("{}: {} ({} symbols)", d.dir, kinds, d.symbols);
        }
    }
    Ok(())
}

fn grep(ctx: &Context, pattern: &str) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    let matches = graph.grep(pattern)?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&matches)
                .map_err(|e| ForgeError::graph(format!("serializing matches: {e}")))?
        );
    } else {
        for m in &matches {
            println!("{}:{}: {}", m.file.display(), m.line, m.text);
        }
        if matches.is_empty() {
            println!("no matches for {pattern:?}");
        }
    }
    Ok(())
}

fn callers(ctx: &Context, symbol: &str) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    let callers = graph.callers(symbol);
    if ctx.global.json {
        let out: Vec<serde_json::Value> = callers
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "kind": s.kind,
                    "file": s.file,
                    "line": s.line,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::graph(format!("serializing callers: {e}")))?
        );
    } else {
        for s in &callers {
            println!("{}:{}: {} ({})", s.file, s.line, s.name, s.kind);
        }
        if callers.is_empty() {
            println!("no callers of {symbol:?} found");
        }
    }
    Ok(())
}

fn blast(ctx: &Context, path: &str) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    let affected = graph.blast(path);
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path,
                "affected": affected,
            }))
            .map_err(|e| ForgeError::graph(format!("serializing blast radius: {e}")))?
        );
    } else {
        println!("blast radius of {path}:");
        for f in &affected {
            println!("  {f}");
        }
        if affected.is_empty() {
            println!("  (nothing imports this file)");
        }
    }
    Ok(())
}

fn context(ctx: &Context, query: &str) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    let hits = graph.context(query, 10);
    if ctx.global.json {
        let out: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "path": h.path,
                    "score": h.score,
                    "reasons": h.reasons,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::graph(format!("serializing context: {e}")))?
        );
    } else {
        for h in &hits {
            println!("{:>4} {} — {}", h.score, h.path, h.reasons.join("; "));
        }
        if hits.is_empty() {
            println!("no relevant files for {query:?}");
        }
    }
    Ok(())
}
