use std::sync::Arc;

use async_trait::async_trait;
use forge_core::embed::Embedder;
use forge_core::error::ForgeError;
use tokio::sync::{mpsc, oneshot};

use crate::backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};

enum Job {
    Decide(
        String,
        Vec<String>,
        oneshot::Sender<Result<Decision, BackendError>>,
    ),
    Embed(
        Vec<String>,
        oneshot::Sender<Result<Vec<Vec<f32>>, BackendError>>,
    ),
    Extract(
        String,
        String,
        oneshot::Sender<Result<String, BackendError>>,
    ),
    ToolCall(
        String,
        String,
        oneshot::Sender<Result<Option<NeedleToolCall>, BackendError>>,
    ),
    Info(oneshot::Sender<Result<(String, usize), BackendError>>),
}

pub struct NeedleEngine {
    tx: mpsc::Sender<Job>,
}

impl NeedleEngine {
    pub fn spawn(mut backend: impl NeedleBackend) -> Self {
        let (tx, mut rx) = mpsc::channel::<Job>(64);
        std::thread::Builder::new()
            .name("needle-engine".to_string())
            .spawn(move || {
                let mut loaded: Option<Result<(), BackendError>> = None;
                while let Some(job) = rx.blocking_recv() {
                    // Lazy load once; a failed load is reported per job,
                    // retried only when the failure was WeightsMissing
                    // (weights may appear after `forge init`).
                    if !matches!(loaded, Some(Ok(()))) {
                        let attempt = backend.load();
                        let retryable = matches!(attempt, Err(BackendError::WeightsMissing(_)));
                        if attempt.is_ok() || !retryable {
                            loaded = Some(attempt);
                        } else {
                            respond_err(job, attempt.err());
                            continue;
                        }
                    }
                    if let Some(Err(_)) = &loaded {
                        respond_err(job, Some(BackendError::NotLoaded));
                        continue;
                    }
                    match job {
                        Job::Decide(task, opts, tx) => {
                            let _ = tx.send(backend.decide(&task, &opts));
                        }
                        Job::Embed(texts, tx) => {
                            let _ = tx.send(backend.embed(&texts));
                        }
                        Job::Extract(text, schema, tx) => {
                            let _ = tx.send(backend.extract(&text, &schema));
                        }
                        Job::ToolCall(prompt, tools, tx) => {
                            let _ = tx.send(backend.tool_call(&prompt, &tools));
                        }
                        Job::Info(tx) => {
                            let _ = tx.send(Ok((backend.model_id(), backend.dimensions())));
                        }
                    }
                }
            })
            .ok(); // spawn failure surfaces as closed-channel errors below
        Self { tx }
    }

    async fn send<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, BackendError>>) -> Job,
    ) -> Result<T, ForgeError> {
        let (otx, orx) = oneshot::channel();
        self.tx
            .send(make(otx))
            .await
            .map_err(|_| ForgeError::router("needle engine thread is gone"))?;
        orx.await
            .map_err(|_| ForgeError::router("needle engine dropped the reply"))?
            .map_err(|e| ForgeError::router(format!("needle: {e}")))
    }

    pub async fn decide(&self, task: String, options: Vec<String>) -> Result<Decision, ForgeError> {
        self.send(|tx| Job::Decide(task, options, tx)).await
    }
    pub async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.send(|tx| Job::Embed(texts, tx)).await
    }
    pub async fn extract(&self, text: String, schema_json: String) -> Result<String, ForgeError> {
        self.send(|tx| Job::Extract(text, schema_json, tx)).await
    }
    pub async fn tool_call(
        &self,
        prompt: String,
        tools_json: String,
    ) -> Result<Option<NeedleToolCall>, ForgeError> {
        self.send(|tx| Job::ToolCall(prompt, tools_json, tx)).await
    }
    pub async fn info(&self) -> Result<(String, usize), ForgeError> {
        self.send(Job::Info).await
    }
}

fn respond_err(job: Job, err: Option<BackendError>) {
    let e = err.unwrap_or(BackendError::NotLoaded);
    match job {
        Job::Decide(_, _, tx) => drop(tx.send(Err(e))),
        Job::Embed(_, tx) => drop(tx.send(Err(e))),
        Job::Extract(_, _, tx) => drop(tx.send(Err(e))),
        Job::ToolCall(_, _, tx) => drop(tx.send(Err(e))),
        Job::Info(tx) => drop(tx.send(Err(e))),
    }
}

/// Adapter: NeedleEngine as a forge-core Embedder. `model_id`/`dimensions`
/// are fetched once from the engine at construction and cached, since
/// `Embedder`'s accessors are synchronous but the engine is not.
pub struct EngineEmbedder {
    engine: Arc<NeedleEngine>,
    model_id: String,
    dimensions: usize,
}

impl EngineEmbedder {
    pub async fn new(engine: Arc<NeedleEngine>) -> Result<Self, ForgeError> {
        let (model_id, dimensions) = engine.info().await?;
        Ok(Self {
            engine,
            model_id,
            dimensions,
        })
    }
}

#[async_trait]
impl Embedder for EngineEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.engine.embed(texts.to_vec()).await
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }
    fn model_id(&self) -> String {
        self.model_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_backend::HashBackend;

    #[tokio::test]
    async fn engine_decides_via_backend() {
        let engine = NeedleEngine::spawn(HashBackend::new());
        let d = engine
            .decide(
                "refactor the parser module".to_string(),
                vec!["parser-model".to_string(), "other".to_string()],
            )
            .await
            .expect("decides");
        assert_eq!(d.choice, "parser-model"); // token overlap wins
        assert!(d.confidence > 0.0 && d.confidence <= 1.0);
    }

    #[tokio::test]
    async fn engine_embeds_deterministically() {
        let engine = NeedleEngine::spawn(HashBackend::new());
        let a = engine
            .embed(vec!["hello world".to_string()])
            .await
            .expect("embeds");
        let b = engine
            .embed(vec!["hello world".to_string()])
            .await
            .expect("embeds");
        assert_eq!(a, b);
        let (_, dims) = engine.info().await.expect("info");
        assert_eq!(a[0].len(), dims);
    }

    #[tokio::test]
    async fn engine_survives_no_options() {
        let engine = NeedleEngine::spawn(HashBackend::new());
        let err = engine.decide("task".to_string(), vec![]).await;
        assert!(err.is_err()); // Declined maps to ForgeError, caller falls back
    }
}
