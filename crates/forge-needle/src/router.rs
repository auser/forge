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
        Self {
            engine,
            registry,
            timeout,
        }
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
                        task.required_capabilities
                            .iter()
                            .all(|c| c.satisfied_by(caps))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_backend::HashBackend;
    use forge_core::model::ModelCapabilities;
    use forge_core::router::Capability;
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
    async fn slow_decision_times_out_and_says_so() {
        // The `timeout` arm of `route` had no coverage: a backend slower
        // than `router_timeout_ms` must produce an Err naming the timeout
        // (which `FallbackRouter` then degrades to static rules) rather
        // than blocking the request for as long as inference takes.
        use crate::engine::tests::SlowBackend;
        use std::sync::atomic::AtomicUsize;

        let slow = NeedleRouter::new(
            Arc::new(NeedleEngine::spawn(SlowBackend {
                decide_calls: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(500),
            })),
            vec![("qwen3-coder".to_string(), caps(true))],
            Duration::from_millis(10),
        );

        let started = std::time::Instant::now();
        let err = slow
            .route(&RoutingRequest::new("anything"))
            .await
            .expect_err("a decision slower than the timeout must error");
        assert!(
            err.to_string().contains("timed out"),
            "error should name the timeout: {err}"
        );
        // It must return at the timeout, not after the full inference.
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "route returned after {:?}, so it waited for the backend instead of timing out",
            started.elapsed()
        );
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
