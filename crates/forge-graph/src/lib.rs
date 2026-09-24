//! Deterministic, local, incremental project graph. No model calls, no
//! network. State lives at `<project>/.forge/graph/graph.json`; freshness
//! is decided by per-file mtime + content hash.

pub mod embed_index;
mod graph;
mod parse;
pub mod query;
mod state;

pub use embed_index::EmbeddingIndex;
pub use forge_core::ContextHit;
pub use graph::{BuildReport, DirSummary, FreshnessReport, LocalGraph};
pub use query::{EMBEDDINGS_REL_PATH, ScoredHit, blended_context, semantic_grep};
pub use state::{FileKind, FileNode, ImportEdge, SymbolNode};

#[cfg(test)]
mod tests;
