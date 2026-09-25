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

pub(crate) fn optimistic_caps() -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        tools: true,
        structured_output: true,
        vision: true,
        max_context: usize::MAX,
    }
}

/// Appended to every "laya is not answering" error. `router = "laya"` is a
/// pre-needle setting, so an unreachable adapter almost always means the
/// config predates the embedded brain rather than that the adapter crashed
/// — and the fallback hides that, leaving only this warn to explain itself.
/// Shape-of-response errors deliberately do *not* get this hint: those mean
/// laya IS running.
const LAYA_LEGACY_HINT: &str = "hint: laya is no longer the default — delete the `router` line to use the embedded \
     needle brain, or run `forge router serve` to serve laya";

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

    /// Fluent alternative to [`LayaRouter::new`]'s four positional
    /// arguments; `new` stays for backwards compatibility with existing
    /// call sites.
    pub fn builder() -> LayaRouterBuilder {
        LayaRouterBuilder::default()
    }
}

/// Builder for [`LayaRouter`]. All fields default the same way `new`'s
/// `None`/zero-length arguments would.
#[derive(Default)]
pub struct LayaRouterBuilder {
    url: Option<String>,
    key_env: Option<String>,
    timeout: Duration,
    criteria: std::collections::HashMap<String, String>,
}

impl LayaRouterBuilder {
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

    pub fn criteria(mut self, criteria: std::collections::HashMap<String, String>) -> Self {
        self.criteria = criteria;
        self
    }

    pub fn build(self) -> Result<LayaRouter, ForgeError> {
        LayaRouter::new(self.url, self.key_env, self.timeout, self.criteria)
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
                ForgeError::router(format!(
                    "laya router request to {} timed out — {LAYA_LEGACY_HINT}",
                    self.url
                ))
            } else {
                ForgeError::router(format!(
                    "laya router request to {} failed: {e} — {LAYA_LEGACY_HINT}",
                    self.url
                ))
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ForgeError::router(format!(
                "laya router endpoint {} returned {status} — {LAYA_LEGACY_HINT}",
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

/// One router-mode constructor: builds a fresh router from config/registry,
/// ignoring whichever parameter it doesn't need. Kept as a plain function
/// (not a trait) per YAGNI — there is exactly one thing every mode does
/// ("build me one of these"), and a name -> fn-pointer table already gives
/// dispatch without a trait that would only ever have these seven impls.
type RouterCtor =
    fn(&Config, &[(String, ModelCapabilities)]) -> Result<Arc<dyn DecisionRouter>, ForgeError>;

/// `static`, `mock`, `cheapest` are local; `http`/`laya` are HTTP; `needle`
/// is the embedded on-device Needle 3 decision router (env
/// `FORGE_NEEDLE_BACKEND=hash` selects the deterministic test/BDD backend
/// instead of the real weights-backed engine); `jev` here is always the
/// *primary*-role constructor (escalation goes through [`build_jev`]
/// directly with the escalation-role credential resolution — see
/// [`resolved_jev_url`]).
const ROUTER_CTORS: &[(&str, RouterCtor)] = &[
    ("static", build_static),
    ("mock", build_mock),
    ("cheapest", build_cheapest),
    ("http", build_http),
    ("laya", build_laya),
    ("needle", build_needle),
    ("jev", build_jev_primary),
];

/// Build a router by name via the [`ROUTER_CTORS`] dispatch table.
fn build_router(
    name: &str,
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    // The backstop for every role a router name can appear in. The primary
    // role degrades before reaching here (see
    // [`RouterStackBuilder::resolve_primary`]); a `router_fallback` — or any
    // future role — pointed at a remote decision service is a genuine config
    // error, because there is nothing left to degrade to.
    if let Some(reason) = local_only_block(name, config) {
        return Err(ForgeError::router(format!(
            "router = {name:?} {reason}, but local_only is set; hint: use \
             router = \"needle\" or \"static\", point the router at a local \
             endpoint, or unset local_only / FORGE_LOCAL_ONLY"
        )));
    }
    match ROUTER_CTORS.iter().find(|(n, _)| *n == name) {
        Some((_, ctor)) => ctor(config, registry),
        // `mock` is accepted (it is in ROUTER_CTORS) but deliberately not
        // listed: it is test-only and refused without FORGE_TEST_MOCKS, so
        // advertising it here would be pointing users at a dead end.
        None => Err(ForgeError::router(format!(
            "unknown router {name:?} (expected needle, jev, laya, http, static, or cheapest)"
        ))),
    }
}

fn build_static(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    Ok(Arc::new(
        StaticRouter::new(config.model.clone()).with_registry(registry.to_vec()),
    ))
}

/// The test-only mock router. Gated like the mock models: configuration may
/// only select it under `FORGE_TEST_MOCKS=1` (see
/// [`forge_config::test_mocks`]).
fn build_mock(
    config: &Config,
    _registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    forge_config::ensure_test_mocks_allowed("router = \"mock\"")?;
    Ok(Arc::new(MockRouter::selecting(config.model.clone())))
}

fn build_cheapest(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    let costs = config
        .model_entries()
        .iter()
        .map(|(name, entry)| (name.clone(), entry.costs()))
        .collect();
    Ok(Arc::new(CheapestRouter::new(costs, registry.to_vec())))
}

fn build_http(
    config: &Config,
    _registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    let url = config.router_url.as_deref().ok_or_else(|| {
        ForgeError::router("router = \"http\" requires router_url to be configured")
    })?;
    Ok(Arc::new(HttpRouter::new(
        url,
        config.router_key_env.clone(),
        Duration::from_millis(config.router_timeout_ms),
    )?))
}

fn build_laya(
    config: &Config,
    _registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
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
    Ok(Arc::new(
        LayaRouter::builder()
            .url(config.router_url.clone())
            .key_env(config.router_key_env.clone())
            .timeout(Duration::from_millis(config.router_timeout_ms))
            .criteria(criteria)
            .build()?,
    ))
}

fn build_needle(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    let engine = forge_needle::select_engine(&config.needle)?;
    Ok(Arc::new(forge_needle::NeedleRouter::new(
        engine,
        registry.to_vec(),
        Duration::from_millis(config.router_timeout_ms),
    )))
}

/// `router = "jev"` as *primary* — always the primary-role credential
/// resolution (`escalation = false`); see [`build_jev`].
fn build_jev_primary(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    build_jev(config, registry, false)
}

/// Resolve the Jev endpoint for a given role. **Escalation must never
/// silently reuse a leftover `router_url`** meant for a different primary
/// router (`http`/`laya`) — a stale `router_url` pointed at some other
/// System One-compatible endpoint would otherwise receive the
/// `TYPESAFE_API_KEY` credential. So escalation only ever falls back to
/// `jev_url` or the compiled-in default, never `router_url`. The primary
/// role (`router = "jev"`) *does* fall back to `router_url` for
/// backwards-compatibility with how `http`/`laya` already reuse the
/// generic field when there's no more specific one.
pub fn resolved_jev_url(config: &Config, escalation: bool) -> Option<String> {
    config.jev_url.clone().or_else(|| {
        if escalation {
            None
        } else {
            config.router_url.clone()
        }
    })
}

/// Resolve the Jev credential env var name for a given role — same scoping
/// rationale as [`resolved_jev_url`]: escalation never falls back to the
/// generic `router_key_env` (which could belong to an unrelated
/// `http`/`laya` setup and send its token to `api.typesafe.ai`).
pub fn resolved_jev_key_env(config: &Config, escalation: bool) -> String {
    config
        .jev_key_env
        .clone()
        .or_else(|| {
            if escalation {
                None
            } else {
                config.router_key_env.clone()
            }
        })
        .unwrap_or_else(|| crate::JevRouter::DEFAULT_KEY_ENV.to_string())
}

/// Whether a non-empty Jev credential is present in the environment right
/// now, using the same role-scoped resolution as [`resolved_jev_key_env`].
/// Checked once at router-construction time (not per-request): the
/// escalation tier is either wired into the stack or it isn't for the
/// lifetime of this router.
pub fn jev_credential_present(config: &Config, escalation: bool) -> bool {
    std::env::var(resolved_jev_key_env(config, escalation))
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Build a `JevRouter` for the given role (see [`resolved_jev_url`] /
/// [`resolved_jev_key_env`] for why the role matters).
fn build_jev(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
    escalation: bool,
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    Ok(Arc::new(
        crate::JevRouter::builder()
            .url(resolved_jev_url(config, escalation))
            .key_env(Some(resolved_jev_key_env(config, escalation)))
            .timeout(Duration::from_millis(config.router_timeout_ms))
            .registry(registry.to_vec())
            .build()?,
    ))
}

/// Why `local_only` will not let this router class run, phrased to slot
/// into "router = \"x\" {reason}" — or `None` when it may.
///
/// A decision router is handed the user's task text, so a remote one is a
/// disclosure just like a remote model endpoint; the two planes share
/// [`crate::endpoint_is_local`] so "local" means one thing.
///
/// `static`, `cheapest`, `needle` and the test-only `mock` never leave the
/// process, so they are always allowed.
///
/// `http`/`laya` are judged on their **endpoint**, not their name, because
/// their endpoint is very often local: `LayaRouter::DEFAULT_URL` is
/// `http://127.0.0.1:8788/decide`, the adapter `forge serve` auto-starts.
/// Blanket-pruning them would break a setup that sends nothing off the
/// machine, which is over-blocking, not enforcement.
///
/// `jev` is pruned unconditionally, even when `jev_url` names a loopback
/// self-hosted OpenJev. Its escalation-tier design shipped that promise
/// ("`--local-only` prunes the tier entirely"), `forge doctor` reports it,
/// and relaxing a shipped confidentiality promise is not this function's
/// job. The asymmetry is deliberate and documented in the README.
fn local_only_block(name: &str, config: &Config) -> Option<String> {
    if !config.local_only {
        return None;
    }
    match name {
        "jev" => Some("requires network access".to_string()),
        "http" | "laya" => {
            let url = router_endpoint(name, config)?;
            (!crate::endpoint_is_local(&url)).then(|| {
                format!("would send routing requests to {url}, which is not a local endpoint")
            })
        }
        _ => None,
    }
}

/// The endpoint an `http`/`laya` router will dial, resolved exactly as its
/// constructor does. `None` for `http` with no `router_url` — `build_http`
/// already errors on that, and a router that cannot be built cannot leak.
fn router_endpoint(name: &str, config: &Config) -> Option<String> {
    match name {
        "http" => config.router_url.clone(),
        "laya" => Some(
            config
                .router_url
                .clone()
                .unwrap_or_else(|| LayaRouter::DEFAULT_URL.to_string()),
        ),
        _ => None,
    }
}

/// Builds the full decision-router stack from configuration. Each step is
/// a small, independently testable method; [`router_from_config`] is a
/// thin public shim over [`RouterStackBuilder::build`] so callers see no
/// API change.
struct RouterStackBuilder<'a> {
    config: &'a Config,
    registry: &'a [(String, ModelCapabilities)],
}

impl<'a> RouterStackBuilder<'a> {
    fn new(config: &'a Config, registry: &'a [(String, ModelCapabilities)]) -> Self {
        Self { config, registry }
    }

    /// The effective primary router name and its freshly-built instance
    /// (unwrapped — no threshold gate yet, see [`Self::threshold_wrap`]).
    ///
    /// **`local_only`**: a primary router that `local_only` refuses (see
    /// [`local_only_block`] — `jev` always, `http`/`laya` when their
    /// endpoint is off-device) degrades to `static` with a warning rather
    /// than erroring the whole build: the same "prefer a working,
    /// less-capable router over refusing to run" philosophy `needle`'s
    /// no-weights fallback already uses. The escalation role is pruned the
    /// same way, in [`Self::escalation_tier`]; every other role hard-errors
    /// in [`build_router`], which has nothing left to degrade to.
    fn resolve_primary(&self) -> Result<(String, Arc<dyn DecisionRouter>), ForgeError> {
        let configured = &self.config.router;
        let name = match local_only_block(configured, self.config) {
            Some(reason) => {
                tracing::warn!(
                    "router = {configured:?} {reason}; --local-only forces static routing instead"
                );
                "static".to_string()
            }
            None => configured.clone(),
        };
        let router = build_router(&name, self.config, self.registry)?;
        Ok((name, router))
    }

    /// Reject-below-threshold gate for the router classes that report a
    /// meaningful confidence (`http`/`laya`/`needle`/`jev`); everything else
    /// (`static`, `mock`, `cheapest`) passes through unwrapped, since their
    /// confidence is always 1.0 by construction.
    fn threshold_wrap(
        &self,
        name: &str,
        router: Arc<dyn DecisionRouter>,
    ) -> Arc<dyn DecisionRouter> {
        if matches!(name, "http" | "laya" | "needle" | "jev") {
            Arc::new(ThresholdRouter::new(
                router,
                self.config.router_confidence_threshold,
            ))
        } else {
            router
        }
    }

    /// The needle -> jev escalation tier, if it applies: `None` when the
    /// primary isn't `needle`, escalation is off, `--local-only` is set, or
    /// no credential is present — in every one of those cases, today's
    /// plain `Fallback(Threshold(needle), router_fallback)` behavior is
    /// unchanged. The escalation tier resolves its endpoint/credential from
    /// `jev_url`/`jev_key_env` (or the compiled-in defaults) only — see
    /// [`resolved_jev_url`] — deliberately never from the generic
    /// `router_url`/`router_key_env`, which in `needle` mode belong to no
    /// router at all and, if left over from a previous `http`/`laya` setup,
    /// would otherwise silently receive the Jev credential or send an
    /// unrelated token to `api.typesafe.ai`.
    ///
    /// A `build_jev` construction failure (e.g. a bad `router_timeout_ms`
    /// producing an unbuildable HTTP client) skips the escalation tier with
    /// a warning (`Ok(None)`) rather than aborting the whole router build —
    /// needle -> static must keep working even if jev can't be wired in.
    /// A failure building `router_fallback` itself, however, still
    /// propagates as a real `Err`: that fallback is needed regardless of
    /// escalation, so a broken one is a genuine config error.
    fn escalation_tier(
        &self,
        primary_name: &str,
    ) -> Result<Option<Arc<dyn DecisionRouter>>, ForgeError> {
        let escalation_applies = primary_name == "needle"
            && self.config.router_escalate == "auto"
            && !self.config.local_only
            && jev_credential_present(self.config, true);
        if !escalation_applies {
            return Ok(None);
        }

        let jev = match build_jev(self.config, self.registry, true) {
            Ok(jev) => jev,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "jev escalation tier failed to construct; falling back to needle -> {}",
                    self.config.router_fallback
                );
                return Ok(None);
            }
        };
        let jev = self.threshold_wrap("jev", jev);
        let base_fallback = build_router(&self.config.router_fallback, self.config, self.registry)?;
        Ok(Some(Arc::new(FallbackRouter::new(jev, base_fallback))))
    }

    /// The outer fallback wrap: `primary` unwrapped when it already *is*
    /// `router_fallback` (avoids a redundant self-fallback hop), else
    /// `Fallback(primary, router_fallback)`.
    fn fallback_chain(
        &self,
        primary_name: &str,
        primary: Arc<dyn DecisionRouter>,
    ) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
        if primary_name == self.config.router_fallback {
            return Ok(primary);
        }
        let fallback = build_router(&self.config.router_fallback, self.config, self.registry)?;
        Ok(Arc::new(FallbackRouter::new(primary, fallback)))
    }

    fn build(&self) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
        let (primary_name, primary) = self.resolve_primary()?;
        let primary = self.threshold_wrap(&primary_name, primary);

        if let Some(escalation) = self.escalation_tier(&primary_name)? {
            return Ok(Arc::new(FallbackRouter::new(primary, escalation)));
        }

        self.fallback_chain(&primary_name, primary)
    }
}

/// Build the decision router from configuration: `static`, `mock`,
/// `cheapest`, `http`, `laya`, `needle`, or `jev`. See [`RouterStackBuilder`]
/// for the decomposed steps (primary resolution, threshold gating,
/// needle -> jev escalation, and the outer fallback wrap).
pub fn router_from_config(
    config: &Config,
    registry: &[(String, ModelCapabilities)],
) -> Result<Arc<dyn DecisionRouter>, ForgeError> {
    RouterStackBuilder::new(config, registry).build()
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
    fn build_http_requires_router_url_directly() {
        // Direct unit test of the extracted per-mode constructor (not just
        // through the public `router_from_config` entry point above).
        let config = Config {
            router: "http".to_string(),
            ..Config::default()
        };
        match build_http(&config, &[]) {
            Err(e) => assert!(matches!(e, ForgeError::Router(_)), "got: {e:?}"),
            Ok(_) => panic!("must fail without router_url"),
        }

        let config = Config {
            router_url: Some("http://127.0.0.1:9/route".to_string()),
            ..config
        };
        assert!(build_http(&config, &[]).is_ok());
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
        // Defensive: guarantee no jev escalation kicks in here even if a
        // prior (possibly panicked) jev test in this same process leaked a
        // credential — this test is about needle's own fallback, not jev's.
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
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

    // --- jev / escalation ---

    fn jev_body(choice: &str, confidence: f64) -> serde_json::Value {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "model": {"type": "choice", "choice": choice, "confidence": confidence},
            },
        })
    }

    #[test]
    fn router_from_config_builds_jev_explicitly() {
        let config = Config {
            router: "jev".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("jev builds");
        drop(router);
    }

    #[tokio::test]
    #[serial]
    async fn router_from_config_jev_escalation_used_when_needle_unavailable_and_credential_present()
    {
        // (a) needle unavailable (no weights) + jev credential present +
        // wiremock jev responding -> the decision comes from "jev" and is
        // marked fallback_used (FallbackRouter always marks a degraded hop).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("local-coder", 0.88)))
            .mount(&server)
            .await;

        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let mut config = Config {
            router: "needle".to_string(),
            jev_url: Some(server.uri()),
            router_fallback: "static".to_string(),
            model: "local-coder".to_string(),
            ..Config::default()
        };
        config.models.insert(
            "local-coder".to_string(),
            forge_config::ModelEntry::default(),
        );
        let router = router_from_config(&config, &[("local-coder".to_string(), caps(true))])
            .expect("builds");
        let decision = router
            .route(&RoutingRequest::new("offline task"))
            .await
            .expect("escalation routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.router_name, "jev");
        assert!(decision.fallback_used);
        assert_eq!(decision.selected_model, "local-coder");
    }

    #[tokio::test]
    #[serial]
    async fn router_from_config_escalation_ignores_poisoned_generic_router_url() {
        // A leftover `router_url` from an unrelated http/laya setup must
        // never be hijacked by the jev escalation tier: the credential
        // must not be POSTed to it, and its response (if any) must not be
        // used as the decision. Only `jev_url` is consulted.
        let poisoned = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("wrong-model", 0.99)))
            .expect(0)
            .mount(&poisoned)
            .await;

        let real_jev = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("local-coder", 0.88)))
            .mount(&real_jev)
            .await;

        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let mut config = Config {
            router: "needle".to_string(),
            router_url: Some(poisoned.uri()),
            jev_url: Some(real_jev.uri()),
            router_fallback: "static".to_string(),
            model: "local-coder".to_string(),
            ..Config::default()
        };
        config.models.insert(
            "local-coder".to_string(),
            forge_config::ModelEntry::default(),
        );
        let router = router_from_config(&config, &[("local-coder".to_string(), caps(true))])
            .expect("builds");
        let decision = router
            .route(&RoutingRequest::new("offline task"))
            .await
            .expect("escalation routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.router_name, "jev");
        assert_eq!(decision.selected_model, "local-coder");
        assert_eq!(
            poisoned.received_requests().await.expect("requests").len(),
            0,
            "the generic router_url must never be contacted by the escalation tier"
        );
    }

    #[tokio::test]
    #[serial]
    async fn router_from_config_no_jev_credential_keeps_todays_static_fallback() {
        // (b) no credential -> static fallback exactly as before jev
        // existed; jev must never even be contacted.
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("mock-local", 0.9)))
            .expect(0)
            .mount(&server)
            .await;

        let config = Config {
            router: "needle".to_string(),
            jev_url: Some(server.uri()),
            router_fallback: "static".to_string(),
            model: "mock-local".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("builds");
        let decision = router
            .route(&RoutingRequest::new("offline task"))
            .await
            .expect("fallback routes");

        assert_eq!(decision.router_name, "static");
        assert!(decision.fallback_used);
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            0,
            "jev must never be contacted without a credential"
        );
    }

    #[tokio::test]
    #[serial]
    async fn router_from_config_local_only_prunes_jev_escalation_even_with_credential() {
        // (c) --local-only + credential present -> jev never contacted.
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("mock-local", 0.9)))
            .expect(0)
            .mount(&server)
            .await;

        let config = Config {
            router: "needle".to_string(),
            jev_url: Some(server.uri()),
            router_fallback: "static".to_string(),
            local_only: true,
            model: "mock-local".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("builds");
        let decision = router
            .route(&RoutingRequest::new("offline task"))
            .await
            .expect("fallback routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.router_name, "static");
        assert!(decision.fallback_used);
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            0,
            "--local-only must prune jev from the escalation tier"
        );
    }

    #[tokio::test]
    #[serial]
    async fn router_from_config_jev_primary_under_local_only_falls_back_to_static() {
        // --local-only must also prune jev in the *primary* role: rather
        // than erroring the whole build, it degrades straight to static
        // (no FallbackRouter hop at all, since router_fallback is already
        // "static" — matching needle's own no-weights-under-local-only
        // philosophy of preferring a working router over refusing to run).
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let config = Config {
            router: "jev".to_string(),
            router_url: Some("http://127.0.0.1:9/systemone".to_string()),
            local_only: true,
            model: "mock-local".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("builds");
        let decision = router
            .route(&RoutingRequest::new("x"))
            .await
            .expect("routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

        assert_eq!(decision.router_name, "static");
        assert!(!decision.fallback_used);
    }

    /// `http` is the other half of the promise: a remote decision endpoint
    /// receives the user's task text, so `local_only` degrades it to static
    /// exactly the way it degrades jev.
    #[tokio::test]
    async fn router_from_config_remote_http_primary_under_local_only_falls_back_to_static() {
        let config = Config {
            router: "http".to_string(),
            router_url: Some("https://router.example.com/route".to_string()),
            local_only: true,
            model: "mock-local".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("builds");
        let decision = router
            .route(&RoutingRequest::new("x"))
            .await
            .expect("routes");
        assert_eq!(decision.router_name, "static");
        assert!(!decision.fallback_used);
    }

    /// ...but a *local* laya adapter — the one `forge serve` auto-starts on
    /// `127.0.0.1:8788` — sends nothing off the machine, so `local_only`
    /// leaves it alone. Over-blocking it would be a different bug.
    #[tokio::test]
    async fn router_from_config_loopback_laya_survives_local_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(laya_body("mock-local", 0.95)))
            .expect(1)
            .mount(&server)
            .await;

        let config = Config {
            router: "laya".to_string(),
            router_url: Some(server.uri()),
            local_only: true,
            model: "mock-local".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("builds");
        let request = RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![],
            candidates: vec!["mock-local".to_string()],
        };
        let decision = router.route(&request).await.expect("routes");
        assert_eq!(decision.router_name, "laya");
        assert_eq!(decision.selected_model, "mock-local");
    }

    /// A remote laya endpoint gets the same treatment as a remote http one.
    #[tokio::test]
    async fn router_from_config_remote_laya_under_local_only_falls_back_to_static() {
        let config = Config {
            router: "laya".to_string(),
            router_url: Some("https://laya.example.com/decide".to_string()),
            local_only: true,
            model: "mock-local".to_string(),
            ..Config::default()
        };
        let router = router_from_config(&config, &[]).expect("builds");
        let decision = router
            .route(&RoutingRequest::new("x"))
            .await
            .expect("routes");
        assert_eq!(decision.router_name, "static");
    }

    /// The roles a degrade cannot rescue hard-error instead: there is no
    /// fallback behind `router_fallback`, so a remote one under `local_only`
    /// is a config error rather than a silent network call.
    #[test]
    fn remote_router_as_the_fallback_is_refused_under_local_only() {
        let config = Config {
            router: "static".to_string(),
            router_fallback: "http".to_string(),
            router_url: Some("https://router.example.com/route".to_string()),
            local_only: true,
            ..Config::default()
        };
        let err = router_from_config(&config, &[])
            .err()
            .expect("a remote fallback router must be refused");
        let message = err.to_string();
        assert!(matches!(err, ForgeError::Router(_)), "{message}");
        assert!(message.contains("local_only"), "{message}");
        assert!(
            message.contains("https://router.example.com/route"),
            "{message}"
        );
    }

    /// The predicate behind all of the above, exercised directly.
    #[test]
    fn local_only_block_judges_http_and_laya_by_endpoint_and_jev_by_name() {
        let remote = Config {
            local_only: true,
            router_url: Some("https://router.example.com/route".to_string()),
            ..Config::default()
        };
        assert!(local_only_block("http", &remote).is_some());
        assert!(local_only_block("laya", &remote).is_some());
        assert!(local_only_block("jev", &remote).is_some());
        for local in ["static", "cheapest", "needle", "mock"] {
            assert!(
                local_only_block(local, &remote).is_none(),
                "{local} never leaves the process"
            );
        }

        // Loopback endpoints (and laya's loopback default) are allowed;
        // `http` without a URL cannot be built at all, so it is not blocked
        // here.
        let loopback = Config {
            local_only: true,
            router_url: Some("http://127.0.0.1:8788/decide".to_string()),
            ..Config::default()
        };
        assert!(local_only_block("http", &loopback).is_none());
        assert!(local_only_block("laya", &loopback).is_none());
        assert!(local_only_block("jev", &loopback).is_some());

        let laya_default = Config {
            local_only: true,
            ..Config::default()
        };
        assert!(local_only_block("laya", &laya_default).is_none());
        assert!(local_only_block("http", &laya_default).is_none());

        // Nothing is blocked when the setting is off.
        let off = Config {
            router_url: Some("https://router.example.com/route".to_string()),
            ..Config::default()
        };
        for name in ["http", "laya", "jev"] {
            assert!(local_only_block(name, &off).is_none(), "{name}");
        }
    }

    #[tokio::test]
    #[serial]
    async fn jev_unavailable_falls_back_via_config_chain() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let mut config = Config {
            router: "jev".to_string(),
            router_url: Some("http://127.0.0.1:9/systemone".to_string()),
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
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert_eq!(decision.selected_model, "local-coder");
        assert!(decision.fallback_used);
    }

    #[tokio::test]
    #[serial]
    async fn jev_low_confidence_escalates_to_fallback() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.3)))
            .mount(&server)
            .await;

        let config = Config {
            router: "jev".to_string(),
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
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert_eq!(decision.selected_model, "mock-local");
        assert!(decision.fallback_used);
        assert!(
            decision.reason.contains("confidence"),
            "{}",
            decision.reason
        );
    }

    // --- RouterStackBuilder (direct unit tests of the decomposed steps;
    // the full-stack tests above and below are the behavior lock) ---

    #[test]
    #[serial]
    fn router_stack_resolve_primary_forces_static_for_jev_under_local_only() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        let config = Config {
            router: "jev".to_string(),
            local_only: true,
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let (name, _router) = stack.resolve_primary().expect("resolves");
        assert_eq!(name, "static");
    }

    #[test]
    fn router_stack_resolve_primary_passes_through_otherwise() {
        let config = Config {
            router: "static".to_string(),
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let (name, _router) = stack.resolve_primary().expect("resolves");
        assert_eq!(name, "static");

        // jev primary WITHOUT local_only keeps its own name (only the
        // local_only + jev combination substitutes "static").
        let config = Config {
            router: "jev".to_string(),
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let (name, _router) = stack.resolve_primary().expect("resolves");
        assert_eq!(name, "jev");
    }

    #[tokio::test]
    async fn router_stack_threshold_wrap_gates_threshold_classes_only() {
        let config = Config {
            router_confidence_threshold: 0.7,
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let low_confidence: Arc<dyn DecisionRouter> = Arc::new(MockRouter::new(RoutingDecision {
            selected_model: "m".to_string(),
            confidence: 0.1,
            router_name: "mock".to_string(),
            fallback_used: false,
            reason: "low".to_string(),
        }));

        // "needle" is threshold-gated: a low-confidence decision errors.
        let gated = stack.threshold_wrap("needle", low_confidence.clone());
        assert!(gated.route(&RoutingRequest::new("x")).await.is_err());

        // "static" is not: the same low-confidence decision passes through.
        let ungated = stack.threshold_wrap("static", low_confidence);
        assert!(ungated.route(&RoutingRequest::new("x")).await.is_ok());
    }

    #[test]
    #[serial]
    fn router_stack_escalation_tier_none_when_primary_is_not_needle() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let config = Config::default();
        let stack = RouterStackBuilder::new(&config, &[]);
        let result = stack.escalation_tier("jev").expect("no error");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert!(result.is_none());
    }

    #[test]
    #[serial]
    fn router_stack_escalation_tier_none_when_escalate_is_off() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let config = Config {
            router_escalate: "off".to_string(),
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let result = stack.escalation_tier("needle").expect("no error");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert!(result.is_none());
    }

    #[test]
    #[serial]
    fn router_stack_escalation_tier_none_under_local_only() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let config = Config {
            local_only: true,
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let result = stack.escalation_tier("needle").expect("no error");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert!(result.is_none());
    }

    #[test]
    #[serial]
    fn router_stack_escalation_tier_none_without_credential() {
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        let config = Config::default();
        let stack = RouterStackBuilder::new(&config, &[]);
        assert!(stack.escalation_tier("needle").expect("no error").is_none());
    }

    #[tokio::test]
    #[serial]
    async fn router_stack_escalation_tier_some_when_all_conditions_met() {
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "test-escalation-key") };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_body("cheap-a", 0.9)))
            .mount(&server)
            .await;

        let config = Config {
            jev_url: Some(server.uri()),
            router_fallback: "static".to_string(),
            model: "cheap-a".to_string(),
            ..Config::default()
        };
        let registry = [("cheap-a".to_string(), caps(true))];
        let stack = RouterStackBuilder::new(&config, &registry);
        let escalation = stack
            .escalation_tier("needle")
            .expect("no error")
            .expect("escalation tier present");
        let decision = escalation
            .route(&RoutingRequest::new("x"))
            .await
            .expect("routes");
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
        assert_eq!(decision.router_name, "jev");
    }

    #[test]
    fn router_stack_fallback_chain_returns_primary_unwrapped_when_names_match() {
        let config = Config {
            router: "static".to_string(),
            router_fallback: "static".to_string(),
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let primary: Arc<dyn DecisionRouter> = Arc::new(MockRouter::selecting("m"));
        let result = stack
            .fallback_chain("static", primary.clone())
            .expect("builds");
        // Same object identity: no FallbackRouter wrap was introduced.
        assert!(Arc::ptr_eq(&primary, &result));
    }

    #[test]
    fn router_stack_fallback_chain_wraps_when_names_differ() {
        let config = Config {
            router: "http".to_string(),
            router_fallback: "static".to_string(),
            ..Config::default()
        };
        let stack = RouterStackBuilder::new(&config, &[]);
        let primary: Arc<dyn DecisionRouter> = Arc::new(MockRouter::selecting("m"));
        let result = stack
            .fallback_chain("http", primary.clone())
            .expect("builds");
        assert!(!Arc::ptr_eq(&primary, &result));
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
            ForgeError::Router(msg) => {
                assert!(msg.contains("ghost"), "got: {msg}");
                // Laya answered, so this is not a "laya is not the default
                // any more" situation — no legacy hint here.
                assert!(!msg.contains("no longer the default"), "got: {msg}");
            }
            other => panic!("expected router error, got {other:?}"),
        }
    }

    /// The reported failure's other half: the laya endpoint 500s (or is
    /// simply not there), the fallback silently absorbs it, and the only
    /// thing the user ever sees is this warn — so it has to say that laya
    /// stopped being the default and name both ways forward.
    #[tokio::test]
    async fn laya_transport_errors_carry_the_legacy_default_hint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let router = LayaRouter::new(
            Some(server.uri()),
            None,
            Duration::from_secs(5),
            std::collections::HashMap::new(),
        )
        .expect("construct");
        let request = RoutingRequest::new("x");
        let err = router.route(&request).await.expect_err("500 fails");
        let ForgeError::Router(msg) = err else {
            panic!("expected a router error");
        };
        assert!(msg.contains("500"), "got: {msg}");
        assert!(msg.contains("no longer the default"), "got: {msg}");
        assert!(msg.contains("forge router serve"), "got: {msg}");

        // Same hint when nothing is listening at all.
        let dead = LayaRouter::new(
            Some("http://127.0.0.1:9".to_string()),
            None,
            Duration::from_millis(200),
            std::collections::HashMap::new(),
        )
        .expect("construct");
        let err = dead.route(&request).await.expect_err("unreachable fails");
        let ForgeError::Router(msg) = err else {
            panic!("expected a router error");
        };
        assert!(msg.contains("no longer the default"), "got: {msg}");
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

    #[tokio::test]
    async fn laya_router_builder_produces_a_working_router() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/decide"))
            .respond_with(ResponseTemplate::new(200).set_body_json(laya_body("cheap-a", 0.95)))
            .mount(&server)
            .await;

        let criteria = [("cheap-a".to_string(), "cheap general model".to_string())]
            .into_iter()
            .collect();
        let router = LayaRouter::builder()
            .url(Some(format!("{}/decide", server.uri())))
            .timeout(Duration::from_secs(5))
            .criteria(criteria)
            .build()
            .expect("builder constructs");

        let decision = router
            .route(&RoutingRequest::new("fix the bug"))
            .await
            .expect("routes");
        assert_eq!(decision.selected_model, "cheap-a");
        assert_eq!(decision.router_name, "laya");
    }
}
