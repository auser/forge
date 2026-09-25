use std::path::PathBuf;

/// The single command that turns a brain-less forge into one with a working
/// brain, quoted verbatim by every place that has to tell someone so:
/// [`BackendError::EngineMissing`] (what a failed route reports), `forge
/// init`'s weights step, and `forge doctor`'s needle line-pair.
///
/// It lives here, in the library that knows the engine is absent, rather than
/// being retyped in each caller — because the failure mode this whole constant
/// exists to prevent was three sites each telling a *different*, individually
/// correct half of the story. One string means they cannot drift.
///
/// Exactly one command, and no mention of `forge init` — that is the whole
/// point. Telling a backend-less build's user to run `forge init` is the
/// contradiction this constant exists to end; the weights fetch resumes by
/// itself once the binary has a backend, and the callers that are *about* to
/// run init say so in their own words.
///
/// `--git` rather than `--path`: someone hitting this may well have installed
/// a prebuilt binary and have no checkout to build from. Release binaries for
/// the platforms with a verified engine already ship with the brain, so the
/// other honest answer — upgrade — is named in README's install section rather
/// than here, where it would dilute the one command.
pub const ENGINE_REMEDY: &str = "install a build with the brain: \
     `cargo install --locked --features needle-ffi --git https://github.com/auser/forge forge-cli`";

/// Error surface of a Needle backend. `Declined` is a designed outcome:
/// Needle refuses to guess; callers must fall back, never retry blindly.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BackendError {
    #[error("needle backend not loaded")]
    NotLoaded,
    /// This binary has no inference engine compiled in at all — not a
    /// filesystem problem, and *not* something `forge init` can fix, which is
    /// why it is a separate variant from [`Self::WeightsMissing`] rather than
    /// a reuse of it.
    // No "needle: " prefix: `NeedleRouter` already prefixes what it surfaces,
    // and the user's report showed the duplicate ("router error: needle:
    // needle: …") reads as a bug in the message itself.
    #[error(
        "this build has no embedded inference backend, so on-device routing cannot run — \
         {ENGINE_REMEDY}"
    )]
    EngineMissing,
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

/// The backend `engine_from_config` picks when this binary was built without
/// the `ffi` feature: there is no engine in the process at all. `load()`
/// always fails, so any `NeedleRouter` built on it always errors and the
/// `FallbackRouter` wrapping it in `router_from_config` degrades to the
/// configured fallback (`static`, by default). `router = "needle"` is
/// therefore a safe default in every build; without the engine it is simply
/// honest about being unavailable.
///
/// It deliberately holds **no weights path**. It used to, and reported
/// [`BackendError::WeightsMissing`] naming it — which read as "your weights
/// are missing, run `forge init`" in a binary whose `forge init` skips the
/// fetch *because* there is no backend. The absence of a field here is the
/// fix: there is nothing about the filesystem to report, so there is no way
/// to accidentally report it.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableBackend;

impl NeedleBackend for UnavailableBackend {
    fn load(&mut self) -> Result<(), BackendError> {
        Err(BackendError::EngineMissing)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this exists to prevent: a binary with no inference backend
    /// reported `WeightsMissing`, whose message says "run `forge init` to
    /// fetch" — while `forge init` in that same binary *skips* the fetch
    /// precisely because there is no backend. Two correct sentences, one
    /// impossible loop. The no-backend case must therefore never produce a
    /// weights-shaped error.
    #[test]
    fn a_backend_less_build_never_blames_missing_weights() {
        let mut backend = UnavailableBackend;
        let err = backend.load().expect_err("there is no engine to load");

        assert!(
            matches!(err, BackendError::EngineMissing),
            "expected EngineMissing, got {err:?}"
        );
        let message = err.to_string();
        assert!(
            !message.contains("forge init"),
            "the remedy is not `forge init` — init cannot help a backend-less build: {message}"
        );
        assert!(
            !message.to_lowercase().contains("weights"),
            "nothing is wrong with the weights: {message}"
        );
    }

    /// Whatever the wording, the message has to end in something the reader
    /// can actually run, and it has to be the same one `forge init` and
    /// `forge doctor` name — hence one shared constant.
    #[test]
    fn the_no_backend_error_carries_the_one_remedy() {
        let message = BackendError::EngineMissing.to_string();
        assert!(
            message.contains(ENGINE_REMEDY),
            "the error must carry the shared remedy verbatim: {message}"
        );
        assert!(
            ENGINE_REMEDY.contains("needle-ffi"),
            "the remedy must name the feature: {ENGINE_REMEDY}"
        );
        assert!(
            !ENGINE_REMEDY.contains("forge init"),
            "the remedy is a reinstall, not a refetch: {ENGINE_REMEDY}"
        );
    }

    /// `WeightsMissing` keeps its `forge init` hint — it is correct there, and
    /// only there: a build that *has* a backend and no weights is exactly what
    /// `forge init` fixes.
    #[test]
    fn missing_weights_still_points_at_forge_init() {
        let err = BackendError::WeightsMissing(PathBuf::from("/cache/needle3.cact"));
        let message = err.to_string();
        assert!(message.contains("forge init"), "{message}");
        assert!(message.contains("/cache/needle3.cact"), "{message}");
    }
}
