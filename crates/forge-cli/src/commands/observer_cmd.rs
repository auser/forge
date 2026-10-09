//! Observer wiring and inspection. Status never starts a provider or a worker.
use std::path::Path;
use std::sync::Arc;

use forge_context::{BudgetLimits, FsObserverQueue, ObserverQueue};
use forge_core::ForgeError;
use forge_runtime::observer::{ObserverPrices, ObserverSupervisor};
use forge_session::JsonlSessionStore;

use super::Context;

fn micro_usd(value: f64, round_up: bool) -> Result<u64, ForgeError> {
    ObserverPrices::micro_usd(value, round_up)
}

fn budget(config: &forge_config::Config) -> Result<BudgetLimits, ForgeError> {
    Ok(BudgetLimits {
        session_micro_usd: micro_usd(config.observer.session_usd, false)?,
        daily_micro_usd: micro_usd(config.observer.daily_usd, false)?,
    })
}

pub(super) fn build(
    config: &forge_config::Config,
    root: &Path,
    sessions: Arc<JsonlSessionStore>,
) -> Result<Option<Arc<ObserverSupervisor>>, ForgeError> {
    if !config.observer.enabled {
        return Ok(None);
    }
    let (input, output) = forge_providers::observer_prices_from_config(config)
        .ok_or_else(|| ForgeError::config("observer requires known input and output prices"))?;
    let provider = forge_providers::observer_model_from_config(config, root)?;
    let context_root = root.join(".forge/context");
    let policy = forge_context::ObserverPolicy {
        observer_version: forge_context::OBSERVER_VERSION.into(),
        model: config
            .observer
            .model
            .clone()
            .ok_or_else(|| ForgeError::config("observer requires an explicit model"))?,
        prompt_version: forge_context::OBSERVER_PROMPT_VERSION.into(),
        limits: forge_context::ObserverLimits::default(),
    };
    let supervisor = Arc::new(ObserverSupervisor::new(
        provider,
        Arc::new(FsObserverQueue::new(&context_root)),
        Arc::new(forge_context::FsObservationStore::new(&context_root)),
        sessions,
        policy,
        budget(config)?,
        ObserverPrices {
            input_micro_usd_per_million: micro_usd(input, true)?,
            output_micro_usd_per_million: micro_usd(output, true)?,
        },
    )?);
    supervisor.start()?;
    Ok(Some(supervisor))
}

pub fn status(ctx: &Context) -> Result<(), ForgeError> {
    let root = ctx.project_root()?;
    let config = ctx.resolve_config()?.config;
    let queue = FsObserverQueue::new(root.join(".forge/context"));
    let status = queue
        .status()
        .map_err(|_| ForgeError::session("observer status unavailable"))?;
    let prices = forge_providers::observer_prices_from_config(&config);
    let mut report = serde_json::json!({
        "enabled": config.observer.enabled,
        "model": config.observer.model,
        "allow_remote": config.observer.allow_remote && !config.local_only,
        "local_only": config.local_only,
        "session_usd": config.observer.session_usd,
        "daily_usd": config.observer.daily_usd,
        "prices_known": prices.is_some(),
        "queue": status,
        "live_model_quality": "unqualified",
        "inspection_only": true,
    });
    forge_session::Redactor::new().redact_value(&mut report);
    if ctx.global.json {
        println!("{report}");
    } else {
        println!(
            "Observer (inspection only; no jobs started)\n{}",
            serde_json::to_string_pretty(&report)
                .map_err(|_| ForgeError::session("observer status unavailable"))?
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_rounds_down_but_prices_round_up() {
        assert_eq!(micro_usd(0.05, false).unwrap(), 50_000);
        assert_eq!(micro_usd(0.25, false).unwrap(), 250_000);
        assert_eq!(micro_usd(0.0000001, false).unwrap(), 0);
        assert_eq!(micro_usd(0.0000001, true).unwrap(), 1);
        assert_eq!(micro_usd(0.0, true).unwrap(), 0);
        for invalid in [-1.0, f64::NAN, f64::INFINITY, f64::MAX] {
            assert!(micro_usd(invalid, false).is_err());
            assert!(micro_usd(invalid, true).is_err());
        }
    }
}
