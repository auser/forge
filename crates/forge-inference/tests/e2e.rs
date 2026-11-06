//! End-to-end tests for the real rten backend against a real encoder.
//!
//! Opt in explicitly — these need the model on disk (34 MB for the int8
//! default), so they are not part of `just verify`:
//!
//! ```sh
//! FORGE_RTEN_E2E_MODEL_DIR=~/.cache/forge/models/rten/bge-small-en-v1.5-int8 \
//!   cargo test -p forge-inference --features rten-e2e --test e2e -- --nocapture
//! ```
//!
//! The directory must contain the files the pinned catalog names:
//! `model_quantized.onnx` and `tokenizer.json`.
//!
//! # Why these are worth having
//!
//! Everything else in this crate is checked against fakes: a wiremock
//! server for the download, a checksum for the bytes. None of that proves
//! the runtime can actually *run* a model — that the graph's operators are
//! covered, that the dtype matches, that the pooling produces something
//! meaningful. Only a real model does.

#![cfg(feature = "rten-e2e")]

use std::path::PathBuf;

use forge_config::EmbeddingConfig;
use forge_core::embed::Embedder;
use forge_inference::RtenEmbedder;

/// Directory holding the real model's files.
const MODEL_DIR_ENV: &str = "FORGE_RTEN_E2E_MODEL_DIR";

fn model_dir() -> PathBuf {
    std::env::var(MODEL_DIR_ENV)
        .unwrap_or_else(|_| {
            eprintln!(
                "skipping: set {MODEL_DIR_ENV} to a directory holding the model's files \
             (model_quantized.onnx + tokenizer.json)"
            );
            std::process::exit(0);
        })
        .into()
}

async fn embedder() -> RtenEmbedder {
    let dir = model_dir();
    let config = EmbeddingConfig {
        provider: "rten".to_string(),
        model: String::new(),
        weights_dir: dir.display().to_string(),
        autofetch: false,
        ..EmbeddingConfig::default()
    };
    forge_inference::embedder_from_config(&config, false)
        .await
        .expect("embedder")
        .expect("embedder present")
}

/// Cosine similarity. The vectors are already L2-normalised, so this is
/// just a dot product — computed here explicitly so the test does not
/// depend on that invariant being true.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

#[tokio::test]
async fn embeds_real_text_into_meaningful_vectors() {
    let embedder = embedder().await;

    let code = "pub fn parse_config(path: &Path) -> Result<Config, ForgeError>".to_string();
    let same_code = "pub fn parse_config(path: &Path) -> Result<Config, ForgeError>".to_string();
    let other_code = "struct ForgeError { message: String }".to_string();
    let prose = "The quick brown fox jumps over the lazy dog".to_string();

    let vectors = embedder
        .embed(&[code.clone(), same_code, other_code, prose])
        .await
        .expect("embed");

    assert_eq!(vectors.len(), 4, "one vector per input");
    for vector in &vectors {
        assert_eq!(
            vector.len(),
            embedder.dimensions(),
            "every vector has the model's width"
        );
        assert!(
            vector.iter().all(|v| v.is_finite()),
            "every component is finite: {vector:?}"
        );
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "vectors are L2-normalised, got norm {norm}"
        );
    }

    // Identical text must produce identical vectors.
    let identical = cosine(&vectors[0], &vectors[1]);
    assert!(
        identical > 0.999,
        "identical text gives identical vectors, got {identical}"
    );

    // Different text must not produce identical vectors.
    let different = cosine(&vectors[0], &vectors[2]);
    assert!(
        different < 0.999,
        "different code gives different vectors, got {different}"
    );

    // And the separation has to be *meaningful*, not just non-zero: a
    // code snippet and a sentence about a fox should not look alike.
    let unrelated = cosine(&vectors[0], &vectors[3]);
    assert!(
        unrelated < 0.8,
        "code and prose should not look alike, got {unrelated}"
    );
}

#[tokio::test]
async fn empty_input_is_an_empty_result() {
    let embedder = embedder().await;
    let vectors = embedder.embed(&[]).await.expect("embed");
    assert!(vectors.is_empty(), "no inputs means no vectors");
}
