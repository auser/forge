//! Pure-Rust local inference for Forge.
//!
//! This crate runs models **in-process**: no daemon to start, no server to
//! reach, no Python, and — unlike the embedded Needle brain — no native
//! toolchain and no per-platform static blob to fetch at build time. The
//! runtime is [`rten`](https://github.com/robertknight/rten), a pure-Rust
//! ONNX runtime; the weights are pinned by SHA-256 and fetched on demand.
//!
//! The shape follows `rusto-rs`, which made the same move for its OCR
//! engine: a runtime-agnostic session trait, a concrete backend behind it,
//! and a native path kept as an *option* rather than a requirement.
//!
//! ## What is here
//!
//! - [`session::InferenceSession`] — the seam. Everything above it depends
//!   on this trait, never on rten.
//! - [`rten_backend::RtenEncoder`] — the backend that actually runs a model.
//! - [`model`] — the pinned catalog: what forge will run, and the SHA-256
//!   of every byte.
//! - [`weights`] — fetch, verify, and place model files.
//! - [`cache`] — one loaded model per (model, directory) per process.
//! - [`embedder::RtenEmbedder`] — the adapter onto `forge-core`'s
//!   `Embedder` trait, which the semantic project index is written against.
//!
//! ## Why it exists
//!
//! Forge's embedded Needle brain is a C++ engine behind an FFI boundary,
//! and it does not link on every platform forge ships: three of the five
//! release targets ship without it. The "local model" story in the
//! generation plane is a separately-running server — Ollama, llama.cpp,
//! LM Studio — which is a daemon forge does not own.
//!
//! A pure-Rust runtime fixes both: one code path, every platform, no
//! daemon, no native dependency. The cost is that models must be ONNX
//! exports, and CPU-only — there is no GPU backend.
//!
//! See `docs/superpowers/specs/2026-10-30-local-inference-runtime-exploration.md`
//! for the measurements behind these claims.

pub mod cache;
pub mod embedder;
pub mod model;
pub mod rten_backend;
pub mod session;
pub mod weights;

pub use embedder::{RtenEmbedder, embedder_from_config, weights_dir};
pub use model::{DEFAULT_MODEL_ID, MODELS, ModelSpec, Pooling, spec_for, spec_for_or_default};
pub use rten_backend::RtenEncoder;
pub use session::InferenceSession;
pub use weights::{WeightsStatus, default_weights_dir, ensure_weights};

/// Version of the vector-production contract.
///
/// Bumped when anything that changes the *meaning* of a vector changes:
/// pooling, normalisation, or the prompt shape. The stored semantic index
/// records the model id, so a bump invalidates old indexes instead of
/// mixing incompatible vectors.
const VECTOR_VERSION: &str = "v1";

/// The identity a local model's vectors are recorded under.
///
/// Every stored index carries this string. Two models with different ids
/// produce different vectors, and a change to this format must change this
/// string — otherwise an old index would be silently compared against new
/// vectors.
pub fn model_id(spec: &ModelSpec) -> String {
    format!("rten:{}:{}:{VECTOR_VERSION}", spec.id, spec.dimensions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_id_names_the_model_and_the_vector_contract() {
        let spec = spec_for(DEFAULT_MODEL_ID).expect("default model is pinned");
        let id = model_id(spec);
        assert!(id.starts_with("rten:"), "id: {id}");
        assert!(id.contains(spec.id), "id: {id}");
        assert!(id.contains(&spec.dimensions.to_string()), "id: {id}");
    }

    #[test]
    fn different_models_have_different_ids() {
        let a = model_id(spec_for("bge-small-en-v1.5-int8").expect("pinned"));
        let b = model_id(spec_for("bge-small-en-v1.5").expect("pinned"));
        assert_ne!(a, b, "a stored index must not mix the two models");
    }

    #[test]
    fn spec_for_or_default_falls_back_to_the_default() {
        assert_eq!(
            spec_for_or_default("").expect("default").id,
            DEFAULT_MODEL_ID
        );
        assert_eq!(
            spec_for_or_default("   ").expect("default").id,
            DEFAULT_MODEL_ID
        );
        assert!(spec_for_or_default("no-such-model").is_err());
    }
}
