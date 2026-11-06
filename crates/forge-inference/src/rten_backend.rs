//! The rten backend: a loaded encoder, ready to embed.
//!
//! This is where the runtime actually happens. Everything above it —
//! [`InferenceSession`](crate::session::InferenceSession), the `Embedder`
//! adapter, the semantic index — is runtime-agnostic.
//!
//! **No unsafe, no native code.** `rten` is a pure-Rust ONNX runtime, so
//! this file compiles with nothing but a Rust toolchain: no CMake, no NDK,
//! no C++ runtime probing, no per-platform static blob. The cost that used
//! to be paid at install time simply does not exist here.
//!
//! ## Why a worker thread
//!
//! `rten-text`'s `Tokenizer` is neither `Send` nor `Sync`: it stores its
//! model and normalizer as `Box<dyn Model>` / `Box<dyn Normalizer>`, and
//! those traits carry no thread bounds. So a tokenizer cannot be shared,
//! and cannot even be moved onto another thread once it exists — it has to
//! be *created* on the thread that will use it.
//!
//! The encoder therefore owns a dedicated worker thread that holds both the
//! tokenizer and the model, and speaks to it over a channel. This is the
//! same shape `forge_needle::NeedleEngine` uses for the FFI brain, for the
//! same reason. It has a pleasant side effect: the inference itself runs
//! off the async runtime's worker threads rather than parking one of them
//! for the duration of the matmuls.

use std::path::Path;
use std::thread;

use forge_core::ForgeError;
use rten::Model;
use rten_tensor::NdTensor;
use rten_tensor::prelude::*;
use rten_text::tokenizer::{EncodeOptions, Tokenizer};

use crate::model::{ModelSpec, Pooling};
use crate::session::InferenceSession;

/// The output node an encoder graph is expected to expose.
const LAST_HIDDEN_STATE: &str = "last_hidden_state";

/// A fallback for exports that ship an already-pooled output instead of
/// per-token states.
const POOLER_OUTPUT: &str = "pooler_output";

/// A unit of work for the encoder thread.
enum Job {
    /// Embed a batch of texts.
    Embed {
        texts: Vec<String>,
        reply: tokio::sync::oneshot::Sender<Result<Vec<Vec<f32>>, String>>,
    },
}

/// The model and tokenizer, held by the worker thread.
///
/// Deliberately neither `Send` nor `Sync` — it cannot be, because the
/// tokenizer is not. It never leaves the thread that created it.
struct Loaded {
    model: Model,
    tokenizer: Tokenizer,
    spec: &'static ModelSpec,
}

/// A loaded encoder, ready to embed.
pub struct RtenEncoder {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
    dimensions: usize,
    model_id: String,
}

impl RtenEncoder {
    /// Load a model graph and its tokenizer, and start the thread that
    /// will serve inference requests.
    ///
    /// The caller is responsible for having verified the bytes — see
    /// [`crate::weights`]. This constructor does not re-hash anything; it
    /// would be hashing the same file twice for every session.
    pub async fn load(
        spec: &'static ModelSpec,
        model_path: &Path,
        tokenizer_path: &Path,
    ) -> Result<Self, ForgeError> {
        let (jobs, mut inbox) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (load_tx, load_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        let spec_for_thread = spec;
        let model_path = model_path.to_path_buf();
        let tokenizer_path = tokenizer_path.to_path_buf();
        // Detached on purpose: the thread stops when the channel closes,
        // which happens when the encoder is dropped.
        let _worker = thread::Builder::new()
            .name("forge-inference".to_string())
            .spawn(move || {
                let loaded = match Loaded::load(spec_for_thread, &model_path, &tokenizer_path) {
                    Ok(loaded) => loaded,
                    Err(e) => {
                        let _ = load_tx.send(Err(e));
                        return;
                    }
                };
                let _ = load_tx.send(Ok(()));

                // `blocking_recv` is correct here and nowhere else: this is
                // a dedicated thread, not an async task.
                while let Some(job) = inbox.blocking_recv() {
                    match job {
                        Job::Embed { texts, reply } => {
                            let result = loaded.embed_batch(&texts).map_err(|e| e.to_string());
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|e| {
                ForgeError::provider(format!("starting the local inference thread: {e}"))
            })?;

        match load_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(ForgeError::provider(format!("loading local model: {e}")));
            }
            Err(_) => {
                return Err(ForgeError::provider(
                    "the local inference thread stopped before the model loaded",
                ));
            }
        }

        Ok(Self {
            jobs,
            dimensions: spec.dimensions,
            model_id: crate::model_id(spec),
        })
    }
}

/// The worker thread's copy of the model.
impl Loaded {
    fn load(
        spec: &'static ModelSpec,
        model_path: &Path,
        tokenizer_path: &Path,
    ) -> Result<Self, String> {
        let model =
            Model::load_file(model_path).map_err(|e| format!("{}: {e}", model_path.display()))?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| format!("{}: {e}", tokenizer_path.display()))?;

        // Fail now, at load time, rather than on the first embedding: a
        // graph without the expected output is a packaging problem, and
        // the user should hear about it when the model is set up, not when
        // they wait for a semantic search.
        if model.find_node(LAST_HIDDEN_STATE).is_none() && model.find_node(POOLER_OUTPUT).is_none()
        {
            return Err(format!(
                "{} exposes neither {LAST_HIDDEN_STATE:?} nor {POOLER_OUTPUT:?}; it is not a usable encoder",
                model_path.display()
            ));
        }
        for input in ["input_ids", "attention_mask"] {
            if model.find_node(input).is_none() {
                return Err(format!(
                    "{} has no {input:?} input; it is not a usable encoder",
                    model_path.display()
                ));
            }
        }
        tracing::debug!(model = %model_path.display(), "loaded local encoder");

        Ok(Self {
            model,
            tokenizer,
            spec,
        })
    }

    /// Tokenize a batch, run the graph, pool, and normalise.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let encoded = self.tokenize(texts)?;
        let max_sequence_len = encoded
            .iter()
            .map(|enc| enc.token_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);
        let batch = texts.len();

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

        // `token_type_ids` is optional: only BERT-family exports that
        // declare it need it, and every value is zero for a single
        // sequence.
        let type_ids: NdTensor<i32, 2>;
        let mut inputs: Vec<(rten::NodeId, rten::ValueOrView)> = vec![
            (
                self.model.node_id("input_ids").map_err(rten_error)?,
                input_ids.view().into(),
            ),
            (
                self.model.node_id("attention_mask").map_err(rten_error)?,
                attention_mask.view().into(),
            ),
        ];
        if let Some(id) = self.model.find_node("token_type_ids") {
            type_ids = NdTensor::<i32, 2>::zeros([batch, max_sequence_len]);
            inputs.push((id, type_ids.view().into()));
        }

        let output_id = if self.model.find_node(LAST_HIDDEN_STATE).is_some() {
            self.model.node_id(LAST_HIDDEN_STATE).map_err(rten_error)?
        } else {
            self.model.node_id(POOLER_OUTPUT).map_err(rten_error)?
        };
        let raw = self
            .model
            .run_n(inputs, [output_id], None)
            .map_err(|e| ForgeError::provider(format!("running local encoder: {e}")))?[0]
            .clone();

        let pooled = match raw {
            rten::Value::FloatTensor(t) if t.shape().len() == 3 => {
                let hidden: NdTensor<f32, 3> = t
                    .try_into()
                    .map_err(|e| ForgeError::provider(format!("reading encoder output: {e:?}")))?;
                (0..batch)
                    .map(|i| self.pool_one(&hidden, i, encoded[i].token_ids().len()))
                    .collect::<Result<Vec<_>, _>>()?
            }
            rten::Value::FloatTensor(t) if t.shape().len() == 2 => {
                let hidden: NdTensor<f32, 2> = t
                    .try_into()
                    .map_err(|e| ForgeError::provider(format!("reading encoder output: {e:?}")))?;
                (0..batch)
                    .map(|i| {
                        let v: Vec<f32> = hidden.slice((i, ..)).iter().copied().collect();
                        normalize(v)
                    })
                    .collect::<Result<Vec<_>, ForgeError>>()?
            }
            other => {
                return Err(ForgeError::provider(format!(
                    "local encoder produced an output of rank {}, expected 2 or 3",
                    other.shape().len()
                )));
            }
        };

        if pooled.len() != batch {
            return Err(ForgeError::provider(format!(
                "local encoder produced {} vectors for {batch} inputs",
                pooled.len()
            )));
        }
        for vector in &pooled {
            if vector.len() != self.spec.dimensions {
                return Err(ForgeError::provider(format!(
                    "local encoder produced {}-value vectors, expected {}",
                    vector.len(),
                    self.spec.dimensions
                )));
            }
        }
        Ok(pooled)
    }

    /// Pool one sequence's token vectors into a single vector.
    fn pool_one(
        &self,
        hidden: &NdTensor<f32, 3>,
        index: usize,
        seq_len: usize,
    ) -> Result<Vec<f32>, ForgeError> {
        let raw = match self.spec.pooling {
            Pooling::Cls => hidden.slice((index, 0..1, ..)).iter().copied().collect(),
            Pooling::Mean => {
                let len = seq_len.min(hidden.shape()[1]);
                if len == 0 {
                    return Err(ForgeError::provider(
                        "local encoder produced an empty sequence for one input",
                    ));
                }
                let dims = hidden.shape()[2];
                let values: Vec<f32> = hidden.slice((index, 0..len, ..)).iter().copied().collect();
                if values.len() != len * dims {
                    return Err(ForgeError::provider(
                        "local encoder produced an unexpected number of token vectors",
                    ));
                }
                let mut sum = vec![0.0f32; dims];
                for (i, value) in values.iter().enumerate() {
                    sum[i % dims] += value;
                }
                sum.iter().map(|v| v / len as f32).collect()
            }
        };
        normalize(raw)
    }

    fn tokenize<'a>(
        &self,
        texts: &'a [String],
    ) -> Result<Vec<rten_text::tokenizer::Encoded<'a>>, ForgeError> {
        texts
            .iter()
            .map(|text| {
                self.tokenizer
                    .encode(
                        text,
                        Some(EncodeOptions {
                            max_chunk_len: Some(self.spec.max_sequence_len),
                            ..Default::default()
                        }),
                    )
                    .map_err(|e| ForgeError::provider(format!("tokenizing input: {e}")))
            })
            .collect()
    }
}

fn rten_error(e: rten::RunError) -> ForgeError {
    ForgeError::provider(format!("local encoder: {e}"))
}

/// L2-normalise a vector so a dot product is a cosine similarity.
///
/// A zero vector has no direction, so it cannot be normalised; returning an
/// error beats returning a vector that silently compares as dissimilar to
/// everything.
fn normalize(mut vector: Vec<f32>) -> Result<Vec<f32>, ForgeError> {
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(ForgeError::provider(
            "local encoder produced a vector with non-finite values",
        ));
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return Err(ForgeError::provider(
            "local encoder produced a zero or non-finite vector",
        ));
    }
    vector.iter_mut().for_each(|value| *value /= norm);
    Ok(vector)
}

#[async_trait::async_trait]
impl InferenceSession for RtenEncoder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Job::Embed {
                texts: texts.to_vec(),
                reply,
            })
            .map_err(|_| {
                ForgeError::provider("the local inference thread is no longer running".to_string())
            })?;
        match answer.await {
            Ok(Ok(vectors)) => Ok(vectors),
            Ok(Err(e)) => Err(ForgeError::provider(format!("local encoder: {e}"))),
            Err(_) => Err(ForgeError::provider(
                "the local inference thread stopped before answering".to_string(),
            )),
        }
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn model_id(&self) -> String {
        self.model_id.clone()
    }
}
