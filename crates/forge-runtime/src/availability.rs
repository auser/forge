use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use forge_core::{ForgeError, ProviderFailureKind};
use serde::Serialize;

/// Time seam for deterministic cooldown tests. Values are process-relative;
/// Forge never turns them into a claimed provider reset timestamp.
pub trait AvailabilityClock: Send + Sync {
    fn now_millis(&self) -> u64;
}

struct MonotonicClock {
    origin: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl AvailabilityClock for MonotonicClock {
    fn now_millis(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    Available,
    RateLimited,
    AuthenticationFailed,
    TransientFailure,
    InvalidRequest,
    EntitlementFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AvailabilitySnapshot {
    pub model: String,
    pub eligible: bool,
    pub state: AvailabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Observation {
    state: AvailabilityState,
    retry_at_millis: Option<u64>,
}

struct Inner {
    clock: Arc<dyn AvailabilityClock>,
    observations: Mutex<HashMap<String, Observation>>,
}

/// Process-local evidence about provider health. Only typed provider failures
/// enter the table; generic diagnostic strings never affect routing.
#[derive(Clone)]
pub struct ProviderAvailability {
    inner: Arc<Inner>,
}

impl Default for ProviderAvailability {
    fn default() -> Self {
        Self::with_clock(Arc::new(MonotonicClock::default()))
    }
}

impl ProviderAvailability {
    pub fn with_clock(clock: Arc<dyn AvailabilityClock>) -> Self {
        Self {
            inner: Arc::new(Inner {
                clock,
                observations: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn observe_failure(&self, model: &str, error: &ForgeError) {
        let observation = match error {
            ForgeError::ProviderRateLimited {
                retry_after_seconds,
                ..
            } => Observation {
                state: AvailabilityState::RateLimited,
                retry_at_millis: retry_after_seconds.map(|seconds| {
                    self.inner
                        .clock
                        .now_millis()
                        .saturating_add(seconds.saturating_mul(1_000))
                }),
            },
            ForgeError::ProviderFailure { kind, .. } => Observation {
                state: match kind {
                    ProviderFailureKind::Authentication => AvailabilityState::AuthenticationFailed,
                    ProviderFailureKind::Endpoint | ProviderFailureKind::Transient => {
                        AvailabilityState::TransientFailure
                    }
                    ProviderFailureKind::Capability
                    | ProviderFailureKind::ResponseShape
                    | ProviderFailureKind::InvalidRequest => AvailabilityState::InvalidRequest,
                    ProviderFailureKind::Entitlement => AvailabilityState::EntitlementFailed,
                },
                retry_at_millis: None,
            },
            _ => return,
        };
        self.inner
            .observations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(model.to_string(), observation);
    }

    pub fn observe_success(&self, model: &str) {
        self.inner
            .observations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(model);
    }

    pub fn snapshot(&self, model: &str) -> AvailabilitySnapshot {
        let now = self.inner.clock.now_millis();
        let mut observations = self
            .inner
            .observations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(observation) = observations.get(model).copied() else {
            return available(model);
        };
        if observation
            .retry_at_millis
            .is_some_and(|retry_at| retry_at <= now)
        {
            observations.remove(model);
            return available(model);
        }
        let retry_after_seconds = observation.retry_at_millis.map(|retry_at| {
            retry_at
                .saturating_sub(now)
                .saturating_add(999)
                .saturating_div(1_000)
        });
        AvailabilitySnapshot {
            model: model.to_string(),
            eligible: match observation.state {
                AvailabilityState::RateLimited => retry_after_seconds.is_none(),
                AvailabilityState::AuthenticationFailed | AvailabilityState::EntitlementFailed => {
                    false
                }
                AvailabilityState::Available
                | AvailabilityState::TransientFailure
                | AvailabilityState::InvalidRequest => true,
            },
            state: observation.state,
            retry_after_seconds,
        }
    }

    pub fn is_eligible(&self, model: &str) -> bool {
        self.snapshot(model).eligible
    }
}

fn available(model: &str) -> AvailabilitySnapshot {
    AvailabilitySnapshot {
        model: model.to_string(),
        eligible: true,
        state: AvailabilityState::Available,
        retry_after_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[derive(Default)]
    struct TestClock(AtomicU64);

    impl TestClock {
        fn advance(&self, millis: u64) {
            self.0.fetch_add(millis, Ordering::SeqCst);
        }
    }

    impl AvailabilityClock for TestClock {
        fn now_millis(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn observed_cooldown_excludes_then_expires_on_the_injected_clock() {
        let clock = Arc::new(TestClock::default());
        let table = ProviderAvailability::with_clock(clock.clone());
        table.observe_failure(
            "limited",
            &ForgeError::provider_rate_limited("limited", Some(3)),
        );
        assert_eq!(
            table.snapshot("limited"),
            AvailabilitySnapshot {
                model: "limited".to_string(),
                eligible: false,
                state: AvailabilityState::RateLimited,
                retry_after_seconds: Some(3),
            }
        );
        clock.advance(3_000);
        assert_eq!(table.snapshot("limited"), available("limited"));
    }

    #[test]
    fn generic_errors_never_change_routing_eligibility() {
        let table = ProviderAvailability::default();
        table.observe_failure("model", &ForgeError::provider("opaque failure"));
        assert_eq!(table.snapshot("model"), available("model"));
    }

    #[test]
    fn pins_can_inspect_auth_failure_while_routing_sees_it_as_ineligible() {
        let table = ProviderAvailability::default();
        table.observe_failure(
            "expired",
            &ForgeError::provider_failure(
                "expired",
                ProviderFailureKind::Authentication,
                Some(401),
                "expired",
            ),
        );
        let snapshot = table.snapshot("expired");
        assert!(!snapshot.eligible);
        assert_eq!(snapshot.state, AvailabilityState::AuthenticationFailed);
    }

    #[test]
    fn contract_failures_keep_their_routing_policy_classes() {
        for (kind, expected) in [
            (
                ProviderFailureKind::Endpoint,
                AvailabilityState::TransientFailure,
            ),
            (
                ProviderFailureKind::Capability,
                AvailabilityState::InvalidRequest,
            ),
            (
                ProviderFailureKind::ResponseShape,
                AvailabilityState::InvalidRequest,
            ),
        ] {
            let table = ProviderAvailability::default();
            table.observe_failure(
                "model",
                &ForgeError::provider_failure("model", kind, None, "contract failure"),
            );
            assert_eq!(table.snapshot("model").state, expected);
        }
    }
}
