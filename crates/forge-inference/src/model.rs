//! The pinned model catalog.
//!
//! Every model forge can run locally is listed here with the exact files
//! that make it up and the SHA-256 of each. Nothing in this crate reaches
//! the network for a model it has not pinned, and nothing runs a file whose
//! bytes do not hash to the value recorded here.
//!
//! **Why a catalog and not "point it at a directory".** A local model that
//! forge downloads is a supply-chain surface: forge would execute whatever
//! bytes arrived. Pinning the digest means the bytes forge runs are the
//! bytes a human verified once, and a mirror that serves different bytes is
//! rejected rather than executed. An operator who wants to run their own
//! model can point `embedding.weights_dir` at it and take responsibility
//! for it themselves.
//!
//! **The int8 default is deliberate.** `bge-small-en-v1.5` is published in
//! both float32 and int8. Measured on an Apple Silicon Mac against the
//! same text batch, int8 was *both* smaller (34 MB vs 133 MB) and faster
//! (77–185 ms load vs 278–1093 ms; 83–88 ms/text batched vs 133–162
//! ms/text), because the weights stay resident in cache instead of being
//! streamed through the matmul path at full width. The vectors barely
//! moved: the code-vs-prose cosine separation was 0.3609 for int8 against
//! 0.3627 for float32. The float32 model stays in the catalog for anyone
//! who wants maximum fidelity.
//!
//! **Pooled, not raw.** Every model here is an encoder: it maps a token
//! sequence to one vector. The pooling step that turns a sequence of token
//! vectors into that single vector is part of the model's contract, not a
//! runtime detail, so it is recorded here next to the checksum.

use forge_core::ForgeError;

/// One downloadable file belonging to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact {
    /// Path within the model's repository on the model hub. The file lands
    /// on disk under [`filename`](Artifact::filename); this is where it
    /// comes *from*.
    pub repo_path: &'static str,
    /// File name inside the model's weights directory.
    pub filename: &'static str,
    /// SHA-256 of the file's bytes, lowercase hex.
    pub sha256: &'static str,
    /// Expected size in bytes. Checked before the bytes are trusted, so a
    /// truncated transfer is caught even if it somehow hashed correctly.
    pub bytes: u64,
}

/// How a model's token vectors become one vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Take the first token's vector (the `[CLS]` token). This is what the
    /// `bge` family was trained to use.
    Cls,
    /// Average the non-padding token vectors.
    Mean,
}

/// A pinned, locally-runnable encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// Stable id, used as `embedding.model` in config.
    pub id: &'static str,
    /// Human-readable name for `forge doctor` and friends.
    pub display_name: &'static str,
    /// Width of the vectors this model produces.
    pub dimensions: usize,
    /// Longest token sequence fed to the model. Longer inputs are truncated
    /// at this many tokens.
    pub max_sequence_len: usize,
    /// How token vectors are pooled into one.
    pub pooling: Pooling,
    /// The model graph itself.
    pub model: Artifact,
    /// The tokenizer that feeds it.
    pub tokenizer: Artifact,
}

/// The tokenizer is shared by every `bge-small-en-v1.5` export.
const BGE_TOKENIZER: Artifact = Artifact {
    repo_path: "tokenizer.json",
    filename: "tokenizer.json",
    sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    bytes: 711_396,
};

/// Every model forge can run locally.
///
/// Verified 2026-10-31: each `sha256` was computed from a fresh download
/// of the pinned URL and re-checked against a second independent download.
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "bge-small-en-v1.5-int8",
        display_name: "BAAI bge-small-en-v1.5 (int8)",
        dimensions: 384,
        max_sequence_len: 512,
        pooling: Pooling::Cls,
        model: Artifact {
            repo_path: "onnx/model_quantized.onnx",
            filename: "model_quantized.onnx",
            sha256: "6c9c6101a956d62dfb5e7190c538226c0c5bb9cb27b651234b6df063ee7dbfe4",
            bytes: 34_014_426,
        },
        tokenizer: BGE_TOKENIZER,
    },
    ModelSpec {
        id: "bge-small-en-v1.5",
        display_name: "BAAI bge-small-en-v1.5 (float32)",
        dimensions: 384,
        max_sequence_len: 512,
        pooling: Pooling::Cls,
        model: Artifact {
            repo_path: "onnx/model.onnx",
            filename: "model.onnx",
            sha256: "828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35",
            bytes: 133_093_490,
        },
        tokenizer: BGE_TOKENIZER,
    },
];

/// The model used when config does not name one.
pub const DEFAULT_MODEL_ID: &str = "bge-small-en-v1.5-int8";

/// Look up a pinned model by id.
///
/// The error names every id that *is* pinned, so a reader who typed
/// `bge-small` learns the exact spelling without reading the source.
pub fn spec_for(id: &str) -> Result<&'static ModelSpec, ForgeError> {
    MODELS.iter().find(|spec| spec.id == id).ok_or_else(|| {
        let available: Vec<&str> = MODELS.iter().map(|spec| spec.id).collect();
        ForgeError::config(format!(
            "no pinned local model {id:?}; available: {}",
            available.join(", ")
        ))
    })
}

/// Look up a pinned model by id, falling back to [`DEFAULT_MODEL_ID`] when
/// `id` is empty.
///
/// `embedding.model` is empty by default — the same shape `needle.variant`
/// has — so "unset" means "the default local model" rather than an error.
pub fn spec_for_or_default(id: &str) -> Result<&'static ModelSpec, ForgeError> {
    if id.trim().is_empty() {
        spec_for(DEFAULT_MODEL_ID)
    } else {
        spec_for(id)
    }
}
