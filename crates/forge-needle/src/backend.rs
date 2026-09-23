use std::path::PathBuf;

/// Error surface of a Needle backend. `Declined` is a designed outcome:
/// Needle refuses to guess; callers must fall back, never retry blindly.
#[derive(Debug, Clone, thiserror::Error)]
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

/// Generic fallback cache location, used only by `UnavailableBackend`'s
/// `Default` impl (tests, and any caller without a resolved
/// `NeedleConfig` to hand): `~/.cache/forge/models/needle3-full.bin`,
/// matching `NeedleConfig::default().variant` ("full" — the only variant
/// with a hosted, pinned artifact today). Mirrors the
/// `std::env::home_dir()` fallback pattern `forge_config::Config` uses for
/// `~/.config/forge/config.toml`. Real, variant-aware resolution
/// (including the pinned filename and any `weights_path` override) lives in
/// `crate::weights::weights_path` (Task 6); `engine_from_config` always
/// resolves through that and passes the result to `UnavailableBackend::new`.
pub(crate) fn default_weights_path() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("forge")
        .join("models")
        .join("needle3-full.bin")
}

/// Placeholder backend used by `engine_from_config` until the real FFI
/// backend (Task 8) and weights resolution (Task 6) land. `load()` always
/// fails with `WeightsMissing`, so any `NeedleRouter` built on it always
/// errors and the `FallbackRouter` wrapping it in `router_from_config`
/// degrades to the configured fallback (`static`, by default). This makes
/// `router = "needle"` a safe, honest default before real inference exists.
pub struct UnavailableBackend {
    weights_path: PathBuf,
}

impl UnavailableBackend {
    /// Build one that reports the real resolved weights path (from
    /// `weights::weights_path`, Task 6) instead of the generic default.
    pub fn new(weights_path: PathBuf) -> Self {
        Self { weights_path }
    }
}

impl Default for UnavailableBackend {
    fn default() -> Self {
        Self {
            weights_path: default_weights_path(),
        }
    }
}

impl NeedleBackend for UnavailableBackend {
    fn load(&mut self) -> Result<(), BackendError> {
        Err(BackendError::WeightsMissing(self.weights_path.clone()))
    }

    fn model_id(&self) -> String {
        "unavailable".to_string()
    }

    fn dimensions(&self) -> usize {
        0
    }

    // `load()` always fails, so the engine never dispatches jobs to these —
    // they exist only to satisfy the trait.
    fn decide(&mut self, _task: &str, _options: &[String]) -> Result<Decision, BackendError> {
        Err(BackendError::NotLoaded)
    }

    fn embed(&mut self, _texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        Err(BackendError::NotLoaded)
    }

    fn extract(&mut self, _text: &str, _schema_json: &str) -> Result<String, BackendError> {
        Err(BackendError::NotLoaded)
    }

    fn tool_call(
        &mut self,
        _prompt: &str,
        _tools_json: &str,
    ) -> Result<Option<NeedleToolCall>, BackendError> {
        Err(BackendError::NotLoaded)
    }
}
