//! The real on-device backend: [`NeedleBackend`] implemented over
//! `libneedle`'s C API (crate `needle-sys`, feature `ffi`).
//!
//! # Mapping the trait onto the C API
//!
//! `libneedle` exposes one primitive for generation — `needle_complete`,
//! which answers with tool calls constrained by a byte-level grammar compiled
//! from the tools JSON handed to `needle_init` — plus `needle_embed` for
//! vectors. Three of the four trait methods therefore ride on the same
//! primitive with different tool surfaces:
//!
//! | trait method | tool surface installed by `needle_init` |
//! |---|---|
//! | [`FfiBackend::decide`]    | one no-argument tool per option; the selected tool *is* the choice |
//! | [`FfiBackend::extract`]   | a single record tool whose `parameters` are the caller's schema |
//! | [`FfiBackend::tool_call`] | the caller's tools JSON, unchanged |
//! | [`FfiBackend::embed`]     | none — `needle_embed` needs only loaded weights |
//!
//! Every operation is one-shot: `needle_reset()` then `needle_init()` then
//! `needle_complete()`. The trait's methods are independent of one another, so
//! carrying conversation state between them would only let an earlier task
//! contaminate a later one (Needle's own docs warn that unrelated queries on
//! one conversation lose accuracy).
//!
//! # Safety model
//!
//! The C API is **one process-global, non-thread-safe model**. Two things
//! uphold that:
//!
//! 1. `NeedleEngine` owns the backend on a single dedicated thread, so calls
//!    are serialised by construction.
//! 2. [`FfiBackend::load`] claims a process-global flag, so a second engine
//!    in the same process fails with a typed error instead of racing the
//!    first one through shared C state.
//!
//! Beyond that, the wrapper defends against three behaviours measured on the
//! real library rather than promised by the header:
//!
//! * `needle_init` **does not validate its tools JSON** — `"{not json"`
//!   returns success. Every JSON string is parsed here before it is handed
//!   over, and built with `serde_json` rather than string formatting.
//! * `needle_complete` **truncates silently** — a short output buffer still
//!   returns success, with invalid JSON in it. The buffer is 256 KiB
//!   (envelopes run a few hundred bytes) and truncation is reported as a
//!   typed error rather than mistaken for a bad envelope.
//! * `needle_last_error` returns a pointer the runtime invalidates on the
//!   next call, so it is copied into an owned `String` immediately.
//!
//! No pointer returned by the library is ever retained, and nothing here has
//! to be freed: every output goes into a caller-owned buffer.
//! `needle_load` copies the archive (verified by revoking access to the
//! source buffer with `mprotect(PROT_NONE)` and continuing to infer), so the
//! weights `Vec` is dropped right after loading.

use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::backend::{BackendError, Decision, NeedleBackend, NeedleToolCall};

/// Output buffer for `needle_complete`. Envelopes for forge's tool surfaces
/// run a few hundred bytes; 256 KiB leaves room for a large extraction record
/// while keeping truncation effectively unreachable.
const OUT_CAPACITY: usize = 256 * 1024;

/// Matches the Python SDK's default. Decisions and records are short; this is
/// a ceiling, not a target.
const MAX_NEW_TOKENS: i32 = 512;

/// Fallback name for the record tool in [`FfiBackend::extract`] when the
/// schema carries no `title`.
const RECORD_TOOL: &str = "record";

/// Is a live `FfiBackend` already holding the process-global model? The C API
/// has exactly one, and it cannot be unloaded.
static LIVE: AtomicBool = AtomicBool::new(false);

/// Which weights this process has bound, if any. `libneedle` cannot swap
/// weights once loaded, so a request for a different archive has to fail
/// loudly rather than silently answer from the first one.
static BOUND_WEIGHTS: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Needle 3 over `libneedle`. Construct with [`FfiBackend::new`] and hand it
/// to `NeedleEngine::spawn`; the engine calls [`NeedleBackend::load`] lazily
/// on the first job and retries while it reports
/// [`BackendError::WeightsMissing`], so weights that appear later (after
/// `forge init`) start working without restarting the process.
pub struct FfiBackend {
    weights: PathBuf,
    /// Set once this instance has taken the [`LIVE`] claim, so a retried
    /// `load()` doesn't fight itself for it.
    claimed: bool,
    /// Captured during `load()` from `needle_embed(_, null, 0)`;
    /// `dimensions()` is a `&self` method and cannot call into C.
    dimensions: usize,
    /// Reused `needle_complete` output buffer, allocated on first load.
    out: Vec<u8>,
    ready: bool,
}

impl FfiBackend {
    /// Bind to the `.cact` archive at `weights`. Nothing is loaded and no
    /// global claim is taken until [`NeedleBackend::load`] runs.
    pub fn new(weights: PathBuf) -> Self {
        Self {
            weights,
            claimed: false,
            dimensions: 0,
            out: Vec::new(),
            ready: false,
        }
    }

    /// One inference: install `tools_json` as the tool surface, then complete
    /// `input`. `tools_json` must already be validated — `needle_init` accepts
    /// malformed JSON silently.
    ///
    /// `needle_init` runs on **every** call, even when the tool surface is
    /// unchanged. That looks wasteful and is not: `needle_init` is what
    /// tokenizes and caches the static prefix, so it makes the following
    /// `needle_complete` faster rather than slower. Skipping it for a repeated
    /// tool surface — the obvious optimisation, and what the Python SDK's
    /// bind-once path appears to do — was measured on macos-arm64 /
    /// `needle3.cact` and made a warm route round-trip *30x worse*
    /// (~0.5 s → 16.5 s on the first call after the skip). Do not
    /// reintroduce it without re-measuring.
    fn run(&mut self, tools_json: &str, input: &str) -> Result<Envelope, BackendError> {
        if !self.ready {
            return Err(BackendError::NotLoaded);
        }
        let tools = cstring(tools_json, "tools JSON")?;
        let text = cstring(input, "input text")?;
        let capacity = i32::try_from(self.out.len()).map_err(|_| {
            BackendError::Inference("output buffer larger than the C API's int capacity".into())
        })?;

        // SAFETY: all pointers come from live `CString`/`Vec` values owned by
        // this stack frame and outlive the calls. The engine is serialised
        // onto one thread by `NeedleEngine` and claimed globally by `load()`,
        // satisfying the C API's "one process-global, non-thread-safe model"
        // contract. `needle_complete` honours `out_capacity` (verified
        // against a guard page).
        let rc = unsafe {
            needle_sys::needle_reset();
            let prefix = needle_sys::needle_init(
                c"".as_ptr(),
                tools.as_ptr(),
                // No tool index: forge's tool surfaces are small, and a
                // persisted index would be another file to invalidate.
                std::ptr::null(),
            );
            if prefix < 0 {
                return Err(BackendError::Inference(format!(
                    "needle_init failed (code {prefix}): {}",
                    last_error()
                )));
            }
            needle_sys::needle_complete(
                text.as_ptr(),
                MAX_NEW_TOKENS,
                self.out.as_mut_ptr().cast(),
                capacity,
            )
        };
        if rc < 0 {
            return Err(BackendError::Inference(format!(
                "needle_complete failed (code {rc}): {}",
                last_error()
            )));
        }

        // The buffer holds a NUL-terminated string. `rc` is a *token* count,
        // not a byte count, so the NUL is the only length signal.
        let written = self
            .out
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.out.len());
        let raw = std::str::from_utf8(&self.out[..written]).map_err(|e| {
            BackendError::Inference(format!("needle returned a non-UTF-8 envelope: {e}"))
        })?;
        serde_json::from_str::<Envelope>(raw).map_err(|e| {
            // A full buffer means the envelope was cut off mid-JSON rather
            // than the engine emitting something malformed.
            if written >= self.out.len().saturating_sub(1) {
                BackendError::Inference(format!(
                    "needle output exceeded the {}-byte buffer and was truncated",
                    self.out.len()
                ))
            } else {
                BackendError::Inference(format!("needle returned an unparseable envelope: {e}"))
            }
        })
    }
}

impl NeedleBackend for FfiBackend {
    fn load(&mut self) -> Result<(), BackendError> {
        if self.ready {
            return Ok(());
        }
        // Checked before claiming anything global: a backend pointed at
        // weights that aren't there yet must stay retryable (the engine
        // re-attempts `load()` on every job while it reports
        // `WeightsMissing`) without holding the process-wide claim hostage.
        if !self.weights.is_file() {
            return Err(BackendError::WeightsMissing(self.weights.clone()));
        }
        if !self.claimed {
            if LIVE.swap(true, Ordering::SeqCst) {
                return Err(BackendError::Inference(
                    "another needle FFI backend already owns this process's libneedle runtime; \
                     the C API supports one model per process"
                        .into(),
                ));
            }
            self.claimed = true;
        }

        {
            let mut bound = lock(&BOUND_WEIGHTS);
            match bound.as_ref() {
                // Same archive already in the engine (a previous backend in
                // this process loaded it). libneedle keeps it; re-reading
                // 35 MB to load it again would be pure waste.
                Some(already) if already == &self.weights => {}
                Some(other) => {
                    return Err(BackendError::Inference(format!(
                        "libneedle cannot rebind weights: this process already loaded {}, \
                         but this backend wants {}",
                        other.display(),
                        self.weights.display()
                    )));
                }
                None => {
                    let bytes = std::fs::read(&self.weights).map_err(|e| {
                        BackendError::Inference(format!(
                            "reading needle weights {}: {e}",
                            self.weights.display()
                        ))
                    })?;
                    let n = u64::try_from(bytes.len()).map_err(|_| {
                        BackendError::Inference("weights file length does not fit u64".into())
                    })?;
                    // SAFETY: `bytes` is live for the duration of the call.
                    // `needle_load` copies the archive, so dropping `bytes`
                    // afterwards is sound (verified by revoking the source
                    // pages after loading and continuing to infer).
                    let rc = unsafe { needle_sys::needle_load(bytes.as_ptr(), n) };
                    if rc < 0 {
                        return Err(BackendError::Inference(format!(
                            "needle_load({}) failed (code {rc}): {}",
                            self.weights.display(),
                            last_error()
                        )));
                    }
                    *bound = Some(self.weights.clone());
                }
            }
        }

        // A null output asks for the embedding dimension without computing.
        // SAFETY: weights are loaded (checked above); a null `out` with
        // capacity 0 is the documented dimension probe.
        let dim = unsafe { needle_sys::needle_embed(c"".as_ptr(), std::ptr::null_mut(), 0) };
        if dim <= 0 {
            return Err(BackendError::Inference(format!(
                "needle_embed could not report the embedding dimension (code {dim}): {}",
                last_error()
            )));
        }
        self.dimensions = usize::try_from(dim).map_err(|_| {
            BackendError::Inference(format!("needle reported a negative dimension: {dim}"))
        })?;
        self.out = vec![0u8; OUT_CAPACITY];
        self.ready = true;
        Ok(())
    }

    /// Derived from the weights filename, because this value versions any
    /// stored embedding index (see `forge_core::embed::Embedder::model_id`):
    /// swapping in a tuned `.cact` must invalidate vectors built with the
    /// base model.
    fn model_id(&self) -> String {
        self.weights
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("needle3")
            .to_string()
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// One no-argument tool per option: the tool Needle selects *is* the
    /// choice. Needle returns the tool name exactly as it was given
    /// (hyphens, dots and slashes all round-trip, verified with real model
    /// ids like `openai/gpt-5.1-mini`), so options are matched back by exact
    /// string equality and a name outside the offered set is an error rather
    /// than a guess.
    ///
    /// An off-topic task yields empty `function_calls`, and a call the engine
    /// withheld for low confidence lands in `suppressed_calls`. Both become
    /// [`BackendError::Declined`] — the caller falls back rather than acting
    /// on a guess.
    fn decide(&mut self, task: &str, options: &[String]) -> Result<Decision, BackendError> {
        if options.is_empty() {
            return Err(BackendError::Declined);
        }
        // Duplicate tool names would make the decode grammar ambiguous and
        // the chosen name unattributable.
        let mut unique: Vec<&String> = Vec::with_capacity(options.len());
        for option in options {
            if !unique.contains(&option) {
                unique.push(option);
            }
        }
        let tools: Vec<Value> = unique
            .iter()
            .map(|option| {
                json!({
                    "name": option,
                    // The trait hands us bare option strings, so the name is
                    // also the only description available. Needle reads tool
                    // names and descriptions for semantics, which is why
                    // option strings should stay human-meaningful.
                    "description": option,
                    "parameters": {"type": "object", "properties": {}, "required": []},
                })
            })
            .collect();
        let tools_json = to_json(&tools)?;

        let envelope = self.run(&tools_json, task)?;
        envelope.check_engine_error()?;
        let call = envelope
            .function_calls
            .first()
            .ok_or(BackendError::Declined)?;
        let choice = unique
            .iter()
            .find(|option| ***option == call.name)
            .ok_or_else(|| {
                BackendError::Inference(format!(
                    "needle selected {:?}, which is not one of the offered options",
                    call.name
                ))
            })?;
        Ok(Decision {
            choice: (*choice).clone(),
            confidence: envelope.calibrated_confidence(),
            reason: envelope.reasoning.clone().unwrap_or_default(),
        })
    }

    /// One vector per text, L2-normalised by the engine, `dimensions()` long.
    ///
    /// The C API embeds a single string per call — there is no batch entry
    /// point — so this loops. It is the cheap path regardless: a `needle_embed`
    /// call runs in single-digit milliseconds and needs no `needle_init`.
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        if !self.ready {
            return Err(BackendError::NotLoaded);
        }
        let dim = i32::try_from(self.dimensions).map_err(|_| {
            BackendError::Inference("embedding dimension does not fit the C API's int".into())
        })?;
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let input = cstring(text, "embedding input")?;
            let mut vector = vec![0f32; self.dimensions];
            // SAFETY: `input` outlives the call; `vector` has exactly `dim`
            // floats of capacity, which is what we declare. A capacity
            // smaller than the model's dimension is rejected by the library
            // (returns -1) rather than overflowing.
            let rc = unsafe { needle_sys::needle_embed(input.as_ptr(), vector.as_mut_ptr(), dim) };
            if rc != dim {
                return Err(BackendError::Inference(format!(
                    "needle_embed wrote {rc} of {dim} floats: {}",
                    last_error()
                )));
            }
            out.push(vector);
        }
        Ok(out)
    }

    /// Grammar-constrained extraction: the caller's JSON Schema becomes the
    /// `parameters` of a single tool, and the tool's arguments are the
    /// extracted record. The returned string is that arguments object,
    /// re-serialised — it parses by construction.
    ///
    /// **The schema's `title` and `description` matter.** Needle reads the
    /// tool's name and description for semantics, so a schema with neither
    /// gives it no handle on what the record *means* and it will often refuse
    /// (measured: `{"type":"object","properties":{"city":{"type":"string"}}}`
    /// over "weather in Paris" is declined, while the same schema with
    /// `"title": "weather_query"` — or a description naming a weather request
    /// — extracts `{"city":"Paris"}`). `title` becomes the tool name,
    /// mirroring the Python SDK, where a Pydantic model's class name does the
    /// same job.
    ///
    /// A refusal (nothing extracted, or a record withheld for low confidence)
    /// is [`BackendError::Declined`], never an invented record.
    fn extract(&mut self, text: &str, schema_json: &str) -> Result<String, BackendError> {
        let schema: Value = serde_json::from_str(schema_json)
            .map_err(|e| BackendError::Inference(format!("extraction schema is not JSON: {e}")))?;
        if !schema.is_object() {
            return Err(BackendError::Inference(
                "extraction schema must be a JSON object".into(),
            ));
        }
        let name = schema
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(RECORD_TOOL);
        let description = schema
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
            .unwrap_or("Extract the fields described by the schema from the text");
        let tools = vec![json!({
            "name": name,
            "description": description,
            "parameters": schema,
        })];
        let tools_json = to_json(&tools)?;

        let envelope = self.run(&tools_json, text)?;
        envelope.check_engine_error()?;
        let record = envelope
            .function_calls
            .first()
            .ok_or(BackendError::Declined)?;
        to_json(&record.arguments)
    }

    /// Native tool calling with the caller's tools JSON verbatim. `None` is
    /// Needle's refusal: an off-topic prompt returns empty `function_calls`
    /// (and a withheld low-confidence call lands in `suppressed_calls`)
    /// rather than a fabricated call.
    fn tool_call(
        &mut self,
        prompt: &str,
        tools_json: &str,
    ) -> Result<Option<NeedleToolCall>, BackendError> {
        // `needle_init` accepts malformed JSON silently, so validate here or
        // never find out.
        let tools: Value = serde_json::from_str(tools_json)
            .map_err(|e| BackendError::Inference(format!("tools JSON is not JSON: {e}")))?;
        match tools.as_array() {
            Some(array) if array.iter().all(Value::is_object) => {}
            _ => {
                return Err(BackendError::Inference(
                    "tools JSON must be an array of tool objects".into(),
                ));
            }
        }

        let envelope = self.run(tools_json, prompt)?;
        envelope.check_engine_error()?;
        let Some(call) = envelope.function_calls.first() else {
            return Ok(None);
        };
        Ok(Some(NeedleToolCall {
            name: call.name.clone(),
            arguments_json: to_json(&call.arguments)?,
            confidence: envelope.calibrated_confidence(),
        }))
    }
}

impl Drop for FfiBackend {
    fn drop(&mut self) {
        // libneedle offers no unload, so the weights stay in the process.
        // Releasing the claim lets a replacement engine bind the *same*
        // archive (`BOUND_WEIGHTS` short-circuits the reload); a different
        // archive still fails loudly, which is the truth about the library.
        if self.claimed {
            LIVE.store(false, Ordering::SeqCst);
        }
    }
}

/// The JSON envelope `needle_complete` writes. Unknown fields (timings,
/// `peak_ram_mb`, `validation`, ...) are ignored; every field forge reads is
/// optional so an envelope shape change degrades to a typed error rather
/// than a parse failure.
#[derive(Debug, serde::Deserialize)]
struct Envelope {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    function_calls: Vec<Call>,
    #[serde(default)]
    reasoning: Option<String>,
    /// `null` when the loaded archive carries no confidence head — local
    /// `needle build --lora` output, as opposed to the published model or a
    /// platform fine-tune.
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Debug, serde::Deserialize)]
struct Call {
    name: String,
    #[serde(default)]
    arguments: Value,
}

impl Envelope {
    /// The engine reports its own failures inside the envelope as well as
    /// through the return code (e.g. calling `needle_complete` before
    /// `needle_init` yields `{"type":"error","error":"needle_init not
    /// called"}`).
    fn check_engine_error(&self) -> Result<(), BackendError> {
        let failed = self.success == Some(false) || self.r#type.as_deref() == Some("error");
        if failed {
            return Err(BackendError::Inference(self.error.clone().unwrap_or_else(
                || "needle reported a failure without a message".to_string(),
            )));
        }
        Ok(())
    }

    /// Confidence clamped to `[0, 1]`. Weights with no confidence head report
    /// `null`, which becomes `0.0`: forge's routing treats low confidence as
    /// "escalate to the fallback", so an unknown score must not read as a
    /// confident one.
    fn calibrated_confidence(&self) -> f64 {
        self.confidence.unwrap_or(0.0).clamp(0.0, 1.0)
    }
}

/// Copy `needle_last_error` out before the next call invalidates it.
fn last_error() -> String {
    // SAFETY: the runtime owns this string and keeps it valid until the next
    // API call; `to_string_lossy().into_owned()` copies before then. The
    // pointer is non-null in practice (it points at "" when nothing failed),
    // but that is not promised, so it is checked.
    let ptr = unsafe { needle_sys::needle_last_error() };
    if ptr.is_null() {
        return "no error message".to_string();
    }
    let message = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    if message.trim().is_empty() {
        "no error message".to_string()
    } else {
        message
    }
}

/// NUL-terminate a Rust string for the C API. An interior NUL cannot be
/// passed at all, so it is a typed error rather than a silently truncated
/// prompt.
fn cstring(value: &str, what: &str) -> Result<CString, BackendError> {
    CString::new(value)
        .map_err(|_| BackendError::Inference(format!("{what} contains an interior NUL byte")))
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, BackendError> {
    serde_json::to_string(value)
        .map_err(|e| BackendError::Inference(format!("serialising JSON for needle: {e}")))
}

/// A poisoned lock here means a previous holder panicked while recording
/// which weights were bound. The value is a `PathBuf` with no invariant to
/// violate, so recovering is safe — and better than propagating a panic into
/// the engine thread.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests exercise the pure envelope/argument handling, which is
    // where the wrapper's decisions live. Anything that calls into
    // `libneedle` needs real weights and a linked engine and lives in
    // `tests/e2e.rs` behind feature `needle-e2e`.

    fn envelope(json: &str) -> Envelope {
        serde_json::from_str(json).expect("test envelope parses")
    }

    #[test]
    fn real_engine_envelope_parses() {
        // Captured verbatim from `libneedle` (macos-arm64, needle3.cact).
        let e = envelope(
            r#"{"type":"call","success":true,"error":null,"error_code":null,"reason":null,
                "function_calls":[{"name":"qwen3-coder","arguments":{}}],"suppressed_calls":[],
                "reasoning":"Query asks to refactor code.","confidence":0.7826,
                "prefill_tps":56.3,"decode_tps":4.3,"peak_ram_mb":91.2,
                "validation":{"ungrounded":[],"negation":false}}"#,
        );
        assert!(e.check_engine_error().is_ok());
        assert_eq!(e.function_calls.len(), 1);
        assert_eq!(e.function_calls[0].name, "qwen3-coder");
        assert!((e.calibrated_confidence() - 0.7826).abs() < 1e-9);
    }

    #[test]
    fn error_envelope_becomes_a_typed_error() {
        // The shape the engine writes when `needle_init` was never called.
        let e = envelope(r#"{"type":"error","error":"needle_init not called"}"#);
        let err = e.check_engine_error().expect_err("must be an error");
        assert!(err.to_string().contains("needle_init not called"), "{err}");
    }

    #[test]
    fn success_false_becomes_a_typed_error_even_without_a_message() {
        let e = envelope(r#"{"type":"call","success":false}"#);
        let err = e.check_engine_error().expect_err("must be an error");
        assert!(matches!(err, BackendError::Inference(_)));
    }

    #[test]
    fn missing_confidence_reads_as_zero_not_as_confident() {
        // Weights built by `needle build --lora` carry no confidence head.
        let e = envelope(r#"{"type":"call","success":true,"function_calls":[],"confidence":null}"#);
        assert_eq!(e.calibrated_confidence(), 0.0);
    }

    #[test]
    fn out_of_range_confidence_is_clamped() {
        assert_eq!(
            envelope(r#"{"confidence":1.7}"#).calibrated_confidence(),
            1.0
        );
        assert_eq!(
            envelope(r#"{"confidence":-0.5}"#).calibrated_confidence(),
            0.0
        );
    }

    #[test]
    fn unknown_envelope_fields_do_not_break_parsing() {
        // Forward compatibility: a new engine field must not turn every
        // inference into an error.
        let e = envelope(r#"{"type":"call","success":true,"brand_new_field":{"a":1}}"#);
        assert!(e.check_engine_error().is_ok());
        assert!(e.function_calls.is_empty());
    }

    #[test]
    fn interior_nul_is_rejected_rather_than_truncating_the_prompt() {
        let err = cstring("before\0after", "input text").expect_err("must reject");
        assert!(err.to_string().contains("interior NUL"), "{err}");
    }

    #[test]
    fn model_id_tracks_the_weights_file_so_indexes_invalidate() {
        let base = FfiBackend::new(PathBuf::from("/cache/needle3.cact"));
        let tuned = FfiBackend::new(PathBuf::from("/cache/my-tuned.cact"));
        assert_eq!(base.model_id(), "needle3");
        assert_eq!(tuned.model_id(), "my-tuned");
        assert_ne!(base.model_id(), tuned.model_id());
    }

    #[test]
    fn model_id_falls_back_when_the_path_has_no_stem() {
        assert_eq!(FfiBackend::new(PathBuf::from("/")).model_id(), "needle3");
    }

    #[test]
    fn missing_weights_are_reported_before_any_global_claim_is_taken() {
        // Must be `WeightsMissing` (which the engine retries) and must not
        // consume the process-global claim — otherwise a backend waiting for
        // `forge init` would lock out every other engine in the process.
        let mut backend = FfiBackend::new(PathBuf::from("/definitely/not/here/needle3.cact"));
        let err = backend.load().expect_err("no weights there");
        assert!(matches!(err, BackendError::WeightsMissing(_)), "{err}");
        assert!(!backend.claimed, "a failed load must not hold the claim");
        assert!(
            !LIVE.load(Ordering::SeqCst),
            "the global claim must be free"
        );
    }

    #[test]
    fn calls_before_load_report_not_loaded_instead_of_touching_c_state() {
        let mut backend = FfiBackend::new(PathBuf::from("/definitely/not/here/needle3.cact"));
        assert!(matches!(
            backend.embed(&["x".to_string()]),
            Err(BackendError::NotLoaded)
        ));
        assert!(matches!(
            backend.run("[]", "x"),
            Err(BackendError::NotLoaded)
        ));
    }

    #[test]
    fn decide_without_options_declines_without_calling_the_engine() {
        let mut backend = FfiBackend::new(PathBuf::from("/definitely/not/here/needle3.cact"));
        assert!(matches!(
            backend.decide("t", &[]),
            Err(BackendError::Declined)
        ));
    }

    #[test]
    fn tool_call_rejects_malformed_tools_json_that_the_c_api_would_accept() {
        // `needle_init("{not json")` returns success, so this validation is
        // the only thing standing between a typo and a silently degraded
        // tool surface.
        let mut backend = FfiBackend::new(PathBuf::from("/definitely/not/here/needle3.cact"));
        for bad in ["{not json", "{\"name\":\"x\"}", "[1,2,3]", "\"a string\""] {
            let err = backend
                .tool_call("p", bad)
                .expect_err(&format!("{bad:?} must be rejected"));
            assert!(matches!(err, BackendError::Inference(_)), "{bad:?}: {err}");
        }
    }

    #[test]
    fn extract_rejects_a_schema_that_is_not_a_json_object() {
        let mut backend = FfiBackend::new(PathBuf::from("/definitely/not/here/needle3.cact"));
        assert!(backend.extract("text", "[]").is_err());
        assert!(backend.extract("text", "not json").is_err());
    }
}
