//! The `Embedder` adapter over an [`InferenceSession`].
//!
//! `forge-core` owns the `Embedder` trait because the semantic project
//! index is written against it, not against any particular runtime. This
//! module is the adapter that makes a local session usable through that
//! trait, so the semantic index — and every caller of it — gains a local
//! embedder without knowing that rten exists.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use forge_config::EmbeddingConfig;
use forge_core::ForgeError;
use forge_core::embed::Embedder;

use crate::cache::session_for;
use crate::model::{ModelSpec, spec_for_or_default};
use crate::session::InferenceSession;

/// A local embedder backed by a loaded model.
pub struct RtenEmbedder {
    session: Arc<dyn InferenceSession>,
}

impl RtenEmbedder {
    /// Wrap an already-loaded session.
    pub fn new(session: Arc<dyn InferenceSession>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl Embedder for RtenEmbedder {
    /// Embed a batch of texts.
    ///
    /// The session owns the thread the inference runs on, so this does not
    /// park an async runtime worker for the duration of the matmuls.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.session.embed(texts).await
    }

    fn dimensions(&self) -> usize {
        self.session.dimensions()
    }

    fn model_id(&self) -> String {
        self.session.model_id()
    }
}

/// Where a model's files live: the operator's override when set, else the
/// per-model directory under the forge model cache.
pub fn weights_dir(config: &EmbeddingConfig, spec: &ModelSpec) -> PathBuf {
    if !config.weights_dir.trim().is_empty() {
        PathBuf::from(&config.weights_dir)
    } else {
        crate::weights::default_weights_dir(spec)
    }
}

/// Build the local embedder described by `config`.
///
/// `allow_download` is the caller's egress decision, not a config value:
/// `forge --local-only` hard-blocks all network, and a model that is not
/// already on disk cannot be fetched under it. The caller owns that policy
/// because it is the layer that knows about `local_only`; this crate only
/// runs models.
///
/// Returns `Ok(None)` when the model is simply not on disk and downloading
/// is not allowed — the same graceful degradation the Needle embedder shows
/// when this build has no brain. The caller reports "no semantic search"
/// rather than failing the whole process.
///
/// An unpinned model id is *not* degraded: it is a configuration mistake,
/// and it returns an error naming the ids that are pinned.
pub async fn embedder_from_config(
    config: &EmbeddingConfig,
    allow_download: bool,
) -> Result<Option<RtenEmbedder>, ForgeError> {
    let spec = spec_for_or_default(&config.model)?;

    let dir = weights_dir(config, spec);
    let autofetch = config.autofetch && allow_download;
    let status = crate::weights::ensure_weights(spec, &dir, autofetch).await?;
    match status {
        crate::weights::WeightsStatus::Present(_) | crate::weights::WeightsStatus::Fetched(_) => {}
        crate::weights::WeightsStatus::Missing { path, reason } => {
            tracing::warn!(
                reason = %reason,
                path = %path.display(),
                "local embedding model unavailable; semantic search is off"
            );
            return Ok(None);
        }
    }

    let session = session_for(spec, &dir).await?;
    Ok(Some(RtenEmbedder::new(session)))
}
