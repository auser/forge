//! Spend accounting for `[budget]` ceilings: pricing one completion, and
//! accumulating a session's (and the day's) spend from the decision records
//! the loop itself writes.
//!
//! The decision log is the source of truth — a run re-derives its starting
//! position by scanning it, so a resumed session is billed for the turns it
//! already ran, and the daily ceiling sees every process that wrote today.
//! The tracker only adds in-loop accrual on top of that scan.

use std::path::Path;

use forge_config::BudgetConfig;
use forge_core::Usage;
use forge_session::{SpendTotals, scan_spend_today};

/// USD cost of one completion at per-million-token prices
/// (`cost_input_per_mtok` / `cost_output_per_mtok`).
///
/// `None` when there is no usage to price or no price to price it with — a
/// missing price must surface as *unknown*, never as 0.0, which would read
/// as "known free". A real (0.0, 0.0) price — a local model — prices as
/// Some(0.0), which is free for true.
pub fn completion_cost(usage: Option<Usage>, costs: Option<(f64, f64)>) -> Option<f64> {
    let usage = usage?;
    let (input_per_mtok, output_per_mtok) = costs?;
    Some(
        f64::from(usage.prompt_tokens) / 1e6 * input_per_mtok
            + f64::from(usage.completion_tokens) / 1e6 * output_per_mtok,
    )
}

/// One ceiling the current spend trips, with what the error and the prompt
/// need: the ceiling's name, its configured limit, and the current spend.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetTrip {
    pub ceiling: &'static str,
    pub limit: String,
    pub current: String,
}

impl BudgetTrip {
    /// e.g. `session_usd ceiling $5.00 exceeded: $5.42 spent`.
    pub fn describe(&self) -> String {
        format!(
            "{} ceiling {} exceeded: {} spent",
            self.ceiling, self.limit, self.current
        )
    }
}

/// Session and daily spend so far: scanned from the decision log at run
/// start, then accrued in memory as the loop records completions.
pub struct SpendTracker {
    session: SpendTotals,
    daily: SpendTotals,
}

impl SpendTracker {
    /// Seed from the `Complete` records already in the decision logs under
    /// `root` — `session_id` scopes the session totals, today (UTC) the
    /// daily ones. Malformed lines and unreadable files are skipped inside
    /// [`scan_spend`], so a damaged log degrades the ceiling instead of
    /// failing the run.
    pub fn scan(root: &Path, session_id: &str) -> Self {
        let (session, daily) = scan_spend_today(root, Some(session_id));
        Self { session, daily }
    }

    /// No scanning at all when the config sets no ceiling: a budget-free
    /// run must not pay for one read of every decision log on disk.
    pub fn scan_if_budgeted(root: &Path, session_id: &str, budget: &BudgetConfig) -> Self {
        if budget.has_ceilings() {
            Self::scan(root, session_id)
        } else {
            Self {
                session: SpendTotals::default(),
                daily: SpendTotals::default(),
            }
        }
    }

    /// Accrue one recorded completion. Unknown usage or cost adds nothing,
    /// which is exactly what keeps a zero-priced (local) model off the USD
    /// ceilings.
    pub fn record(&mut self, usage: Option<Usage>, cost_usd: Option<f64>) {
        if let Some(usage) = usage {
            self.session.input_tokens += u64::from(usage.prompt_tokens);
            self.session.output_tokens += u64::from(usage.completion_tokens);
            self.daily.input_tokens += u64::from(usage.prompt_tokens);
            self.daily.output_tokens += u64::from(usage.completion_tokens);
        }
        if let Some(cost) = cost_usd {
            self.session.cost_usd += cost;
            self.daily.cost_usd += cost;
        }
    }

    /// The first configured ceiling the current spend has reached. Checked
    /// *before* a model call: spend at the ceiling means the next call is
    /// the one that would exceed it.
    pub fn tripped(&self, budget: &BudgetConfig) -> Option<BudgetTrip> {
        if let Some(limit) = budget.session_tokens
            && self.session.total_tokens() >= limit
        {
            return Some(BudgetTrip {
                ceiling: "session_tokens",
                limit: format!("{limit} tokens"),
                current: format!("{} tokens", self.session.total_tokens()),
            });
        }
        if let Some(limit) = budget.session_usd
            && self.session.cost_usd >= limit
        {
            return Some(BudgetTrip {
                ceiling: "session_usd",
                limit: format!("${limit:.2}"),
                current: format!("${:.4}", self.session.cost_usd),
            });
        }
        if let Some(limit) = budget.daily_usd
            && self.daily.cost_usd >= limit
        {
            return Some(BudgetTrip {
                ceiling: "daily_usd",
                limit: format!("${limit:.2}"),
                current: format!("${:.4}", self.daily.cost_usd),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u32, completion: u32) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
        }
    }

    #[test]
    fn cost_is_priced_per_million_tokens() {
        // 1000 in + 500 out at $1.25/$10.00 per Mtok.
        let cost = completion_cost(Some(usage(1_000, 500)), Some((1.25, 10.0))).expect("priced");
        assert!((cost - (0.00125 + 0.005)).abs() < 1e-12, "{cost}");
    }

    #[test]
    fn cost_is_none_without_usage_or_price_never_zero_pretending_free() {
        assert_eq!(completion_cost(None, Some((1.0, 1.0))), None);
        assert_eq!(completion_cost(Some(usage(1, 1)), None), None);
        // A genuinely free model prices as Some(0.0) — that is known-free,
        // not missing data.
        assert_eq!(
            completion_cost(Some(usage(1, 1)), Some((0.0, 0.0))),
            Some(0.0)
        );
    }

    #[test]
    fn no_ceilings_never_trips_and_skips_the_scan() {
        let budget = BudgetConfig::default();
        let tracker = SpendTracker::scan_if_budgeted(Path::new("/nonexistent"), "s1", &budget);
        assert_eq!(tracker.tripped(&budget), None);
    }

    #[test]
    fn each_ceiling_trips_at_its_own_limit() {
        let mut budget = BudgetConfig {
            session_tokens: Some(150),
            session_usd: Some(0.01),
            daily_usd: Some(0.02),
            ..BudgetConfig::default()
        };
        let mut tracker = SpendTracker {
            session: SpendTotals::default(),
            daily: SpendTotals::default(),
        };
        tracker.record(Some(usage(100, 50)), Some(0.01));

        let trip = tracker
            .tripped(&budget)
            .expect("session_tokens trips first");
        assert_eq!(trip.ceiling, "session_tokens");
        assert_eq!(
            trip.describe(),
            "session_tokens ceiling 150 tokens exceeded: 150 tokens spent"
        );

        budget.session_tokens = None;
        let trip = tracker.tripped(&budget).expect("session_usd trips");
        assert_eq!(trip.ceiling, "session_usd");
        assert!(trip.describe().contains("$0.0100"), "{}", trip.describe());

        budget.session_usd = None;
        assert_eq!(tracker.tripped(&budget), None, "daily 0.01 < 0.02");
        tracker.record(Some(usage(10, 10)), Some(0.01));
        let trip = tracker.tripped(&budget).expect("daily_usd trips");
        assert_eq!(trip.ceiling, "daily_usd");
    }

    #[test]
    fn zero_cost_models_stay_off_the_usd_ceilings() {
        let budget = BudgetConfig {
            session_usd: Some(0.0001),
            daily_usd: Some(0.0001),
            ..BudgetConfig::default()
        };
        let mut tracker = SpendTracker {
            session: SpendTotals::default(),
            daily: SpendTotals::default(),
        };
        // A local model: usage reported, cost genuinely 0.0 — and a model
        // with no price at all (None) — neither may trip a USD ceiling.
        tracker.record(Some(usage(1_000_000, 1_000_000)), Some(0.0));
        tracker.record(Some(usage(1_000_000, 1_000_000)), None);
        assert_eq!(tracker.tripped(&budget), None);
    }

    #[test]
    fn the_scan_counts_prior_runs_of_the_session_and_today() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = std::sync::Arc::new(forge_session::DecisionLog::new(tmp.path().to_path_buf()));
        let handle = forge_session::DecisionLog::handle(&log, "s1");
        handle.record_usage(1, "gpt-5", Some(usage(100, 50)), Some(0.01), 1);
        let other = forge_session::DecisionLog::handle(&log, "s2");
        other.record_usage(1, "gpt-5", Some(usage(10, 10)), Some(0.02), 1);

        let tracker = SpendTracker::scan(tmp.path(), "s1");
        assert_eq!(tracker.session.total_tokens(), 150);
        assert!((tracker.session.cost_usd - 0.01).abs() < 1e-12);
        // Daily spans sessions: both completions were today.
        assert!((tracker.daily.cost_usd - 0.03).abs() < 1e-12);
    }
}
