use async_trait::async_trait;

use crate::error::ForgeError;

/// Produces vector embeddings for local semantic search and routing.
/// `model_id` versions the vectors: any stored index built with a
/// different `model_id` or `dimensions` must be rebuilt, never mixed.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError>;

    /// Embed corpus entries. Retrieval models may apply a document-specific
    /// instruction; symmetric embedders keep their existing behaviour.
    async fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.embed(texts).await
    }

    /// Embed search text. Retrieval models may apply a query-specific
    /// instruction; symmetric embedders keep their existing behaviour.
    async fn embed_queries(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.embed(texts).await
    }

    fn dimensions(&self) -> usize;
    fn model_id(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedEmbedder;

    #[async_trait]
    impl Embedder for FixedEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
            Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn model_id(&self) -> String {
            "fixed-test".to_string()
        }
    }

    #[tokio::test]
    async fn embedder_returns_one_vector_per_text() {
        let e = FixedEmbedder;
        let out = e
            .embed(&["a".to_string(), "b".to_string()])
            .await
            .expect("embeds");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), e.dimensions());
    }
}
