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
    confidence: f32,
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

/// Build the decision router from configuration: `static`, `mock`, or
/// `http` (requires `router_url`). Non-static routers are wrapped in a
/// `FallbackRouter` with a static fallback so a router outage degrades to
/// deterministic routing instead of failing the run.
pub fn router_from_config(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    let static_fallback = || {
        Arc::new(StaticRouter::new(config.model.clone()).with_registry(registry.to_vec()))
            as Arc<dyn DecisionRouter>
    };

    match config.router.as_str() {
        "static" => Ok(static_fallback()),
        "mock" => {
            let mock = Arc::new(MockRouter::selecting(config.model.clone()));
            Ok(Arc::new(FallbackRouter::new(mock, static_fallback())))
        }
        "http" => {
            let url = config.router_url.as_deref().ok_or_else(|| {
                ForgeError::router("router = \"http\" requires router_url to be configured")
            })?;
            let http = Arc::new(HttpRouter::new(
                url,
                config.router_key_env.clone(),
                Duration::from_millis(config.router_timeout_ms),
            )?);
            Ok(Arc::new(FallbackRouter::new(http, static_fallback())))
        }
        other => Err(ForgeError::router(format!(
            "unknown router {other:?} (expected static, mock, or http)"
        ))),
    }
}

#[cfg(test)]
mod tests {
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
        // No requirements: everything passes.
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
        assert!((decision.confidence - 0.9).abs() < f32::EPSILON);
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
    fn router_from_config_builds_static_by_default() {
        let config = Config::default();
        let router = router_from_config(&config, &[]).expect("static builds");
        drop(router);
    }

    #[test]
    fn router_from_config_http_requires_url() {
        let config = Config {
            router: "http".to_string(),
            ..Config::default()
        };
        let err = match router_from_config(&config, &[]) {
            Err(e) => e,
            Ok(_) => panic!("must fail"),
        };
        assert!(matches!(err, ForgeError::Router(_)));
    }

    #[test]
    fn router_from_config_rejects_unknown_router() {
        let config = Config {
            router: "jev-magic".to_string(),
            ..Config::default()
        };
        assert!(router_from_config(&config, &[]).is_err());
    }
}
