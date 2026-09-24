//! Ranked project-context queries over the structural graph, optionally
//! blended with the local semantic index.
//!
//! This module is the single implementation behind `forge graph context`,
//! `forge graph grep --semantic` and the MCP tools `forge_graph_context` /
//! `forge_graph_grep` — the CLI and the MCP adapter must not drift apart on
//! ranking.
//!
//! The crate's model-free promise is preserved: nothing here constructs an
//! embedder or touches the network. Callers pass an [`Embedder`] they built
//! (via `forge_needle::engine_if_available`), or `None` for the pure
//! lexical path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use forge_core::embed::Embedder;
use forge_core::{ContextHit, ForgeError, ProjectGraph};

use crate::embed_index::EmbeddingIndex;
use crate::graph::LocalGraph;

/// Where the local semantic index lives, relative to the project root —
/// alongside `graph.json` under the same `.forge/graph/` directory. Kept
/// separate from `graph.json` itself: the structural graph is model-free by
/// design, while this file only exists when a needle engine was available
/// at build time.
pub const EMBEDDINGS_REL_PATH: &str = ".forge/graph/embeddings.bin";

/// One context result after blending — `score` is always a float here
/// (unlike [`ContextHit::score`], a `u32`), so the blended and
/// lexical-only paths share one output shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredHit {
    pub path: String,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// How many lexical candidates to consider before truncating to `limit`: a
/// wider pool than the final list, so a file the semantic side ranks highly
/// but lexical search only weakly matched still has a rank score to blend
/// with (rather than defaulting to 0).
const LEXICAL_POOL: usize = 20;

/// How many semantic neighbours to pull for the blend.
const SEMANTIC_POOL: usize = 50;

/// Load the semantic index for `graph` when it exists AND was built by a
/// model matching `embedder`. A stored index built with a different
/// model/dimensionality is not comparable to fresh vectors, so it is
/// ignored rather than mixed.
fn matching_index(root: &Path, embedder: &dyn Embedder) -> Option<EmbeddingIndex> {
    EmbeddingIndex::load(&root.join(EMBEDDINGS_REL_PATH))
        .filter(|idx| idx.matches_model(&embedder.model_id(), embedder.dimensions()))
}

/// Embed a single piece of text, unwrapping the one-vector-per-text
/// contract.
async fn embed_one(embedder: &dyn Embedder, text: &str) -> Result<Vec<f32>, ForgeError> {
    embedder
        .embed(&[text.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ForgeError::graph("embedder returned no vector for its input"))
}

/// Rank project files for `query`, blending lexical context hits with the
/// semantic index when both a working embedder and a matching index exist;
/// otherwise the lexical ranking is returned unchanged.
///
/// Blend formula: `final = 0.5 * lexical_rank_score + 0.5 * cosine`, where
/// `lexical_rank_score = 1 / (1 + rank)` over the lexical order (rank 0 =
/// best lexical match) and `cosine` is the best (max) similarity among that
/// path's symbols in the semantic index. Candidates are the union of both
/// sides — a file the semantic side considers a strong match shows up even
/// if lexical search missed it entirely (rank score 0 for that half), and
/// vice versa.
pub async fn blended_context(
    graph: &LocalGraph,
    embedder: Option<&dyn Embedder>,
    query: &str,
    limit: usize,
) -> Result<Vec<ScoredHit>, ForgeError> {
    let lexical = graph.context(query, LEXICAL_POOL.max(limit));
    let mut out = match embedder {
        Some(embedder) => match matching_index(graph.root(), embedder) {
            Some(index) => blend(&index, embedder, query, &lexical).await?,
            None => lexical_only(&lexical),
        },
        None => lexical_only(&lexical),
    };
    out.truncate(limit);
    Ok(out)
}

fn lexical_only(lexical: &[ContextHit]) -> Vec<ScoredHit> {
    lexical
        .iter()
        .map(|h| ScoredHit {
            path: h.path.clone(),
            score: h.score as f64,
            reasons: h.reasons.clone(),
        })
        .collect()
}

async fn blend(
    index: &EmbeddingIndex,
    embedder: &dyn Embedder,
    query: &str,
    lexical: &[ContextHit],
) -> Result<Vec<ScoredHit>, ForgeError> {
    let query_vector = embed_one(embedder, query).await?;

    // Best (max) cosine per path, from the top semantic matches.
    let mut cosine_by_path: BTreeMap<String, f32> = BTreeMap::new();
    for (key, score) in index.search(&query_vector, SEMANTIC_POOL) {
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

/// Search the semantic index instead of literal/regex matching. Requires a
/// working embedder AND an index built by a matching model; either gap is
/// reported the same way, since the fix is the same either way (`forge
/// init` to fetch weights, then `forge graph build`).
pub async fn semantic_grep(
    graph: &LocalGraph,
    embedder: Option<&dyn Embedder>,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, f32)>, ForgeError> {
    let embedder = embedder.ok_or_else(|| {
        ForgeError::graph("semantic search needs needle weights (run forge init)")
    })?;
    let index = matching_index(graph.root(), embedder).ok_or_else(|| {
        ForgeError::graph("semantic index not built yet; run `forge graph build`")
    })?;
    let query_vector = embed_one(embedder, query).await?;
    Ok(index.search(&query_vector, limit))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;

    /// Embedder whose vectors are decided by a fixed table: any text
    /// containing `needle` embeds to `[1, 0]`, everything else to `[0, 1]`.
    /// Deterministic and offline — no engine, no weights.
    struct TableEmbedder;

    #[async_trait]
    impl Embedder for TableEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.contains("needle") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn model_id(&self) -> String {
            "table-test".to_string()
        }
    }

    fn project_with_graph(dir: &Path) -> LocalGraph {
        std::fs::write(dir.join("alpha.rs"), "fn alpha_marker() {}\n").expect("write alpha");
        std::fs::write(dir.join("beta.rs"), "fn beta_other() {}\n").expect("write beta");
        let mut graph = LocalGraph::open(dir).expect("open");
        graph.build().expect("build");
        graph
    }

    #[tokio::test]
    async fn context_without_embedder_is_the_lexical_ranking() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = project_with_graph(tmp.path());

        let hits = blended_context(&graph, None, "alpha_marker", 10)
            .await
            .expect("context");

        let lexical = graph.context("alpha_marker", 20);
        assert_eq!(hits.len(), lexical.len().min(10));
        assert_eq!(hits[0].path, lexical[0].path);
        assert_eq!(hits[0].score, lexical[0].score as f64);
    }

    #[tokio::test]
    async fn context_honours_the_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = project_with_graph(tmp.path());

        let hits = blended_context(&graph, None, "fn", 1).await.expect("ctx");
        assert!(hits.len() <= 1);
    }

    #[tokio::test]
    async fn context_ignores_an_index_built_by_another_model() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = project_with_graph(tmp.path());
        // Dimensions deliberately disagree with TableEmbedder's 2.
        let mut index = EmbeddingIndex::new("other-model".to_string(), 3);
        index.upsert(
            "beta.rs::beta_other".into(),
            "h".into(),
            vec![1.0, 0.0, 0.0],
        );
        index
            .save(&tmp.path().join(EMBEDDINGS_REL_PATH))
            .expect("save");

        let blended = blended_context(&graph, Some(&TableEmbedder), "alpha_marker", 10)
            .await
            .expect("context");
        let lexical = blended_context(&graph, None, "alpha_marker", 10)
            .await
            .expect("context");
        assert_eq!(blended, lexical, "mismatched index must be ignored");
    }

    #[tokio::test]
    async fn context_blends_semantic_neighbours_into_the_ranking() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = project_with_graph(tmp.path());
        let mut index = EmbeddingIndex::new("table-test".to_string(), 2);
        // beta.rs is the semantic match for a "needle" query; lexical
        // search cannot see it (its text has no such token).
        index.upsert("beta.rs::beta_other".into(), "h".into(), vec![1.0, 0.0]);
        index
            .save(&tmp.path().join(EMBEDDINGS_REL_PATH))
            .expect("save");

        let hits = blended_context(&graph, Some(&TableEmbedder), "needle", 10)
            .await
            .expect("context");
        assert_eq!(
            hits.first().map(|h| h.path.as_str()),
            Some("beta.rs"),
            "semantic-only match should lead: {hits:?}"
        );
    }

    #[tokio::test]
    async fn semantic_grep_without_an_embedder_names_the_fix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = project_with_graph(tmp.path());

        let err = semantic_grep(&graph, None, "needle", 5)
            .await
            .expect_err("no embedder");
        assert!(err.to_string().contains("forge init"), "{err}");
    }

    #[tokio::test]
    async fn semantic_grep_without_an_index_names_the_fix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = project_with_graph(tmp.path());

        let err = semantic_grep(&graph, Some(&TableEmbedder), "needle", 5)
            .await
            .expect_err("no index");
        assert!(err.to_string().contains("forge graph build"), "{err}");
    }

    #[tokio::test]
    async fn semantic_grep_returns_scored_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = project_with_graph(tmp.path());
        let mut index = EmbeddingIndex::new("table-test".to_string(), 2);
        index.upsert("beta.rs::beta_other".into(), "h".into(), vec![1.0, 0.0]);
        index
            .save(&tmp.path().join(EMBEDDINGS_REL_PATH))
            .expect("save");

        let hits = semantic_grep(&graph, Some(&TableEmbedder), "needle", 5)
            .await
            .expect("grep");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "beta.rs::beta_other");
    }
}
