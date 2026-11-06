//! Process-wide cache of loaded sessions.
//!
//! **Why this exists.** Loading a model is the expensive part: the graph is
//! read, parsed and prepared, and the weights are materialised. Forge asks
//! for "the configured embedder" from more than one place — the MCP graph
//! tools ask on *every* tool call, the chat host asks once at construction
//! — so without a cache a single `forge serve` would re-load the model
//! per request.
//!
//! The cache mirrors `forge-needle`'s engine cache: keyed by what actually
//! distinguishes one session from another (the model id and the directory
//! the files came from), handing out `Arc` clones so every caller shares
//! one loaded model.
//!
//! Nothing here evicts. A session is process-scoped state in every caller
//! forge has, and an idle session costs resident memory but no CPU and no
//! lock — the session itself is `Send + Sync` and serves concurrent callers
//! without a mutex.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use forge_core::ForgeError;

use crate::model::ModelSpec;
use crate::rten_backend::RtenEncoder;
use crate::session::InferenceSession;

/// The session cache.
fn session_cache() -> &'static Mutex<HashMap<String, Arc<dyn InferenceSession>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<dyn InferenceSession>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The key that distinguishes one session from another.
///
/// The model id alone is not enough: two configs can name the same model
/// but point at different directories (an operator's own copy, a different
/// cache), and those are different models in every way that matters.
fn cache_key(spec: &ModelSpec, dir: &Path) -> String {
    format!("{}:{}", spec.id, dir.display())
}

/// Load a session for `spec` from `dir`, or hand back the one already
/// loaded for this model and directory.
///
/// A session is loaded at most once per (model, directory) pair per
/// process. Concurrent first calls race harmlessly: the loser of the race
/// discards its own freshly loaded model and returns the winner's, which
/// is a wasted load but never a second session in the cache.
pub async fn session_for(
    spec: &'static ModelSpec,
    dir: &Path,
) -> Result<Arc<dyn InferenceSession>, ForgeError> {
    let key = cache_key(spec, dir);

    {
        let cache = session_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = cache.get(&key) {
            return Ok(Arc::clone(session));
        }
    }

    let model_path = dir.join(spec.model.filename);
    let tokenizer_path = dir.join(spec.tokenizer.filename);
    let session: Arc<dyn InferenceSession> =
        Arc::new(RtenEncoder::load(spec, &model_path, &tokenizer_path).await?);

    let mut cache = session_cache().lock().unwrap_or_else(|e| e.into_inner());
    // Another caller may have won the race while this one was loading.
    // Prefer the session that is already cached so every caller shares one.
    if let Some(existing) = cache.get(&key) {
        return Ok(Arc::clone(existing));
    }
    cache.insert(key, Arc::clone(&session));
    Ok(session)
}

/// How many sessions are currently cached. Exposed for tests and for
/// `forge doctor`.
pub fn cached_session_count() -> usize {
    session_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::spec_for;

    #[test]
    fn cache_key_distinguishes_model_and_directory() {
        let spec = spec_for(crate::model::DEFAULT_MODEL_ID).expect("pinned");
        let a = cache_key(spec, Path::new("/one"));
        let b = cache_key(spec, Path::new("/two"));
        assert_ne!(a, b, "a different directory is a different session");

        let other = spec_for("bge-small-en-v1.5").expect("pinned");
        assert_ne!(
            cache_key(spec, Path::new("/one")),
            cache_key(other, Path::new("/one")),
            "a different model is a different session"
        );
    }
}
