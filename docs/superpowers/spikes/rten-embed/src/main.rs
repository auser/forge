//! Scratch spike: can rten run a real encoder model in-process, and how fast?
//! Not part of forge. Throwaway.

use std::error::Error;
use std::time::Instant;

use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::NdTensor;
use rten_text::tokenizer::{EncodeOptions, Tokenizer};

fn embed_batch(
    sentences: &[&str],
    tokenizer: &Tokenizer,
    model: &Model,
    max_seq_len: usize,
) -> Result<Vec<Vec<f32>>, Box<dyn Error>> {
    let mut encoded = Vec::new();
    for &s in sentences {
        encoded.push(tokenizer.encode(
            s,
            Some(EncodeOptions {
                max_chunk_len: Some(max_seq_len),
                ..Default::default()
            }),
        )?);
    }

    let max_sequence_len = encoded
        .iter()
        .map(|e| e.token_ids().len())
        .max()
        .unwrap_or(0)
        .max(1);
    let batch = sentences.len();

    let mut input_ids = NdTensor::<i32, 2>::zeros([batch, max_sequence_len]);
    let mut attention_mask = NdTensor::<i32, 2>::zeros([batch, max_sequence_len]);
    for (i, enc) in encoded.iter().enumerate() {
        let ids = enc.token_ids();
        for (dst, &tid) in input_ids
            .slice_mut((i, ..ids.len()))
            .iter_mut()
            .zip(ids.iter())
        {
            *dst = tid as i32;
        }
        attention_mask.slice_mut((i, ..ids.len())).fill(1i32);
    }

    let mut inputs: Vec<(rten::NodeId, rten::ValueOrView)> = vec![
        (model.node_id("input_ids")?, input_ids.view().into()),
        (model.node_id("attention_mask")?, attention_mask.view().into()),
    ];
    let type_ids: NdTensor<i32, 2>;
    if let Some(id) = model.find_node("token_type_ids") {
        type_ids = NdTensor::<i32, 2>::zeros([batch, max_sequence_len]);
        inputs.push((id, type_ids.view().into()));
    }

    // bge exports `last_hidden_state`; fall back to `pooler_output`.
    let out_id = match model.find_node("last_hidden_state") {
        Some(id) => id,
        None => model.node_id("pooler_output")?,
    };
    let raw = model.run_n(inputs, [out_id], None)?[0].clone();

    // (batch, seq, dim) -> CLS pooling + L2 normalise, or (batch, dim) as-is.
    let pooled: Vec<Vec<f32>> = match raw {
        rten::Value::FloatTensor(t) if t.shape().len() == 3 => {
            let hs: NdTensor<f32, 3> = t.try_into()?;
            (0..batch)
                .map(|i| {
                    let cls: Vec<f32> = hs.slice((i, 0..1, ..)).iter().copied().collect();
                    let norm = cls.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                    cls.iter().map(|x| x / norm).collect()
                })
                .collect()
        }
        rten::Value::FloatTensor(t) if t.shape().len() == 2 => {
            let hs: NdTensor<f32, 2> = t.try_into()?;
            (0..batch)
                .map(|i| {
                    let v: Vec<f32> = hs.slice((i, ..)).iter().copied().collect();
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                    v.iter().map(|x| x / norm).collect()
                })
                .collect()
        }
        other => return Err(format!("unexpected output rank: {:?}", other.shape()).into()),
    };
    Ok(pooled)
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn main() -> Result<(), Box<dyn Error>> {
    let model_path = std::env::args().nth(1).expect("usage: rten-spike <model> <tokenizer>");
    let tok_path = std::env::args().nth(2).expect("usage: rten-spike <model> <tokenizer>");

    let t0 = Instant::now();
    let model = Model::load_file(&model_path)?;
    let load_ms = t0.elapsed().as_millis();
    println!("[load]  model loaded in {load_ms} ms");
    println!("[graph] {} inputs, {} outputs", model.input_ids().len(), model.output_ids().len());
    for name in ["input_ids", "attention_mask", "token_type_ids"] {
        println!("[graph]   has {name}: {}", model.find_node(name).is_some());
    }
    for name in ["last_hidden_state", "pooler_output"] {
        println!("[graph]   has {name}: {}", model.find_node(name).is_some());
    }

    let t0 = Instant::now();
    let tokenizer = Tokenizer::from_file(&tok_path)?;
    println!("[load]  tokenizer loaded in {} ms", t0.elapsed().as_millis());

    let samples = [
        "pub fn parse_config(path: &Path) -> Result<Config, ForgeError>",
        "struct ForgeError { message: String }",
        "The quick brown fox jumps over the lazy dog",
        "fn main() { println!(\"hello\"); }",
    ];

    // Warm-up (first call includes lazy init / page faults).
    let t0 = Instant::now();
    embed_batch(&samples, &tokenizer, &model, 512)?;
    println!("[warm]  4 texts in {} ms", t0.elapsed().as_millis());

    let t0 = Instant::now();
    let one = embed_batch(&samples[..1], &tokenizer, &model, 512)?;
    let single_ms = t0.elapsed().as_millis();
    println!(
        "[batch1] 1 text in {single_ms} ms -> dims {}",
        one[0].len()
    );

    let many: Vec<&str> = (0..32).map(|i| samples[i % samples.len()]).collect();
    let t0 = Instant::now();
    let big = embed_batch(&many, &tokenizer, &model, 512)?;
    let many_ms = t0.elapsed().as_millis();
    println!(
        "[batch32] 32 texts in {many_ms} ms ({:.1} ms/text) -> dims {}",
        many_ms as f64 / 32.0,
        big[0].len()
    );

    println!("[check] cos(code, same code) = {:.4} (expect ~1.0)", cos(&one[0], &big[0]));
    println!("[check] cos(code, prose)     = {:.4} (expect << 1.0)", cos(&one[0], &big[2]));

    Ok(())
}
