use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Graph state schema/extraction version. Bump when the state layout OR
/// the extraction heuristics change — a mismatch discards stored state
/// and forces a full rebuild.
pub const GRAPH_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Source,
    Test,
    Config,
    Doc,
    Other,
}

/// Classify a file by extension and path heuristics.
pub fn classify(path: &str) -> FileKind {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let looks_test = path.split('/').any(|seg| seg == "tests" || seg == "test")
        || file_name.starts_with("test_")
        || file_name.ends_with("_test.go")
        || file_name.ends_with("_test.py")
        || file_name.ends_with(".test.js")
        || file_name.ends_with(".test.ts")
        || file_name.ends_with(".spec.js")
        || file_name.ends_with(".spec.ts");
    if looks_test {
        return FileKind::Test;
    }

    match ext.as_str() {
        "rs" | "py" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "go" | "c" | "cc" | "cpp"
        | "h" | "hpp" | "java" | "rb" | "sh" | "swift" | "kt" => FileKind::Source,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "lock" | "gitignore" => FileKind::Config,
        "md" | "rst" | "txt" | "adoc" => FileKind::Doc,
        _ => FileKind::Other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    /// Project-relative path.
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    /// Milliseconds since the Unix epoch; 0 when unavailable.
    pub mtime_ms: i64,
    /// SHA-256 hex of the file contents.
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolNode {
    pub name: String,
    /// function | struct | enum | trait | impl | module | class | type | …
    pub kind: String,
    /// Project-relative file path.
    pub file: String,
    pub line: u32,
    /// Callee names referenced inside this symbol (best-effort).
    #[serde(default)]
    pub calls: Vec<String>,
}

/// Edge: file → imported file (when resolvable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportEdge {
    /// Project-relative importing file.
    pub from: String,
    /// The raw import text as written (`crate::foo::bar`, `./util`, …).
    pub raw: String,
    /// Project-relative target file, when it resolves inside the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

/// Persisted graph state (`graph.json`). Deterministic: sorted maps/vecs,
/// no timestamps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphState {
    pub version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, FileNode>,
    #[serde(default)]
    pub symbols: Vec<SymbolNode>,
    #[serde(default)]
    pub imports: Vec<ImportEdge>,
}

impl GraphState {
    pub fn empty() -> Self {
        Self {
            version: GRAPH_VERSION,
            ..Self::default()
        }
    }
}
