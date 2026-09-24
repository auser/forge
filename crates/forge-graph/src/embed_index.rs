//! Local semantic index over graph symbols: pure data, no model or
//! embedding-provider dependency. Embedding happens in forge-cli (via
//! `forge_core::embed::Embedder`, backed by the needle engine); this crate
//! only stores the resulting vectors and answers nearest-neighbor queries
//! over them, so `forge-graph` stays free of any dependency on
//! `forge-needle` or model code.
//!
//! Persisted at `.forge/graph/embeddings.bin`: an 8-byte magic prefix
//! (`FRGEMB01`) followed by one `serde_json` object. A magic mismatch or a
//! JSON parse failure (truncated/corrupt file) makes `load` return `None`
//! rather than erroring — callers rebuild the index from scratch, which is
//! always a safe, cheap fallback here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use forge_core::ForgeError;

/// 8-byte magic prefix so `load` can cheaply reject foreign/corrupt files
/// before attempting to parse JSON.
const MAGIC: &[u8; 8] = b"FRGEMB01";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    content_hash: String,
    vector: Vec<f32>,
}

/// Index key = `"<path>::<symbol_name>"` (not just `<symbol_name>`) so two
/// symbols with the same name in different files are both independently
/// searchable. Header (`model_id`, `dimensions`) versions the vectors: any
/// stored index built with a different model or dimensionality must be
/// discarded wholesale (see `matches_model`), never mixed with new vectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingIndex {
    model_id: String,
    dimensions: usize,
    entries: BTreeMap<String, Entry>,
}

impl EmbeddingIndex {
    pub fn new(model_id: String, dimensions: usize) -> Self {
        Self {
            model_id,
            dimensions,
            entries: BTreeMap::new(),
        }
    }

    /// Load a previously saved index. Missing file, wrong magic, or
    /// unparseable JSON all return `None` — never a panic or an error the
    /// caller has to route around — since a rebuild from scratch is always
    /// available and cheap.
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return None;
        }
        serde_json::from_slice(&bytes[MAGIC.len()..]).ok()
    }

    pub fn save(&self, path: &Path) -> Result<(), ForgeError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
        }
        let body = serde_json::to_vec(self)
            .map_err(|e| ForgeError::graph(format!("serializing embedding index: {e}")))?;
        let mut out = Vec::with_capacity(MAGIC.len() + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&body);
        std::fs::write(path, out).map_err(ForgeError::Io)
    }

    /// Whether this index was built with the given model/dimensions — a
    /// mismatch (different embedder, or the same embedder upgraded to a
    /// new dimensionality) means the stored vectors are not comparable to
    /// fresh ones and the whole index must be dropped and rebuilt.
    pub fn matches_model(&self, model_id: &str, dimensions: usize) -> bool {
        self.model_id == model_id && self.dimensions == dimensions
    }

    /// Keys from `current` (key, content_hash) that are new or whose
    /// content hash changed since the index was last updated — i.e. need
    /// (re-)embedding. Keys already present with a matching hash are
    /// omitted, which is what makes rebuilds incremental.
    pub fn stale_keys(&self, current: &[(String, String)]) -> Vec<String> {
        current
            .iter()
            .filter(|(key, hash)| self.entries.get(key).map(|e| &e.content_hash) != Some(hash))
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub fn upsert(&mut self, key: String, content_hash: String, vector: Vec<f32>) {
        self.entries.insert(
            key,
            Entry {
                content_hash,
                vector,
            },
        );
    }

    /// Drop any entry whose key is not in `current_keys` (deleted/renamed
    /// symbols since the last build).
    pub fn remove_missing(&mut self, current_keys: &BTreeSet<String>) {
        self.entries.retain(|key, _| current_keys.contains(key));
    }

    /// Top-`k` entries by cosine similarity to `query`, descending; ties
    /// break on key for determinism.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let mut scored: Vec<(String, f32)> = self
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), cosine(query, &entry.vector)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("embeddings.bin");

        let mut index = EmbeddingIndex::new("hash-trigram".to_string(), 3);
        index.upsert(
            "src/main.rs::main".to_string(),
            "hash-a".to_string(),
            vec![1.0, 0.0, 0.0],
        );
        index.save(&path).expect("save");

        let loaded = EmbeddingIndex::load(&path).expect("load");
        assert!(loaded.matches_model("hash-trigram", 3));
        assert_eq!(
            loaded.search(&[1.0, 0.0, 0.0], 1),
            vec![("src/main.rs::main".to_string(), 1.0)]
        );
    }

    #[test]
    fn matches_model_is_false_on_different_model_id_or_dims() {
        let index = EmbeddingIndex::new("model-a".to_string(), 4);
        assert!(index.matches_model("model-a", 4));
        assert!(!index.matches_model("model-b", 4));
        assert!(!index.matches_model("model-a", 8));
    }

    #[test]
    fn load_on_missing_file_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.bin");
        assert!(EmbeddingIndex::load(&path).is_none());
    }

    #[test]
    fn load_on_truncated_file_returns_none_not_a_crash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("embeddings.bin");

        // Valid magic, but the JSON body is cut off mid-object.
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(br#"{"model_id":"m","dimensions":2,"entries":{"#);
        std::fs::write(&path, bytes).expect("write");

        assert!(EmbeddingIndex::load(&path).is_none());
    }

    #[test]
    fn load_on_foreign_file_without_magic_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("embeddings.bin");
        std::fs::write(&path, b"not-an-embeddings-file-at-all").expect("write");
        assert!(EmbeddingIndex::load(&path).is_none());
    }

    #[test]
    fn stale_keys_returns_only_new_or_changed_hash_keys() {
        let mut index = EmbeddingIndex::new("m".to_string(), 2);
        index.upsert("a::x".to_string(), "hash1".to_string(), vec![1.0, 0.0]);
        index.upsert("b::y".to_string(), "hash2".to_string(), vec![0.0, 1.0]);

        let current = vec![
            ("a::x".to_string(), "hash1".to_string()), // unchanged
            ("b::y".to_string(), "hash2-changed".to_string()), // changed
            ("c::z".to_string(), "hash3".to_string()), // new
        ];
        let mut stale = index.stale_keys(&current);
        stale.sort();
        assert_eq!(stale, vec!["b::y".to_string(), "c::z".to_string()]);
    }

    #[test]
    fn two_symbols_with_the_same_name_in_different_files_are_both_searchable() {
        let mut index = EmbeddingIndex::new("m".to_string(), 2);
        index.upsert(
            "src/a.rs::run".to_string(),
            "hash-a".to_string(),
            vec![1.0, 0.0],
        );
        index.upsert(
            "src/b.rs::run".to_string(),
            "hash-b".to_string(),
            vec![0.0, 1.0],
        );

        let results = index.search(&[1.0, 0.0], 10);
        let keys: Vec<&str> = results.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"src/a.rs::run"));
        assert!(keys.contains(&"src/b.rs::run"));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_orders_by_cosine_similarity_descending() {
        let mut index = EmbeddingIndex::new("m".to_string(), 2);
        index.upsert("exact::match".to_string(), "h1".to_string(), vec![1.0, 0.0]);
        index.upsert(
            "orthogonal::match".to_string(),
            "h2".to_string(),
            vec![0.0, 1.0],
        );
        index.upsert(
            "opposite::match".to_string(),
            "h3".to_string(),
            vec![-1.0, 0.0],
        );

        let results = index.search(&[1.0, 0.0], 3);
        let keys: Vec<&str> = results.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["exact::match", "orthogonal::match", "opposite::match"]
        );
        assert!(results[0].1 > results[1].1);
        assert!(results[1].1 > results[2].1);
    }

    #[test]
    fn remove_missing_drops_entries_not_in_current_keys() {
        let mut index = EmbeddingIndex::new("m".to_string(), 1);
        index.upsert("keep".to_string(), "h".to_string(), vec![1.0]);
        index.upsert("drop".to_string(), "h".to_string(), vec![1.0]);

        let mut keep: BTreeSet<String> = BTreeSet::new();
        keep.insert("keep".to_string());
        index.remove_missing(&keep);

        assert_eq!(index.search(&[1.0], 10).len(), 1);
        assert_eq!(index.search(&[1.0], 10)[0].0, "keep");
    }
}
