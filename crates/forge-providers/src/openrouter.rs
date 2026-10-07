//! The OpenRouter *catalogue* client: model facts, not generation.
//!
//! `GET {base}/api/v1/models` answers "what models exist and what do they
//! cost" — ids, context lengths, per-token pricing — while generation
//! keeps going through the generic OpenAI-compatible provider
//! (`crate::model`). What this module fetches is written to the local cache
//! (`forge_config::catalogue`); routing never fetches, it only ever reads
//! that cache. Fetching happens here, and only from `forge model refresh`
//! / `forge init` — never on the routing hot path, and never under
//! `local_only`.

use std::path::PathBuf;
use std::time::Duration;

use forge_config::catalogue::{Catalogue, CatalogueModel};
use forge_core::ForgeError;

use crate::local_only::EgressPolicy;

/// The public catalogue endpoint's base (`{base}/api/v1/models`).
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai";

/// Endpoint override for tests and self-hosted brokers, mirroring
/// `FORGE_NEEDLE_WEIGHTS_BASE_URL` in `forge-needle`'s weights fetch.
pub const BASE_URL_ENV: &str = "FORGE_OPENROUTER_BASE_URL";

/// The base URL fetches go to: [`BASE_URL_ENV`] when set, else
/// [`DEFAULT_BASE_URL`].
pub fn base_url() -> String {
    std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

/// Fetch the catalogue. The whole response failing (transport, HTTP
/// status, a body with no `data` array) is an `Err` — the caller degrades
/// to config-only prices. Individual malformed entries never are: a
/// broker changing one model's shape must not break the rest, so entries
/// without a usable id are skipped and entries without usable pricing
/// still contribute their context length.
pub async fn fetch_catalogue(
    base: &str,
    timeout: Duration,
    egress: EgressPolicy,
) -> Result<Catalogue, ForgeError> {
    let client = egress
        .client(timeout)
        .map_err(|e| ForgeError::provider(format!("building HTTP client: {e}")))?;
    let url = format!("{}/api/v1/models", base.trim_end_matches('/'));
    let response = client.get(&url).send().await.map_err(|e| {
        ForgeError::provider(format!(
            "catalogue request to {url} failed: {}",
            crate::local_only::error_detail(&e)
        ))
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ForgeError::provider(format!(
            "catalogue endpoint {url} returned {status}"
        )));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| ForgeError::provider(format!("invalid catalogue response from {url}: {e}")))?;
    parse_catalogue(&body)
}

/// Fetch and write the cache in one step — what `forge model refresh` and
/// `forge init` call. Returns the model count and the cache path written.
/// Refused outright under `local_only`: the catalogue is a network source
/// and prunes like every other one.
pub async fn refresh_cache(config: &forge_config::Config) -> Result<(usize, PathBuf), ForgeError> {
    if config.local_only {
        return Err(ForgeError::config(
            "catalogue refresh fetches from the network, but local_only is set; unset \
             --local-only / FORGE_LOCAL_ONLY to run it"
                .to_string(),
        ));
    }
    let catalogue = fetch_catalogue(
        &base_url(),
        Duration::from_secs(15),
        EgressPolicy::from_config(config),
    )
    .await?;
    let count = catalogue.models.len();
    let path = forge_config::catalogue::cache_path();
    forge_config::catalogue::save(&path, &catalogue)?;
    Ok((count, path))
}

fn parse_catalogue(body: &serde_json::Value) -> Result<Catalogue, ForgeError> {
    let data = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ForgeError::provider("catalogue response has no `data` array".to_string())
        })?;
    let mut models = Vec::new();
    for entry in data {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let context_length = entry
            .get("context_length")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize);
        let pricing = entry.get("pricing");
        models.push(CatalogueModel {
            id: id.to_string(),
            context_length,
            cost_input_per_mtok: pricing.and_then(|p| p.get("prompt")).and_then(parse_price),
            cost_output_per_mtok: pricing
                .and_then(|p| p.get("completion"))
                .and_then(parse_price),
        });
    }
    Ok(Catalogue {
        fetched_at: chrono::Utc::now(),
        models,
    })
}

/// One price field: the API reports per-token prices as decimal strings
/// ("0.000003"); forge prices per million tokens everywhere else.
fn parse_price(value: &serde_json::Value) -> Option<f64> {
    let per_token: f64 = value.as_str()?.parse().ok()?;
    Some(per_token * 1e6)
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn catalogue_body() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "anthropic/claude-sonnet-4.5",
                    "context_length": 200000,
                    "pricing": { "prompt": "0.000003", "completion": "0.000015" }
                },
                {
                    "id": "deepseek/deepseek-chat",
                    "context_length": 128000,
                    "pricing": { "prompt": "0.00000014", "completion": "0.00000028" }
                }
            ]
        })
    }

    async fn serve(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn fetch_parses_ids_context_lengths_and_per_mtok_prices() {
        let server = serve(catalogue_body()).await;
        let catalogue = fetch_catalogue(
            &server.uri(),
            Duration::from_secs(5),
            EgressPolicy::default(),
        )
        .await
        .expect("fetches");

        assert_eq!(catalogue.models.len(), 2);
        let sonnet = catalogue
            .model("anthropic/claude-sonnet-4.5")
            .expect("sonnet");
        assert_eq!(sonnet.context_length, Some(200_000));
        // Per-token strings × 1e6 → per-million-token prices.
        assert_eq!(sonnet.costs(), Some((3.0, 15.0)));
        let deepseek = catalogue.model("deepseek/deepseek-chat").expect("deepseek");
        let (input, output) = deepseek.costs().expect("priced");
        assert!((input - 0.14).abs() < 1e-9, "{input}");
        assert!((output - 0.28).abs() < 1e-9, "{output}");
    }

    #[tokio::test]
    async fn partial_entries_degrade_but_never_fail_the_fetch() {
        // One entry with no pricing at all (context only), one with an
        // unparseable price, one with no id (skipped entirely): a broker
        // changing one model's shape must not lose the rest.
        let server = serve(serde_json::json!({
            "data": [
                { "id": "free/model", "context_length": 8192 },
                { "id": "odd/model", "context_length": 4096, "pricing": { "prompt": "soon", "completion": "0.000001" } },
                { "context_length": 1024, "pricing": { "prompt": "0.000001", "completion": "0.000001" } },
                { "id": "ok/model", "context_length": 1000, "pricing": { "prompt": "0.000001", "completion": "0.000002" } }
            ]
        }))
        .await;
        let catalogue = fetch_catalogue(
            &server.uri(),
            Duration::from_secs(5),
            EgressPolicy::default(),
        )
        .await
        .expect("fetches");

        assert_eq!(catalogue.models.len(), 3, "the id-less entry is skipped");
        assert_eq!(catalogue.model("free/model").expect("free").costs(), None);
        assert_eq!(
            catalogue.model("free/model").expect("free").context_length,
            Some(8_192)
        );
        let odd = catalogue.model("odd/model").expect("odd");
        assert_eq!(odd.cost_input_per_mtok, None, "unparseable price is absent");
        assert_eq!(odd.cost_output_per_mtok, Some(1.0));
        assert_eq!(
            catalogue.model("ok/model").expect("ok").costs(),
            Some((1.0, 2.0))
        );
    }

    #[tokio::test]
    async fn a_body_without_a_data_array_is_an_error() {
        let server = serve(serde_json::json!({ "models": [] })).await;
        let err = fetch_catalogue(
            &server.uri(),
            Duration::from_secs(5),
            EgressPolicy::default(),
        )
        .await
        .expect_err("must fail");
        assert!(matches!(err, ForgeError::Provider(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn http_failure_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = fetch_catalogue(
            &server.uri(),
            Duration::from_secs(5),
            EgressPolicy::default(),
        )
        .await
        .expect_err("must fail");
        match err {
            ForgeError::Provider(msg) => assert!(msg.contains("503"), "got: {msg}"),
            other => panic!("expected provider error, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn refresh_cache_writes_the_cache_file() {
        let server = serve(catalogue_body()).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::set_var("XDG_CACHE_HOME", tmp.path());
        }
        let result = refresh_cache(&forge_config::Config::default()).await;
        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var("XDG_CACHE_HOME");
        }
        let (count, path) = result.expect("refreshed");
        assert_eq!(count, 2);
        assert!(path.ends_with("forge/openrouter/models.json"), "{path:?}");
        let loaded = forge_config::catalogue::load(&path, Duration::from_secs(7 * 86_400))
            .expect("cache loads back");
        assert_eq!(loaded.catalogue.models.len(), 2);
    }

    #[tokio::test]
    #[serial]
    async fn refresh_cache_refuses_under_local_only() {
        let config = forge_config::Config {
            local_only: true,
            ..forge_config::Config::default()
        };
        let err = refresh_cache(&config).await.expect_err("must refuse");
        assert!(err.to_string().contains("local_only"), "got: {err}");
    }
}
