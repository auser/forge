use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use forge_core::{ForgeError, GraphStats, GrepMatch, ProjectGraph, SymbolInfo};
use sha2::{Digest, Sha256};

use crate::parse::{FileParse, parse_source};
use crate::state::{FileKind, FileNode, GraphState, ImportEdge, SymbolNode, classify};

/// Directories never indexed.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".forge",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "dist",
    "build",
    ".hg",
    ".svn",
];

const GRAPH_REL_PATH: &str = ".forge/graph/graph.json";

/// What a build did, per file. Exposed so tests and users can see the
/// incremental behavior.
#[derive(Debug, Clone, Default)]
pub struct BuildReport {
    pub parsed: Vec<String>,
    pub reused: Vec<String>,
    pub removed: Vec<String>,
}

/// Freshness comparison between the working tree and the stored graph.
#[derive(Debug, Clone, Default)]
pub struct FreshnessReport {
    pub fresh: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
}

/// Per-directory summary for `graph map`.
#[derive(Debug, Clone, Default)]
pub struct DirSummary {
    pub dir: String,
    /// File counts by kind (kind name → count).
    pub files: BTreeMap<String, usize>,
    pub symbols: usize,
}

/// One ranked context result for `graph context`.
#[derive(Debug, Clone)]
pub struct ContextHit {
    pub path: String,
    pub score: u32,
    pub reasons: Vec<String>,
}

/// Local, deterministic, incremental project graph.
pub struct LocalGraph {
    root: PathBuf,
    state: GraphState,
    last_report: Option<BuildReport>,
}

impl LocalGraph {
    /// Open the graph for a project, loading stored state when present and
    /// version-compatible.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ForgeError> {
        let root = root.into();
        let path = root.join(GRAPH_REL_PATH);
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let parsed: GraphState = serde_json::from_str(&text)
                    .map_err(|e| ForgeError::graph(format!("parsing {}: {e}", path.display())))?;
                if parsed.version == crate::state::GRAPH_VERSION {
                    parsed
                } else {
                    GraphState::empty()
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => GraphState::empty(),
            Err(e) => return Err(ForgeError::Io(e)),
        };
        Ok(Self {
            root,
            state,
            last_report: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn graph_file(&self) -> PathBuf {
        self.root.join(GRAPH_REL_PATH)
    }

    pub fn state(&self) -> &GraphState {
        &self.state
    }

    pub fn last_build_report(&self) -> Option<&BuildReport> {
        self.last_report.as_ref()
    }

    /// Walk the working tree: project-relative path → (kind, size, mtime).
    fn walk(&self) -> Result<BTreeMap<String, (FileKind, u64, i64)>, ForgeError> {
        let mut out = BTreeMap::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| {
                !(e.file_type().is_dir()
                    && e.depth() > 0
                    && SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
            })
        {
            let entry = entry.map_err(|e| ForgeError::graph(format!("walking project: {e}")))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(&self.root)
                .map_err(|e| ForgeError::graph(format!("stripping root: {e}")))?
                .to_string_lossy()
                .replace('\\', "/");
            let meta = entry
                .metadata()
                .map_err(|e| ForgeError::graph(format!("reading metadata: {e}")))?;
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.insert(rel.clone(), (classify(&rel), meta.len(), mtime_ms));
        }
        Ok(out)
    }

    fn hash_file(path: &Path) -> Result<String, ForgeError> {
        let mut file = std::fs::File::open(path).map_err(ForgeError::Io)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = file.read(&mut buf).map_err(ForgeError::Io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Compare the working tree against stored state without rewriting.
    pub fn freshness(&self) -> Result<FreshnessReport, ForgeError> {
        let current = self.walk()?;
        let mut report = FreshnessReport::default();

        for (path, (_, size, mtime_ms)) in &current {
            match self.state.files.get(path) {
                None => report.added.push(path.clone()),
                Some(stored) => {
                    if stored.size != *size || stored.mtime_ms != *mtime_ms {
                        let hash = Self::hash_file(&self.root.join(path))?;
                        if hash != stored.hash {
                            report.modified.push(path.clone());
                        }
                    }
                }
            }
        }
        for path in self.state.files.keys() {
            if !current.contains_key(path) {
                report.removed.push(path.clone());
            }
        }
        report.fresh =
            report.added.is_empty() && report.removed.is_empty() && report.modified.is_empty();
        Ok(report)
    }

    /// Build or incrementally refresh the graph, persist it, and return
    /// stats plus the per-file build report.
    pub fn build_report(&mut self) -> Result<(GraphStats, BuildReport), ForgeError> {
        let started = Instant::now();
        let current = self.walk()?;
        let old = std::mem::take(&mut self.state);
        let mut report = BuildReport::default();

        // Old per-file derived data for reuse.
        let mut old_symbols: BTreeMap<&str, Vec<SymbolNode>> = BTreeMap::new();
        for sym in &old.symbols {
            old_symbols
                .entry(sym.file.as_str())
                .or_default()
                .push(sym.clone());
        }
        let mut old_imports: BTreeMap<&str, Vec<ImportEdge>> = BTreeMap::new();
        for imp in &old.imports {
            old_imports
                .entry(imp.from.as_str())
                .or_default()
                .push(imp.clone());
        }

        let mut files = BTreeMap::new();
        let mut symbols: Vec<SymbolNode> = Vec::new();
        let mut imports: Vec<ImportEdge> = Vec::new();

        for (path, (kind, size, mtime_ms)) in &current {
            let stored = old.files.get(path);
            let reusable = match stored {
                Some(node) if node.size == *size && node.mtime_ms == *mtime_ms => {
                    Some(node.hash.clone())
                }
                Some(node) => {
                    let hash = Self::hash_file(&self.root.join(path))?;
                    (hash == node.hash).then_some(hash)
                }
                None => None,
            };

            match reusable {
                Some(hash) => {
                    report.reused.push(path.clone());
                    files.insert(
                        path.clone(),
                        FileNode {
                            path: path.clone(),
                            kind: *kind,
                            size: *size,
                            mtime_ms: *mtime_ms,
                            hash,
                        },
                    );
                    if let Some(syms) = old_symbols.get(path.as_str()) {
                        symbols.extend(syms.iter().cloned());
                    }
                    if let Some(imps) = old_imports.get(path.as_str()) {
                        imports.extend(imps.iter().cloned());
                    }
                }
                None => {
                    report.parsed.push(path.clone());
                    let content =
                        std::fs::read_to_string(self.root.join(path)).map_err(ForgeError::Io)?;
                    let hash = {
                        let mut hasher = Sha256::new();
                        hasher.update(content.as_bytes());
                        format!("{:x}", hasher.finalize())
                    };
                    files.insert(
                        path.clone(),
                        FileNode {
                            path: path.clone(),
                            kind: *kind,
                            size: *size,
                            mtime_ms: *mtime_ms,
                            hash,
                        },
                    );
                    let parsed: FileParse = parse_source(path, &content);
                    attach_parsed(&mut symbols, &mut imports, path, parsed);
                }
            }
        }

        for path in old.files.keys() {
            if !current.contains_key(path) {
                report.removed.push(path.clone());
            }
        }

        // (Re)resolve import edges against the full current file index.
        let index: HashSet<&str> = files.keys().map(String::as_str).collect();
        for edge in &mut imports {
            edge.resolved = resolve_import(&edge.from, &edge.raw, &index);
        }

        symbols.sort_by(|a, b| (&a.file, a.line, &a.name).cmp(&(&b.file, b.line, &b.name)));
        symbols.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.name == b.name);
        imports.sort_by(|a, b| (&a.from, &a.raw).cmp(&(&b.from, &b.raw)));
        imports.dedup_by(|a, b| a.from == b.from && a.raw == b.raw);

        let tests = files.values().filter(|f| f.kind == FileKind::Test).count();
        let stats = GraphStats {
            files: files.len(),
            directories: files
                .keys()
                .filter_map(|p| Path::new(p).parent().map(|p| p.to_path_buf()))
                .collect::<HashSet<_>>()
                .len(),
            symbols: symbols.len(),
            imports: imports.len(),
            tests,
            duration_ms: started.elapsed().as_millis() as u64,
        };

        self.state = GraphState {
            version: crate::state::GRAPH_VERSION,
            files,
            symbols,
            imports,
        };
        self.save()?;
        self.last_report = Some(report.clone());
        Ok((stats, report))
    }

    fn save(&self) -> Result<(), ForgeError> {
        let path = self.graph_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
        }
        let text = serde_json::to_string_pretty(&self.state)
            .map_err(|e| ForgeError::graph(format!("serializing graph: {e}")))?;
        std::fs::write(&path, text + "\n").map_err(ForgeError::Io)
    }

    /// Stats from the current in-memory state (no rebuild).
    pub fn stats(&self) -> GraphStats {
        GraphStats {
            files: self.state.files.len(),
            directories: self
                .state
                .files
                .keys()
                .filter_map(|p| Path::new(p).parent().map(|p| p.to_path_buf()))
                .collect::<HashSet<_>>()
                .len(),
            symbols: self.state.symbols.len(),
            imports: self.state.imports.len(),
            tests: self
                .state
                .files
                .values()
                .filter(|f| f.kind == FileKind::Test)
                .count(),
            duration_ms: 0,
        }
    }

    /// Per-directory file/symbol summary.
    pub fn map(&self) -> Vec<DirSummary> {
        let mut dirs: BTreeMap<String, DirSummary> = BTreeMap::new();
        for node in self.state.files.values() {
            let dir = parent_dir(&node.path);
            let entry = dirs.entry(dir.clone()).or_insert_with(|| DirSummary {
                dir,
                ..DirSummary::default()
            });
            let kind = format!("{:?}", node.kind).to_lowercase();
            *entry.files.entry(kind).or_insert(0) += 1;
        }
        for sym in &self.state.symbols {
            let dir = parent_dir(&sym.file);
            let entry = dirs.entry(dir.clone()).or_insert_with(|| DirSummary {
                dir,
                ..DirSummary::default()
            });
            entry.symbols += 1;
        }
        dirs.into_values().collect()
    }

    /// Symbols whose recorded call sites reference `symbol`.
    pub fn callers(&self, symbol: &str) -> Vec<&SymbolNode> {
        self.state
            .symbols
            .iter()
            .filter(|s| s.calls.iter().any(|c| c == symbol))
            .collect()
    }

    /// Files importing `path`, plus their direct importers (two hops).
    pub fn blast(&self, path: &str) -> Vec<String> {
        let target = normalize_rel(path, &self.root, &self.state);
        let mut out: Vec<String> = Vec::new();
        let mut frontier = vec![target.clone()];
        for _ in 0..2 {
            let mut next = Vec::new();
            for edge in &self.state.imports {
                if let Some(resolved) = &edge.resolved
                    && frontier.contains(resolved)
                    && edge.from != target
                    && !out.contains(&edge.from)
                {
                    out.push(edge.from.clone());
                    next.push(edge.from.clone());
                }
            }
            frontier = next;
        }
        out.sort();
        out
    }

    /// Top-N files relevant to a free-text query (naive token match).
    pub fn context(&self, query: &str, limit: usize) -> Vec<ContextHit> {
        let tokens: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .filter(|t| t.len() >= 2)
            .collect();
        if tokens.is_empty() {
            return Vec::new();
        }

        let mut hits = Vec::new();
        for node in self.state.files.values() {
            let path_lower = node.path.to_lowercase();
            let mut score = 0u32;
            let mut reasons = Vec::new();
            for token in &tokens {
                if path_lower.contains(token) {
                    score += 2;
                    reasons.push(format!("path matches {token:?}"));
                }
                let symbol_matches: Vec<&str> = self
                    .state
                    .symbols
                    .iter()
                    .filter(|s| s.file == node.path && s.name.to_lowercase().contains(token))
                    .map(|s| s.name.as_str())
                    .collect();
                if !symbol_matches.is_empty() {
                    score += 3 * symbol_matches.len() as u32;
                    reasons.push(format!(
                        "symbols match {token:?}: {}",
                        symbol_matches.join(", ")
                    ));
                }
                if self
                    .state
                    .imports
                    .iter()
                    .any(|i| i.from == node.path && i.raw.to_lowercase().contains(token))
                {
                    score += 1;
                    reasons.push(format!("imports match {token:?}"));
                }
            }
            if score > 0 {
                hits.push(ContextHit {
                    path: node.path.clone(),
                    score,
                    reasons,
                });
            }
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        hits.truncate(limit);
        hits
    }
}

fn parent_dir(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

/// Normalize a user-supplied path to a project-relative one, when it
/// matches a known file.
fn normalize_rel(path: &str, root: &Path, state: &GraphState) -> String {
    let trimmed = path.trim_start_matches("./");
    if state.files.contains_key(trimmed) {
        return trimmed.to_string();
    }
    let joined = root.join(trimmed);
    if let Ok(canon) = joined.canonicalize()
        && let Ok(rel) = canon.strip_prefix(root)
    {
        let rel = rel.to_string_lossy().replace('\\', "/");
        if state.files.contains_key(&rel) {
            return rel;
        }
    }
    trimmed.to_string()
}

fn attach_parsed(
    symbols: &mut Vec<SymbolNode>,
    imports: &mut Vec<ImportEdge>,
    path: &str,
    parsed: FileParse,
) {
    let mut nodes: Vec<SymbolNode> = parsed
        .symbols
        .iter()
        .map(|s| SymbolNode {
            name: s.name.clone(),
            kind: s.kind.to_string(),
            file: path.to_string(),
            line: s.line,
            calls: Vec::new(),
        })
        .collect();
    for (enclosing, callee, _line) in &parsed.calls {
        if enclosing.is_empty() {
            continue;
        }
        if let Some(node) = nodes.iter_mut().find(|n| &n.name == enclosing)
            && !node.calls.contains(callee)
        {
            node.calls.push(callee.clone());
        }
    }
    for node in &mut nodes {
        node.calls.sort();
    }
    symbols.extend(nodes);
    for (raw, _line) in &parsed.imports {
        imports.push(ImportEdge {
            from: path.to_string(),
            raw: raw.clone(),
            resolved: None,
        });
    }
}

/// Best-effort resolution of an import string to a project-relative file.
fn resolve_import(from: &str, raw: &str, index: &HashSet<&str>) -> Option<String> {
    let cleaned = raw.trim().trim_matches('"').trim_matches('\'');
    if cleaned.is_empty() {
        return None;
    }
    let normalized = cleaned.replace("::", "/");
    let normalized = normalized
        .strip_prefix("crate/")
        .map(|rest| format!("src/{rest}"))
        .unwrap_or(normalized);

    let from_dir = Path::new(from)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();

    let mut bases: Vec<String> = Vec::new();
    if normalized.starts_with("./") || normalized.starts_with("../") {
        bases.push(from_dir.clone());
    } else {
        bases.push(String::new());
        bases.push(from_dir.clone());
        bases.push("src".to_string());
    }

    const EXTENSIONS: &[&str] = &["", ".rs", ".py", ".js", ".ts", ".jsx", ".tsx", ".go"];
    const SUFFIXES: &[&str] = &["/mod.rs", "/__init__.py", "/index.js", "/index.ts"];

    // The last segment of `crate::foo::bar` / `a.b.c` may be a symbol
    // rather than a module — try the full path and the trimmed one.
    let mut variants = vec![normalized.clone()];
    if let Some(idx) = normalized.rfind('/') {
        variants.push(normalized[..idx].to_string());
    }

    for base in &bases {
        for variant in &variants {
            let joined = if base.is_empty() {
                variant.clone()
            } else {
                normalize_join(base, variant)
            };
            for ext in EXTENSIONS {
                let candidate = format!("{joined}{ext}");
                if index.contains(candidate.as_str()) {
                    return Some(candidate);
                }
            }
            for suffix in SUFFIXES {
                let candidate = format!("{joined}{suffix}");
                if index.contains(candidate.as_str()) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Join a base dir with a possibly `./`/`../`-prefixed path and normalize
/// out the dot segments (lexically, without touching the filesystem).
fn normalize_join(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

impl ProjectGraph for LocalGraph {
    fn build(&mut self) -> Result<GraphStats, ForgeError> {
        self.build_report().map(|(stats, _)| stats)
    }

    fn is_fresh(&self) -> bool {
        self.freshness().map(|r| r.fresh).unwrap_or(false)
    }

    fn files(&self) -> Vec<PathBuf> {
        self.state.files.keys().map(PathBuf::from).collect()
    }

    fn symbols(&self) -> Vec<SymbolInfo> {
        self.state
            .symbols
            .iter()
            .map(|s| SymbolInfo {
                name: s.name.clone(),
                kind: s.kind.clone(),
                file: PathBuf::from(&s.file),
                line: s.line,
            })
            .collect()
    }

    fn grep(&self, pattern: &str) -> Result<Vec<GrepMatch>, ForgeError> {
        let regex = regex::Regex::new(pattern).ok();
        let matches = |text: &str| -> bool {
            match &regex {
                Some(re) => re.is_match(text),
                None => text.contains(pattern),
            }
        };
        let mut out = Vec::new();
        for sym in &self.state.symbols {
            if matches(&sym.name) {
                out.push(GrepMatch {
                    file: PathBuf::from(&sym.file),
                    line: sym.line,
                    text: format!("symbol {} {}", sym.kind, sym.name),
                });
            }
        }
        for edge in &self.state.imports {
            if matches(&edge.raw) {
                let target = edge.resolved.as_deref().unwrap_or("unresolved");
                out.push(GrepMatch {
                    file: PathBuf::from(&edge.from),
                    line: 0,
                    text: format!("import {} -> {target}", edge.raw),
                });
            }
        }
        Ok(out)
    }
}
