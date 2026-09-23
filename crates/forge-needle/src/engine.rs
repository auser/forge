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
                    // Lazy load once. Never-attempted -> try. Failed with
                    // WeightsMissing -> retry every job (weights may appear
                    // after `forge init`). Failed with anything else ->
                    // sticky: never call load() again, and keep answering
                    // every job with that original error.
                    let status = match loaded.take() {
                        None => backend.load(),
                        Some(Err(BackendError::WeightsMissing(_))) => backend.load(),
                        Some(other) => other,
                    };
                    loaded = Some(status.clone());
                    if let Err(e) = status {
                        respond_err(job, e);
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

fn respond_err(job: Job, err: BackendError) {
    match job {
        Job::Decide(_, _, tx) => drop(tx.send(Err(err))),
        Job::Embed(_, tx) => drop(tx.send(Err(err))),
        Job::Extract(_, _, tx) => drop(tx.send(Err(err))),
        Job::ToolCall(_, _, tx) => drop(tx.send(Err(err))),
        Job::Info(tx) => drop(tx.send(Err(err))),
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::hash_backend::HashBackend;

    /// Backend whose `load()` fails with `WeightsMissing` a fixed number
    /// of times, then succeeds. Lets tests assert the engine keeps
    /// retrying a transient (weights-not-yet-fetched) load failure.
    struct FlakyLoadBackend {
        load_calls: Arc<AtomicUsize>,
        fail_times: usize,
    }

    impl NeedleBackend for FlakyLoadBackend {
        fn load(&mut self) -> Result<(), BackendError> {
            let attempt = self.load_calls.fetch_add(1, Ordering::SeqCst);
            if attempt < self.fail_times {
                Err(BackendError::WeightsMissing(PathBuf::from("/weights")))
            } else {
                Ok(())
            }
        }
        fn model_id(&self) -> String {
            "flaky".to_string()
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn decide(&mut self, _task: &str, options: &[String]) -> Result<Decision, BackendError> {
            Ok(Decision {
                choice: options.first().cloned().unwrap_or_default(),
                confidence: 1.0,
                reason: "flaky-ok".to_string(),
            })
        }
        fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
            Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
        }
        fn extract(&mut self, _text: &str, _schema_json: &str) -> Result<String, BackendError> {
            Ok("{}".to_string())
        }
        fn tool_call(
            &mut self,
            _prompt: &str,
            _tools_json: &str,
        ) -> Result<Option<NeedleToolCall>, BackendError> {
            Ok(None)
        }
    }

    /// Backend whose `load()` always fails with a non-retryable error.
    /// Lets tests assert the engine calls `load()` exactly once and then
    /// answers every later job with the original error, without ever
    /// calling `load()` again.
    struct PermanentFailBackend {
        load_calls: Arc<AtomicUsize>,
    }

    impl NeedleBackend for PermanentFailBackend {
        fn load(&mut self) -> Result<(), BackendError> {
            self.load_calls.fetch_add(1, Ordering::SeqCst);
            Err(BackendError::Inference("corrupt weights".to_string()))
        }
        fn model_id(&self) -> String {
            "permanent-fail".to_string()
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn decide(&mut self, _task: &str, _options: &[String]) -> Result<Decision, BackendError> {
            Err(BackendError::Declined)
        }
        fn embed(&mut self, _texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
            Err(BackendError::Declined)
        }
        fn extract(&mut self, _text: &str, _schema_json: &str) -> Result<String, BackendError> {
            Err(BackendError::Declined)
        }
        fn tool_call(
            &mut self,
            _prompt: &str,
            _tools_json: &str,
        ) -> Result<Option<NeedleToolCall>, BackendError> {
            Err(BackendError::Declined)
        }
    }

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

    #[tokio::test]
    async fn engine_retries_load_on_weights_missing_until_it_succeeds() {
        let load_calls = Arc::new(AtomicUsize::new(0));
        let engine = NeedleEngine::spawn(FlakyLoadBackend {
            load_calls: load_calls.clone(),
            fail_times: 2,
        });

        // First two jobs hit WeightsMissing; the engine must retry load()
        // on each of them rather than giving up.
        let first = engine.decide("t".to_string(), vec!["a".to_string()]).await;
        assert!(first.is_err());
        let second = engine.decide("t".to_string(), vec!["a".to_string()]).await;
        assert!(second.is_err());

        // Third job: load() succeeds, and the job itself succeeds too.
        let third = engine.decide("t".to_string(), vec!["a".to_string()]).await;
        assert!(third.is_ok());

        assert_eq!(load_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn engine_sticky_load_failure_preserves_error_and_stops_retrying() {
        let load_calls = Arc::new(AtomicUsize::new(0));
        let engine = NeedleEngine::spawn(PermanentFailBackend {
            load_calls: load_calls.clone(),
        });

        let first = engine
            .decide("t".to_string(), vec!["a".to_string()])
            .await
            .expect_err("first job fails");
        assert!(first.to_string().contains("corrupt weights"));

        let second = engine
            .decide("t".to_string(), vec!["a".to_string()])
            .await
            .expect_err("second job also fails, without a fresh load attempt");
        assert!(second.to_string().contains("corrupt weights"));

        // load() must have been called exactly once: the failure is
        // sticky, not retried on every job.
        assert_eq!(load_calls.load(Ordering::SeqCst), 1);
    }
}
