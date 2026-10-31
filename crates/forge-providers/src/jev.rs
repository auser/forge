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
//!
//! ## The self-hosted family
//!
//! The same contract is served by servers **you** run, which changes two
//! things for this router:
//!
//! - **The credential becomes optional.** lev
//!   (<https://github.com/franckverrot/lev>), OpenJev and Kev either ignore
//!   the bearer token or are fronted by something that does, so a loopback
//!   endpoint is contacted without one. A *remote* endpoint still needs it:
//!   TypeSafe rejects unauthenticated requests, and that stays a typed
//!   error naming the env var rather than a request that fails at the far
//!   end.
//! - **One extension becomes available.** lev and Kev expose
//!   `POST /v1/systemone/permute`, which re-asks one choice question under
//!   several option orders and reports whether the winner survives
//!   re-ordering. That is a check TypeSafe's published contract does not
//!   offer, and it is off by default (`jev_order_check`) because it is an
//!   extra forward pass per order — see [`JevRouterBuilder::order_check`].

use std::time::Duration;

use async_trait::async_trait;
use forge_core::{
    DecisionRouter, ForgeError, ModelCapabilities, RoutingDecision, RoutingPhase, RoutingRequest,
};

use crate::local_only::EgressPolicy;
use crate::router::{filter_candidates, optimistic_caps};

/// System One-compatible router speaking the verified Jev/OpenJev wire
/// contract (see module docs). Unlike `LayaRouter`, candidate filtering
/// reuses `filter_candidates` against a supplied registry, so Jev is never
/// even asked to choose a candidate that lacks a required capability.
pub struct JevRouter {
    client: reqwest::Client,
    url: String,
    /// Always resolved to a concrete name by `new()` (default
    /// `TYPESAFE_API_KEY`). Unlike `LayaRouter`/`HttpRouter`, a missing
    /// credential is a routing error here — for a **remote** endpoint. A
    /// local one (lev, OpenJev, Kev on loopback) needs no credential at all,
    /// so it is contacted unauthenticated rather than refused.
    key_env: String,
    /// The top-level `model` id sent on every request. Defaults to
    /// [`JevRouter::MODEL_ALIAS`]; a backend that wants its own id
    /// (meraGPT's `sd-1`, a self-hosted Kev/CLM server's) gets it via the
    /// `jev_model` config key.
    model_id: String,
    /// Whether to re-ask the routing question under several option orders
    /// and refuse a choice whose winner does not survive re-ordering. Off
    /// by default: it is an extension of the self-hosted family, not of
    /// TypeSafe's contract, and it costs one forward pass per order.
    order_check: bool,
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

    /// `egress` decides how far this client may travel, redirects included
    /// (see [`EgressPolicy`]). `local_only` prunes this router entirely, so
    /// in practice it is only ever built unrestricted — the parameter exists
    /// so that stays a choice the code makes rather than one it forgets.
    pub fn new(
        url: Option<String>,
        key_env: Option<String>,
        timeout: Duration,
        registry: Vec<(String, ModelCapabilities)>,
        egress: EgressPolicy,
    ) -> Result<Self, ForgeError> {
        Self::with_model_id(url, key_env, None, timeout, registry, egress)
    }

    /// [`JevRouter::new`] with an explicit request model id (`None` keeps
    /// [`JevRouter::MODEL_ALIAS`], the value every known-compatible
    /// backend accepts or ignores safely).
    pub fn with_model_id(
        url: Option<String>,
        key_env: Option<String>,
        model_id: Option<String>,
        timeout: Duration,
        registry: Vec<(String, ModelCapabilities)>,
        egress: EgressPolicy,
    ) -> Result<Self, ForgeError> {
        Self::with_options(url, key_env, model_id, timeout, registry, egress, false)
    }

    /// Everything [`JevRouter::with_model_id`] takes, plus the option-order
    /// check. Private because the builder is the public way to set it (and
    /// because the positional-argument list is already at the limit where
    /// another one stops being readable).
    #[allow(clippy::too_many_arguments)]
    fn with_options(
        url: Option<String>,
        key_env: Option<String>,
        model_id: Option<String>,
        timeout: Duration,
        registry: Vec<(String, ModelCapabilities)>,
        egress: EgressPolicy,
        order_check: bool,
    ) -> Result<Self, ForgeError> {
        let client = egress
            .client(timeout)
            .map_err(|e| ForgeError::router(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            url: url.unwrap_or_else(|| Self::DEFAULT_URL.to_string()),
            key_env: key_env.unwrap_or_else(|| Self::DEFAULT_KEY_ENV.to_string()),
            model_id: model_id.unwrap_or_else(|| Self::MODEL_ALIAS.to_string()),
            order_check,
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
/// `egress` has no default on purpose — see [`JevRouterBuilder::egress`].
#[derive(Default)]
pub struct JevRouterBuilder {
    url: Option<String>,
    key_env: Option<String>,
    model_id: Option<String>,
    timeout: Duration,
    registry: Vec<(String, ModelCapabilities)>,
    egress: Option<EgressPolicy>,
    order_check: bool,
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

    pub fn model_id(mut self, model_id: Option<String>) -> Self {
        self.model_id = model_id;
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

    /// Re-ask the routing question under several option orders and refuse a
    /// choice whose winner does not survive re-ordering.
    ///
    /// A classifier that lists its options in a different order and picks a
    /// different one has not made a decision — it has reported the order.
    /// The self-hosted System One servers (lev, Kev) expose an endpoint for
    /// exactly this (`POST /v1/systemone/permute`); TypeSafe's published
    /// contract does not, so against a hosted endpoint this check reports
    /// itself unavailable and the decision stands.
    ///
    /// Off by default: each order costs one forward pass at the far end.
    pub fn order_check(mut self, order_check: bool) -> Self {
        self.order_check = order_check;
        self
    }

    /// **Required** (see [`EgressPolicy`]). `EgressPolicy::default()` is the
    /// permissive variant, so a builder that silently defaulted would make
    /// *forgetting* the confidentiality-relevant choice the unrestricted one.
    /// [`Self::build`] refuses instead — the direct constructor's required
    /// parameter with the same effect.
    pub fn egress(mut self, egress: EgressPolicy) -> Self {
        self.egress = Some(egress);
        self
    }

    pub fn build(self) -> Result<JevRouter, ForgeError> {
        let egress = self.egress.ok_or_else(|| {
            ForgeError::router(
                "JevRouter::builder() requires .egress(EgressPolicy::…): the egress policy \
                 decides whether local_only applies to this client, and defaulting it would \
                 default to unrestricted",
            )
        })?;
        JevRouter::with_options(
            self.url,
            self.key_env,
            self.model_id,
            self.timeout,
            self.registry,
            egress,
            self.order_check,
        )
    }
}

#[async_trait]
impl DecisionRouter for JevRouter {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError> {
        // The credential is read at request time (never logged). A **local**
        // endpoint needs none: the self-hosted System One servers this
        // crate speaks to (lev, OpenJev, Kev) ignore the bearer token, so
        // refusing the request would refuse a setup that works. A **remote**
        // endpoint does need one — TypeSafe rejects unauthenticated
        // requests — and its absence stays a typed error naming the env
        // var, not a silently-unauthenticated request that fails at the far
        // end.
        let key = std::env::var(&self.key_env).ok().filter(|k| !k.is_empty());
        if key.is_none() && !crate::endpoint_is_local(&self.url) {
            return Err(ForgeError::router(format!(
                "jev router requires a credential in ${} (not set)",
                self.key_env
            )));
        }

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
        let state = match task.phase {
            RoutingPhase::General => task.task.clone(),
            RoutingPhase::Plan => format!("[phase: plan] {}", task.task),
            RoutingPhase::Execute => format!("[phase: execute] {}", task.task),
            RoutingPhase::Review => format!("[phase: review] {}", task.task),
        };
        let mut questions = serde_json::Map::new();
        questions.insert(
            MODEL_QUESTION_ID.to_string(),
            serde_json::json!({
                "type": "choice",
                "instructions": match task.phase {
                    RoutingPhase::Plan => "Which model should design the implementation plan? Prefer the strongest reasoning model available.",
                    RoutingPhase::Review => "Which model should independently review this change? Prefer strong reasoning for subtle defects.",
                    RoutingPhase::General | RoutingPhase::Execute => "Which model should handle this software task? Prefer the least expensive capable model.",
                },
                "criteria": criteria,
            }),
        );
        let body = serde_json::json!({
            "state": state,
            "model": self.model_id,
            "questions": questions,
        });

        let mut request = self.client.post(&self.url);
        if let Some(key) = key.as_deref() {
            request = request.bearer_auth(key);
        }
        let response = request.json(&body).send().await.map_err(|e| {
            if e.is_timeout() {
                ForgeError::router(format!("jev router request to {} timed out", self.url))
            } else {
                // Source chain included so a `local_only` redirect
                // refusal explains itself (see
                // `local_only::error_detail`).
                ForgeError::router(format!(
                    "jev router request to {} failed: {}",
                    self.url,
                    crate::local_only::error_detail(&e)
                ))
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

        // The order check runs on the *request* that produced this choice, so
        // the far end re-asks exactly the same question — only with the
        // candidate order shuffled.
        let mut reason = format!("jev choice (confidence {confidence:.2})");
        if self.order_check {
            match self.check_order_stability(&body, key.as_deref()).await {
                OrderCheck::Stable => {
                    reason.push_str(&format!(
                        "; order-stable across {ORDER_CHECK_PERMUTATIONS} option orders"
                    ));
                }
                OrderCheck::Unavailable(why) => {
                    // A missing extension is not a failed decision: TypeSafe's
                    // contract has no permute endpoint at all. The reason
                    // string carries the caveat so the routing event does
                    // too — a decision nobody can verify must not read as one
                    // that was verified.
                    tracing::warn!(why = %why, "jev order check unavailable; keeping the choice");
                    reason.push_str(&format!("; order check unavailable ({why})"));
                }
                OrderCheck::Unstable { choices } => {
                    // Not a decision — a report of the order the candidates
                    // happened to be listed in. Refusing it sends the run
                    // down the fallback chain, which is the honest outcome:
                    // a wrong-but-confident model is worse than the
                    // configured fallback.
                    return Err(ForgeError::router(format!(
                        "jev choice is order-sensitive: the top candidate changed across \
                         {ORDER_CHECK_PERMUTATIONS} option orders (saw {choices:?}); \
                         refusing to route on it"
                    )));
                }
            }
        }

        Ok(RoutingDecision {
            selected_model: choice.to_string(),
            confidence,
            router_name: "jev".to_string(),
            fallback_used: false,
            reason,
        })
    }
}

impl JevRouter {
    /// Re-ask the routing question under several option orders and report
    /// whether the winner survives re-ordering.
    ///
    /// Never fails the routing decision on its own: a backend without the
    /// endpoint, an unreachable one, or an unreadable answer all resolve to
    /// [`OrderCheck::Unavailable`], which keeps the choice and says so. Only
    /// a *positive* finding — the winner actually changing — refuses it.
    async fn check_order_stability(
        &self,
        body: &serde_json::Value,
        key: Option<&str>,
    ) -> OrderCheck {
        let url = permute_url(&self.url);
        let mut request = self.client.post(&url);
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        let payload = serde_json::json!({
            "request": body,
            "question": MODEL_QUESTION_ID,
            "n_perm": ORDER_CHECK_PERMUTATIONS,
            "seed": 0,
        });
        let response = match request.json(&payload).send().await {
            Ok(response) => response,
            Err(e) => {
                return OrderCheck::Unavailable(format!(
                    "request to {url} failed: {}",
                    crate::local_only::error_detail(&e)
                ));
            }
        };
        let status = response.status();
        if !status.is_success() {
            return OrderCheck::Unavailable(format!("{url} returned {status}"));
        }
        let parsed: serde_json::Value = match response.json().await {
            Ok(parsed) => parsed,
            Err(e) => {
                return OrderCheck::Unavailable(format!("invalid response from {url}: {e}"));
            }
        };
        match parsed
            .pointer("/argmax_stable")
            .and_then(serde_json::Value::as_bool)
        {
            Some(true) => OrderCheck::Stable,
            Some(false) => OrderCheck::Unstable {
                choices: distinct_choices(&parsed),
            },
            None => OrderCheck::Unavailable(format!("{url} did not report argmax stability")),
        }
    }
}

/// The question id forge asks its routing question under — and the id the
/// order check re-asks.
const MODEL_QUESTION_ID: &str = "model";

/// How many option orders the order check tries. lev's own default; each
/// order is one forward pass at the far end.
const ORDER_CHECK_PERMUTATIONS: usize = 6;

/// The outcome of the option-order check.
enum OrderCheck {
    /// The same candidate won under every order.
    Stable,
    /// Different candidates won under different orders: the choice reflects
    /// the order the candidates were listed in, not the task.
    Unstable { choices: Vec<String> },
    /// The check could not run. No evidence of instability — the decision
    /// stands, and says that the check did not run.
    Unavailable(String),
}

/// The distinct winners across the permute runs, in the order they appear.
fn distinct_choices(parsed: &serde_json::Value) -> Vec<String> {
    let mut choices: Vec<String> = Vec::new();
    for run in parsed
        .pointer("/runs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(choice) = run.pointer("/choice").and_then(serde_json::Value::as_str) {
            let choice = choice.to_string();
            if !choices.contains(&choice) {
                choices.push(choice);
            }
        }
    }
    choices
}

/// The option-order endpoint for a System One URL.
///
/// lev and Kev expose `POST /v1/systemone/permute` next to
/// `POST /v1/systemone`; deriving it from the configured URL keeps the two
/// in step without a second config key. A URL that does not end in
/// `/systemone` gets `/permute` appended — the honest guess for other
/// layouts, and a backend without such an endpoint answers 404, which the
/// caller reads as "check unavailable" rather than as a failure.
fn permute_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    match trimmed.strip_suffix("/systemone") {
        Some(base) => format!("{base}/systemone/permute"),
        None => format!("{trimmed}/permute"),
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
            EgressPolicy::default(),
        )
        .expect("construct");

        let request = RoutingRequest {
            task: "fix the failing test".to_string(),
            phase: RoutingPhase::General,
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

    /// A backend that wants its own model id (meraGPT's `sd-1`, a
    /// self-hosted Kev/CLM server's) gets it on the wire; the alias stays
    /// the default.
    #[tokio::test]
    #[serial]
    async fn jev_router_sends_the_configured_model_id() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.9)))
            .mount(&server)
            .await;

        let router = JevRouter::with_model_id(
            Some(server.uri()),
            None,
            Some("sd-1".to_string()),
            Duration::from_secs(5),
            vec![("cheap-a".to_string(), caps(true))],
            EgressPolicy::default(),
        )
        .expect("construct");

        let request = RoutingRequest {
            task: "route me".to_string(),
            phase: RoutingPhase::General,
            required_capabilities: vec![],
            candidates: vec!["cheap-a".to_string()],
        };
        router.route(&request).await.expect("routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        let received = server.received_requests().await.expect("requests");
        let sent: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("request json");
        assert_eq!(sent["model"], "sd-1");
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
            EgressPolicy::default(),
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "x".to_string(),
            phase: RoutingPhase::General,
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

    /// A **remote** endpoint still needs a credential: TypeSafe rejects
    /// unauthenticated requests, so a missing key is a typed error naming
    /// the env var rather than a request that fails at the far end. The
    /// error names the credential rather than the transport — which is the
    /// proof that nothing was sent.
    #[tokio::test]
    #[serial]
    async fn jev_router_missing_credential_is_typed_error_with_no_network_call() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        let router = JevRouter::new(
            Some("http://router.example.com/systemone".to_string()),
            None,
            Duration::from_millis(200),
            vec![("cheap-a".to_string(), caps(true))],
            EgressPolicy::default(),
        )
        .expect("construct");
        let request = RoutingRequest::new("x");
        let err = router
            .route(&request)
            .await
            .expect_err("must fail without a credential");
        match err {
            ForgeError::Router(msg) => assert!(msg.contains("TYPESAFE_API_KEY"), "got: {msg}"),
            other => panic!("expected router error, got {other:?}"),
        }
    }

    /// A **local** endpoint needs no credential at all: the self-hosted
    /// System One servers (lev, OpenJev, Kev) ignore the bearer token, and
    /// demanding one would refuse a setup that works. The request goes out
    /// without an `Authorization` header — not with an empty one.
    #[tokio::test]
    #[serial]
    async fn a_local_endpoint_is_consulted_without_a_credential() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.82)))
            .mount(&server)
            .await;

        let router = JevRouter::new(
            Some(format!("{}/systemone", server.uri())),
            None,
            Duration::from_secs(5),
            vec![("cheap-a".to_string(), caps(true))],
            EgressPolicy::default(),
        )
        .expect("construct");

        let decision = router
            .route(&RoutingRequest::new("fix the failing test"))
            .await
            .expect("routes without a credential");

        assert_eq!(decision.selected_model, "cheap-a");
        assert_eq!(decision.router_name, "jev");

        let received = server.received_requests().await.expect("requests");
        assert_eq!(received.len(), 1);
        assert!(
            received[0].headers.get("authorization").is_none(),
            "no credential means no Authorization header at all"
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
            EgressPolicy::default(),
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "x".to_string(),
            phase: RoutingPhase::General,
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
            EgressPolicy::default(),
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "use tools".to_string(),
            phase: RoutingPhase::General,
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
            EgressPolicy::default(),
        )
        .expect("construct");
        let request = RoutingRequest {
            task: "use tools".to_string(),
            phase: RoutingPhase::General,
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
            .egress(EgressPolicy::default())
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

    /// Forgetting the egress policy must not silently produce an
    /// unrestricted client: for a confidentiality-relevant knob, omission has
    /// to fail rather than pick the permissive side.
    #[test]
    fn the_builder_refuses_to_construct_without_an_egress_policy() {
        let err = JevRouter::builder()
            .url(Some("http://127.0.0.1:9/systemone".to_string()))
            .timeout(Duration::from_secs(5))
            .build()
            .err()
            .expect("omitting .egress() must fail");
        assert!(matches!(err, ForgeError::Router(_)), "{err}");
        assert!(err.to_string().contains("requires .egress("), "{err}");
    }

    /// A router with the order check on, pointing at a mock that answers
    /// both endpoints the way lev does.
    fn order_check_router(url: &str) -> Result<JevRouter, ForgeError> {
        JevRouter::builder()
            .url(Some(format!("{url}/systemone")))
            .timeout(Duration::from_secs(5))
            .registry(vec![
                ("cheap-a".to_string(), caps(true)),
                ("pricey-b".to_string(), caps(true)),
            ])
            .order_check(true)
            .egress(EgressPolicy::default())
            .build()
    }

    /// lev's permute response, shaped exactly as it answers.
    fn permute_body(stable: bool) -> serde_json::Value {
        let (first, second) = if stable {
            ("cheap-a", "cheap-a")
        } else {
            ("cheap-a", "pricey-b")
        };
        serde_json::json!({
            "runs": [
                {"order": ["cheap-a", "pricey-b"], "choice": first,
                 "probabilities": {"cheap-a": 0.7, "pricey-b": 0.3}},
                {"order": ["pricey-b", "cheap-a"], "choice": second,
                 "probabilities": {"cheap-a": 0.66, "pricey-b": 0.34}},
            ],
            "argmax_stable": stable,
            "spread": {"cheap-a": 0.04, "pricey-b": 0.04},
        })
    }

    /// A choice that wins under every option order is a decision. The
    /// check runs, finds the winner unchanged, and the decision says so.
    #[tokio::test]
    #[serial]
    async fn the_order_check_keeps_a_choice_that_survives_re_ordering() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.82)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/systemone/permute"))
            .respond_with(ResponseTemplate::new(200).set_body_json(permute_body(true)))
            .mount(&server)
            .await;

        let router = order_check_router(&server.uri()).expect("construct");
        let decision = router
            .route(&RoutingRequest::new("route me"))
            .await
            .expect("routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.selected_model, "cheap-a");
        assert!(
            decision.reason.contains("order-stable"),
            "the reason must record that the check ran: {}",
            decision.reason
        );
    }

    /// A choice whose winner changes with the option order is not a
    /// decision — it is the order talking. The router refuses it, so the
    /// fallback chain takes over instead of routing on an artefact.
    #[tokio::test]
    #[serial]
    async fn the_order_check_refuses_an_order_sensitive_choice() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.82)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/systemone/permute"))
            .respond_with(ResponseTemplate::new(200).set_body_json(permute_body(false)))
            .mount(&server)
            .await;

        let router = order_check_router(&server.uri()).expect("construct");
        let err = router
            .route(&RoutingRequest::new("route me"))
            .await
            .expect_err("an order-sensitive choice must be refused");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        match err {
            ForgeError::Router(msg) => {
                assert!(msg.contains("order-sensitive"), "got: {msg}");
                // Both winners are named: the message says what it saw.
                assert!(msg.contains("cheap-a"), "got: {msg}");
                assert!(msg.contains("pricey-b"), "got: {msg}");
            }
            other => panic!("expected router error, got {other:?}"),
        }
    }

    /// A backend without the permute endpoint (TypeSafe's published
    /// contract has none) must not break routing: the check reports itself
    /// unavailable, the decision stands, and the reason carries the caveat
    /// — a decision nobody could verify must not read as one that was.
    #[tokio::test]
    #[serial]
    async fn the_order_check_is_skipped_when_the_backend_has_no_such_endpoint() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-jev-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.82)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/systemone/permute"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let router = order_check_router(&server.uri()).expect("construct");
        let decision = router
            .route(&RoutingRequest::new("route me"))
            .await
            .expect("a missing extension must not break routing");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.selected_model, "cheap-a");
        assert!(
            decision.reason.contains("order check unavailable"),
            "the reason must record that the check did not run: {}",
            decision.reason
        );
    }

    /// The permute endpoint is derived from the configured URL, so the two
    /// cannot drift into different halves of one server.
    #[test]
    fn the_permute_endpoint_is_derived_from_the_configured_url() {
        assert_eq!(
            permute_url("http://127.0.0.1:8009/v1/systemone"),
            "http://127.0.0.1:8009/v1/systemone/permute"
        );
        assert_eq!(
            permute_url("http://127.0.0.1:8009/v1/systemone/"),
            "http://127.0.0.1:8009/v1/systemone/permute"
        );
        assert_eq!(
            permute_url("http://127.0.0.1:8009/v1"),
            "http://127.0.0.1:8009/v1/permute"
        );
    }
}
