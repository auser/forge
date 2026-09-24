//! Jev router: System One protocol shared by TypeSafe's hosted Jev API and
//! the wire-compatible OpenJev family (razorback16/openjev, GitHub30/OpenJev
//! — both explicitly documented as wire-compatible with TypeSafe's API so
//! its SDKs work against them unchanged).
//!
//! ## Verified wire contract (2026-09-24)
//!
//! Sources, independently cross-checked: TypeSafe's own API reference
//! (<https://docs.typesafe.ai/api.md>), LiteLLM's TypeSafe pass-through docs
//! (<https://docs.litellm.ai/docs/pass_through/typesafe>), and both OpenJev
//! READMEs (<https://github.com/razorback16/openjev>,
//! <https://github.com/GitHub30/OpenJev>). All four agree:
//!
//! `POST https://api.typesafe.ai/v1/systemone` (or a self-hosted OpenJev
//! server's `/v1/systemone`), `Authorization: Bearer <TYPESAFE_API_KEY>`:
//!
//! ```json
//! {"state": "<task text>", "model": "jev-latest",
//!  "questions": {"model": {"type": "choice", "instructions": "...",
//!                          "criteria": {"<candidate>": "<description>"}}}}
//! ```
//! →
//! ```json
//! {"model": "jev-1.13.0",
//!  "answers": {"model": {"type": "choice", "choice": "<candidate>",
//!                         "probabilities": {"...": 0.0}, "confidence": 0.82}},
//!  "usage": {"input_tokens": 0, "output_tokens": 0}}
//! ```
//!
//! This **genuinely differs** from `LayaRouter`'s assumed shape in this
//! codebase: Laya nests `state` as a structured object (`{task,
//! required_capabilities}`) with no top-level `model` field and no `usage`;
//! the real Jev/OpenJev contract sends `state` as plain text and requires a
//! top-level `model` (the *inference* model id, not the routing choice —
//! `jev-latest` is documented by OpenJev as an alias both TypeSafe and
//! OpenJev accept, "so TypeSafe SDK defaults work"). Per the brief: since
//! the shapes differ, `JevRouter` stays self-contained rather than sharing
//! request/response types with `LayaRouter`.
//!
//! Endpoint note: the brief suggested checking `console.typesafe.ai`; the
//! verified hosted API host is actually `api.typesafe.ai`.

use std::time::Duration;

use async_trait::async_trait;
use forge_core::{DecisionRouter, ForgeError, ModelCapabilities, RoutingDecision, RoutingRequest};

use crate::router::{filter_candidates, optimistic_caps};

/// System One-compatible router speaking the verified Jev/OpenJev wire
/// contract (see module docs). Unlike `LayaRouter`, candidate filtering
/// reuses `filter_candidates` against a supplied registry, so Jev is never
/// even asked to choose a candidate that lacks a required capability.
pub struct JevRouter {
    client: reqwest::Client,
    url: String,
    /// Always resolved to a concrete name by `new()` (default
    /// `TYPESAFE_API_KEY`) — unlike `LayaRouter`/`HttpRouter`, a missing
    /// credential is a routing error here, not a silently-unauthenticated
    /// request (see the brief: "missing key" is a named error case).
    key_env: String,
    registry: Vec<(String, ModelCapabilities)>,
}

impl JevRouter {
    /// TypeSafe's hosted endpoint, verified 2026-09-24 against
    /// `docs.typesafe.ai/api.md` and `docs.litellm.ai/docs/pass_through/typesafe`.
    pub const DEFAULT_URL: &'static str = "https://api.typesafe.ai/v1/systemone";
    /// Conventional credential env var (matches the OpenJev READMEs' own
    /// examples and the brief).
    pub const DEFAULT_KEY_ENV: &'static str = "TYPESAFE_API_KEY";
    /// Model alias accepted by both TypeSafe's hosted Jev and OpenJev
    /// servers ("the server also accepts jev-latest ... so TypeSafe SDK
    /// defaults work" — OpenJev README), so one alias round-trips against
    /// either backend without extra configuration.
    const MODEL_ALIAS: &'static str = "jev-latest";

    pub fn new(
        url: Option<String>,
        key_env: Option<String>,
        timeout: Duration,
        registry: Vec<(String, ModelCapabilities)>,
    ) -> Result<Self, ForgeError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ForgeError::router(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            url: url.unwrap_or_else(|| Self::DEFAULT_URL.to_string()),
            key_env: key_env.unwrap_or_else(|| Self::DEFAULT_KEY_ENV.to_string()),
            registry,
        })
    }

    /// Fluent alternative to [`JevRouter::new`]'s four positional
    /// arguments; `new` stays for backwards compatibility with existing
    /// call sites.
    pub fn builder() -> JevRouterBuilder {
        JevRouterBuilder::default()
    }
}

/// Builder for [`JevRouter`]. All fields default the same way `new`'s
/// `None`/empty-`Vec` arguments would; `timeout` defaults to
/// [`Duration::ZERO`] (`Duration` has no natural "unset" value), so callers
/// building for real use always set it explicitly.
#[derive(Default)]
pub struct JevRouterBuilder {
    url: Option<String>,
    key_env: Option<String>,
    timeout: Duration,
    registry: Vec<(String, ModelCapabilities)>,
}

impl JevRouterBuilder {
    pub fn url(mut self, url: Option<String>) -> Self {
        self.url = url;
        self
    }

    pub fn key_env(mut self, key_env: Option<String>) -> Self {
        self.key_env = key_env;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn registry(mut self, registry: Vec<(String, ModelCapabilities)>) -> Self {
        self.registry = registry;
        self
    }

    pub fn build(self) -> Result<JevRouter, ForgeError> {
        JevRouter::new(self.url, self.key_env, self.timeout, self.registry)
    }
}

#[async_trait]
impl DecisionRouter for JevRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        // The credential is read at request time (never logged) and its
        // absence is a typed error naming the env var, not a silent
        // unauthenticated request — Jev/OpenJev both require auth.
        let key = std::env::var(&self.key_env)
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                ForgeError::router(format!(
                    "jev router requires a credential in ${} (not set)",
                    self.key_env
                ))
            })?;

        // Candidate pool: requested candidates (capability-checked against
        // the registry) or, when the request names none, the whole
        // registry — either way, capability-filtered so Jev is only ever
        // asked to choose among candidates that satisfy the task.
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
        let candidates = filter_candidates(&pool, &task.required_capabilities);
        if candidates.is_empty() {
            return Err(ForgeError::router(format!(
                "no candidate satisfies the required capabilities ({:?}) for jev routing",
                task.required_capabilities
            )));
        }

        let criteria: serde_json::Map<String, serde_json::Value> = candidates
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    serde_json::Value::String(format!("model {name}")),
                )
            })
            .collect();
        let body = serde_json::json!({
            "state": task.task,
            "model": Self::MODEL_ALIAS,
            "questions": {
                "model": {
                    "type": "choice",
                    "instructions": "Which model should handle this software task?",
                    "criteria": criteria,
                }
            }
        });

        let response = self
            .client
            .post(&self.url)
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ForgeError::router(format!("jev router request to {} timed out", self.url))
                } else {
                    ForgeError::router(format!("jev router request to {} failed: {e}", self.url))
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(ForgeError::router(format!(
                "jev router endpoint {} returned {status}",
                self.url
            )));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ForgeError::router(format!("invalid jev router response: {e}")))?;
        let choice = body
            .pointer("/answers/model/choice")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ForgeError::router("jev response missing answers.model.choice"))?;
        if !candidates.iter().any(|c| c.as_str() == choice) {
            return Err(ForgeError::router(format!(
                "jev answered unknown model {choice:?} (not among candidates)"
            )));
        }
        let confidence = body
            .pointer("/answers/model/confidence")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        Ok(RoutingDecision {
            selected_model: choice.to_string(),
            confidence,
            router_name: "jev".to_string(),
            fallback_used: false,
            reason: format!("jev choice (confidence {confidence:.2})"),
        })
    }
}

#[cfg(test)]
mod tests {
    use forge_core::Capability;
    use serial_test::serial;
    use wiremock::matchers::{header, method, path};
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

    fn jev_body(choice: &str, confidence: f64) -> serde_json::Value {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "model": {
                    "type": "choice",
                    "choice": choice,
                    "probabilities": {},
                    "confidence": confidence,
                }
            },
            "usage": {"input_tokens": 10, "output_tokens": 0},
        })
    }

    #[tokio::test]
    #[serial]
    async fn jev_router_maps_typed_choice_and_sends_bearer_auth() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .and(header("authorization", "Bearer test-jev-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.82)))
            .mount(&server)
            .await;

        let router = JevRouter::new(
            Some(format!("{}/systemone", server.uri())),
            None,
            Duration::from_secs(5),
            vec![
                ("cheap-a".to_string(), caps(true)),
                ("pricey-b".to_string(), caps(true)),
            ],
        )
        .expect("construct");

        let request = RoutingRequest {
            task: "fix the failing test".to_string(),
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string(), "pricey-b".to_string()],
        };
        let decision = router.route(&request).await.expect("routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.selected_model, "cheap-a");
        assert!((decision.confidence - 0.82).abs() < f64::EPSILON);
        assert_eq!(decision.router_name, "jev");
        assert!(!decision.fallback_used);

        // Verify the real wire contract shape: flat string `state`, a
        // top-level `model` alias, and the typed-questions `criteria` map
        // — distinct from LayaRouter's nested `state` object.
        let received = server.received_requests().await.expect("requests");
        assert_eq!(received.len(), 1);
        let sent: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("request json");
        assert_eq!(sent["state"], "fix the failing test");
        assert_eq!(sent["model"], "jev-latest");
        assert_eq!(sent["questions"]["model"]["type"], "choice");
        assert!(sent["questions"]["model"]["criteria"]["cheap-a"].is_string());
    }

    #[tokio::test]
    #[serial]
    async fn jev_router_rejects_unknown_choice() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("ghost", 0.9)))
            .mount(&server)
            .await;

        let router = JevRouter::new(
            Some(server.uri()),
            None,
            Duration::from_secs(5),
            vec![("cheap-a".to_string(), caps(true))],
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string()],
        };
        let err = router.route(&request).await.expect_err("unknown choice");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("ghost"), "got: {msg}"),
            other => panic!("expected router error, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn jev_router_missing_credential_is_typed_error_with_no_network_call() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.9)))
            .expect(0)
            .mount(&server)
            .await;

        let router = JevRouter::new(
            Some(format!("{}/systemone", server.uri())),
            None,
            Duration::from_secs(5),
            vec![("cheap-a".to_string(), caps(true))],
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string()],
        };
        let err = router
            .route(&request)
            .await
            .expect_err("must fail without a credential");
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("TYPESAFE_API_KEY"), "got: {msg}"),
            other => panic!("expected router error, got {other:?}"),
        }
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            0,
            "no network call should be attempted without a credential"
        );
    }

    #[tokio::test]
    #[serial]
    async fn jev_router_timeout_is_typed_router_error() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .mount(&server)
            .await;

        let router = JevRouter::new(
            Some(server.uri()),
            None,
            Duration::from_millis(50),
            vec![("cheap-a".to_string(), caps(true))],
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string()],
        };
        let err = router.route(&request).await.expect_err("must time out");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("timed out"), "got: {msg}"),
            other => panic!("expected router error, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn jev_router_never_offers_incapable_candidate() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("strong", 0.9)))
            .mount(&server)
            .await;

        let router = JevRouter::new(
            Some(server.uri()),
            None,
            Duration::from_secs(5),
            vec![
                ("weak".to_string(), caps(false)),
                ("strong".to_string(), caps(true)),
            ],
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "use tools".to_string(),
            required_capabilities: vec![Capability::Tools],
            candidates: vec!["weak".to_string(), "strong".to_string()],
        };
        let decision = router.route(&request).await.expect("routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert_eq!(decision.selected_model, "strong");

        let received = server.received_requests().await.expect("requests");
        let sent: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("request json");
        // "weak" must never even be offered as a criterion.
        assert!(sent["questions"]["model"]["criteria"]["weak"].is_null());
        assert!(sent["questions"]["model"]["criteria"]["strong"].is_string());
    }

    #[tokio::test]
    #[serial]
    async fn jev_router_errors_when_nothing_capable() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };
        // Pointed at a mock (never actually hit, since the capability
        // filter must reject before any network attempt) rather than
        // `None`/the real DEFAULT_URL — so a future reordering of the
        // credential/capability checks can't turn this unit test into a
        // live call against api.typesafe.ai.
        let server = MockServer::start().await;
        let router = JevRouter::new(
            Some(server.uri()),
            None,
            Duration::from_secs(5),
            vec![("weak".to_string(), caps(false))],
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "use tools".to_string(),
            required_capabilities: vec![Capability::Tools],
            candidates: vec!["weak".to_string()],
        };
        let err = router.route(&request).await.expect_err("must fail");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("required capabilities")),
            other => panic!("expected router error, got {other:?}"),
        }
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            0,
            "no network call should be attempted when nothing is capable"
        );
    }

    #[tokio::test]
    #[serial]
    async fn jev_router_builder_produces_a_working_router() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.9)))
            .mount(&server)
            .await;

        let router = JevRouter::builder()
            .url(Some(server.uri()))
            .timeout(Duration::from_secs(5))
            .registry(vec![("cheap-a".to_string(), caps(true))])
            .build()
            .expect("builder constructs");

        let decision = router
            .route(&RoutingRequest::new("anything"))
            .await
            .expect("routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.selected_model, "cheap-a");
        assert_eq!(decision.router_name, "jev");
    }
}
