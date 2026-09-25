pub mod backend;
pub mod engine;
#[cfg(feature = "ffi")]
pub mod ffi_backend;
pub mod hash_backend;
pub mod router;
pub mod weights;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub use backend::{BackendError, Decision, ENGINE_REMEDY, NeedleBackend, NeedleToolCall};
pub use engine::{EngineEmbedder, NeedleEngine};
#[cfg(feature = "ffi")]
pub use ffi_backend::FfiBackend;
pub use hash_backend::HashBackend;
pub use router::NeedleRouter;
pub use weights::{WeightsSpec, WeightsStatus, ensure_weights, spec_for, verify, weights_path};

/// Build an engine from `[needle]` config: resolve where weights for
/// `needle.variant` should live (`weights::weights_path`), then pick a
/// backend.
///
/// With the `ffi` feature enabled this spawns the real [`FfiBackend`] over
/// `libneedle`; without it — the default — it spawns `UnavailableBackend`,
/// whose `load()` reports [`BackendError::EngineMissing`]: *this build has no
/// engine*, carrying [`ENGINE_REMEDY`]. Not a weights complaint — a
/// backend-less binary's `forge init` skips the weights fetch on purpose, so
/// blaming the weights would send the reader in a circle. Either way the
/// `FallbackRouter` wrapping the needle router degrades to static rules;
/// forge is fully functional, and the FFI backend is an upgrade, not a
/// requirement.
///
/// Note what this deliberately does *not* do: it never checks whether the
/// weights exist here. `FfiBackend::load()` reports a missing file as
/// `WeightsMissing`, which the engine retries on every job, so weights that
/// appear after the process started (a concurrent `forge init`) start working
/// without a restart. A one-shot existence check at construction would
/// instead pin the process to "unavailable" for its whole life.
///
/// Any path-resolution failure (e.g. an unpinned variant with no cached
/// override) degrades to `UnavailableBackend` with a best-effort path rather
/// than erroring `engine_from_config` itself — routing built on it still falls
/// back to the configured fallback router instead of guessing.
pub fn engine_from_config(
    needle: &forge_config::NeedleConfig,
) -> Result<NeedleEngine, forge_core::error::ForgeError> {
    #[cfg(feature = "ffi")]
    {
        let path =
            weights::weights_path(needle).unwrap_or_else(|_| weights::best_effort_path(needle));
        Ok(NeedleEngine::spawn(ffi_backend::FfiBackend::new(path)))
    }
    #[cfg(not(feature = "ffi"))]
    {
        // No engine in this build, so where the weights would live is not
        // information anyone can act on — and saying it invites exactly the
        // wrong remedy. `select_engine` still keys its cache on the resolved
        // path, which is the only place that path matters here.
        let _ = needle;
        Ok(NeedleEngine::spawn(backend::UnavailableBackend))
    }
}

/// Process-wide engine cache, keyed by what actually distinguishes one
/// engine from another: the backend kind and, for the real backend, the
/// resolved weights path.
fn engine_cache() -> &'static Mutex<HashMap<String, Arc<NeedleEngine>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<NeedleEngine>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Select a Needle engine per `[needle]` config: env `FORGE_NEEDLE_BACKEND=hash`
/// picks the deterministic test/BDD `HashBackend`; otherwise
/// `engine_from_config` resolves the real backend (`FfiBackend` under the
/// `ffi` feature, `UnavailableBackend` without it). This is exactly
/// `build_router`'s "needle" arm in forge-providers, factored out here so
/// that router construction and any other caller needing "the configured
/// needle engine" (like `forge graph build`'s embedding step, via
/// `engine_if_available` below) share one place that knows the selection
/// rule instead of duplicating the `match`.
///
/// **Engines are shared, not duplicated.** Every `NeedleEngine` owns a
/// dedicated OS thread and, under `ffi`, its own loaded copy of the weights
/// (tens of MB); a single `forge serve` process legitimately asks for "the
/// needle engine" from more than one place (the router, and the agent
/// loop's dispatch fast path), and spawning one engine each would double
/// both. So an engine is cached per backend kind + resolved weights path
/// and handed out as an `Arc` clone: same config in a process → the very
/// same engine, which also means the FFI backend's single-bound-weights
/// global (`BOUND_WEIGHTS`/`LIVE` in `ffi_backend.rs`) sees one bind for one
/// archive instead of two callers racing to rebind it. Different configs
/// (a different variant or `weights_path`) still get their own engine.
///
/// The cache holds `Arc`s for the life of the process; nothing here evicts,
/// because "the configured engine" is process-scoped state in every caller
/// Forge has (a CLI command or a server's service) and an engine whose
/// thread is still parked costs a thread, not weights — `load()` is lazy,
/// performed by the engine thread on its first real job.
pub fn select_engine(
    needle: &forge_config::NeedleConfig,
) -> Result<Arc<NeedleEngine>, forge_core::error::ForgeError> {
    let hash_backend = matches!(std::env::var("FORGE_NEEDLE_BACKEND").as_deref(), Ok("hash"));
    // Mirrors `engine_from_config`'s own resolution, so two callers with the
    // same config compute the same key.
    let key = if hash_backend {
        "hash".to_string()
    } else {
        let path =
            weights::weights_path(needle).unwrap_or_else(|_| weights::best_effort_path(needle));
        format!("config:{}", path.display())
    };

    let mut cache = engine_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(engine) = cache.get(&key) {
        return Ok(Arc::clone(engine));
    }
    let engine = Arc::new(if hash_backend {
        NeedleEngine::spawn(HashBackend::new())
    } else {
        engine_from_config(needle)?
    });
    cache.insert(key, Arc::clone(&engine));
    Ok(engine)
}

/// Construct a Needle engine and confirm it can actually answer — for
/// callers that need a real, working brain rather than merely one that
/// spawned successfully.
///
/// `select_engine`/`engine_from_config` always succeed even when the
/// resolved backend is `UnavailableBackend` (no `ffi` feature built, or
/// weights not yet fetched): construction is pure and never touches the
/// filesystem, so nothing there can fail. But `UnavailableBackend::load()`
/// always errors, and the engine's job loop (see `engine.rs`) runs `load()`
/// before answering *any* job — including a bare `info()` — so a cheap,
/// near-instant `info()` call under a short timeout is a reliable proxy for
/// "is this brain actually usable," without adding a separate
/// is-available accessor that every `NeedleBackend` impl would have to
/// implement honestly (and that `FfiBackend` couldn't answer without
/// attempting the load anyway). This is the seam this module exposes for
/// that check; `build_router` deliberately does *not* use it, since a
/// router is expected to construct even when unavailable and degrade at
/// request time via `FallbackRouter`/`ThresholdRouter` instead.
pub async fn engine_if_available(config: &forge_config::Config) -> Option<Arc<NeedleEngine>> {
    let engine = select_engine(&config.needle).ok()?;
    let timeout = std::time::Duration::from_millis(config.router_timeout_ms);
    match tokio::time::timeout(timeout, engine.info()).await {
        Ok(Ok(_)) => Some(engine),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One engine per config per process: the router and the agent loop's
    /// fast path must not each spawn their own thread (and, under `ffi`,
    /// their own weights load).
    #[test]
    #[serial_test::serial]
    fn select_engine_shares_one_engine_per_config() {
        // SAFETY: single-threaded test, serialized against other env tests.
        unsafe { std::env::set_var("FORGE_NEEDLE_BACKEND", "hash") };
        let needle = forge_config::NeedleConfig::default();

        let first = select_engine(&needle).expect("engine");
        let second = select_engine(&needle).expect("engine");
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same config must hand back the same engine"
        );

        unsafe { std::env::remove_var("FORGE_NEEDLE_BACKEND") };
        // A different backend kind is a different engine.
        let real = select_engine(&needle).expect("engine");
        assert!(!Arc::ptr_eq(&first, &real));
        let real_again = select_engine(&needle).expect("engine");
        assert!(Arc::ptr_eq(&real, &real_again));
    }
}
