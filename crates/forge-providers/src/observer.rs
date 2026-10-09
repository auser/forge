//! Observer model construction deliberately has no routing or model fallback.
use std::sync::Arc;

use forge_config::Config;
use forge_core::{ForgeError, ModelProvider};

fn observer_config(config: &Config) -> Result<Config, ForgeError> {
    if !config.observer.enabled {
        return Err(ForgeError::config("observer is disabled".to_string()));
    }
    let model = config
        .observer
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| ForgeError::config("observer requires an explicit model".to_string()))?;
    let mut observer = config.clone();
    observer.model = model.to_owned();
    observer.model_response_max_bytes = Some(256 * 1024);
    // Carry the restriction into the HTTP client, including redirect policy.
    // Remote consent can never override the application's local-only policy.
    observer.local_only = config.local_only || !config.observer.allow_remote;
    Ok(observer)
}

/// Build only the explicitly selected observer through the normal provider
/// factory. Errors are deliberately content-free; observer status must not
/// inherit endpoint credentials or other private provider diagnostics.
pub fn observer_model_from_config(
    config: &Config,
    project_root: &std::path::Path,
) -> Result<Arc<dyn ModelProvider>, ForgeError> {
    let observer = observer_config(config)?;
    crate::model_from_config(&observer, project_root).map_err(|_| {
        ForgeError::config("observer model unavailable under configured policy".to_string())
    })
}

/// Resolve both observer prices without network access. Unlike interactive
/// compatibility pricing, a half-specified explicit price is not known-free
/// on its missing side. Invalid or incomplete rates refuse observer dispatch.
pub fn observer_prices_from_config(config: &Config) -> Option<(f64, f64)> {
    let model = config.observer.model.as_deref()?;
    if let Some(entry) = config.model_entries().get(model)
        && (entry.cost_input_per_mtok.is_some() || entry.cost_output_per_mtok.is_some())
    {
        let prices = (entry.cost_input_per_mtok?, entry.cost_output_per_mtok?);
        return valid_prices(prices);
    }
    let catalogue = forge_config::catalogue::load_for_routing(config);
    let book = forge_config::CostBook::new(
        config.model_entries(),
        catalogue.as_ref().map(|cached| &cached.catalogue),
    );
    valid_prices(book.price(model)?)
}

fn valid_prices(prices: (f64, f64)) -> Option<(f64, f64)> {
    [prices.0, prices.1]
        .iter()
        .all(|price| price.is_finite() && *price >= 0.0)
        .then_some(prices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_partial_or_invalid_prices_never_become_free() {
        let mut config = Config {
            local_only: true,
            ..Config::default()
        };
        config.observer.model = Some("explicit-observer".into());
        assert_eq!(observer_prices_from_config(&config), None);
        for (input, output, expected) in [
            (Some(0.0), None, None),
            (None, Some(0.0), None),
            (Some(f64::NAN), Some(0.0), None),
            (Some(-1.0), Some(0.0), None),
            (Some(0.0), Some(f64::INFINITY), None),
            (Some(0.0), Some(0.0), Some((0.0, 0.0))),
            (Some(1.0), Some(2.0), Some((1.0, 2.0))),
        ] {
            config.models.insert(
                "explicit-observer".into(),
                forge_config::ModelEntry {
                    cost_input_per_mtok: input,
                    cost_output_per_mtok: output,
                    ..Default::default()
                },
            );
            assert_eq!(observer_prices_from_config(&config), expected);
        }
    }

    #[test]
    fn requires_opt_in_and_exact_model_without_active_model_fallback() {
        let mut config = Config::default();
        assert!(observer_config(&config).is_err());
        config.observer.enabled = true;
        assert!(observer_config(&config).is_err());
        config.observer.model = Some("separate-observer".into());
        let observer = observer_config(&config).unwrap();
        assert_eq!(observer.model, "separate-observer");
        assert!(observer.local_only);
    }

    #[test]
    fn remote_consent_never_overrides_local_only() {
        for local_only in [false, true] {
            for allow_remote in [false, true] {
                let mut config = Config {
                    local_only,
                    ..Config::default()
                };
                config.observer.enabled = true;
                config.observer.model = Some("explicit".into());
                config.observer.allow_remote = allow_remote;
                let observer = observer_config(&config).unwrap();
                assert_eq!(observer.local_only, local_only || !allow_remote);
            }
        }
    }

    #[test]
    fn remote_endpoint_is_refused_before_credentials_or_requests() {
        let mut config = Config::default();
        config.observer.enabled = true;
        config.observer.model = Some("hosted-observer".into());
        config.models.insert(
            "hosted-observer".into(),
            forge_config::ModelEntry {
                base_url: Some("https://example.invalid/v1".into()),
                key_env: Some("PRIVATE_OBSERVER_KEY_NAME".into()),
                ..Default::default()
            },
        );
        let error = observer_model_from_config(&config, std::path::Path::new("."))
            .err()
            .expect("remote observer needs separate consent");
        let error = error.to_string();
        assert!(!error.contains("PRIVATE_OBSERVER_KEY_NAME"));
        assert!(!error.contains("example.invalid"));
    }
}
