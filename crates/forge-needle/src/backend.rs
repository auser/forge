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
