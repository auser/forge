use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::ForgeError;

/// Summary of a (re)built graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStats {
    pub files: usize,
    pub directories: usize,
    pub symbols: usize,
    pub imports: usize,
    pub tests: usize,
    pub duration_ms: u64,
}

/// A symbol (function, type, etc.) discovered in a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: String,
    pub file: PathBuf,
    pub line: u32,
}

/// One match from a content search over the indexed files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub file: PathBuf,
    pub line: u32,
    pub text: String,
}

/// One ranked context-selection result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHit {
    pub path: String,
    pub score: u32,
    pub reasons: Vec<String>,
}

/// Deterministic, local, incremental, regenerable structural project graph.
/// Stored below `.forge/graph/`; never requires model calls to build.
pub trait ProjectGraph: Send + Sync {
    /// Build or incrementally refresh the graph from the working tree.
    fn build(&mut self) -> Result<GraphStats, ForgeError>;

    /// True when the stored graph matches current file mtimes/hashes.
    fn is_fresh(&self) -> bool;

    fn files(&self) -> Vec<PathBuf>;

    fn symbols(&self) -> Vec<SymbolInfo>;

    /// Search indexed symbol names and import paths.
    fn grep(&self, pattern: &str) -> Result<Vec<GrepMatch>, ForgeError>;

    /// Top-N files relevant to a free-text query. Default: no results
    /// (used when no graph is available).
    fn context(&self, query: &str, limit: usize) -> Vec<ContextHit> {
        let _ = (query, limit);
        Vec::new()
    }
}
