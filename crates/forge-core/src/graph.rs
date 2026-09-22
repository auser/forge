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
}
