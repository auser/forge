//! Where a run stands — the typed discriminant adapters classify by.
//!
//! Before this existed, ACP decided a turn's stop reason with
//! `message.contains("cancelled")` and MCP passed `&'static str` status
//! labels between its own functions. Both are now derived from types: a
//! `ForgeError` variant or an [`EventKind`]. There is deliberately no
//! classify-from-message constructor — that is the thing being replaced, and
//! keeping one "for the cases without a type" is how it comes back.

use crate::error::ForgeError;
use crate::events::{Event, EventKind};

/// Where a run stands: in flight, parked, or over.
///
/// The five wire-visible spellings (`as_str`) are the ones forge's adapters
/// have always used; `AwaitingApproval` is the sixth and names something
/// they previously lumped in with failure — see its docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// The loop is working.
    Running,
    /// The loop is parked on an approval request that nobody has answered
    /// yet. Answerable: deliver input and it continues.
    WaitingForApproval,
    /// The loop produced a final answer.
    Completed,
    /// Cancelled — by `forge cancel`, the in-process token, or the
    /// cross-process marker file.
    Cancelled,
    /// The loop *stopped* because a risky operation needed approval and no
    /// answer could arrive (no terminal, closed input channel). Distinct
    /// from [`Failed`](Self::Failed): nothing crashed, the work simply did
    /// not happen — which is why ACP reports it as a refusal rather than an
    /// internal error.
    AwaitingApproval,
    /// Anything else that ended the run: provider error, turn budget,
    /// tool dispatch failure.
    Failed,
}

impl RunState {
    /// True once no further events can arrive for the run.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::WaitingForApproval)
    }

    /// Stable snake_case name, as adapters put it on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingForApproval => "waiting_for_approval",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Failed => "failed",
        }
    }

    /// Classify a run's own error.
    pub fn of_error(error: &ForgeError) -> Self {
        match error {
            ForgeError::Cancelled(_) => Self::Cancelled,
            ForgeError::ApprovalRequired { .. } => Self::AwaitingApproval,
            _ => Self::Failed,
        }
    }

    /// Classify a finished run's result.
    pub fn of_result<T>(result: &Result<T, ForgeError>) -> Self {
        match result {
            Ok(_) => Self::Completed,
            Err(error) => Self::of_error(error),
        }
    }

    /// Classify a run from its stored events — the only source available
    /// for a run this process does not own (`forge run` elsewhere, or an
    /// evicted bookkeeping entry).
    ///
    /// A run whose last event is an approval request is *parked*, not
    /// running: it needs input, not patience.
    pub fn of_events(events: &[Event]) -> Self {
        match events.last().map(|e| &e.kind) {
            Some(EventKind::Completed { .. }) => Self::Completed,
            Some(EventKind::Cancelled { .. }) => Self::Cancelled,
            Some(EventKind::Error { .. }) => Self::Failed,
            Some(EventKind::ApprovalRequested { .. }) => Self::WaitingForApproval,
            _ => Self::Running,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::RiskLevel;

    #[test]
    fn errors_classify_by_variant_not_by_message() {
        assert_eq!(
            RunState::of_error(&ForgeError::cancelled("at a turn checkpoint")),
            RunState::Cancelled
        );
        assert_eq!(
            RunState::of_error(&ForgeError::ApprovalRequired {
                description: "rm -rf build".into(),
                risk: RiskLevel::Destructive,
            }),
            RunState::AwaitingApproval
        );
        // A message that merely mentions cancellation is still a failure
        // when the variant says so — that is the whole point.
        assert_eq!(
            RunState::of_error(&ForgeError::provider("the upstream cancelled our stream")),
            RunState::Failed
        );
    }

    #[test]
    fn results_classify_ok_as_completed() {
        let ok: Result<(), ForgeError> = Ok(());
        assert_eq!(RunState::of_result(&ok), RunState::Completed);
        let err: Result<(), ForgeError> = Err(ForgeError::agent("budget exhausted"));
        assert_eq!(RunState::of_result(&err), RunState::Failed);
    }

    #[test]
    fn events_classify_by_the_last_one() {
        let event = |kind| vec![Event::new("r", "s", kind)];
        assert_eq!(RunState::of_events(&[]), RunState::Running);
        assert_eq!(
            RunState::of_events(&event(EventKind::RunStarted {
                provider: "p".into(),
                model: "m".into(),
                prompt: "x".into(),
            })),
            RunState::Running
        );
        assert_eq!(
            RunState::of_events(&event(EventKind::ApprovalRequested {
                command: "rm".into(),
                risk: RiskLevel::Destructive,
            })),
            RunState::WaitingForApproval
        );
        assert_eq!(
            RunState::of_events(&event(EventKind::Completed {
                summary: "done".into()
            })),
            RunState::Completed
        );
        assert_eq!(
            RunState::of_events(&event(EventKind::Cancelled {
                reason: "user".into()
            })),
            RunState::Cancelled
        );
        assert_eq!(
            RunState::of_events(&event(EventKind::Error {
                message: "boom".into()
            })),
            RunState::Failed
        );
    }

    #[test]
    fn terminality_and_names_are_stable() {
        for (state, name, terminal) in [
            (RunState::Running, "running", false),
            (RunState::WaitingForApproval, "waiting_for_approval", false),
            (RunState::Completed, "completed", true),
            (RunState::Cancelled, "cancelled", true),
            (RunState::AwaitingApproval, "awaiting_approval", true),
            (RunState::Failed, "failed", true),
        ] {
            assert_eq!(state.as_str(), name);
            assert_eq!(state.is_terminal(), terminal, "{name}");
        }
    }
}
