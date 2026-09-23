pub mod backend;
pub mod engine;
pub mod hash_backend;
pub mod router;
pub mod weights;

pub use backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};
pub use engine::{EngineEmbedder, NeedleEngine};
pub use hash_backend::HashBackend;
pub use router::NeedleRouter;
pub use weights::{WeightsSpec, WeightsStatus, ensure_weights, spec_for, verify, weights_path};

/// Build an engine from `[needle]` config: resolve where weights for
/// `needle.variant` should live (`weights::weights_path`), then pick a
/// backend.
///
/// With the `ffi` feature enabled (Task 8's real libneedle backend) and a
/// verified weights file already on disk, this would load `FfiBackend`;
/// until then — and always without `ffi` — it spawns `UnavailableBackend`
/// with the real resolved path, so `BackendError::WeightsMissing` names
/// exactly where `forge init` should have put the weights. Any resolution
/// failure (e.g. an unpinned variant with no cached override) degrades to
/// the same `UnavailableBackend` with a best-effort path rather than
/// erroring `engine_from_config` itself — routing built on it still falls
/// back to the configured fallback router instead of guessing.
pub fn engine_from_config(
    needle: &forge_config::NeedleConfig,
) -> Result<NeedleEngine, forge_core::error::ForgeError> {
    let path = weights::weights_path(needle).unwrap_or_else(|_| weights::best_effort_path(needle));

    #[cfg(feature = "ffi")]
    {
        // Task 8 seam: once `FfiBackend` exists, this arm becomes
        //   if path.is_file() && weights::verify(&path, spec.sha256)? {
        //       Ok(NeedleEngine::spawn(backend::FfiBackend::load(&path)?))
        //   } else { ...UnavailableBackend as below }
        // `ffi` has no backend yet, so behavior is identical to the
        // `not(ffi)` arm below.
        Ok(NeedleEngine::spawn(backend::UnavailableBackend::new(path)))
    }
    #[cfg(not(feature = "ffi"))]
    {
        Ok(NeedleEngine::spawn(backend::UnavailableBackend::new(path)))
    }
}
