use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use crate::error::ForgeError;

/// The development phase whose work is being routed.
///
/// Routers may spend more on planning or review while preferring an
/// economical capable model for execution. `General` preserves the behavior
/// of callers that do not run Forge's development workflow.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPhase {
    #[default]
    General,
    Plan,
    Execute,
    Review,
}

impl RoutingPhase {
    /// The phase's name as it appears in config, events, and messages.
    /// One spelling, so a profile's tier key, a routing event, and an
    /// error message cannot drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Plan => "plan",
            Self::Execute => "execute",
            Self::Review => "review",
        }
    }
}

/// A capability a task requires from the selected model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Streaming,
    Tools,
    StructuredOutput,
    Vision,
    MinContext(usize),
}

impl Capability {
    pub fn satisfied_by(&self, caps: &crate::model::ModelCapabilities) -> bool {
        match self {
            Self::Streaming => caps.streaming,
            Self::Tools => caps.tools,
            Self::StructuredOutput => caps.structured_output,
            Self::Vision => caps.vision,
            Self::MinContext(min) => caps.max_context >= *min,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRequest {
    pub task: String,
    #[serde(default)]
    pub phase: RoutingPhase,
    #[serde(default)]
    pub required_capabilities: Vec<Capability>,
    #[serde(default)]
    pub candidates: Vec<String>,
}

impl RoutingRequest {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            phase: RoutingPhase::General,
            required_capabilities: Vec::new(),
            candidates: Vec::new(),
        }
    }

    pub fn for_phase(task: impl Into<String>, phase: RoutingPhase) -> Self {
        Self {
            phase,
            ..Self::new(task)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub selected_model: String,
    pub confidence: f64,
    pub router_name: String,
    pub fallback_used: bool,
    pub reason: String,
}

/// Chooses a model (and later workflows, skills, and budgets) for a task.
/// Implemented by static rules, mocks, and System One-compatible HTTP
/// services such as TypeSafe Jev, Kev, or local variants.
#[async_trait]
pub trait DecisionRouter: Send + Sync {
    async fn route(&self, task: &RoutingRequest) -> Result<RoutingDecision, ForgeError>;
}

/// Deserialize a value that may be written as one string or as a list of
/// strings, into a list either way.
///
/// `plan = "claude-opus"` and `plan = ["claude-opus", "gpt-5"]` are both
/// natural to write, and the difference between them is not a different
/// *kind* of policy but a different *size* of tier — so both normalize to a
/// list, and the tier's size decides how it is resolved (one model is a
/// decision; several are a menu).
fn one_or_many<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => Ok(vec![one]),
        OneOrMany::Many(many) => Ok(many),
    }
}

/// A phase→model policy: which models may serve which phase.
///
/// This is the declarative form of what Claude Code calls the `opusplan`
/// alias — a plan gets the high-fidelity model, the build gets the cheap
/// one — written the way a router needs it: by *phase*, not by vendor, so
/// the same policy works across Anthropic, OpenRouter, or a local model.
///
/// Each phase lists the models allowed for it. A phase with one model is a
/// decision (the router returns it directly, no inference); a phase with
/// several is a menu the decision router chooses from, so cost-aware
/// selection still applies *within* the tier. A phase with no models falls
/// back to the ordinary candidate pool.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    /// Models allowed while planning (read-only design passes).
    #[serde(default, deserialize_with = "one_or_many")]
    pub plan: Vec<String>,
    /// Models allowed while implementing an approved plan.
    #[serde(default, deserialize_with = "one_or_many")]
    pub execute: Vec<String>,
    /// Models allowed while independently reviewing a change.
    #[serde(default, deserialize_with = "one_or_many")]
    pub review: Vec<String>,
    /// Models allowed when no specific phase applies.
    #[serde(default, deserialize_with = "one_or_many")]
    pub general: Vec<String>,
}

impl ProfileConfig {
    /// The models this policy allows for `phase`.
    ///
    /// A phase with no models of its own falls back to the `general` tier,
    /// so a policy can be written as "plan with the strong model, and the
    /// rest with this other one" without naming every phase. When
    /// `general` is empty too, the phase is unconstrained — the ordinary
    /// candidate pool applies.
    pub fn tier_for(&self, phase: RoutingPhase) -> &[String] {
        let tier = match phase {
            RoutingPhase::Plan => &self.plan,
            RoutingPhase::Execute => &self.execute,
            RoutingPhase::Review => &self.review,
            RoutingPhase::General => &self.general,
        };
        if tier.is_empty() && phase != RoutingPhase::General {
            &self.general
        } else {
            tier
        }
    }

    /// Whether this policy constrains anything at all. A policy that names
    /// no models changes nothing, and is reported as inert rather than
    /// silently ignored.
    pub fn is_empty(&self) -> bool {
        self.plan.is_empty()
            && self.execute.is_empty()
            && self.review.is_empty()
            && self.general.is_empty()
    }

    /// Every model this policy names, in phase order, without duplicates.
    pub fn models(&self) -> Vec<&str> {
        let mut seen = BTreeSet::new();
        let mut models = Vec::new();
        for phase in [
            RoutingPhase::Plan,
            RoutingPhase::Execute,
            RoutingPhase::Review,
            RoutingPhase::General,
        ] {
            for model in self.tier_for(phase) {
                if seen.insert(model.as_str()) {
                    models.push(model.as_str());
                }
            }
        }
        models
    }
}

/// An active [`ProfileConfig`] together with the name it was selected by.
///
/// The name is carried so decisions can say *which* policy chose a model —
/// `planned with claude-opus (profile opusplan)` — rather than leaving the
/// user to infer it from the model name alone.
#[derive(Clone, Debug, PartialEq)]
pub struct PhasePolicy {
    pub name: String,
    pub profile: ProfileConfig,
}

impl PhasePolicy {
    pub fn new(name: impl Into<String>, profile: ProfileConfig) -> Self {
        Self {
            name: name.into(),
            profile,
        }
    }
}

/// A shared, swappable policy slot.
///
/// The policy is behind a lock rather than baked into the router so a
/// session can change it mid-run (the chat's `/profile` command, the
/// `--profile` flag) without rebuilding the router stack — rebuilding would
/// reload Laya's weights and re-run endpoint probing for what is, in the
/// end, a table of model names.
#[derive(Clone, Debug, Default)]
pub struct PhasePolicyHandle {
    inner: Arc<RwLock<Option<PhasePolicy>>>,
}

impl PhasePolicyHandle {
    pub fn new(policy: Option<PhasePolicy>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(policy)),
        }
    }

    /// The active policy, if any.
    pub fn get(&self) -> Option<PhasePolicy> {
        // A poisoned lock means some other thread panicked while holding
        // it. The data it guards is a plain table with no invariant that a
        // panic could have broken, so recovering the inner value is safer
        // than propagating a panic into an unrelated request.
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replace the active policy. `None` clears it, returning routing to the
    /// unconstrained behaviour.
    pub fn set(&self, policy: Option<PhasePolicy>) {
        *self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
    }

    /// Whether a policy is currently in force.
    pub fn is_active(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}
