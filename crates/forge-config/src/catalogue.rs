//! The cached OpenRouter model catalogue: a local facts file (model ids,
//! context lengths, per-million-token prices) that cost resolution reads —
//! never the network. Fetching lives in `forge_providers::openrouter` and
//! happens only on `forge init` / `forge model refresh`; the routing hot
//! path only ever loads the cache this module writes.
//!
//! Staleness contract: a cache older than `catalogue_ttl_days` is still
//! used, with a warning naming its age — stale prices beat no prices, and a
//! routing decision must not block on a network call. No cache at all
//! (never fetched, corrupt, or `local_only`) is not an error: cost
//! resolution falls back to hand-typed config prices, exactly the behaviour
//! forge had before the catalogue existed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use forge_core::ForgeError;
use serde::{Deserialize, Serialize};

use crate::Config;

/// One brokered model's facts. Prices are per million tokens, converted at
/// fetch time from the API's per-token figures. Either price may be absent
/// when the broker did not report one; such an entry still informs
/// `max_context` but never cost ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogueModel {
    pub id: String,
    #[serde(default)]
    pub context_length: Option<usize>,
    #[serde(default)]
    pub cost_input_per_mtok: Option<f64>,
    #[serde(default)]
    pub cost_output_per_mtok: Option<f64>,
}

impl CatalogueModel {
    /// Both prices, when the broker reported both — cost resolution needs
    /// the pair, so a half-priced entry is unpriced here too.
    pub fn costs(&self) -> Option<(f64, f64)> {
        Some((self.cost_input_per_mtok?, self.cost_output_per_mtok?))
    }
}

/// The cached catalogue and its fetch timestamp (what the TTL is measured
/// against).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalogue {
    pub fetched_at: DateTime<Utc>,
    pub models: Vec<CatalogueModel>,
}

impl Catalogue {
    pub fn model(&self, id: &str) -> Option<&CatalogueModel> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// Where the cache lives: `$XDG_CACHE_HOME/forge/openrouter/models.json`,
/// or `~/.cache/forge/openrouter/models.json` when `XDG_CACHE_HOME` is
/// unset — the same resolution `forge router serve`'s adapter cache uses.
pub fn cache_path() -> PathBuf {
    let base = if let Some(dir) = std::env::var_os("XDG_CACHE_HOME")
        && !dir.is_empty()
    {
        PathBuf::from(dir)
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache")
    };
    base.join("forge").join("openrouter").join("models.json")
}

/// Write the cache, creating its directory. The fetch side
/// (`forge_providers::openrouter`) produces the [`Catalogue`]; this is the
/// one writer, so the file only ever holds fully-written JSON.
pub fn save(path: &Path, catalogue: &Catalogue) -> Result<(), ForgeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
    }
    let text = serde_json::to_string(catalogue)
        .map_err(|e| ForgeError::config(format!("serializing catalogue: {e}")))?;
    std::fs::write(path, text).map_err(ForgeError::Io)
}

/// A loaded cache with the two facts every consumer needs: how old it is,
/// and the TTL that age is judged against.
#[derive(Debug, Clone)]
pub struct CachedCatalogue {
    pub catalogue: Catalogue,
    pub age: Duration,
    pub ttl: Duration,
}

impl CachedCatalogue {
    pub fn is_stale(&self) -> bool {
        self.age > self.ttl
    }

    /// The warning a stale cache must surface, naming its age — or `None`
    /// when the cache is fresh.
    pub fn staleness_warning(&self) -> Option<String> {
        self.is_stale().then(|| {
            format!(
                "OpenRouter catalogue cache is {} old (ttl {}); using stale prices",
                format_age(self.age),
                format_age(self.ttl)
            )
        })
    }
}

/// Load the cache at `path`. A missing file is `None`, silently — never
/// fetched is the normal pre-`forge model refresh` state. A corrupt or
/// unreadable file is also `None` (with a warning): a broker or disk that
/// produced garbage must degrade cost resolution to config-only, never
/// fail a run.
pub fn load(path: &Path, ttl: Duration) -> Option<CachedCatalogue> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "catalogue cache unreadable; using config prices only");
            return None;
        }
    };
    let catalogue: Catalogue = match serde_json::from_str(&text) {
        Ok(catalogue) => catalogue,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "catalogue cache is malformed; using config prices only");
            return None;
        }
    };
    let age = (Utc::now() - catalogue.fetched_at)
        .to_std()
        .unwrap_or(Duration::ZERO);
    Some(CachedCatalogue {
        catalogue,
        age,
        ttl,
    })
}

/// The cache as the routing/budget hot path may see it: nothing under
/// `local_only` (the catalogue is pruned like every other network-derived
/// source), otherwise the cache at [`cache_path`] judged against
/// `config.catalogue_ttl_days`, with the stale warning logged here so no
/// caller can forget it.
pub fn load_for_routing(config: &Config) -> Option<CachedCatalogue> {
    if config.local_only {
        return None;
    }
    let cached = load(
        &cache_path(),
        Duration::from_secs(config.catalogue_ttl_days * 86_400),
    )?;
    if let Some(warning) = cached.staleness_warning() {
        tracing::warn!(%warning);
    }
    Some(cached)
}

/// Human age for the staleness warning: the largest unit that reads
/// plainly ("9 days", "3 hours", "12 minutes").
fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    let (n, unit) = if secs >= 86_400 {
        (secs / 86_400, "day")
    } else if secs >= 3_600 {
        (secs / 3_600, "hour")
    } else {
        (secs / 60, "minute")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue(ids: &[&str]) -> Catalogue {
        Catalogue {
            fetched_at: Utc::now(),
            models: ids
                .iter()
                .map(|id| CatalogueModel {
                    id: (*id).to_string(),
                    context_length: Some(200_000),
                    cost_input_per_mtok: Some(1.0),
                    cost_output_per_mtok: Some(2.0),
                })
                .collect(),
        }
    }

    #[test]
    fn save_then_load_round_trips_and_finds_models_by_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested").join("models.json");
        let saved = catalogue(&["a/b", "c"]);
        save(&path, &saved).expect("save");
        let loaded = load(&path, Duration::from_secs(7 * 86_400)).expect("load");
        assert_eq!(loaded.catalogue, saved);
        assert_eq!(loaded.catalogue.model("a/b").expect("found").id, "a/b");
        assert!(loaded.catalogue.model("nope").is_none());
        assert!(!loaded.is_stale());
        assert!(loaded.staleness_warning().is_none());
    }

    #[test]
    fn a_missing_cache_is_none_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(load(&tmp.path().join("models.json"), Duration::from_secs(1)).is_none());
    }

    #[test]
    fn a_malformed_cache_degrades_to_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("models.json");
        std::fs::write(&path, "{not json").expect("write");
        assert!(load(&path, Duration::from_secs(7 * 86_400)).is_none());
    }

    #[test]
    fn a_stale_cache_is_used_and_the_warning_names_its_age() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("models.json");
        let mut old = catalogue(&["a"]);
        old.fetched_at = Utc::now() - chrono::Duration::days(9);
        save(&path, &old).expect("save");

        let loaded = load(&path, Duration::from_secs(7 * 86_400)).expect("stale cache still loads");
        assert!(loaded.is_stale());
        let warning = loaded.staleness_warning().expect("stale warns");
        assert!(warning.contains("9 days old"), "warning: {warning}");
        assert!(warning.contains("ttl 7 days"), "warning: {warning}");
    }

    #[test]
    fn local_only_sees_no_catalogue() {
        let config = Config {
            local_only: true,
            ..Config::default()
        };
        assert!(load_for_routing(&config).is_none());
    }

    #[test]
    fn ages_format_as_the_largest_plain_unit() {
        assert_eq!(format_age(Duration::from_secs(9 * 86_400)), "9 days");
        assert_eq!(format_age(Duration::from_secs(86_400)), "1 day");
        assert_eq!(format_age(Duration::from_secs(3 * 3_600)), "3 hours");
        assert_eq!(format_age(Duration::from_secs(12 * 60)), "12 minutes");
    }
}
