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
