pub mod backend;
pub mod engine;
#[cfg(feature = "ffi")]
pub mod ffi_backend;
pub mod hash_backend;
pub mod router;
pub mod weights;

use std::sync::Arc;

pub use backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};
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
/// `libneedle`; without it — the default — it spawns `UnavailableBackend`, so
/// `BackendError::WeightsMissing` names exactly where `forge init` should have
/// put the weights and the `FallbackRouter` wrapping the needle router
/// degrades to static rules. Forge is fully functional either way; the FFI
/// backend is an upgrade, not a requirement.
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
    let path = weights::weights_path(needle).unwrap_or_else(|_| weights::best_effort_path(needle));

    #[cfg(feature = "ffi")]
    {
        Ok(NeedleEngine::spawn(ffi_backend::FfiBackend::new(path)))
    }
    #[cfg(not(feature = "ffi"))]
    {
        Ok(NeedleEngine::spawn(backend::UnavailableBackend::new(path)))
    }
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
pub fn select_engine(
    needle: &forge_config::NeedleConfig,
) -> Result<NeedleEngine, forge_core::error::ForgeError> {
    match std::env::var("FORGE_NEEDLE_BACKEND").as_deref() {
        Ok("hash") => Ok(NeedleEngine::spawn(HashBackend::new())),
        _ => engine_from_config(needle),
    }
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
        Ok(Ok(_)) => Some(Arc::new(engine)),
        _ => None,
    }
}
