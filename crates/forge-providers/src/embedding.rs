use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_config::{Config, EmbeddingConfig};
use forge_core::ForgeError;
use forge_core::embed::Embedder;
use serde::{Deserialize, Serialize};

use crate::local_only::{EgressPolicy, endpoint_is_local, error_detail};

const PROMPT_VERSION: &str = "code-retrieval-v1";

/// The model EmbeddingGemma 2 is asked for when `embedding.model` is unset.
///
/// `embedding.model` is provider-specific, so it defaults to empty and each
/// provider supplies its own default. Naming it here keeps the default in
/// one place and keeps it out of `forge-config`, which must not know what
/// any one provider's models are called.
const DEFAULT_EMBEDDINGGEMMA2_MODEL: &str = "embeddinggemma-2";

/// Resolve the semantic-search embedder independently from the decision
/// router. `None` means no embedder is usable: the default Needle embedder
/// is not installed, or the local model named by `embedding.provider =
/// "rten"` is not on disk.
pub async fn embedder_from_config(
    config: &Config,
) -> Result<Option<Arc<dyn Embedder>>, ForgeError> {
    match config.embedding.provider.as_str() {
        "needle" => {
            let Some(engine) = forge_needle::engine_if_available(config).await else {
                return Ok(None);
            };
            Ok(Some(Arc::new(
                forge_needle::EngineEmbedder::new(engine).await?,
            )))
        }
        "embeddinggemma2" => Ok(Some(Arc::new(EmbeddingGemma2::new(
            &config.embedding,
            EgressPolicy::from_config(config),
        )?))),
        "rten" => {
            // `local_only` hard-blocks all network, so the pinned model may
            // only be *used* under it, never fetched. A model that is not
            // already on disk leaves semantic search off. This is the same
            // policy every other client in this file is built through.
            let egress = EgressPolicy::from_config(config);
            let Some(embedder) = forge_inference::embedder_from_config(
                &config.embedding,
                egress == EgressPolicy::Unrestricted,
            )
            .await?
            else {
                return Ok(None);
            };
            Ok(Some(Arc::new(embedder)))
        }
        other => Err(ForgeError::config(format!(
            "unsupported embedding provider {other:?}"
        ))),
    }
}

struct EmbeddingGemma2 {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    dimensions: usize,
}

impl EmbeddingGemma2 {
    fn new(config: &EmbeddingConfig, egress: EgressPolicy) -> Result<Self, ForgeError> {
        let endpoint = format!("{}/embeddings", config.base_url.trim_end_matches('/'));
        if egress == EgressPolicy::LocalOnly && !endpoint_is_local(&endpoint) {
            return Err(ForgeError::config(format!(
                "local_only refuses non-local embedding endpoint {endpoint}"
            )));
        }
        let client = egress
            .client(Duration::from_secs(120))
            .map_err(|e| ForgeError::provider(format!("building embedding client: {e}")))?;
        Ok(Self {
            client,
            endpoint,
            model: if config.model.trim().is_empty() {
                DEFAULT_EMBEDDINGGEMMA2_MODEL.to_string()
            } else {
                config.model.clone()
            },
            dimensions: config.dimensions,
        })
    }

    async fn request(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, ForgeError> {
        let expected = input.len();
        let response = self
            .client
            .post(&self.endpoint)
            .json(&EmbeddingRequest {
                model: &self.model,
                input,
            })
            .send()
            .await
            .map_err(|e| {
                ForgeError::provider(format!("EmbeddingGemma 2 request: {}", error_detail(&e)))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ForgeError::provider(format!(
                "EmbeddingGemma 2 returned HTTP {status}"
            )));
        }
        let mut data = response
            .json::<EmbeddingResponse>()
            .await
            .map_err(|e| ForgeError::provider(format!("decoding EmbeddingGemma 2 response: {e}")))?
            .data;
        data.sort_by_key(|item| item.index);
        if data.len() != expected
            || data
                .iter()
                .enumerate()
                .any(|(index, item)| item.index != index)
        {
            return Err(ForgeError::provider(format!(
                "EmbeddingGemma 2 returned {} vectors for {expected} inputs",
                data.len()
            )));
        }
        data.into_iter()
            .map(|item| truncate_and_normalize(item.embedding, self.dimensions))
            .collect()
    }
}

#[async_trait]
impl Embedder for EmbeddingGemma2 {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.embed_documents(texts).await
    }

    async fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.request(
            texts
                .iter()
                .map(|text| format!("title: none | text: {text}"))
                .collect(),
        )
        .await
    }

    async fn embed_queries(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.request(
            texts
                .iter()
                .map(|text| format!("task: code retrieval | query: {text}"))
                .collect(),
        )
        .await
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn model_id(&self) -> String {
        format!(
            "embeddinggemma2:{}:{}:{PROMPT_VERSION}",
            self.model, self.dimensions
        )
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: Vec<String>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

fn truncate_and_normalize(mut vector: Vec<f32>, dimensions: usize) -> Result<Vec<f32>, ForgeError> {
    if vector.len() < dimensions || vector.iter().any(|value| !value.is_finite()) {
        return Err(ForgeError::provider(format!(
            "EmbeddingGemma 2 returned an invalid {}-value vector; need {dimensions} finite values",
            vector.len()
        )));
    }
    vector.truncate(dimensions);
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return Err(ForgeError::provider(
            "EmbeddingGemma 2 returned a zero or non-finite vector",
        ));
    }
    vector.iter_mut().for_each(|value| *value /= norm);
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn prefixes_roles_truncates_and_normalizes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(body_json(json!({
                "model": "embeddinggemma-2-q8",
                "input": ["task: code retrieval | query: find auth"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"index": 0, "embedding": [3.0, 4.0, 9.0]}]
            })))
            .mount(&server)
            .await;
        let config = EmbeddingConfig {
            provider: "embeddinggemma2".into(),
            base_url: format!("{}/v1", server.uri()),
            model: "embeddinggemma-2-q8".into(),
            dimensions: 2,
            weights_dir: String::new(),
            autofetch: true,
        };
        let embedder = EmbeddingGemma2::new(&config, EgressPolicy::Unrestricted).unwrap();
        let vectors = embedder.embed_queries(&["find auth".into()]).await.unwrap();
        assert_eq!(vectors, vec![vec![0.6, 0.8]]);
    }

    #[test]
    fn invalid_vectors_are_rejected_before_indexing() {
        assert!(truncate_and_normalize(vec![1.0], 2).is_err());
        assert!(truncate_and_normalize(vec![0.0, 0.0], 2).is_err());
        assert!(truncate_and_normalize(vec![f32::NAN, 1.0], 2).is_err());
    }

    #[tokio::test]
    async fn an_unset_model_falls_back_to_the_provider_default() {
        // `embedding.model` is provider-specific and defaults to empty, so
        // each provider names its own default. An unset model must reach
        // the endpoint as that default, not as an empty string.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(body_json(json!({
                "model": "embeddinggemma-2",
                "input": ["task: code retrieval | query: find auth"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"index": 0, "embedding": [3.0, 4.0, 9.0]}]
            })))
            .mount(&server)
            .await;
        let config = EmbeddingConfig {
            provider: "embeddinggemma2".into(),
            base_url: format!("{}/v1", server.uri()),
            model: String::new(),
            dimensions: 2,
            weights_dir: String::new(),
            autofetch: true,
        };
        let embedder = EmbeddingGemma2::new(&config, EgressPolicy::Unrestricted).unwrap();
        let vectors = embedder.embed_queries(&["find auth".into()]).await.unwrap();
        assert_eq!(vectors, vec![vec![0.6, 0.8]]);
        // The recorded model id must stay stable, so an index built before
        // the default moved is still recognised as the same model.
        assert!(
            embedder
                .model_id()
                .starts_with("embeddinggemma2:embeddinggemma-2:"),
            "model id: {}",
            embedder.model_id()
        );
    }

    #[tokio::test]
    async fn rten_provider_degrades_to_no_embedder_when_the_model_is_absent() {
        // `embedding.provider = "rten"` with the model nowhere on disk and
        // autofetch off must not fail the caller: semantic search is off,
        // everything else in forge still works. This is the same graceful
        // degradation the Needle embedder shows when the engine is not
        // linked into this build.
        let dir = tempfile::tempdir().expect("tmp");
        let mut config = forge_config::Config::default();
        config.embedding.provider = "rten".to_string();
        config.embedding.model = "bge-small-en-v1.5-int8".to_string();
        config.embedding.weights_dir = dir.path().join("absent").display().to_string();
        config.embedding.autofetch = false;

        let embedder = embedder_from_config(&config).await.expect("no error");
        assert!(
            embedder.is_none(),
            "an absent local model means no embedder, not an error"
        );
    }

    #[tokio::test]
    async fn local_only_blocks_the_model_download() {
        // `local_only` hard-blocks all network. A model that is not already
        // on disk must therefore leave semantic search off rather than
        // being fetched — the download is the one network call this
        // provider would ever make.
        let dir = tempfile::tempdir().expect("tmp");
        let mut config = forge_config::Config::default();
        config.embedding.provider = "rten".to_string();
        config.embedding.model = "bge-small-en-v1.5-int8".to_string();
        config.embedding.weights_dir = dir.path().join("absent").display().to_string();
        config.embedding.autofetch = true;
        config.local_only = true;

        let embedder = embedder_from_config(&config).await.expect("no error");
        assert!(
            embedder.is_none(),
            "local_only must not fetch the model, so there is no embedder"
        );
        assert!(
            !dir.path().join("absent").exists(),
            "nothing should have been downloaded"
        );
    }

    #[tokio::test]
    async fn rten_provider_rejects_a_model_id_that_is_not_pinned() {
        // A typo is a configuration mistake, not a missing file: it must be
        // loud, and the error must name the ids that are pinned.
        let mut config = forge_config::Config::default();
        config.embedding.provider = "rten".to_string();
        config.embedding.model = "bge-medium".to_string();

        let error = match embedder_from_config(&config).await {
            Ok(_) => panic!("an unpinned model id must be an error"),
            Err(e) => e,
        };
        let message = error.to_string();
        assert!(message.contains("bge-medium"), "message: {message}");
        assert!(
            message.contains("bge-small-en-v1.5-int8"),
            "the message must name a pinned model: {message}"
        );
    }

    #[tokio::test]
    async fn an_unknown_provider_is_rejected() {
        let mut config = forge_config::Config::default();
        config.embedding.provider = "nope".to_string();
        let error = match embedder_from_config(&config).await {
            Ok(_) => panic!("an unknown provider must be an error"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("nope"));
    }
}
