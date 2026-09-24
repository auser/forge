use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge_config::Config;
use forge_core::{
    Capability, DecisionRouter, ForgeError, ModelCapabilities, ModelProvider, RoutingDecision,
    RoutingRequest,
};
use serde::{Deserialize, Serialize};

/// Drop candidates that lack any required capability.
pub fn filter_candidates(
    candidates: &[(String, ModelCapabilities)],
    required: &[Capability],
) -> Vec<String> {
    candidates
        .iter()
        .filter(|(_, caps)| required.iter().all(|need| need.satisfied_by(caps)))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Deterministic rule-based router: optional keyword → model rules over a
/// default model, always capability-filtered. Confidence 1.0.
pub struct StaticRouter {
    default_model: String,
    rules: Vec<(String, String)>,
    registry: Vec<(String, ModelCapabilities)>,
}

impl StaticRouter {
    pub fn new(default_model: impl Into<String>) -> Self {
        Self {
            default_model: default_model.into(),
            rules: Vec::new(),
            registry: Vec::new(),
        }
    }

    /// Keyword → model rules; first case-insensitive substring match wins.
    pub fn with_rules(mut self, rules: Vec<(String, String)>) -> Self {
        self.rules = rules;
        self
    }

    /// Known models and their capabilities, used for capability filtering.
    pub fn with_registry(mut self, registry: Vec<(String, ModelCapabilities)>) -> Self {
        self.registry = registry;
        self
    }

    fn capable_candidates(&self, request: &RoutingRequest) -> Vec<String> {
        // Pool: requested candidates, or (when the request names none) the
        // default model, rule targets, and the registry.
        let mut names: Vec<String> = if request.candidates.is_empty() {
            let mut names = vec![self.default_model.clone()];
            names.extend(self.rules.iter().map(|(_, m)| m.clone()));
            names.extend(self.registry.iter().map(|(n, _)| n.clone()));
            names
        } else {
            request.candidates.clone()
        };
        names.dedup();

        let pool: Vec<(String, ModelCapabilities)> = names
            .into_iter()
            .map(|name| (self.capabilities_of(&name), name))
            .map(|(caps, name)| (name, caps))
            .collect();
        filter_candidates(&pool, &request.required_capabilities)
    }

    /// Capabilities for a model: the registry entry when known, the
    /// built-in mock's capabilities for the mock names, and optimistic
    /// defaults otherwise — the static router is the deterministic
    /// fallback and must be able to select unregistered models, while
    /// never selecting a model *known* to lack a required capability.
    fn capabilities_of(&self, name: &str) -> ModelCapabilities {
        if let Some((_, caps)) = self.registry.iter().find(|(n, _)| n == name) {
            return *caps;
        }
        if name == "mock" || name == "mock-local" {
            return crate::model::MockModel::new().capabilities();
        }
        ModelCapabilities {
            streaming: true,
            tools: true,
            structured_output: true,
            vision: true,
            max_context: usize::MAX,
        }
    }
}

#[async_trait]
impl DecisionRouter for StaticRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        let capable = self.capable_candidates(task);
        if capable.is_empty() {
            return Err(ForgeError::router(format!(
                "no candidate satisfies the required capabilities ({:?})",
                task.required_capabilities
            )));
        }

        let task_lower = task.task.to_lowercase();
        let ruled = self
            .rules
            .iter()
            .find(|(keyword, _)| task_lower.contains(&keyword.to_lowercase()))
            .map(|(_, model)| model.clone());

        let (selected, fallback_used, reason) = match ruled {
            Some(model) if capable.contains(&model) => (
                model.clone(),
                false,
                format!("keyword rule selected {model}"),
            ),
            Some(model) => (
                capable[0].clone(),
                true,
                format!(
                    "keyword rule selected {model}, but it lacks required capabilities; using {}",
                    capable[0]
                ),
            ),
            None if capable.contains(&self.default_model) => (
                self.default_model.clone(),
                false,
                format!("default model {}", self.default_model),
            ),
            None => (
                capable[0].clone(),
                true,
                format!(
                    "default model {} lacks required capabilities; using {}",
                    self.default_model, capable[0]
                ),
            ),
        };

        Ok(RoutingDecision {
            selected_model: selected,
            confidence: 1.0,
            router_name: "static".to_string(),
            fallback_used,
            reason,
        })
    }
}

/// Returns a preset decision and records requests. For tests/BDD.
pub struct MockRouter {
    decision: RoutingDecision,
    requests: Mutex<Vec<RoutingRequest>>,
}

impl MockRouter {
    pub fn new(decision: RoutingDecision) -> Self {
        Self {
            decision,
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn selecting(model: impl Into<String>) -> Self {
        Self::new(RoutingDecision {
            selected_model: model.into(),
            confidence: 1.0,
            router_name: "mock".to_string(),
            fallback_used: false,
            reason: "preset mock decision".to_string(),
        })
    }

    pub fn recorded(&self) -> Vec<RoutingRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl DecisionRouter for MockRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task.clone());
        Ok(self.decision.clone())
    }
}

/// System One-compatible HTTP router (TypeSafe Jev, Kev, local variants).
/// POSTs `{task, candidates, required_capabilities}` to `router_url`
/// (treated as the full endpoint URL) and expects
/// `{selected_model, confidence, reason}` back. No URLs or key names are
/// hard-coded; the bearer token comes from the env var named by
/// `router_key_env` and is never logged.
pub struct HttpRouter {
    client: reqwest::Client,
    url: String,
    key_env: Option<String>,
}

#[derive(Serialize)]
struct RouteRequest<'a> {
    task: &'a str,
    candidates: &'a [String],
    required_capabilities: &'a [Capability],
}

#[derive(Deserialize)]
struct RouteResponse {
    selected_model: String,
    confidence: f64,
    #[serde(default)]
    reason: String,
}

impl HttpRouter {
    pub fn new(
        url: impl Into<String>,
        key_env: Option<String>,
        timeout: Duration,
    ) -> Result<Self, ForgeError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ForgeError::router(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            url: url.into(),
            key_env,
        })
    }
}

#[async_trait]
impl DecisionRouter for HttpRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        let body = RouteRequest {
            task: &task.task,
            candidates: &task.candidates,
            required_capabilities: &task.required_capabilities,
        };

        let mut http = self.client.post(&self.url).json(&body);
        if let Some(env_name) = &self.key_env
            && let Ok(key) = std::env::var(env_name)
            && !key.is_empty()
        {
            http = http.bearer_auth(key);
        }

        let response = http.send().await.map_err(|e| {
            if e.is_timeout() {
                ForgeError::router(format!("router request to {} timed out", self.url))
            } else {
                ForgeError::router(format!("router request to {} failed: {e}", self.url))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            return Err(ForgeError::router(format!(
                "router endpoint {} returned {status}",
                self.url
            )));
        }

        let parsed: RouteResponse = response.json().await.map_err(|e| {
            ForgeError::router(format!("invalid router response from {}: {e}", self.url))
        })?;

        Ok(RoutingDecision {
            selected_model: parsed.selected_model,
            confidence: parsed.confidence,
            router_name: "http".to_string(),
            fallback_used: false,
            reason: parsed.reason,
        })
    }
}

/// Routes through `primary`; on primary failure, routes through `fallback`
/// and marks the decision `fallback_used: true`.
pub struct FallbackRouter {
    primary: Arc<dyn DecisionRouter>,
    fallback: Arc<dyn DecisionRouter>,
}

impl FallbackRouter {
    pub fn new(primary: Arc<dyn DecisionRouter>, fallback: Arc<dyn DecisionRouter>) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl DecisionRouter for FallbackRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        match self.primary.route(task).await {
            Ok(decision) => Ok(decision),
            Err(primary_err) => {
                tracing::warn!(error = %primary_err, "primary router failed; using fallback");
                let mut decision = self.fallback.route(task).await.map_err(|fallback_err| {
                    ForgeError::router(format!(
                        "primary router failed ({primary_err}); fallback also failed ({fallback_err})"
                    ))
                })?;
                decision.fallback_used = true;
                decision.reason =
                    format!("primary router failed ({primary_err}); {}", decision.reason);
                Ok(decision)
            }
        }
    }
}

/// Cost-aware router: among capability-satisfying candidates, pick the
/// lowest `cost_input_per_mtok` (tie-break: output cost, then name
/// ascending). Deterministic (confidence 1.0). Candidates absent from the
/// cost table count as free (0.0).
pub struct CheapestRouter {
    costs: std::collections::HashMap<String, (f64, f64)>,
    registry: Vec<(String, ModelCapabilities)>,
}

impl CheapestRouter {
    pub fn new(
        costs: std::collections::HashMap<String, (f64, f64)>,
        registry: Vec<(String, ModelCapabilities)>,
    ) -> Self {
        Self { costs, registry }
    }
}

#[async_trait]
impl DecisionRouter for CheapestRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        // Pool: requested candidates, or every known model.
        let pool: Vec<(String, ModelCapabilities)> = if task.candidates.is_empty() {
            self.registry.clone()
        } else {
            task.candidates
                .iter()
                .map(|name| {
                    let caps = self
                        .registry
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, c)| *c)
                        .unwrap_or_else(optimistic_caps);
                    (name.clone(), caps)
                })
                .collect()
        };
        let capable = filter_candidates(&pool, &task.required_capabilities);
        if capable.is_empty() {
            return Err(ForgeError::router(format!(
                "no candidate satisfies the required capabilities ({:?})",
                task.required_capabilities
            )));
        }
        let cost_of = |name: &str| self.costs.get(name).copied().unwrap_or((0.0, 0.0));
        let mut ranked = capable;
        ranked.sort_by(|a, b| {
            cost_of(a)
                .0
                .total_cmp(&cost_of(b).0)
                .then_with(|| cost_of(a).1.total_cmp(&cost_of(b).1))
                .then_with(|| a.cmp(b))
        });
        let selected = ranked[0].clone();
        let n = ranked.len();
        let (input, _output) = cost_of(&selected);
        Ok(RoutingDecision {
            selected_model: selected.clone(),
            confidence: 1.0,
            router_name: "cheapest".to_string(),
            fallback_used: false,
            reason: format!("cheapest of {n} capable candidates (${input}/1M in)"),
        })
    }
}

fn optimistic_caps() -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        tools: true,
        structured_output: true,
        vision: true,
        max_context: usize::MAX,
    }
}

/// Laya router: System One-compatible HTTP router specialized for Laya's
/// typed-questions shape. POSTs
/// `{"state": {"task", "required_capabilities"}, "questions": {"model":
/// {"type": "choice", "instructions": ..., "criteria": {name: description}}}}`
/// and expects `{"answers": {"model": {"choice", "confidence"}}}`.
pub struct LayaRouter {
    client: reqwest::Client,
    url: String,
    key_env: Option<String>,
    /// Candidate name → routing criteria text (from `[models]` entries).
    criteria: std::collections::HashMap<String, String>,
}

impl LayaRouter {
    pub const DEFAULT_URL: &'static str = "http://127.0.0.1:8788/decide";

    pub fn new(
        url: Option<String>,
        key_env: Option<String>,
        timeout: Duration,
        criteria: std::collections::HashMap<String, String>,
    ) -> Result<Self, ForgeError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ForgeError::router(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            url: url.unwrap_or_else(|| Self::DEFAULT_URL.to_string()),
            key_env,
            criteria,
        })
    }
}

#[async_trait]
impl DecisionRouter for LayaRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        let candidates: Vec<&String> = if task.candidates.is_empty() {
            self.criteria.keys().collect()
        } else {
            task.candidates.iter().collect()
        };
        let criteria: serde_json::Map<String, serde_json::Value> = candidates
            .iter()
            .map(|name| {
                let desc = self
                    .criteria
                    .get(*name)
                    .cloned()
                    .unwrap_or_else(|| format!("model {name}"));
                (name.to_string(), serde_json::Value::String(desc))
            })
            .collect();
        let body = serde_json::json!({
            "state": {
                "task": task.task,
                "required_capabilities": task.required_capabilities,
            },
            "questions": {
                "model": {
                    "type": "choice",
                    "instructions": "Which model should handle this software task?",
                    "criteria": criteria,
                }
            }
        });

        let mut http = self.client.post(&self.url).json(&body);
        if let Some(env_name) = &self.key_env
            && let Ok(key) = std::env::var(env_name)
            && !key.is_empty()
        {
            http = http.bearer_auth(key);
        }
        let response = http.send().await.map_err(|e| {
            if e.is_timeout() {
                ForgeError::router(format!("laya router request to {} timed out", self.url))
            } else {
                ForgeError::router(format!("laya router request to {} failed: {e}", self.url))
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ForgeError::router(format!(
                "laya router endpoint {} returned {status}",
                self.url
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ForgeError::router(format!("invalid laya router response: {e}")))?;
        let choice = body
            .pointer("/answers/model/choice")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ForgeError::router("laya response missing answers.model.choice"))?;
        if !candidates.iter().any(|c| c.as_str() == choice) {
            return Err(ForgeError::router(format!(
                "laya answered unknown model {choice:?} (not among candidates)"
            )));
        }
        let confidence = body
            .pointer("/answers/model/confidence")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        Ok(RoutingDecision {
            selected_model: choice.to_string(),
            confidence,
            router_name: "laya".to_string(),
            fallback_used: false,
            reason: format!("laya choice (confidence {confidence:.2})"),
        })
    }
}

/// Confidence gate: decisions below the threshold become router failures
/// so the fallback chain takes over (the error message records the low
/// confidence for the fallback reason).
pub struct ThresholdRouter {
    inner: Arc<dyn DecisionRouter>,
    threshold: f64,
}

impl ThresholdRouter {
    pub fn new(inner: Arc<dyn DecisionRouter>, threshold: f64) -> Self {
        Self { inner, threshold }
    }
}

#[async_trait]
impl DecisionRouter for ThresholdRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        let decision = self.inner.route(task).await?;
        if decision.confidence < self.threshold {
            return Err(ForgeError::router(format!(
                "confidence {:.2} below threshold {:.2}",
                decision.confidence, self.threshold
            )));
        }
        Ok(decision)
    }
}

/// Build a router by name. `static`, `mock`, `cheapest` are local;
/// `http`/`laya` are HTTP; `needle` is the embedded on-device Needle 3
/// decision router (env `FORGE_NEEDLE_BACKEND=hash` selects the
/// deterministic test/BDD backend instead of the real weights-backed
/// engine). Laya defaults to `127.0.0.1:8788` when `router_url` is unset.
fn build_router(
    name: &str,
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    match name {
        "static" => Ok(Arc::new(
            StaticRouter::new(config.model.clone()).with_registry(registry.to_vec()),
        )),
        "mock" => Ok(Arc::new(MockRouter::selecting(config.model.clone()))),
        "cheapest" => {
            let costs = config
                .model_entries()
                .iter()
                .map(|(name, entry)| (name.clone(), entry.costs()))
                .collect();
            Ok(Arc::new(CheapestRouter::new(costs, registry.to_vec())))
        }
        "http" => {
            let url = config.router_url.as_deref().ok_or_else(|| {
                ForgeError::router("router = \"http\" requires router_url to be configured")
            })?;
            Ok(Arc::new(HttpRouter::new(
                url,
                config.router_key_env.clone(),
                Duration::from_millis(config.router_timeout_ms),
            )?))
        }
        "laya" => {
            let criteria = config
                .model_entries()
                .iter()
                .map(|(name, entry)| {
                    (
                        name.clone(),
                        entry
                            .description
                            .clone()
                            .unwrap_or_else(|| format!("model {name}")),
                    )
                })
                .collect();
            Ok(Arc::new(LayaRouter::new(
                config.router_url.clone(),
                config.router_key_env.clone(),
                Duration::from_millis(config.router_timeout_ms),
                criteria,
            )?))
        }
        "needle" => {
            let engine = forge_needle::select_engine(&config.needle)?;
            Ok(Arc::new(forge_needle::NeedleRouter::new(
                engine,
                registry.to_vec(),
                Duration::from_millis(config.router_timeout_ms),
            )))
        }
        other => Err(ForgeError::router(format!(
            "unknown router {other:?} (expected static, mock, cheapest, http, laya, or needle)"
        ))),
    }
}

/// Build the decision router from configuration: `static`, `mock`,
/// `cheapest`, `http`, `laya`, or `needle`. HTTP-class routers and `needle`
/// are gated by `router_confidence_threshold`; when the primary differs
/// from `router_fallback` it is wrapped in a `FallbackRouter` so failures
/// and low-confidence decisions degrade to the fallback instead of failing
/// the run.
pub fn router_from_config(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    let mut primary = build_router(&config.router, config, registry)?;
    if matches!(config.router.as_str(), "http" | "laya" | "needle") {
        primary = Arc::new(ThresholdRouter::new(
            primary,
            config.router_confidence_threshold,
        ));
    }
    if config.router == config.router_fallback {
        return Ok(primary);
    }
    let fallback = build_router(&config.router_fallback, config, registry)?;
    Ok(Arc::new(FallbackRouter::new(primary, fallback)))
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn caps(tools: bool) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools,
            structured_output: false,
            vision: false,
            max_context: 8_192,
        }
    }

    #[test]
    fn filter_candidates_drops_incapable_models() {
        let candidates = vec![
            ("strong".to_string(), caps(true)),
            ("weak".to_string(), caps(false)),
        ];
        let filtered = filter_candidates(&candidates, &[Capability::Tools]);
        assert_eq!(filtered, vec!["strong".to_string()]);
        assert_eq!(filter_candidates(&candidates, &[]).len(), 2);
    }

    #[tokio::test]
    async fn static_router_uses_default_model() {
        let router = StaticRouter::new("mock-local");
        let decision = router
            .route(&RoutingRequest::new("explain this code"))
            .await
            .expect("routes");
        assert_eq!(decision.selected_model, "mock-local");
        assert_eq!(decision.confidence, 1.0);
        assert_eq!(decision.router_name, "static");
        assert!(!decision.fallback_used);
    }

    #[tokio::test]
    async fn static_router_applies_keyword_rules() {
        let router = StaticRouter::new("default-model").with_rules(vec![
            ("refactor".to_string(), "big-model".to_string()),
            ("typo".to_string(), "small-model".to_string()),
        ]);
        let decision = router
            .route(&RoutingRequest::new("please Refactor this module"))
            .await
            .expect("routes");
        assert_eq!(decision.selected_model, "big-model");
        assert!(decision.reason.contains("keyword rule"));
    }

    #[tokio::test]
    async fn static_router_never_selects_incapable_model() {
        let router = StaticRouter::new("weak").with_registry(vec![
            ("weak".to_string(), caps(false)),
            ("strong".to_string(), caps(true)),
        ]);
        let request = RoutingRequest {
            task: "use tools".to_string(),
            required_capabilities: vec![Capability::Tools],
            candidates: vec!["weak".to_string(), "strong".to_string()],
        };
        let decision = router.route(&request).await.expect("routes");
        assert_eq!(decision.selected_model, "strong");
        assert!(decision.fallback_used);
    }

    #[tokio::test]
    async fn static_router_errors_when_nothing_is_capable() {
        let router =
            StaticRouter::new("weak").with_registry(vec![("weak".to_string(), caps(false))]);
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![Capability::Tools],
            candidates: vec!["weak".to_string()],
        };
        let err = router.route(&request).await.expect_err("must fail");
        assert!(matches!(err, ForgeError::Router(_)));
    }

    #[tokio::test]
    async fn mock_router_records_requests() {
        let router = MockRouter::selecting("preset-model");
        let decision = router
            .route(&RoutingRequest::new("task one"))
            .await
            .expect("routes");
        assert_eq!(decision.selected_model, "preset-model");
        assert_eq!(router.recorded().len(), 1);
        assert_eq!(router.recorded()[0].task, "task one");
    }

    #[tokio::test]
    async fn http_router_maps_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/route"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "selected_model": "chosen",
                "confidence": 0.9,
                "reason": "best fit"
            })))
            .mount(&server)
            .await;

        let router = HttpRouter::new(
            format!("{}/route", server.uri()),
            None,
            Duration::from_secs(5),
        )
        .expect("construct");
        let decision = router
            .route(&RoutingRequest::new("anything"))
            .await
            .expect("routes");
        assert_eq!(decision.selected_model, "chosen");
        assert!((decision.confidence - 0.9).abs() < f64::EPSILON);
        assert_eq!(decision.router_name, "http");
        assert_eq!(decision.reason, "best fit");
    }

    #[tokio::test]
    async fn http_router_timeout_is_typed_router_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .mount(&server)
            .await;

        let router = HttpRouter::new(
            format!("{}/route", server.uri()),
            None,
            Duration::from_millis(50),
        )
        .expect("construct");
        let err = router
            .route(&RoutingRequest::new("x"))
            .await
            .expect_err("must time out");
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("timed out"), "got: {msg}"),
            other => panic!("expected router error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_router_maps_http_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let router = HttpRouter::new(
            format!("{}/route", server.uri()),
            None,
            Duration::from_secs(5),
        )
        .expect("construct");
        let err = router
            .route(&RoutingRequest::new("x"))
            .await
            .expect_err("must fail");
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("503"), "got: {msg}"),
            other => panic!("expected router error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fallback_router_marks_fallback_used_when_primary_fails() {
        // Nothing listens on this port; the primary always fails fast.
        let primary: Arc<dyn DecisionRouter> = Arc::new(
            HttpRouter::new("http://127.0.0.1:9/route", None, Duration::from_millis(200))
                .expect("construct"),
        );
        let fallback: Arc<dyn DecisionRouter> = Arc::new(StaticRouter::new("mock-local"));
        let router = FallbackRouter::new(primary, fallback);

        let decision = router
            .route(&RoutingRequest::new("offline task"))
            .await
            .expect("fallback routes");
        assert_eq!(decision.selected_model, "mock-local");
        assert!(decision.fallback_used);
        assert!(decision.reason.contains("primary router failed"));
    }

    #[tokio::test]
    async fn fallback_router_passes_through_when_primary_succeeds() {
        let primary: Arc<dyn DecisionRouter> = Arc::new(MockRouter::selecting("primary-model"));
        let fallback: Arc<dyn DecisionRouter> = Arc::new(StaticRouter::new("mock-local"));
        let router = FallbackRouter::new(primary, fallback);

        let decision = router
            .route(&RoutingRequest::new("x"))
            .await
            .expect("routes");
        assert_eq!(decision.selected_model, "primary-model");
        assert!(!decision.fallback_used);
    }

    #[test]
    fn router_from_config_builds_needle_chain_by_default() {
        // Default router is needle (threshold-gated, static fallback).
        let config = Config::default();
        assert_eq!(config.router, "needle");
        assert_eq!(config.router_fallback, "static");
        let router = router_from_config(&config, &[]).expect("default chain builds");
        drop(router);
    }

    #[test]
    fn router_from_config_builds_static() {
        let config = Config {
            router: "static".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("static builds");
        drop(router);
    }

    #[test]
    fn router_from_config_builds_laya_explicitly() {
        // Laya is no longer the default but stays available and buildable.
        let config = Config {
            router: "laya".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("laya builds");
        drop(router);
    }

    #[test]
    fn router_from_config_http_requires_url() {
        let config = Config {
            router: "http".to_string(),
            ..Config::default()
        };
        match router_from_config(&config, &[]) {
            Err(e) => assert!(matches!(e, ForgeError::Router(_)), "got: {e:?}"),
            Ok(_) => panic!("must fail"),
        }
    }

    #[test]
    fn router_from_config_rejects_unknown_router() {
        let config = Config {
            router: "jev-magic".to_string(),
            ..Config::default()
        };
        assert!(router_from_config(&config, &[]).is_err());
    }

    // --- needle ---

    #[tokio::test]
    #[serial]
    async fn needle_router_from_config_falls_back_to_static_without_weights() {
        let config = Config {
            router: "needle".to_string(),
            router_fallback: "static".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[("qwen3-coder".to_string(), caps(true))])
            .expect("builds");
        let d = router
            .route(&RoutingRequest::new("explain this"))
            .await
            .expect("fallback routes");
        assert!(d.fallback_used);
        assert_eq!(d.router_name, "static");
    }

    #[tokio::test]
    #[serial]
    async fn needle_router_with_hash_backend_routes_directly() {
        // Env-driven backend selection; #[serial] guards env mutation
        // against the test above, which also builds a "needle" router and
        // would otherwise race on FORGE_NEEDLE_BACKEND.
        unsafe { std::env::set_var("FORGE_NEEDLE_BACKEND", "hash") };
        let config = Config {
            router: "needle".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[("qwen3-coder".to_string(), caps(true))])
            .expect("builds");
        let d = router
            .route(&RoutingRequest::new("qwen3 coder please"))
            .await
            .expect("routes");
        unsafe { std::env::remove_var("FORGE_NEEDLE_BACKEND") };
        assert_eq!(d.router_name, "needle");
        assert!(!d.fallback_used);
    }

    // --- cheapest ---

    fn cheapest(costs: &[(&str, f64, f64)]) -> CheapestRouter {
        CheapestRouter::new(
            costs
                .iter()
                .map(|(n, i, o)| (n.to_string(), (*i, *o)))
                .collect(),
            vec![],
        )
    }

    #[tokio::test]
    async fn cheapest_picks_lowest_input_cost() {
        let router = cheapest(&[("pricey-b", 5.0, 10.0), ("cheap-a", 0.1, 0.2)]);
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["pricey-b".to_string(), "cheap-a".to_string()],
        };
        let decision = router.route(&request).await.expect("routes");
        assert_eq!(decision.selected_model, "cheap-a");
        assert_eq!(decision.confidence, 1.0);
        assert_eq!(decision.router_name, "cheapest");
        assert!(
            decision
                .reason
                .contains("cheapest of 2 capable candidates ($0.1/1M in)"),
            "reason: {}",
            decision.reason
        );
    }

    #[tokio::test]
    async fn cheapest_tie_breaks_by_output_cost_then_name() {
        // Same input cost: lowest output cost wins ("a" beats "b").
        let router = cheapest(&[("b", 1.0, 3.0), ("a", 1.0, 2.0), ("c", 1.0, 2.0)]);
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["b".to_string(), "a".to_string(), "c".to_string()],
        };
        let d1 = router.route(&request).await.expect("routes");
        assert_eq!(d1.selected_model, "a");

        // Full tie (input + output): name ascending wins.
        let router2 = cheapest(&[("c", 1.0, 2.0), ("a", 1.0, 2.0)]);
        let request2 = RoutingRequest {
            candidates: vec!["c".to_string(), "a".to_string()],
            ..request
        };
        let d2 = router2.route(&request2).await.expect("routes");
        assert_eq!(d2.selected_model, "a");
    }

    #[tokio::test]
    async fn cheapest_filters_by_capability() {
        let router = CheapestRouter::new(
            [
                ("cheap".to_string(), (0.1, 0.1)),
                ("strong".to_string(), (9.0, 9.0)),
            ]
            .into_iter()
            .collect(),
            vec![
                ("cheap".to_string(), caps(false)),
                ("strong".to_string(), caps(true)),
            ],
        );
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![Capability::Tools],
            candidates: vec!["cheap".to_string(), "strong".to_string()],
        };
        let decision = router.route(&request).await.expect("routes");
        assert_eq!(decision.selected_model, "strong");
    }

    #[tokio::test]
    async fn cheapest_errors_when_nothing_capable() {
        let router = CheapestRouter::new(
            [("cheap".to_string(), (0.1, 0.1))].into_iter().collect(),
            vec![("cheap".to_string(), caps(false))],
        );
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![Capability::Tools],
            candidates: vec!["cheap".to_string()],
        };
        assert!(matches!(
            router.route(&request).await,
            Err(ForgeError::Router(_))
        ));
    }

    // --- threshold ---

    #[tokio::test]
    async fn threshold_router_escalates_low_confidence() {
        let inner: Arc<dyn DecisionRouter> = Arc::new(MockRouter::new(RoutingDecision {
            selected_model: "some-model".to_string(),
            confidence: 0.3,
            router_name: "mock".to_string(),
            fallback_used: false,
            reason: "meh".to_string(),
        }));
        let router = ThresholdRouter::new(inner, 0.7);
        let err = router
            .route(&RoutingRequest::new("x"))
            .await
            .expect_err("below threshold");
        match err {
            ForgeError::Router(msg) => {
                assert!(msg.contains("0.30"), "got: {msg}");
                assert!(msg.contains("threshold"), "got: {msg}");
            }
            other => panic!("expected router error, got {other:?}"),
        }

        let ok_inner: Arc<dyn DecisionRouter> = Arc::new(MockRouter::new(RoutingDecision {
            selected_model: "some-model".to_string(),
            confidence: 0.9,
            router_name: "mock".to_string(),
            fallback_used: false,
            reason: "good".to_string(),
        }));
        let router = ThresholdRouter::new(ok_inner, 0.7);
        assert!(router.route(&RoutingRequest::new("x")).await.is_ok());
    }

    // --- laya ---

    fn laya_body(choice: &str, confidence: f64) -> serde_json::Value {
        serde_json::json!({
            "answers": { "model": { "choice": choice, "confidence": confidence } },
            "routing": {}
        })
    }

    #[tokio::test]
    async fn laya_router_maps_typed_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/decide"))
            .respond_with(ResponseTemplate::new(200).set_body_json(laya_body("cheap-a", 0.95)))
            .mount(&server)
            .await;

        let criteria = [
            ("cheap-a".to_string(), "cheap general model".to_string()),
            ("pricey-b".to_string(), "expensive strong model".to_string()),
        ]
        .into_iter()
        .collect();
        let router = LayaRouter::new(
            Some(format!("{}/decide", server.uri())),
            None,
            Duration::from_secs(5),
            criteria,
        )
        .expect("construct");

        let request = RoutingRequest {
            task: "fix the bug".to_string(),
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string(), "pricey-b".to_string()],
        };
        let decision = router.route(&request).await.expect("routes");
        assert_eq!(decision.selected_model, "cheap-a");
        assert!((decision.confidence - 0.95).abs() < f64::EPSILON);
        assert_eq!(decision.router_name, "laya");

        // The request body used the typed-questions contract.
        let received = server.received_requests().await.expect("requests");
        let body: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("request json");
        assert_eq!(body["state"]["task"], "fix the bug");
        assert_eq!(body["questions"]["model"]["type"], "choice");
        assert_eq!(
            body["questions"]["model"]["criteria"]["cheap-a"],
            "cheap general model"
        );
    }

    #[tokio::test]
    async fn laya_router_rejects_unknown_choice() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(laya_body("ghost", 0.9)))
            .mount(&server)
            .await;

        let router = LayaRouter::new(
            Some(server.uri()),
            None,
            Duration::from_secs(5),
            std::collections::HashMap::new(),
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string()],
        };
        let err = router.route(&request).await.expect_err("unknown choice");
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("ghost"), "got: {msg}"),
            other => panic!("expected router error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn laya_unavailable_falls_back_via_config_chain() {
        let mut config = Config {
            router: "laya".to_string(),
            router_url: Some("http://127.0.0.1:9/decide".to_string()),
            router_timeout_ms: 200,
            router_fallback: "static".to_string(),
            model: "local-coder".to_string(),
            ..Config::default()
        };
        config.models.insert(
            "local-coder".to_string(),
            forge_config::ModelEntry {
                description: Some("local coder model".to_string()),
                ..Default::default()
            },
        );
        let router = router_from_config(&config, &[]).expect("builds");
        let decision = router
            .route(&RoutingRequest::new("offline task"))
            .await
            .expect("fallback routes");
        assert_eq!(decision.selected_model, "local-coder");
        assert!(decision.fallback_used);
    }

    #[tokio::test]
    async fn laya_low_confidence_escalates_to_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(laya_body("cheap-a", 0.3)))
            .mount(&server)
            .await;

        let config = Config {
            router: "laya".to_string(),
            router_url: Some(server.uri()),
            router_confidence_threshold: 0.7,
            router_fallback: "static".to_string(),
            model: "mock-local".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("builds");
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string(), "mock-local".to_string()],
        };
        let decision = router.route(&request).await.expect("fallback routes");
        assert_eq!(decision.selected_model, "mock-local");
        assert!(decision.fallback_used);
        assert!(
            decision.reason.contains("confidence"),
            "{}",
            decision.reason
        );
    }
}
