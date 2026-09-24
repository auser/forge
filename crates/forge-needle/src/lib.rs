pub mod backend;
pub mod engine;
#[cfg(feature = "ffi")]
pub mod ffi_backend;
pub mod hash_backend;
pub mod router;
pub mod weights;

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
