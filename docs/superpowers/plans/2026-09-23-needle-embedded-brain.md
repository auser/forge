# Needle 3 Embedded Brain Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Embed Needle 3 in-process as forge's default decision brain — routing, guardrails, tool-call filling, extraction, and local embeddings — with zero external processes and full offline fallback.

**Architecture:** A `NeedleBackend` trait separates all logic from FFI: a deterministic `HashBackend` (always compiled) powers tests/BDD, and an `FfiBackend` over the new `needle-sys` crate (cargo feature `ffi`) runs the real model. A `NeedleEngine` owns the backend on one dedicated OS thread (mpsc jobs, oneshot replies, lazy load). `NeedleRouter` implements the existing `DecisionRouter`; `router = "needle"` becomes the default, wrapped in the existing `ThresholdRouter`/`FallbackRouter` combinators so Needle failures degrade to static rules — forge never refuses to run because Needle is unavailable.

**Tech Stack:** Rust edition 2024, tokio, async-trait, thiserror, bindgen (feature-gated), reqwest + sha2 (weights fetch), wiremock (tests), cucumber BDD.

**Spec:** `docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md`

## Global Constraints

- Typed errors via `thiserror`; **no `unwrap`/`expect` in production code** (tests may).
- Every task lands with its tests; run `just verify` (fmt --check + check + clippy -D warnings + test + bdd) before each commit. If a task's commit step says `just test` only, `just verify` still must pass at task end.
- README.md is updated **in the same commit** as the behavior it describes; README describes today, never the roadmap.
- Config precedence: defaults → user file → project file → `FORGE_*` env → CLI flags; new keys must appear in `forge config explain`.
- `--local-only` must prune every network code path (no fetch, no cloud router).
- Default install works offline: no Needle weights → static routing with `fallback_used: true`, never an error.
- Workspace deps come from root `Cargo.toml` `[workspace.dependencies]`; new crates use `edition.workspace = true`-style inheritance like existing crates.
- BDD suite is hermetic and offline (isolated HOME, mock providers) — Needle BDD scenarios use the `HashBackend` via `FORGE_NEEDLE_BACKEND=hash`, never real weights.
- Timeouts: needle decisions honor `router_timeout_ms` (default 5000 in config; engine calls use `tokio::time::timeout`).

## Review Focus

1. **Truncated/corrupt weights file** (interrupted download): loading must fail checksum, refetch once, then produce an actionable error naming the path — never load garbage. → test in Task 6.
2. **Invalid `[needle] variant`** (e.g. `"tiny"`): config load must produce an error naming the valid values, not panic or silently default. → test in Task 4.
3. **Embeddings index built by a different model/variant**: `model_id`/`dimensions` mismatch in the header must force a full rebuild, never mix vectors. → test in Task 9.
4. **Fast path on a destructive prompt** ("delete old logs"): the guardrail must refuse direct dispatch and fall through to the full loop where approval gating applies. → test in Task 10.
5. **Same symbol name in two files**: index keys must be `path::name`, so search returns both, distinctly. → test in Task 9.

---

### Task 1: `Embedder` trait in forge-core

**Files:**
- Create: `crates/forge-core/src/embed.rs`
- Modify: `crates/forge-core/src/lib.rs` (add `pub mod embed;`)

**Interfaces:**
- Produces: `forge_core::embed::Embedder` — consumed by Tasks 2, 9.

- [ ] **Step 1: Write the failing test** (in `embed.rs` `#[cfg(test)]`)

```rust
// crates/forge-core/src/embed.rs
use async_trait::async_trait;

use crate::error::ForgeError;

/// Produces vector embeddings for local semantic search and routing.
/// `model_id` versions the vectors: any stored index built with a
/// different `model_id` or `dimensions` must be rebuilt, never mixed.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError>;
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
```

- [ ] **Step 2: Run to verify it fails to compile** — `cargo test -p forge-core embed` → error: module not declared.
- [ ] **Step 3: Declare the module** — in `crates/forge-core/src/lib.rs`, next to the existing `pub mod` lines add `pub mod embed;`.
- [ ] **Step 4: Run** `cargo test -p forge-core embed` → PASS.
- [ ] **Step 5: Commit** — `git add crates/forge-core && git commit -m "feat(core): Embedder trait for local embeddings"`

---

### Task 2: `forge-needle` crate — backend trait, HashBackend, NeedleEngine

**Files:**
- Create: `crates/forge-needle/Cargo.toml`, `crates/forge-needle/src/lib.rs`, `crates/forge-needle/src/backend.rs`, `crates/forge-needle/src/hash_backend.rs`, `crates/forge-needle/src/engine.rs`
- Modify: root `Cargo.toml` (workspace members + `forge-needle = { path = "crates/forge-needle" }` in workspace deps)

**Interfaces:**
- Consumes: `forge_core::embed::Embedder`, `forge_core::error::ForgeError`.
- Produces (used by Tasks 3, 5–10):

```rust
pub enum BackendError { NotLoaded, WeightsMissing(std::path::PathBuf), Inference(String), Declined }
pub struct Decision { pub choice: String, pub confidence: f64, pub reason: String }
pub struct NeedleToolCall { pub name: String, pub arguments_json: String, pub confidence: f64 }
pub trait NeedleBackend: Send + 'static {
    fn load(&mut self) -> Result<(), BackendError>;
    fn model_id(&self) -> String;
    fn dimensions(&self) -> usize;
    fn decide(&mut self, task: &str, options: &[String]) -> Result<Decision, BackendError>;
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError>;
    fn extract(&mut self, text: &str, schema_json: &str) -> Result<String, BackendError>;
    fn tool_call(&mut self, prompt: &str, tools_json: &str)
        -> Result<Option<NeedleToolCall>, BackendError>;
}
pub struct NeedleEngine { /* Sender<Job> + cached info */ }
impl NeedleEngine {
    pub fn spawn(backend: impl NeedleBackend) -> Self;
    pub async fn decide(&self, task: String, options: Vec<String>) -> Result<Decision, ForgeError>;
    pub async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, ForgeError>;
    pub async fn extract(&self, text: String, schema_json: String) -> Result<String, ForgeError>;
    pub async fn tool_call(&self, prompt: String, tools_json: String)
        -> Result<Option<NeedleToolCall>, ForgeError>;
    pub async fn info(&self) -> Result<(String, usize), ForgeError>; // (model_id, dimensions)
}
pub struct EngineEmbedder(pub std::sync::Arc<NeedleEngine>); // implements forge_core Embedder
```

- [ ] **Step 1: Crate scaffolding**

`crates/forge-needle/Cargo.toml`:

```toml
[package]
name = "forge-needle"
version.workspace = true
edition.workspace = true
license.workspace = true

[features]
default = []
ffi = []           # real libneedle backend, wired in Task 8

[dependencies]
async-trait = { workspace = true }
forge-core = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
sha2 = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

Add `"crates/forge-needle"` to workspace `members` and `forge-needle = { path = "crates/forge-needle" }` to `[workspace.dependencies]`. `lib.rs`:

```rust
pub mod backend;
pub mod engine;
pub mod hash_backend;

pub use backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};
pub use engine::{EngineEmbedder, NeedleEngine};
pub use hash_backend::HashBackend;
```

- [ ] **Step 2: Write failing engine tests** (`crates/forge-needle/src/engine.rs` test module)

```rust
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
        let a = engine.embed(vec!["hello world".to_string()]).await.expect("embeds");
        let b = engine.embed(vec!["hello world".to_string()]).await.expect("embeds");
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
```

- [ ] **Step 3: Run** `cargo test -p forge-needle` → FAIL (nothing implemented).
- [ ] **Step 4: Implement `backend.rs`**

```rust
use std::path::PathBuf;

/// Error surface of a Needle backend. `Declined` is a designed outcome:
/// Needle refuses to guess; callers must fall back, never retry blindly.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("needle backend not loaded")]
    NotLoaded,
    #[error("needle weights missing at {0} (run `forge init` to fetch)")]
    WeightsMissing(PathBuf),
    #[error("needle inference failed: {0}")]
    Inference(String),
    #[error("needle declined to answer")]
    Declined,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub choice: String,
    pub confidence: f64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NeedleToolCall {
    pub name: String,
    pub arguments_json: String,
    pub confidence: f64,
}

/// Synchronous backend contract. Implementations: `HashBackend`
/// (deterministic, test/BDD), `FfiBackend` (real model, feature `ffi`).
/// All methods run on the engine's dedicated thread — implementations
/// may block and need not be Sync.
pub trait NeedleBackend: Send + 'static {
    fn load(&mut self) -> Result<(), BackendError>;
    fn model_id(&self) -> String;
    fn dimensions(&self) -> usize;
    fn decide(&mut self, task: &str, options: &[String]) -> Result<Decision, BackendError>;
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError>;
    fn extract(&mut self, text: &str, schema_json: &str) -> Result<String, BackendError>;
    fn tool_call(
        &mut self,
        prompt: &str,
        tools_json: &str,
    ) -> Result<Option<NeedleToolCall>, BackendError>;
}
```

- [ ] **Step 5: Implement `hash_backend.rs`** — deterministic, dependency-free stand-in with honest semantics (overlap scoring, trigram-hash embeddings, decline-first tool calling):

```rust
use sha2::{Digest, Sha256};

use crate::backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};

const DIMS: usize = 64;

/// Deterministic non-ML backend for tests, BDD, and CI. Selected at
/// runtime with FORGE_NEEDLE_BACKEND=hash. Never the default for users.
#[derive(Default)]
pub struct HashBackend {
    loaded: bool,
}

impl HashBackend {
    pub fn new() -> Self {
        Self::default()
    }

    fn tokens(s: &str) -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }
}

impl NeedleBackend for HashBackend {
    fn load(&mut self) -> Result<(), BackendError> {
        self.loaded = true;
        Ok(())
    }

    fn model_id(&self) -> String {
        "hash-test-v1".to_string()
    }

    fn dimensions(&self) -> usize {
        DIMS
    }

    fn decide(&mut self, task: &str, options: &[String]) -> Result<Decision, BackendError> {
        if options.is_empty() {
            return Err(BackendError::Declined);
        }
        let task_tokens = Self::tokens(task);
        let mut best = (0usize, 0usize); // (index, overlap)
        for (i, opt) in options.iter().enumerate() {
            let overlap = Self::tokens(opt)
                .iter()
                .filter(|t| task_tokens.contains(t))
                .count();
            if overlap > best.1 {
                best = (i, overlap);
            }
        }
        let confidence = if best.1 > 0 { 0.9 } else { 0.5 };
        Ok(Decision {
            choice: options[best.0].clone(),
            confidence,
            reason: format!("hash-backend token overlap: {}", best.1),
        })
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; DIMS];
                let lower = t.to_lowercase();
                let bytes = lower.as_bytes();
                for w in bytes.windows(3) {
                    let h = Sha256::digest(w);
                    v[(h[0] as usize) % DIMS] += 1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                v.iter_mut().for_each(|x| *x /= norm);
                v
            })
            .collect())
    }

    fn extract(&mut self, _text: &str, _schema_json: &str) -> Result<String, BackendError> {
        Err(BackendError::Declined) // honest: no fake extraction
    }

    fn tool_call(
        &mut self,
        prompt: &str,
        tools_json: &str,
    ) -> Result<Option<NeedleToolCall>, BackendError> {
        // Match "<tool_name>: <json-args>" prompts exactly; decline otherwise.
        let names: Vec<String> = serde_json::from_str::<serde_json::Value>(tools_json)
            .ok()
            .and_then(|v| {
                v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                        .map(str::to_string)
                        .collect()
                })
            })
            .unwrap_or_default();
        if let Some((name, rest)) = prompt.split_once(':')
            && names.iter().any(|n| n == name.trim())
            && serde_json::from_str::<serde_json::Value>(rest.trim()).is_ok()
        {
            return Ok(Some(NeedleToolCall {
                name: name.trim().to_string(),
                arguments_json: rest.trim().to_string(),
                confidence: 0.95,
            }));
        }
        Ok(None)
    }
}
```

- [ ] **Step 6: Implement `engine.rs`** — dedicated thread, lazy load on first job:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use forge_core::embed::Embedder;
use forge_core::error::ForgeError;
use tokio::sync::{mpsc, oneshot};

use crate::backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};

enum Job {
    Decide(String, Vec<String>, oneshot::Sender<Result<Decision, BackendError>>),
    Embed(Vec<String>, oneshot::Sender<Result<Vec<Vec<f32>>, BackendError>>),
    Extract(String, String, oneshot::Sender<Result<String, BackendError>>),
    ToolCall(String, String, oneshot::Sender<Result<Option<NeedleToolCall>, BackendError>>),
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

/// Adapter: NeedleEngine as a forge-core Embedder.
pub struct EngineEmbedder(pub Arc<NeedleEngine>);

#[async_trait]
impl Embedder for EngineEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        self.0.embed(texts.to_vec()).await
    }
    fn dimensions(&self) -> usize {
        64 // HashBackend/Needle small ladder; real value read via info() where it matters
    }
    fn model_id(&self) -> String {
        "needle".to_string()
    }
}
```

Note for the implementer: `EngineEmbedder::dimensions`/`model_id` returning constants is wrong for the real backend — Task 9 replaces call sites with `engine.info()` values captured at construction; do it now if simpler: make `EngineEmbedder` store `(model_id, dimensions)` fetched once via `NeedleEngine::info()` by a `pub async fn new(engine: Arc<NeedleEngine>) -> Result<Self, ForgeError>` constructor. Prefer that; the plan's later tasks assume it.

- [ ] **Step 7: Run** `cargo test -p forge-needle` → PASS. Also `cargo check --workspace --all-targets`.
- [ ] **Step 8: Commit** — `git add Cargo.toml Cargo.lock crates/forge-needle && git commit -m "feat(needle): engine, backend trait, deterministic hash backend"`

---

### Task 3: `NeedleRouter` implementing `DecisionRouter`

**Files:**
- Create: `crates/forge-needle/src/router.rs` (+ `pub mod router;` and `pub use router::NeedleRouter;` in `lib.rs`)

**Interfaces:**
- Consumes: `NeedleEngine` (Task 2); `forge_core::router::{DecisionRouter, RoutingRequest, RoutingDecision}`; `forge_providers::router::filter_candidates` — **do not** re-implement; forge-needle adds `forge-providers = { workspace = true }` dependency? **No** — that would invert the dependency (providers will depend on needle in Task 5). Instead copy the tiny capability-filter inline as shown below.
- Produces: `NeedleRouter::new(engine: Arc<NeedleEngine>, registry: Vec<(String, ModelCapabilities)>, timeout: Duration) -> Self` — consumed by Task 5.

- [ ] **Step 1: Write failing tests** (in `router.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_backend::HashBackend;
    use forge_core::model::ModelCapabilities;
    use forge_core::router::{Capability, RoutingRequest};
    use std::sync::Arc;
    use std::time::Duration;

    fn caps(tools: bool) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools,
            structured_output: false,
            vision: false,
            max_context: 32_768,
        }
    }

    fn router() -> NeedleRouter {
        NeedleRouter::new(
            Arc::new(NeedleEngine::spawn(HashBackend::new())),
            vec![
                ("qwen3-coder".to_string(), caps(true)),
                ("no-tools-model".to_string(), caps(false)),
            ],
            Duration::from_millis(5_000),
        )
    }

    #[tokio::test]
    async fn routes_to_candidate_and_reports_name() {
        let d = router()
            .route(&RoutingRequest::new("use qwen3 coder for this"))
            .await
            .expect("routes");
        assert_eq!(d.selected_model, "qwen3-coder");
        assert_eq!(d.router_name, "needle");
        assert!(!d.fallback_used);
    }

    #[tokio::test]
    async fn capability_filter_removes_ineligible() {
        let mut req = RoutingRequest::new("anything with no tools model words");
        req.required_capabilities = vec![Capability::Tools];
        let d = router().route(&req).await.expect("routes");
        assert_eq!(d.selected_model, "qwen3-coder"); // only eligible option
    }

    #[tokio::test]
    async fn empty_candidates_is_an_error_not_a_guess() {
        let empty = NeedleRouter::new(
            Arc::new(NeedleEngine::spawn(HashBackend::new())),
            vec![],
            Duration::from_millis(5_000),
        );
        assert!(empty.route(&RoutingRequest::new("task")).await.is_err());
    }
}
```

- [ ] **Step 2: Run** `cargo test -p forge-needle router` → FAIL.
- [ ] **Step 3: Implement**

```rust
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_core::error::ForgeError;
use forge_core::model::ModelCapabilities;
use forge_core::router::{DecisionRouter, RoutingDecision, RoutingRequest};

use crate::engine::NeedleEngine;

/// On-device decision router. Errors (engine gone, weights missing,
/// declined, timeout) are surfaced as Err — the FallbackRouter that
/// wraps this in `router_from_config` degrades to static rules.
pub struct NeedleRouter {
    engine: Arc<NeedleEngine>,
    registry: Vec<(String, ModelCapabilities)>,
    timeout: Duration,
}

impl NeedleRouter {
    pub fn new(
        engine: Arc<NeedleEngine>,
        registry: Vec<(String, ModelCapabilities)>,
        timeout: Duration,
    ) -> Self {
        Self { engine, registry, timeout }
    }
}

#[async_trait]
impl DecisionRouter for NeedleRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        // Candidate set: explicit request candidates, else full registry,
        // both filtered by required capabilities (mirrors filter_candidates
        // in forge-providers; kept inline to avoid a dependency cycle).
        let pool: Vec<String> = if task.candidates.is_empty() {
            self.registry.iter().map(|(n, _)| n.clone()).collect()
        } else {
            task.candidates.clone()
        };
        let eligible: Vec<String> = pool
            .into_iter()
            .filter(|name| {
                self.registry
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, caps)| {
                        task.required_capabilities.iter().all(|c| c.satisfied_by(caps))
                    })
                    // Unknown models: optimistic, matching existing router behavior.
                    .unwrap_or(true)
            })
            .collect();
        if eligible.is_empty() {
            return Err(ForgeError::router("needle: no eligible candidates"));
        }
        let decision = tokio::time::timeout(
            self.timeout,
            self.engine.decide(task.task.clone(), eligible),
        )
        .await
        .map_err(|_| ForgeError::router("needle: decision timed out"))??;
        Ok(RoutingDecision {
            selected_model: decision.choice,
            confidence: decision.confidence,
            router_name: "needle".to_string(),
            fallback_used: false,
            reason: decision.reason,
        })
    }
}
```

- [ ] **Step 4: Run** `cargo test -p forge-needle` → PASS.
- [ ] **Step 5: Commit** — `git add crates/forge-needle && git commit -m "feat(needle): NeedleRouter implementing DecisionRouter"`

---

### Task 4: `[needle]` config section (default router stays `laya` until Task 5)

**Files:**
- Modify: `crates/forge-config/src/lib.rs` (add `NeedleConfig`, `Config.needle` field, defaults, env mapping)
- Modify: `crates/forge-config/src/tests.rs`
- Modify: `README.md` (Configuration table: three `[needle]` rows)

**Interfaces:**
- Produces (consumed by Tasks 5–7):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NeedleConfig {
    pub variant: String,      // "small" | "medium" | "full"; default "medium"
    pub weights_path: String, // empty → ~/.cache/forge/models/
    pub autofetch: bool,      // default true
}
```

- [ ] **Step 1: Write failing tests** in `crates/forge-config/src/tests.rs`, following the file's existing test style:

```rust
#[test]
fn needle_defaults() {
    let c = Config::default();
    assert_eq!(c.needle.variant, "medium");
    assert_eq!(c.needle.weights_path, "");
    assert!(c.needle.autofetch);
}

#[test]
fn needle_section_parses_and_validates() {
    let c: Config =
        toml::from_str("[needle]\nvariant = \"small\"\nautofetch = false").expect("parses");
    assert_eq!(c.needle.variant, "small");
    assert!(!c.needle.autofetch);
    assert!(c.validate().is_ok());
}

#[test]
fn needle_invalid_variant_names_valid_values() {
    let c: Config = toml::from_str("[needle]\nvariant = \"tiny\"").expect("parses");
    let err = c.validate().expect_err("invalid variant rejected").to_string();
    assert!(err.contains("tiny") && err.contains("small") && err.contains("full"));
}
```

If `Config` has no `validate()` today, add one returning `Result<(), ForgeError>` and call it where config loading completes (find the load/merge entry point; every existing caller gets validation for free — check existing tests still pass).

- [ ] **Step 2: Run** `cargo test -p forge-config needle` → FAIL.
- [ ] **Step 3: Implement** — `NeedleConfig` as above with a `Default` impl (`variant: "medium".into(), weights_path: String::new(), autofetch: true`); add `pub needle: NeedleConfig` to `Config` (serde `#[serde(default)]` on the struct covers it) and to `Config::default()`. Add env mappings alongside the existing `("FORGE_ROUTER", "router")` table: `("FORGE_NEEDLE_VARIANT", "needle.variant")`, `("FORGE_NEEDLE_AUTOFETCH", "needle.autofetch")` — follow how the table applies nested keys; if it only supports flat keys, extend the applier to split on `.` (test: set env var in a test via the file's existing env-test pattern). `validate()`: variant must be one of `small|medium|full`, error message includes offending value and all valid values.
- [ ] **Step 4: Verify `forge config explain needle.variant` works** — check how `config_cmd.rs explain` resolves key paths; if it already walks serde values, nested keys work; otherwise extend its lookup to split on `.` (add a unit test in the explain code's test module asserting `needle.variant` resolves with source `default`).
- [ ] **Step 5: README** — add rows to the Key settings table: `needle.variant` / `medium` / `FORGE_NEEDLE_VARIANT` / "Needle 3 weights ladder (small ≈ 8 MB / medium / full ≈ 29 MB)"; `needle.weights_path` / — / — / "Weights override; empty → ~/.cache/forge/models/"; `needle.autofetch` / `true` / `FORGE_NEEDLE_AUTOFETCH` / "forge init downloads + verifies weights".
- [ ] **Step 6: Run** `cargo test -p forge-config` and `just verify` → PASS.
- [ ] **Step 7: Commit** — `git add crates/forge-config README.md && git commit -m "feat(config): [needle] section with variant validation"`

---

### Task 5: Wire `needle` into `router_from_config` and flip the default

**Files:**
- Modify: `crates/forge-providers/Cargo.toml` (add `forge-needle = { workspace = true }`)
- Modify: `crates/forge-providers/src/router.rs` (`build_router` arm + `router_from_config` threshold wrap + tests)
- Modify: `crates/forge-config/src/lib.rs` (`router: "needle"` default) + affected tests in `crates/forge-config/src/tests.rs`
- Modify: `crates/forge-cli/src/cli.rs` (`--router` help text lists `needle`)
- Modify: `README.md` (default stack paragraphs, Quickstart, DecisionRouter section, config table `router` row)

**Interfaces:**
- Consumes: `NeedleRouter`, `NeedleEngine`, `HashBackend` (Tasks 2–3); `NeedleConfig` (Task 4).
- Produces: `build_router` accepts `"needle"`; env `FORGE_NEEDLE_BACKEND=hash` selects HashBackend (test/BDD hook). Real FFI backend arrives in Task 8; until then `"needle"` without `FORGE_NEEDLE_BACKEND=hash` builds a router whose `route()` errors with the WeightsMissing message → FallbackRouter → static. That is the correct offline default behavior *now and after* Task 8.

- [ ] **Step 1: Write failing tests** in `router.rs`'s existing test module:

```rust
#[tokio::test]
async fn needle_router_from_config_falls_back_to_static_without_weights() {
    let mut config = Config::default();
    config.router = "needle".to_string();
    config.router_fallback = "static".to_string();
    let router = router_from_config(&config, &[("qwen3-coder".to_string(), caps(true))])
        .expect("builds");
    let d = router
        .route(&RoutingRequest::new("explain this"))
        .await
        .expect("fallback routes");
    assert!(d.fallback_used);
    assert_eq!(d.router_name, "static");
}

#[tokio::test]
async fn needle_router_with_hash_backend_routes_directly() {
    // Env-driven backend selection; serial_test guards env mutation.
    // (Add `serial_test = { workspace = true }` to dev-dependencies if absent.)
    #[allow(unused)]
    use serial_test::serial;
    std::env::set_var("FORGE_NEEDLE_BACKEND", "hash");
    let mut config = Config::default();
    config.router = "needle".to_string();
    let router = router_from_config(&config, &[("qwen3-coder".to_string(), caps(true))])
        .expect("builds");
    let d = router
        .route(&RoutingRequest::new("qwen3 coder please"))
        .await
        .expect("routes");
    std::env::remove_var("FORGE_NEEDLE_BACKEND");
    assert_eq!(d.router_name, "needle");
    assert!(!d.fallback_used);
}
```

Mark both `#[serial]` (env manipulation).

- [ ] **Step 2: Run** `cargo test -p forge-providers needle` → FAIL (unknown router).
- [ ] **Step 3: Implement the `build_router` arm** (in the `match` that currently ends with the `other =>` error; also update that error string to include `needle`):

```rust
"needle" => {
    let engine = match std::env::var("FORGE_NEEDLE_BACKEND").as_deref() {
        Ok("hash") => forge_needle::NeedleEngine::spawn(forge_needle::HashBackend::new()),
        _ => forge_needle::engine_from_config(&config.needle)?, // Task 6 completes this; stub now
    };
    Ok(Arc::new(forge_needle::NeedleRouter::new(
        Arc::new(engine),
        registry.to_vec(),
        std::time::Duration::from_millis(config.router_timeout_ms),
    )))
}
```

Add to `forge-needle/src/lib.rs` the stub the arm needs (Task 6 replaces its internals with real weights resolution — the signature is fixed here):

```rust
/// Build an engine from config. Until the FFI backend lands (feature
/// `ffi` + weights on disk), this returns an engine whose backend
/// reports WeightsMissing, so routing falls back to static.
pub fn engine_from_config(
    needle: &forge_config_types::NeedleConfig, // see step note below
) -> Result<NeedleEngine, forge_core::error::ForgeError> {
    Ok(NeedleEngine::spawn(backend::UnavailableBackend::default()))
}
```

Dependency note: `forge-needle` must not depend on `forge-config` if that creates a cycle (`forge-config` does not depend on providers/needle — check with `cargo tree -p forge-config -i`; it does not, so `forge-needle` may depend on `forge-config` directly; use `forge_config::NeedleConfig` and delete the placeholder type name above). `UnavailableBackend`: a tiny `NeedleBackend` in `backend.rs` whose `load()` returns `Err(BackendError::WeightsMissing(default_weights_path()))` — `default_weights_path()` lands properly in Task 6; here it returns `dirs`-free `~/.cache/forge/models/needle3-medium.bin` via `std::env::home_dir()` replacement already used elsewhere in the repo (grep `home_dir\|HOME` in forge-config for the existing helper and reuse it).

- [ ] **Step 4: Threshold wrap** — in `router_from_config`, change `matches!(config.router.as_str(), "http" | "laya")` to `matches!(config.router.as_str(), "http" | "laya" | "needle")`, and update the doc comment.
- [ ] **Step 5: Flip the default** — `router: "needle".to_string()` in `Config::default()`; fix the default-stack comment above it. Run `cargo test --workspace`; update every test asserting `router == "laya"` default (search `"laya"` in forge-config tests, BDD steps, doctor) to the new default — each updated assertion must still have a laya-specific test that sets `router = "laya"` explicitly.
- [ ] **Step 6: CLI + README** — `cli.rs` `--router` doc comment: `static|mock|cheapest|http|laya|needle`. README: intro paragraph ("The default stack is Laya…" → embedded Needle 3 decision routing with static fallback, Laya/Jev-style HTTP still available); Quickstart (drop `forge router serve &` from the default flow — it remains under the Laya section); DecisionRouter section (needle = default, laya/http/static/mock/cheapest as alternates, unchanged fallback semantics); config table `router` row default `needle`.
- [ ] **Step 7: Run** `just verify` → PASS (BDD asserts may need the same default fix; keep BDD offline-true: with no weights the default path must show `fallback_used: true`).
- [ ] **Step 8: Commit** — `git add -A && git commit -m "feat(router): needle is the default decision router with static fallback"`

---

### Task 6: Weights manager + `forge init` autofetch

**Files:**
- Create: `crates/forge-needle/src/weights.rs` (+ `pub mod weights;` in lib.rs)
- Modify: `crates/forge-needle/src/lib.rs` (`engine_from_config` real impl), `crates/forge-needle/Cargo.toml` (add `reqwest`, `tokio-util`; dev-dep `wiremock`)
- Modify: `crates/forge-cli/src/commands/init.rs` (autofetch step)
- Modify: `README.md` (init behavior, offline note)

**Interfaces:**
- Consumes: `NeedleConfig` (Task 4).
- Produces (used by Tasks 7–8):

```rust
pub struct WeightsSpec { pub variant: &'static str, pub filename: &'static str, pub sha256: &'static str, pub url: String }
pub fn spec_for(variant: &str) -> Result<WeightsSpec, ForgeError>;
pub fn weights_path(needle: &NeedleConfig) -> Result<PathBuf, ForgeError>; // override or cache dir
pub fn verify(path: &Path, expected_sha256: &str) -> Result<bool, ForgeError>;
pub async fn ensure_weights(needle: &NeedleConfig) -> Result<WeightsStatus, ForgeError>;
pub enum WeightsStatus { Present(PathBuf), Fetched(PathBuf), Missing { path: PathBuf, reason: String } }
```

- [ ] **Step 1: Pin real artifact URLs + checksums.** Run (network, one-time, on the dev machine — NOT in tests):

```bash
mkdir -p /tmp/needle-weights && cd /tmp/needle-weights
# Weights live on Hugging Face: Cactus-Compute/needle3. Confirm exact
# filenames with: curl -s https://huggingface.co/api/models/Cactus-Compute/needle3 | jq '.siblings[].rfilename'
# Download each variant file and record:
shasum -a 256 *.bin
```

Record the three `(filename, sha256)` pairs in a `const VARIANTS: [WeightsVariant; 3]` table in `weights.rs`, with `url` built as `https://huggingface.co/Cactus-Compute/needle3/resolve/main/<filename>`. Also record the artifact license found on that HF page in `docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md` §8 (Risks) — if the license forbids redistribution or is absent, STOP and raise it to the human partner before continuing (the design anticipates this; autofetch-from-origin is usually still fine, vendoring is not).

- [ ] **Step 2: Write failing tests** (`weights.rs` tests; wiremock serves fake weight bytes; `FORGE_NEEDLE_WEIGHTS_BASE_URL` env overrides the URL base for tests — mark env tests `#[serial]`):

```rust
#[tokio::test]
#[serial_test::serial]
async fn ensure_weights_fetches_verifies_and_is_idempotent() {
    let server = wiremock::MockServer::start().await;
    let body = b"fake-weights".to_vec();
    let sha = hex::encode(sha2::Sha256::digest(&body)); // add hex to dev-deps, or format bytes manually
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tmp");
    std::env::set_var("FORGE_NEEDLE_WEIGHTS_BASE_URL", server.uri());
    std::env::set_var("FORGE_NEEDLE_TEST_SHA256", &sha); // test-only checksum override
    let cfg = NeedleConfig {
        variant: "small".to_string(),
        weights_path: dir.path().join("w.bin").display().to_string(),
        autofetch: true,
    };
    let first = ensure_weights(&cfg).await.expect("fetches");
    assert!(matches!(first, WeightsStatus::Fetched(_)));
    let second = ensure_weights(&cfg).await.expect("present");
    assert!(matches!(second, WeightsStatus::Present(_)));
    std::env::remove_var("FORGE_NEEDLE_WEIGHTS_BASE_URL");
    std::env::remove_var("FORGE_NEEDLE_TEST_SHA256");
}

#[tokio::test]
#[serial_test::serial]
async fn corrupt_download_refetches_once_then_reports() {
    // Server always returns bytes whose hash ≠ expected: ensure_weights
    // must attempt exactly 2 GETs then return WeightsStatus::Missing
    // with a reason containing "checksum" and the path.
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"garbage".to_vec()))
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tmp");
    std::env::set_var("FORGE_NEEDLE_WEIGHTS_BASE_URL", server.uri());
    std::env::set_var("FORGE_NEEDLE_TEST_SHA256", &"0".repeat(64));
    let cfg = NeedleConfig {
        variant: "small".to_string(),
        weights_path: dir.path().join("w.bin").display().to_string(),
        autofetch: true,
    };
    let status = ensure_weights(&cfg).await.expect("completes without hard error");
    match status {
        WeightsStatus::Missing { reason, .. } => assert!(reason.contains("checksum")),
        other => panic!("expected Missing, got {other:?}"),
    }
    std::env::remove_var("FORGE_NEEDLE_WEIGHTS_BASE_URL");
    std::env::remove_var("FORGE_NEEDLE_TEST_SHA256");
}
```

Also test: `truncated existing file on disk fails verify() and is refetched` (write partial bytes to the path first, then run `ensure_weights`, assert `Fetched`).

- [ ] **Step 3: Run** `cargo test -p forge-needle weights` → FAIL.
- [ ] **Step 4: Implement `weights.rs`** — `spec_for` looks up `VARIANTS` (error names valid variants); base URL from `FORGE_NEEDLE_WEIGHTS_BASE_URL` else the HF const; expected sha from `FORGE_NEEDLE_TEST_SHA256` else the pinned const; download to `<path>.part` then atomic rename (this is what makes truncation detectable: the final path only ever holds fully-written bytes, and `verify` re-hashes on every startup anyway); on checksum mismatch delete and retry once; degrade to `WeightsStatus::Missing { .. }` — callers decide severity (init: warn; router: fall back). Cache dir: reuse the repo's existing home-dir helper; path `~/.cache/forge/models/<filename>` unless `weights_path` set.
- [ ] **Step 5: Real `engine_from_config`** — replace the Task 5 stub: resolve `weights_path(needle)`; if file exists and `verify` passes and feature `ffi` is enabled → `FfiBackend` (Task 8; until then gate with `#[cfg(feature = "ffi")]` and keep `UnavailableBackend` in the `#[cfg(not(feature = "ffi"))]` branch); else `UnavailableBackend` (whose error message now includes the real resolved path).
- [ ] **Step 6: `forge init` autofetch** — in `init.rs` `run()`, after `build_graph`: when `config.needle.autofetch && !config.local_only && config.router == "needle"`, call `ensure_weights`; report as an `InitItem` (`fetched needle weights (medium, 17 MB) → ~/.cache/forge/models/…` / `needle weights present (verified)` / warn line from `Missing`). When `local_only`: push an item noting `needle weights: skipped (--local-only); routing falls back to static until weights exist`. Init is sync (`pub fn run`) — check how init.rs reaches async today (grep for `block_on`); use the same pattern, or a small `tokio::runtime::Runtime` if none exists.
- [ ] **Step 7: README** — init section gains the weights line; the offline paragraph states: no weights → deterministic static routing, `fallback_used: true`, `forge init` fetches once (8–29 MB).
- [ ] **Step 8: Run** `just verify` → PASS. **Step 9: Commit** — `git add -A && git commit -m "feat(needle): weights fetch/verify with init autofetch"`

---

### Task 7: `forge doctor` needle probe

**Files:**
- Modify: `crates/forge-cli/src/commands/doctor.rs`
- Modify: `README.md` (doctor description mentions the needle check)

**Interfaces:**
- Consumes: `weights_path`/`verify`/`spec_for` (Task 6), `NeedleEngine::info` + `decide` (Task 2), `FORGE_NEEDLE_BACKEND=hash` hook (Task 5).

- [ ] **Step 1: Write failing BDD-style integration check** — doctor is exercised via BDD; add the unit-level test first in `doctor.rs`'s test module if one exists, else add to the BDD feature in Task 11 and use a focused integration test here: extract the new probe into `fn needle_check(config: &Config) -> Check` and unit-test that:

```rust
#[tokio::test]
async fn needle_check_reports_missing_weights_as_warn_not_fail() {
    let mut config = Config::default();
    config.needle.weights_path = "/nonexistent/needle.bin".to_string();
    let check = needle_check(&config).await;
    assert_eq!(check.status, Status::Warn); // match doctor.rs's existing status enum
    assert!(check.detail.contains("forge init"));
}

#[tokio::test]
async fn needle_check_with_hash_backend_reports_ok_and_latency() {
    std::env::set_var("FORGE_NEEDLE_BACKEND", "hash");
    let check = needle_check(&Config::default()).await;
    std::env::remove_var("FORGE_NEEDLE_BACKEND");
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("ms")); // measured decide() latency
}
```

(Adapt `Status`/field names to the actual `Check` struct at `doctor.rs:13` — read it first; keep `#[serial]` on the env test.)

- [ ] **Step 2: Run** → FAIL. **Step 3: Implement `needle_check`** — when `router != "needle"`: Ok, "needle: not the active router". Else: resolve weights path → missing/corrupt → Warn with fetch hint; present (or hash backend) → build engine, time one `decide("doctor smoke test", ["ok"])` with `router_timeout_ms` timeout, report `needle: ok (model <id>, decide 12 ms)`; failure → Warn (doctor never hard-fails on needle; static fallback keeps forge usable). Insert the check after the model/router checks in `run()`.
- [ ] **Step 4: README** — doctor bullet: "probes model + router endpoints and the embedded needle brain (weights, load, decision latency)".
- [ ] **Step 5:** `just verify` → PASS. **Step 6: Commit** — `git commit -am "feat(doctor): needle brain probe"`

---

### Task 8: `needle-sys` FFI crate + `FfiBackend` (feature `ffi`)

**Files:**
- Create: `crates/needle-sys/Cargo.toml`, `crates/needle-sys/build.rs`, `crates/needle-sys/src/lib.rs`, `crates/needle-sys/wrapper.h`
- Create: `crates/forge-needle/src/ffi_backend.rs` (cfg feature `ffi`)
- Modify: root `Cargo.toml` (member), `crates/forge-needle/Cargo.toml` (`ffi = ["dep:needle-sys"]`), `crates/forge-cli/Cargo.toml` (feature `needle-ffi` forwarding, default off until CI vendors the lib)
- Create: `crates/forge-needle/tests/e2e.rs` (feature `needle-e2e`)

**Interfaces:**
- Consumes: `NeedleBackend` trait (Task 2), `WeightsSpec` (Task 6).
- Produces: `FfiBackend::new(weights: PathBuf) -> Self` implementing `NeedleBackend`.

**Reality check (do this first):** the exact C symbol names in `needle.h` are not pinned in this plan — the Needle 3 SDK ships `libneedle.a` + `needle.h` per platform (`needle build` in the Python tooling downloads them; the Cactus GitHub releases page carries the same artifacts). The `NeedleBackend` trait is the contract that must NOT change; the FFI code below shows the intended shape and MUST be adjusted to the real header once downloaded. If the header's capabilities don't cover a trait method (e.g. no direct `decide`), implement it on top of what exists (Needle's tool-calling primitive with one "tool" per option is the documented equivalent for choice decisions).

- [ ] **Step 1: Acquire the library** — `pip install needle-cactus && needle build --target $(rustc -vV | grep host | cut -d' ' -f2)` (or download from the Cactus release page); place under `NEEDLE_LIB_DIR` or `crates/needle-sys/vendor/<target-triple>/{libneedle.a,needle.h}`. Record the library version + license text location in the spec §8. **Do not `git add` vendor/ until the license is confirmed redistributable** (add `crates/needle-sys/vendor/` to `.gitignore` in this step; revisit when license confirmed).
- [ ] **Step 2: `needle-sys`** — Cargo.toml with `[build-dependencies] bindgen = "0.72"`, `links = "needle"`; `wrapper.h` = `#include "needle.h"`; `build.rs`: resolve lib dir (`NEEDLE_LIB_DIR` env → `vendor/<TARGET>/`), emit `cargo:rustc-link-search` + `cargo:rustc-link-lib=static=needle` (+ `c++` on macOS/Linux if the lib needs it — check `nm libneedle.a` for C++ symbols), run bindgen on `wrapper.h` into `OUT_DIR/bindings.rs`; if the lib dir is missing, `panic!` with a message naming both resolution options (this crate is only built when feature `ffi` is on, so normal builds never hit it). `src/lib.rs`: `#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)] include!(concat!(env!("OUT_DIR"), "/bindings.rs"));`
- [ ] **Step 3: `FfiBackend`** — implements `NeedleBackend` over the generated bindings: `load()` = model-open call with the weights path (map failure to `Inference`); `decide` = tool-call primitive with one no-arg tool per option, choice = selected tool, confidence from the response struct, `Declined` when the model selects nothing; `embed` = the embeddings entry point, batched ≤ 32; `extract` = the grammar-constrained extraction call returning JSON; `tool_call` = native tool-call with the real tools JSON; every returned pointer checked non-null before use, every string copied out before the corresponding free call; `Drop` frees the model handle.
- [ ] **Step 4: e2e test** (`tests/e2e.rs`, `#![cfg(feature = "needle-e2e")]`) — requires real `small` weights at `FORGE_NEEDLE_E2E_WEIGHTS`; asserts: engine loads; `decide("run the tests", ["test-runner","chat-model"])` returns `test-runner` with confidence > 0.5; `embed` returns `dimensions()`-sized normalized vectors, deterministic across calls; `extract` with schema `{"type":"object","properties":{"city":{"type":"string"}}}` on "weather in Paris" yields parseable JSON containing "Paris"; a route round-trip completes in < 500 ms (generous; perf smoke). Run locally: `FORGE_NEEDLE_E2E_WEIGHTS=~/.cache/forge/models/needle3-small.bin cargo test -p forge-needle --features "ffi needle-e2e" --test e2e`. Add a CI job note to `.github/workflows/` only if a workflow already builds with features — otherwise leave CI alone this task (README dev section documents the manual invocation).
- [ ] **Step 5:** `just verify` (default features — FFI stays off, everything still green) → PASS. **Step 6: Commit** — `git add -A && git commit -m "feat(needle): needle-sys FFI crate and real backend behind feature ffi"`

---

### Task 9: Semantic graph index + `graph grep --semantic`

**Files:**
- Create: `crates/forge-graph/src/embed_index.rs` (+ `pub mod embed_index;`) — pure format, no model deps
- Modify: `crates/forge-cli/src/commands/graph_cmd.rs` (build-time embedding, `--semantic` flag, context blend)
- Modify: `crates/forge-cli/src/cli.rs` (flag), `README.md` (graph section)

**Interfaces:**
- Consumes: `Embedder` (Task 1) via `EngineEmbedder` (Task 2); `GraphState.symbols: Vec<SymbolNode>` (existing; key fields `name`, `path`).
- Produces:

```rust
pub struct EmbeddingIndex { /* header + entries + vectors */ }
impl EmbeddingIndex {
    pub fn new(model_id: String, dimensions: usize) -> Self;
    pub fn load(path: &Path) -> Option<Self>;                 // corrupt/missing → None (rebuild)
    pub fn save(&self, path: &Path) -> Result<(), ForgeError>;
    pub fn matches_model(&self, model_id: &str, dimensions: usize) -> bool;
    pub fn stale_keys(&self, current: &[(String, String)]) -> Vec<String>; // (key, content_hash)
    pub fn upsert(&mut self, key: String, content_hash: String, vector: Vec<f32>);
    pub fn remove_missing(&mut self, current_keys: &std::collections::BTreeSet<String>);
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)>;   // cosine, descending
}
```

Index key = `"<path>::<symbol_name>"`; embedded text = `"<kind> <name> in <path>"`. File: `.forge/graph/embeddings.bin`, layout: 8-byte magic `FRGEMB01`, then a bincode/serde_json header (use `serde_json` — already a dep; simplicity over bytes), then raw f32 LE vectors. Simplest correct: serialize the whole struct as one `serde_json` object with vectors as arrays — a few thousand × 64 floats is fine; optimize later only if `graph build` profiling says so.

- [ ] **Step 1: Write failing index tests** (`embed_index.rs`): round-trip save/load; `matches_model` false on different model_id or dims; `load` on truncated file → None; `stale_keys` returns changed-hash and new keys only; two symbols named `run` in different files both searchable (distinct keys, both returned by `search`); `search` orders by cosine similarity.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** Run → PASS.
- [ ] **Step 4: Wire into `graph_cmd`** — after a successful `build_report()` in the build path: if a needle engine is constructible (same selection logic as `build_router` "needle" arm — extract a small `pub fn engine_if_available(config: &Config) -> Option<Arc<NeedleEngine>>` into forge-needle so cli and providers share it; `UnavailableBackend` counts as unavailable), load-or-new the index, drop it entirely when `!matches_model(info)`, embed `stale_keys` in batches of 32, `remove_missing`, save. No engine → skip silently (graph stays model-free, matching its README contract). `graph grep --semantic <q>`: engine required — without it, print `semantic search needs needle weights (run forge init)` to stderr, exit code 1; with it, embed query, `search(k=20)`, print `score  path::symbol` lines. `graph context <query>`: when index + engine exist, final score = `0.5 * lexical_rank_score + 0.5 * cosine` (lexical_rank_score = 1/(1+rank) over the existing ranked list); otherwise unchanged lexical behavior.
- [ ] **Step 5: CLI integration test** — extend the graph tests (see how graph_cmd is tested today — BDD or unit): with `FORGE_NEEDLE_BACKEND=hash`, `graph build` creates `embeddings.bin`; `graph grep --semantic "parse"` returns a symbol whose name contains `parse` above unrelated ones (hash backend trigram overlap makes this deterministic); second `graph build` with no changes re-embeds nothing (assert index file mtime unchanged, or expose embedded-count in the build report note).
- [ ] **Step 6: README** — graph section: build embeds symbols locally when needle weights exist (skipped otherwise — graph itself stays deterministic/offline); document `--semantic`; note `.forge/graph/embeddings.bin`.
- [ ] **Step 7:** `just verify` → PASS. **Step 8: Commit** — `git commit -am "feat(graph): local semantic index and graph grep --semantic"`

---

### Task 10: Direct-dispatch fast path in the agent loop

**Files:**
- Modify: `crates/forge-runtime/Cargo.toml` (add `forge-needle`), `crates/forge-runtime/src/service.rs` (fast-path attempt in `start_run` before the model loop; `with_needle` builder), `crates/forge-cli/src/commands/service.rs` (pass engine when available)
- Modify: `README.md` (Usage: fast-path note)

**Interfaces:**
- Consumes: `NeedleEngine::{tool_call, decide}` (Task 2), `engine_if_available` (Task 9), `tool_definitions()` + `ToolDispatcher` (existing, `crates/forge-runtime/src/tools.rs`).
- Produces: `AgentService::with_needle(self, engine: Option<Arc<NeedleEngine>>) -> Self`. Fast-path decision event: `routing_decision_made` with `router_name: "needle-dispatch"`.

- [ ] **Step 1: Read `service.rs:330` `start_run`** end to end before editing; identify where the first model call happens and where events are emitted. The fast path slots in after `run_started` + skill activation, before the first model call.
- [ ] **Step 2: Write failing tests** (in service.rs's existing test module style, mock execution + `HashBackend` engine):

```rust
#[tokio::test]
async fn fast_path_dispatches_exact_tool_prompt_without_model() {
    // HashBackend tool_call fires on "<tool>: <json>" prompts.
    // Use a real tool from tool_definitions() — read tools.rs; assume
    // "read_file" with {"path": "..."} exists; adjust to actual names.
    let (service, _tmp) = test_service_with_needle(); // helper mirroring existing test setup + hash engine
    let outcome = service_run(&service, "read_file: {\"path\": \"Cargo.toml\"}").await;
    assert!(outcome.completed);
    let events = outcome.events;
    assert!(events.iter().any(|e| e.kind == "routing_decision_made"
        && e.detail_contains("needle-dispatch")));
    assert!(events.iter().all(|e| e.kind != "turn_completed")); // model loop never ran
}

#[tokio::test]
async fn fast_path_declines_destructive_and_falls_through() {
    let (service, _tmp) = test_service_with_needle();
    // "delete" prompts must not fast-path even in valid tool syntax:
    let outcome = service_run(&service, "execute_command: {\"command\": \"rm -rf logs\"}").await;
    // Full loop ran (mock model responds) — fast path refused:
    assert!(outcome.events.iter().all(|e| !e.detail_contains("needle-dispatch")));
}

#[tokio::test]
async fn fast_path_absent_engine_changes_nothing() {
    let (service, _tmp) = test_service(); // no needle engine
    let outcome = service_run(&service, "read_file: {\"path\": \"Cargo.toml\"}").await;
    assert!(outcome.completed); // plain model loop
}
```

Adapt helper names/assertion mechanics to the real test utilities in service.rs — the three behaviors under test are fixed: (a) dispatch happens, model loop skipped, event named `needle-dispatch`; (b) destructive-guardrail refusal falls through with **no** partial execution; (c) `None` engine is byte-for-byte the old behavior.

- [ ] **Step 3: Run** → FAIL. **Step 4: Implement** — in `start_run`, when `self.needle` is `Some(engine)` and the run is a fresh prompt (not a resume seed):

```rust
// Fast path: Needle picks + fills a tool call for well-defined prompts.
// Gates (all must hold): a call was produced; confidence ≥ threshold;
// guardrail says non-destructive; args parse as JSON. Any failure falls
// through to the normal loop — silently, this is an optimization.
let tools_json = serde_json::to_string(&tool_definitions()).unwrap_or_default();
if let Ok(Some(call)) = engine.tool_call(prompt.clone(), tools_json).await
    && call.confidence >= self.config.router_confidence_threshold
{
    let guard = engine
        .decide(
            format!("Is executing `{}` with {} destructive or irreversible?",
                call.name, call.arguments_json),
            vec!["safe".to_string(), "destructive".to_string()],
        )
        .await;
    if matches!(&guard, Ok(d) if d.choice == "safe"
        && d.confidence >= self.config.router_confidence_threshold)
        && serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_ok()
    {
        // emit routing_decision_made{router_name:"needle-dispatch", ...},
        // dispatch through ToolDispatcher (which routes execution through
        // ExecutionProvider — approval gating is preserved), emit the
        // tool_* events exactly as the normal loop does, then the
        // completed event with the tool output as the run text.
        // Follow the normal loop's event emission code in this file.
    }
}
```

The HashBackend guardrail: `decide` on that phrasing with options `["safe","destructive"]` — token overlap with "destructive" appears when the prompt/args contain destructive-ish words (`rm`, `delete`); overlap with neither → confidence 0.5 < 0.7 threshold → falls through. That makes test (b) pass deterministically without teaching HashBackend risk semantics. Verify against the actual HashBackend scoring when writing the guardrail phrase; adjust the phrase (not the backend) if the overlap doesn't discriminate.

- [ ] **Step 5: Wire in `commands/service.rs`** — build the engine via `engine_if_available(&config)` plus the `FORGE_NEEDLE_BACKEND=hash` escape hatch (reuse the shared selection fn), pass `with_needle(...)`.
- [ ] **Step 6: README** — Usage: "Well-defined requests dispatch directly through the on-device brain (sub-100 ms, no LLM call) when confidence is high; everything else runs the full agent loop."
- [ ] **Step 7:** `just verify` → PASS. **Step 8: Commit** — `git commit -am "feat(runtime): needle direct-dispatch fast path"`

---

### Task 11: BDD coverage + final README/spec sweep

**Files:**
- Create: `tests/features/needle_routing.feature`
- Modify: `crates/forge-cli/tests/bdd/steps.rs`
- Modify: `README.md` (final consistency pass), `docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md` (§8 status updates)

- [ ] **Step 1: Write the feature file** (match the Gherkin style of existing features in `tests/features/`):

```gherkin
Feature: Needle embedded decision routing
  Forge's default brain is on-device; without weights it degrades to
  deterministic static routing and never blocks a run.

  Scenario: Default run without weights falls back to static routing
    Given an initialized project with no needle weights
    When I run forge with prompt "explain this project" and model "mock-local"
    Then the run completes successfully
    And the session events contain a routing decision with fallback_used true

  Scenario: Hash-backend routing decides on-device
    Given an initialized project with the hash needle backend
    When I run forge with prompt "explain this project" and model "mock-local"
    Then the run completes successfully
    And the session events contain a routing decision from router "needle"

  Scenario: Doctor reports the needle brain
    Given an initialized project with no needle weights
    When I run forge doctor
    Then the doctor output mentions "needle"

  Scenario: Local-only init skips weight fetching
    Given a fresh project directory
    When I run forge init with --local-only
    Then the init output mentions "skipped"
    And no file exists under the forge cache models directory

  Scenario: Semantic graph search with the hash backend
    Given an initialized project with the hash needle backend and a built graph
    When I run forge graph grep --semantic "parse"
    Then the output lists at least one symbol
```

- [ ] **Step 2: Run** `just bdd` → new scenarios FAIL (missing steps).
- [ ] **Step 3: Implement steps** in `steps.rs` following the existing step patterns (hermetic temp HOME means the cache dir is isolated per scenario — "the forge cache models directory" resolves inside the scenario HOME; "hash needle backend" = set `FORGE_NEEDLE_BACKEND=hash` in the scenario's env; session-event assertions parse `.forge/sessions/*.jsonl` like existing steps do).
- [ ] **Step 4: Run** `just bdd` → PASS; `just verify` → PASS.
- [ ] **Step 5: Final sweep** — reread README top-to-bottom against actual behavior: intro (single binary, embedded decisions), Quickstart (no `forge router serve` in default flow; `forge init && forge run`), DecisionRouter modes list (`needle` default), doctor, graph, known-limitations (add: needle extraction/fast-path quality depends on weights variant; Jev escalation not yet implemented — spec sub-project 2). Update spec §8: license findings from Task 6/8, any API deviations discovered in Task 8.
- [ ] **Step 6: Commit** — `git add -A && git commit -m "test(bdd): needle routing scenarios; README/spec sweep"`

---

## Self-Review (completed)

- **Spec coverage:** §3 crates/weights/default-router → Tasks 2,5,6,8; §4 Embedder/engine/config/CLI → Tasks 1,2,4,7; §5 lifecycle/fast-path/semantic index → Tasks 9,10; §6 error handling → Tasks 3,5,6,7 (fallback, timeout, checksum, degradation); §7 testing → every task + Tasks 8,11; §8 risks → Tasks 1 note, 6 step 1, 8 step 1. Not in this plan (later sub-projects per spec): Jev tier, ACP, MCP, TUI, embedded generation. `extract()`-based arg repair in the agent loop (§5 item 4) is deliberately deferred to the Jev-tier sub-project — it needs real-model quality data first; noted in known-limitations (Task 11).
- **Placeholder scan:** engine_from_config stub in Task 5 is explicitly completed in Task 6 with the signature fixed — intentional two-phase, not a placeholder. FFI symbol names are unpinnable until the header is downloaded; Task 8 fixes the contract at the trait and demands adjustment against the real header.
- **Type consistency:** `NeedleConfig` fields (4→5,6,7), `WeightsStatus` (6→7), `engine_if_available` (9→10), `NeedleToolCall` (2→10), `EngineEmbedder::new` async constructor note (2→9) checked.
- **Review Focus:** all five pinned — truncated weights (Task 6 tests), invalid variant (Task 4), model_id mismatch rebuild (Task 9), destructive fast-path refusal (Task 10), duplicate symbol names (Task 9).
