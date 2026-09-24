use std::collections::BTreeSet;

use forge_core::embed::Embedder;
use forge_core::{ForgeError, ProjectGraph};
use forge_graph::{EMBEDDINGS_REL_PATH, EmbeddingIndex, LocalGraph};
use forge_needle::EngineEmbedder;

use crate::cli::GraphCommand;
use crate::commands::Context;

/// Symbols are embedded in batches so a large first build doesn't hold one
/// giant `Vec<String>` in flight against the engine at once.
const EMBED_BATCH_SIZE: usize = 32;

pub async fn run(ctx: &Context, command: GraphCommand) -> Result<(), ForgeError> {
    match command {
        GraphCommand::Build => build(ctx).await,
        GraphCommand::Check => check(ctx),
        GraphCommand::Map => map(ctx),
        GraphCommand::Grep { pattern, semantic } => {
            if semantic {
                grep_semantic(ctx, &pattern).await
            } else {
                grep(ctx, &pattern)
            }
        }
        GraphCommand::Callers { symbol } => callers(ctx, &symbol),
        GraphCommand::Blast { path } => blast(ctx, &path.to_string_lossy()),
        GraphCommand::Context { query } => context(ctx, &query).await,
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

async fn build(ctx: &Context) -> Result<(), ForgeError> {
    let mut graph = LocalGraph::open(ctx.project_root()?)?;
    let (stats, report) = graph.build_report()?;
    let embedded = embed_after_build(ctx, &graph).await?;
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
                "embedded": embedded,
            }))
            .map_err(|e| ForgeError::graph(format!("serializing stats: {e}")))?
        );
    } else {
        println!(
            "graph built: {} files, {} symbols, {} imports, {} tests ({} ms; {} parsed, {} reused, {} removed, {} embedded)",
            stats.files,
            stats.symbols,
            stats.imports,
            stats.tests,
            stats.duration_ms,
            report.parsed.len(),
            report.reused.len(),
            report.removed.len(),
            embedded,
        );
    }
    Ok(())
}

/// After a successful `build_report()`, refresh the local semantic index —
/// but only when a needle engine is genuinely available (constructible AND
/// able to answer; see `forge_needle::engine_if_available`). Graph
/// structure itself stays model-free (its README promises no model calls,
/// ever); this step is purely additive and, without a working engine,
/// skips silently rather than failing the build. Returns how many symbols
/// were (re-)embedded this run (0 when skipped, or when nothing changed).
async fn embed_after_build(ctx: &Context, graph: &LocalGraph) -> Result<usize, ForgeError> {
    let resolved = ctx.resolve_config()?;
    let Some(engine) = forge_needle::engine_if_available(&resolved.config).await else {
        return Ok(0);
    };
    let embedder = EngineEmbedder::new(engine).await?;

    let index_path = graph.root().join(EMBEDDINGS_REL_PATH);
    // A stored index built with a different model/dimensionality is not
    // comparable to fresh vectors — drop it entirely rather than mixing.
    let mut index = EmbeddingIndex::load(&index_path)
        .filter(|idx| idx.matches_model(&embedder.model_id(), embedder.dimensions()))
        .unwrap_or_else(|| EmbeddingIndex::new(embedder.model_id(), embedder.dimensions()));

    // (key, content_hash, embedding text) for every symbol in the current
    // build.
    let candidates = graph.embedding_candidates();
    let current_hashes: Vec<(String, String)> = candidates
        .iter()
        .map(|(key, hash, _)| (key.clone(), hash.clone()))
        .collect();
    let current_keys: BTreeSet<String> = candidates.iter().map(|(key, _, _)| key.clone()).collect();
    let stale: BTreeSet<String> = index.stale_keys(&current_hashes).into_iter().collect();

    let mut pending: Vec<(String, String, String)> = candidates
        .into_iter()
        .filter(|(key, _, _)| stale.contains(key))
        .collect();
    pending.sort(); // deterministic batch order regardless of symbol scan order

    let mut embedded = 0usize;
    for batch in pending.chunks(EMBED_BATCH_SIZE) {
        let texts: Vec<String> = batch.iter().map(|(_, _, text)| text.clone()).collect();
        let vectors = embedder.embed(&texts).await?;
        for ((key, hash, _), vector) in batch.iter().zip(vectors) {
            index.upsert(key.clone(), hash.clone(), vector);
            embedded += 1;
        }
    }

    index.remove_missing(&current_keys);
    index.save(&index_path)?;
    Ok(embedded)
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

/// Search the local semantic index instead of literal/regex matching.
/// Requires a genuinely working needle engine (not just a constructible
/// one — see `engine_if_available`) and an index built by a matching
/// model; either gap is reported the same way, since the fix is the same
/// either way (`forge init` to fetch weights, then `forge graph build`).
async fn grep_semantic(ctx: &Context, query: &str) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    let embedder = context_embedder(ctx).await?;
    let results = forge_graph::semantic_grep(
        &graph,
        embedder.as_ref().map(|e| e as &dyn Embedder),
        query,
        20,
    )
    .await?;

    if ctx.global.json {
        let out: Vec<serde_json::Value> = results
            .iter()
            .map(|(key, score)| serde_json::json!({ "key": key, "score": score }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::graph(format!("serializing semantic matches: {e}")))?
        );
    } else {
        for (key, score) in &results {
            println!("{score:.4}  {key}");
        }
        if results.is_empty() {
            println!("no semantic matches for {query:?}");
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

async fn context(ctx: &Context, query: &str) -> Result<(), ForgeError> {
    let graph = open_built(ctx)?;
    // One ranking implementation for every adapter: `forge_graph_context`
    // over MCP calls the same function with the same embedder seam.
    let embedder = context_embedder(ctx).await?;
    let blended = forge_graph::blended_context(
        &graph,
        embedder.as_ref().map(|e| e as &dyn Embedder),
        query,
        10,
    )
    .await?;

    if ctx.global.json {
        let out: Vec<serde_json::Value> = blended
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
        for h in &blended {
            println!("{:>7.4} {} — {}", h.score, h.path, h.reasons.join("; "));
        }
        if blended.is_empty() {
            println!("no relevant files for {query:?}");
        }
    }
    Ok(())
}

/// The on-device embedder when one is genuinely usable, else `None`
/// (which keeps ranking lexical). Absence is normal, never an error.
async fn context_embedder(ctx: &Context) -> Result<Option<EngineEmbedder>, ForgeError> {
    let resolved = ctx.resolve_config()?;
    let Some(engine) = forge_needle::engine_if_available(&resolved.config).await else {
        return Ok(None);
    };
    Ok(Some(EngineEmbedder::new(engine).await?))
}
