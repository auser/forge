pub mod backend;
pub mod engine;
pub mod hash_backend;
pub mod router;

pub use backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};
pub use engine::{EngineEmbedder, NeedleEngine};
pub use hash_backend::HashBackend;
pub use router::NeedleRouter;

/// Build an engine from `[needle]` config. Until the FFI backend lands
/// (feature `ffi` + weights on disk, Task 8) and variant-aware weights
/// resolution is implemented (Task 6), this always returns an engine whose
/// backend reports `WeightsMissing`, so routing built on it falls back to
/// the configured fallback router instead of guessing.
pub fn engine_from_config(
    _needle: &forge_config::NeedleConfig,
) -> Result<NeedleEngine, forge_core::error::ForgeError> {
    Ok(NeedleEngine::spawn(backend::UnavailableBackend::default()))
}
