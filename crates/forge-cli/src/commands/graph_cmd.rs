use std::collections::{BTreeMap, BTreeSet};

use forge_core::embed::Embedder;
use forge_core::{ContextHit, ForgeError, ProjectGraph};
use forge_graph::{EmbeddingIndex, LocalGraph};
use forge_needle::EngineEmbedder;

use crate::cli::GraphCommand;
use crate::commands::Context;

/// Where the local semantic index lives, relative to the project root —
/// alongside `graph.json` under the same `.forge/graph/` directory. Kept
/// separate from `graph.json` itself: the structural graph is model-free by
/// design (see the crate README), while this file only exists when a
/// needle engine was available at build time.
const EMBEDDINGS_REL_PATH: &str = ".forge/graph/embeddings.bin";

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
    let resolved = ctx.resolve_config()?;
    let engine = forge_needle::engine_if_available(&resolved.config)
        .await
        .ok_or_else(|| {
            ForgeError::graph("semantic search needs needle weights (run forge init)")
        })?;
    let embedder = EngineEmbedder::new(engine).await?;

    let index_path = graph.root().join(EMBEDDINGS_REL_PATH);
    let index = EmbeddingIndex::load(&index_path)
        .filter(|idx| idx.matches_model(&embedder.model_id(), embedder.dimensions()))
        .ok_or_else(|| {
            ForgeError::graph("semantic index not built yet; run `forge graph build`")
        })?;

    let query_vector = embed_one(&embedder, query).await?;
    let results = index.search(&query_vector, 20);

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

/// Embed a single piece of text via an `Embedder`, unwrapping the
/// one-vector-per-text contract.
async fn embed_one(embedder: &EngineEmbedder, text: &str) -> Result<Vec<f32>, ForgeError> {
    embedder
        .embed(&[text.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ForgeError::graph("embedder returned no vector for its input"))
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
    // A wider lexical candidate pool than the final 10 shown, so a file the
    // semantic side ranks highly but lexical search only weakly matched
    // still has a rank score to blend with (rather than defaulting to 0).
    let lexical = graph.context(query, 20);
    let mut blended = semantic_blend(ctx, &graph, query, &lexical).await?;
    blended.truncate(10);

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

/// One context result after blending, if applicable — `score` is always a
/// float here (unlike `ContextHit::score`, a `u32`), so the blended and
/// lexical-only paths share one output shape.
struct ScoredHit {
    path: String,
    score: f64,
    reasons: Vec<String>,
}

/// Blend lexical context hits with the semantic index when both a working
/// needle engine and a matching embeddings index exist for this project;
/// otherwise return `lexical` unchanged (still sorted/ranked exactly as
/// `LocalGraph::context` produced it).
///
/// Blend formula: `final = 0.5 * lexical_rank_score + 0.5 * cosine`, where
/// `lexical_rank_score = 1 / (1 + rank)` over `lexical`'s existing order
/// (rank 0 = best lexical match) and `cosine` is the best (max) similarity
/// among that path's symbols in the semantic index. Candidates are the
/// union of `lexical`'s paths and whatever paths the semantic search
/// surfaces — a file the semantic side considers a strong match still
/// shows up even if lexical search missed it entirely (rank score 0 for
/// that half), and vice versa.
async fn semantic_blend(
    ctx: &Context,
    graph: &LocalGraph,
    query: &str,
    lexical: &[ContextHit],
) -> Result<Vec<ScoredHit>, ForgeError> {
    let unchanged = || {
        lexical
            .iter()
            .map(|h| ScoredHit {
                path: h.path.clone(),
                score: h.score as f64,
                reasons: h.reasons.clone(),
            })
            .collect::<Vec<_>>()
    };

    let resolved = ctx.resolve_config()?;
    let Some(engine) = forge_needle::engine_if_available(&resolved.config).await else {
        return Ok(unchanged());
    };
    let embedder = EngineEmbedder::new(engine).await?;

    let index_path = graph.root().join(EMBEDDINGS_REL_PATH);
    let Some(index) = EmbeddingIndex::load(&index_path)
        .filter(|idx| idx.matches_model(&embedder.model_id(), embedder.dimensions()))
    else {
        return Ok(unchanged());
    };

    let query_vector = embed_one(&embedder, query).await?;

    // Best (max) cosine per path, from the top semantic matches.
    let mut cosine_by_path: BTreeMap<String, f32> = BTreeMap::new();
    for (key, score) in index.search(&query_vector, 50) {
        if let Some((path, _symbol)) = key.rsplit_once("::") {
            cosine_by_path
                .entry(path.to_string())
                .and_modify(|best| {
                    if score > *best {
                        *best = score;
                    }
                })
                .or_insert(score);
        }
    }

    let lexical_rank: BTreeMap<&str, usize> = lexical
        .iter()
        .enumerate()
        .map(|(rank, h)| (h.path.as_str(), rank))
        .collect();
    let mut reasons_by_path: BTreeMap<String, Vec<String>> = lexical
        .iter()
        .map(|h| (h.path.clone(), h.reasons.clone()))
        .collect();

    let mut paths: BTreeSet<String> = lexical.iter().map(|h| h.path.clone()).collect();
    paths.extend(cosine_by_path.keys().cloned());

    let mut out: Vec<ScoredHit> = paths
        .into_iter()
        .map(|path| {
            let rank_score = lexical_rank
                .get(path.as_str())
                .map(|rank| 1.0 / (1.0 + *rank as f64))
                .unwrap_or(0.0);
            let cosine = cosine_by_path.get(&path).copied().unwrap_or(0.0) as f64;
            let reasons = reasons_by_path.remove(&path).unwrap_or_default();
            ScoredHit {
                score: 0.5 * rank_score + 0.5 * cosine,
                path,
                reasons,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}
