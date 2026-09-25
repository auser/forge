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
                    // The caller gave up (router timeout, dropped future)
                    // before we got to their job. Inference is measured in
                    // hundreds of milliseconds, so a queue of abandoned
                    // work would otherwise delay every live job behind it
                    // to produce answers nobody can receive. Drop it and
                    // move on — including skipping a `load()` we'd only
                    // need for this job.
                    if is_abandoned(&job) {
                        tracing::debug!("needle engine skipped a job whose caller gave up");
                        continue;
                    }
                    // Lazy load once. Never-attempted -> try. Failed with
                    // WeightsMissing -> retry every job (weights may appear
                    // after `forge init`). Failed with anything else ->
                    // sticky: never call load() again, and keep answering
                    // every job with that original error.
                    let status = match loaded.take() {
                        None => guarded(|| backend.load()),
                        Some(Err(BackendError::WeightsMissing(_))) => guarded(|| backend.load()),
                        Some(other) => other,
                    };
                    loaded = Some(status.clone());
                    if let Err(e) = status {
                        respond_err(job, e);
                        continue;
                    }
                    match job {
                        Job::Decide(task, opts, tx) => {
                            let _ = tx.send(guarded(|| backend.decide(&task, &opts)));
                        }
                        Job::Embed(texts, tx) => {
                            let _ = tx.send(guarded(|| backend.embed(&texts)));
                        }
                        Job::Extract(text, schema, tx) => {
                            let _ = tx.send(guarded(|| backend.extract(&text, &schema)));
                        }
                        Job::ToolCall(prompt, tools, tx) => {
                            let _ = tx.send(guarded(|| backend.tool_call(&prompt, &tools)));
                        }
                        Job::Info(tx) => {
                            let _ =
                                tx.send(guarded(|| Ok((backend.model_id(), backend.dimensions()))));
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

/// Has the caller for `job` already dropped its receiver? `NeedleEngine::send`
/// holds the `oneshot::Receiver` inside the future it returns, so a
/// `tokio::time::timeout` firing (or any dropped caller future) closes the
/// channel. That is the engine thread's only signal that the answer is no
/// longer wanted.
fn is_abandoned(job: &Job) -> bool {
    match job {
        Job::Decide(_, _, tx) => tx.is_closed(),
        Job::Embed(_, tx) => tx.is_closed(),
        Job::Extract(_, _, tx) => tx.is_closed(),
        Job::ToolCall(_, _, tx) => tx.is_closed(),
        Job::Info(tx) => tx.is_closed(),
    }
}

/// Run one backend call so that a Rust panic inside it becomes a typed
/// `BackendError::Inference` instead of unwinding out of the engine thread
/// and killing the engine for the rest of the process.
///
/// This matters most for `FfiBackend` (feature `ffi`), whose wrapper code
/// does pointer, length and UTF-8 work around a C library. **It is not a
/// sandbox for the C library itself**: `catch_unwind` cannot catch a C++
/// exception thrown across an `extern "C"` boundary, a `SIGSEGV`, or an
/// `abort()`, and a `panic = "abort"` profile disables it entirely. What it
/// does buy is that the *Rust* half of the FFI layer — and any buggy
/// third-party backend — degrades to "this one call failed, callers fall
/// back" instead of "every later needle call errors with `needle engine
/// thread is gone`".
///
/// A caught panic is reported as `Inference`, not `WeightsMissing`, so the
/// engine's load bookkeeping treats it as sticky rather than retrying it on
/// every subsequent job.
fn guarded<T>(call: impl FnOnce() -> Result<T, BackendError>) -> Result<T, BackendError> {
    // AssertUnwindSafe: the backend lives on this thread and is never
    // observed by anyone else, so a torn intermediate state cannot leak to
    // another thread. A panicking backend may leave itself inconsistent and
    // keep failing; that is strictly better than losing the engine.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(payload) => Err(BackendError::Inference(format!(
            "backend panicked: {}",
            panic_message(payload.as_ref())
        ))),
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
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
pub(crate) mod tests {
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

    /// Backend whose `decide` blocks for `delay`, counting how many times it
    /// actually ran. Lets tests prove that work queued behind an abandoned
    /// job is not delayed by it.
    pub(crate) struct SlowBackend {
        pub(crate) decide_calls: Arc<AtomicUsize>,
        pub(crate) delay: std::time::Duration,
    }

    impl NeedleBackend for SlowBackend {
        fn load(&mut self) -> Result<(), BackendError> {
            Ok(())
        }
        fn model_id(&self) -> String {
            "slow".to_string()
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn decide(&mut self, _task: &str, options: &[String]) -> Result<Decision, BackendError> {
            self.decide_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            Ok(Decision {
                choice: options.first().cloned().unwrap_or_default(),
                confidence: 1.0,
                reason: "slow-ok".to_string(),
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

    /// Backend that panics inside `decide`. Lets tests assert the engine
    /// turns a panic into a typed error and stays alive for later jobs.
    struct PanickingBackend;

    impl NeedleBackend for PanickingBackend {
        fn load(&mut self) -> Result<(), BackendError> {
            Ok(())
        }
        fn model_id(&self) -> String {
            "panicking".to_string()
        }
        fn dimensions(&self) -> usize {
            7
        }
        fn decide(&mut self, _task: &str, _options: &[String]) -> Result<Decision, BackendError> {
            panic!("simulated FFI wrapper bug");
        }
        fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
            Ok(texts.iter().map(|_| vec![0.0; 7]).collect())
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

    /// "This build has no engine" is the one load failure that cannot change
    /// while the process lives — only a reinstall fixes it — so it must be
    /// sticky, and every job must keep reporting it verbatim (remedy
    /// included) rather than degrading into a vaguer error.
    #[tokio::test]
    async fn engine_never_retries_a_build_with_no_backend() {
        let engine = NeedleEngine::spawn(crate::backend::UnavailableBackend);

        for attempt in 1..=3 {
            let err = engine
                .decide("t".to_string(), vec!["a".to_string()])
                .await
                .expect_err("a backend-less build cannot route");
            let message = err.to_string();
            assert!(
                message.contains("no embedded inference backend"),
                "attempt {attempt}: {message}"
            );
            assert!(
                message.contains(crate::ENGINE_REMEDY),
                "attempt {attempt} must still carry the remedy: {message}"
            );
            assert!(
                !message.contains("forge init"),
                "attempt {attempt} must not send the reader to `forge init`: {message}"
            );
        }
    }

    #[tokio::test]
    async fn engine_skips_queued_jobs_whose_callers_gave_up() {
        // A slow backend plus tiny caller timeouts: six decides are enqueued
        // back-to-back and all six time out. Every one that is still in the
        // channel when the engine reaches it must be dropped rather than run.
        const ABANDONED: usize = 6;
        let decide_calls = Arc::new(AtomicUsize::new(0));
        let engine = Arc::new(NeedleEngine::spawn(SlowBackend {
            decide_calls: decide_calls.clone(),
            delay: std::time::Duration::from_millis(400),
        }));

        let abandon = |engine: Arc<NeedleEngine>| async move {
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                engine.decide("t".to_string(), vec!["a".to_string()]),
            )
            .await
        };
        // `join!` polls every future to its first await point before any can
        // finish, so all six sends land in the channel together.
        let outcomes = tokio::join!(
            abandon(engine.clone()),
            abandon(engine.clone()),
            abandon(engine.clone()),
            abandon(engine.clone()),
            abandon(engine.clone()),
            abandon(engine.clone()),
        );
        for (i, outcome) in [
            outcomes.0.is_err(),
            outcomes.1.is_err(),
            outcomes.2.is_err(),
            outcomes.3.is_err(),
            outcomes.4.is_err(),
            outcomes.5.is_err(),
        ]
        .into_iter()
        .enumerate()
        {
            assert!(outcome, "caller {i} must time out");
        }

        // A live job now: it must still get a real answer.
        let started = std::time::Instant::now();
        let live = engine
            .decide("t".to_string(), vec!["a".to_string()])
            .await
            .expect("a live caller still gets a real decision");
        let waited = started.elapsed();
        assert_eq!(live.choice, "a");

        // At most two decides ran: the live one, plus at most one that the
        // engine had already dequeued when the timeouts fired (it cannot
        // un-start that one). Whether that race happens is timing-dependent,
        // so the bound is `<= 2` rather than an exact count — but without
        // skipping it would be ABANDONED + 1 = 7.
        let calls = decide_calls.load(Ordering::SeqCst);
        assert!(
            (1..=2).contains(&calls),
            "expected at most the in-flight job plus the live one to run, got {calls} \
             (without skipping this would be {})",
            ABANDONED + 1
        );
        // And the live caller waited for its own inference plus at most that
        // one in-flight job, never for the whole abandoned queue.
        assert!(
            waited < std::time::Duration::from_millis(3_000),
            "live job waited {waited:?}, which means it queued behind abandoned work"
        );
    }

    #[tokio::test]
    async fn engine_turns_a_panicking_backend_call_into_an_error_and_stays_alive() {
        let engine = NeedleEngine::spawn(PanickingBackend);

        let err = engine
            .decide("t".to_string(), vec!["a".to_string()])
            .await
            .expect_err("a panicking decide must surface as an error");
        assert!(
            err.to_string().contains("simulated FFI wrapper bug"),
            "the panic message should survive into the typed error: {err}"
        );

        // The engine thread must still be alive and serving: a second,
        // non-panicking job succeeds. Before `guarded`, the panic unwound
        // out of the thread and this failed with "needle engine thread is
        // gone".
        let (model_id, dims) = engine
            .info()
            .await
            .expect("engine survives the panic and answers the next job");
        assert_eq!(model_id, "panicking");
        assert_eq!(dims, 7);

        let embedded = engine
            .embed(vec!["still working".to_string()])
            .await
            .expect("later inference jobs work too");
        assert_eq!(embedded[0].len(), 7);
    }
}
