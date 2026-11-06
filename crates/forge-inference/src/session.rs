//! The seam between "a model is loaded" and "what the model is for".
//!
//! Everything above this trait — the [`Embedder`](forge_core::embed::Embedder)
//! adapter, the semantic project index, the MCP graph tools — depends on
//! [`InferenceSession`], never on `rten` directly. A different runtime (a
//! hardware accelerator, a different pure-Rust engine) implements the trait
//! and drops in without touching the pipeline above it.
//!
//! This is the same move `rusto-rs` makes: it defines an `InferenceSession`
//! trait, holds either an rten session or a caller-supplied one behind it,
//! and keeps the OpenCV path an *option* rather than a requirement. The
//! difference is that forge already has a second such seam higher up —
//! [`forge_core::embed::Embedder`] — and this trait sits underneath it.
//!
//! **Scope.** Today this trait covers the *embedding* capability only. It
//! is deliberately not a general "run any model" trait: autoregressive
//! generation needs sampling, stop conditions and a KV cache, which is a
//! different shape of contract and will arrive as its own trait rather than
//! as extra methods nobody calls.

use forge_core::ForgeError;

/// A loaded model, ready to answer inference requests.
///
/// Implementations are shared across the process and must therefore be
/// `Send + Sync`. The trait takes `&self` rather than `&mut self` for the
/// same reason: a session that can serve concurrent callers without a lock
/// is the difference between an inference call that queues behind other
/// callers' inference calls and one that does not.
///
/// The method is async because the work is CPU-bound and belongs on a
/// thread of its own, not on an async runtime's worker.
#[async_trait::async_trait]
pub trait InferenceSession: Send + Sync {
    /// Embed a batch of texts.
    ///
    /// The returned vectors are L2-normalised, so a dot product is a cosine
    /// similarity. The slice has one vector per input text, in the same
    /// order.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError>;

    /// Width of each vector this session produces.
    fn dimensions(&self) -> usize;

    /// Stable identity of the model behind this session.
    ///
    /// Stored indexes record this string, and a vector built by one model
    /// must never be mixed with vectors from another. Changing the model,
    /// the pooling, or the normalisation changes this string.
    fn model_id(&self) -> String;
}
