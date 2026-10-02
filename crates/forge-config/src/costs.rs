//! Cost resolution: the one place "what does this model cost" is answered.
//!
//! Precedence, most specific first:
//!
//! 1. an explicit `cost_*_per_mtok` in the `[models.<name>]` entry — an
//!    operator who typed a number meant it, and the catalogue must not
//!    overrule it;
//! 2. the cached OpenRouter catalogue, matched on model id;
//! 3. nothing — the model is **unpriced**, and must never be treated as
//!    free. Treating unknown as 0.0 is how `cheapest` picks the most
//!    expensive model in the pool by accident.
//!
//! Consumers: `CheapestRouter` ranks only the priced entries, and the
//! budget accounting in `forge-runtime` prices completions through the
//! same book, so routing and spend agree on every price.

use std::collections::BTreeMap;

use crate::ModelEntry;
use crate::catalogue::Catalogue;

/// Where a model's price came from. `Unpriced` is a fact, not a zero:
/// callers ranking on price must exclude it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    /// An explicit `cost_*_per_mtok` in the model's config entry.
    Config,
    /// The cached OpenRouter catalogue, matched on model id.
    Catalogue,
    /// Neither source prices this model.
    Unpriced,
}

/// Per-model prices with provenance, built once from the configured
/// entries plus the cached catalogue (when one is loaded).
#[derive(Debug, Clone, Default)]
pub struct CostBook {
    prices: BTreeMap<String, ((f64, f64), PriceSource)>,
}

impl CostBook {
    pub fn new(entries: &BTreeMap<String, ModelEntry>, catalogue: Option<&Catalogue>) -> Self {
        let mut prices = BTreeMap::new();
        for (name, entry) in entries {
            if let Some(costs) = entry.costs() {
                prices.insert(name.clone(), (costs, PriceSource::Config));
            }
        }
        // The catalogue prices whatever config left unpriced — including
        // ids with no `[models]` entry at all (e.g. a `--model` flag naming
        // one). This is a *price* lookup, not the candidate pool: routers
        // still only rank operator-declared models, so filling here can
        // never make a brokered model routable that wasn't already.
        if let Some(catalogue) = catalogue {
            for model in &catalogue.models {
                if prices.contains_key(&model.id) {
                    continue;
                }
                if let Some(costs) = model.costs() {
                    prices.insert(model.id.clone(), (costs, PriceSource::Catalogue));
                }
            }
        }
        Self { prices }
    }

    /// The price for `model`, or `None` when unpriced — never 0.0
    /// pretending to be free.
    pub fn price(&self, model: &str) -> Option<(f64, f64)> {
        self.prices.get(model).map(|(costs, _)| *costs)
    }

    pub fn source(&self, model: &str) -> PriceSource {
        self.prices
            .get(model)
            .map(|(_, source)| *source)
            .unwrap_or(PriceSource::Unpriced)
    }

    /// Every priced entry — the pool `CheapestRouter` may rank on price.
    pub fn priced(&self) -> impl Iterator<Item = (&str, (f64, f64))> {
        self.prices
            .iter()
            .map(|(name, (costs, _))| (name.as_str(), *costs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::CatalogueModel;

    fn entry(input: Option<f64>, output: Option<f64>) -> ModelEntry {
        ModelEntry {
            cost_input_per_mtok: input,
            cost_output_per_mtok: output,
            ..ModelEntry::default()
        }
    }

    fn catalogue_with(id: &str, input: f64, output: f64) -> Catalogue {
        Catalogue {
            fetched_at: chrono::Utc::now(),
            models: vec![CatalogueModel {
                id: id.to_string(),
                context_length: Some(100_000),
                cost_input_per_mtok: Some(input),
                cost_output_per_mtok: Some(output),
            }],
        }
    }

    #[test]
    fn a_config_price_beats_the_catalogue_for_the_same_id() {
        let entries = BTreeMap::from([("m".to_string(), entry(Some(9.0), Some(9.0)))]);
        let catalogue = catalogue_with("m", 1.0, 1.0);
        let book = CostBook::new(&entries, Some(&catalogue));
        assert_eq!(book.price("m"), Some((9.0, 9.0)));
        assert_eq!(book.source("m"), PriceSource::Config);
    }

    #[test]
    fn the_catalogue_prices_what_config_leaves_unset() {
        let entries = BTreeMap::from([("m".to_string(), entry(None, None))]);
        let catalogue = catalogue_with("m", 1.0, 2.0);
        let book = CostBook::new(&entries, Some(&catalogue));
        assert_eq!(book.price("m"), Some((1.0, 2.0)));
        assert_eq!(book.source("m"), PriceSource::Catalogue);
    }

    #[test]
    fn no_price_anywhere_is_unpriced_never_zero() {
        let entries = BTreeMap::from([("m".to_string(), entry(None, None))]);
        let book = CostBook::new(&entries, None);
        assert_eq!(book.price("m"), None);
        assert_eq!(book.source("m"), PriceSource::Unpriced);
        assert_eq!(book.priced().count(), 0);
        // A catalogue that does not know the id is no price either.
        let catalogue = catalogue_with("someone-else", 1.0, 1.0);
        let book = CostBook::new(&entries, Some(&catalogue));
        assert_eq!(book.price("m"), None);
    }

    #[test]
    fn a_half_priced_catalogue_entry_is_unpriced() {
        let entries = BTreeMap::from([("m".to_string(), entry(None, None))]);
        let mut catalogue = catalogue_with("m", 1.0, 2.0);
        catalogue.models[0].cost_output_per_mtok = None;
        let book = CostBook::new(&entries, Some(&catalogue));
        assert_eq!(book.price("m"), None);
    }

    #[test]
    fn an_explicitly_free_model_is_priced_at_zero() {
        // Local models write 0.0 on purpose: known-free, not unknown.
        let entries = BTreeMap::from([("local".to_string(), entry(Some(0.0), Some(0.0)))]);
        let book = CostBook::new(&entries, None);
        assert_eq!(book.price("local"), Some((0.0, 0.0)));
        assert_eq!(book.source("local"), PriceSource::Config);
    }
}
