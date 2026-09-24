//! Weights lifecycle for the embedded Needle brain: which artifact a
//! `[needle].variant` maps to, where it lives on disk, and how `forge init`
//! (and eventually [`crate::backend`]'s FFI backend, Task 8) fetch and
//! verify it.
//!
//! **Pinning reality (read before touching `VARIANTS`)**: `forge-config`'s
//! `NEEDLE_VARIANTS` accepts `"small" | "medium" | "full"` as *configured*
//! values, but Cactus-Compute (`Cactus-Compute/needle3` on Hugging Face)
//! only publishes one ready-to-run artifact per release: the full 20-layer
//! `needle3.cact`. Smaller subnetworks ("small"/"medium") are produced
//! locally with the `needle build --layers N` CLI (part of the `cactus-needle`
//! Python package) from that same file — they are not separately hosted, so
//! there is no URL or checksum to pin for them yet. Only `"full"` appears in
//! `VARIANTS` below; `spec_for` returns a typed [`ForgeError`] naming the
//! available variants for anything else, and `ensure_weights` turns that
//! into `WeightsStatus::Missing` rather than a hard failure — forge stays
//! fully functional (static routing fallback) either way. See the Task 6
//! report for the full pinning transcript (download + `shasum -a 256`) and
//! the license finding (Apache-2.0, both the HF `cardData.license` and the
//! repo's `LICENSE` file).
//!
//! An operator who wants to run different weights entirely (paired with a
//! `weights_path` override and possibly a custom base URL) can set
//! `[needle].weights_sha256` to their own known-good checksum — it takes
//! precedence over both the test-only env override and the compiled-in
//! pin. See [`ensure_weights`] for the exact precedence order.

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_config::NeedleConfig;
use forge_core::ForgeError;
use sha2::{Digest, Sha256};

/// Overrides the base URL weight artifacts resolve against; `forge init`
/// and the engine never set this, but tests point it at a `wiremock`
/// server so no test touches the network.
const BASE_URL_ENV: &str = "FORGE_NEEDLE_WEIGHTS_BASE_URL";

/// Test-only escape hatch: overrides the expected SHA-256 so tests can
/// verify against small fake payloads instead of a real multi-MB artifact.
/// Never set outside tests. Lower precedence than `[needle].weights_sha256`
/// — see [`ensure_weights`].
const TEST_SHA256_ENV: &str = "FORGE_NEEDLE_TEST_SHA256";

/// Real resolve-URL prefix for the pinned Hugging Face repo.
const DEFAULT_BASE_URL: &str = "https://huggingface.co/Cactus-Compute/needle3/resolve/main";

/// How long a single download attempt may take before it's treated as
/// failed (weights run 8-35 MB; generous for slow links).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

struct WeightsVariant {
    variant: &'static str,
    filename: &'static str,
    sha256: &'static str,
}

/// Pinned weight artifacts. See the module doc for why only `"full"` is
/// listed. Verified 2026-09-23: downloaded `needle3.cact` from
/// `DEFAULT_BASE_URL` and re-hashed it independently with both `shasum -a
/// 256` and Python's `hashlib.sha256`.
const VARIANTS: &[WeightsVariant] = &[WeightsVariant {
    variant: "full",
    filename: "needle3.cact",
    sha256: "c9d915eca282ed42d1a09b143b592adb4cc6744ffe2d294adf5cfc5548170c38",
}];

/// A resolved, checksummed weights artifact for one `[needle].variant`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeightsSpec {
    pub variant: &'static str,
    pub filename: &'static str,
    pub sha256: &'static str,
    pub url: String,
}

/// Outcome of [`ensure_weights`]. Callers decide severity: `forge init`
/// warns on `Missing`, routers fall back to `router_fallback`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeightsStatus {
    /// Already on disk and verified; nothing was fetched.
    Present(PathBuf),
    /// Downloaded (and verified) this call.
    Fetched(PathBuf),
    /// No usable weights at `path`; `reason` is human-readable and safe to
    /// print (never includes secrets — only variant names, paths, URLs).
    Missing { path: PathBuf, reason: String },
}

/// Look up the pinned artifact for `variant`. Errors name every variant
/// that *is* pinned, so the message is directly actionable.
pub fn spec_for(variant: &str) -> Result<WeightsSpec, ForgeError> {
    let entry = VARIANTS
        .iter()
        .find(|v| v.variant == variant)
        .ok_or_else(|| {
            let available: Vec<&str> = VARIANTS.iter().map(|v| v.variant).collect();
            ForgeError::config(format!(
                "no pinned weights artifact for needle variant {variant:?}; available: {}",
                available.join(", ")
            ))
        })?;
    let base = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let url = format!("{}/{}", base.trim_end_matches('/'), entry.filename);
    Ok(WeightsSpec {
        variant: entry.variant,
        filename: entry.filename,
        sha256: entry.sha256,
        url,
    })
}

/// Cache directory for weights when `[needle].weights_path` is unset:
/// `~/.cache/forge/models/`. Mirrors `forge_config::Config`'s
/// `std::env::home_dir()` fallback pattern for `~/.config/forge/`.
fn cache_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("forge")
        .join("models")
}

/// Where weights for `needle` should live on disk: the configured override
/// when set, else `~/.cache/forge/models/<filename>`. Requires a pinned
/// spec for `needle.variant` — the override only changes *where* the file
/// goes, not which checksum applies to it.
pub fn weights_path(needle: &NeedleConfig) -> Result<PathBuf, ForgeError> {
    let spec = spec_for(&needle.variant)?;
    if !needle.weights_path.trim().is_empty() {
        return Ok(PathBuf::from(&needle.weights_path));
    }
    Ok(cache_dir().join(spec.filename))
}

/// Best-effort path for [`WeightsStatus::Missing`] when `needle.variant`
/// has no pinned spec at all — there's no real filename to anchor to, but
/// callers still want a path to show/log. Also used by `engine_from_config`
/// for the same reason.
pub(crate) fn best_effort_path(needle: &NeedleConfig) -> PathBuf {
    if !needle.weights_path.trim().is_empty() {
        return PathBuf::from(&needle.weights_path);
    }
    cache_dir().join(format!("needle3-{}.bin", needle.variant))
}

/// SHA-256 re-hash of `path` against `expected_sha256` (case-insensitive
/// hex). A missing file is "not verified", not an error: `Ok(false)`.
pub fn verify(path: &Path, expected_sha256: &str) -> Result<bool, ForgeError> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ForgeError::Io(e)),
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(ForgeError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = format!("{:x}", hasher.finalize());
    Ok(actual.eq_ignore_ascii_case(expected_sha256))
}

fn part_path(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

fn http_client() -> Result<reqwest::Client, ForgeError> {
    reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| ForgeError::config(format!("building HTTP client: {e}")))
}

/// Download `url` to `dest` via a `<dest>.part` temp file, then atomically
/// rename into place — `dest` only ever holds fully-written bytes, which is
/// what makes truncation on disk detectable by [`verify`] alone. Any
/// leftover `.part` file from a failed attempt is cleaned up.
async fn download(url: &str, dest: &Path) -> Result<(), ForgeError> {
    let part = part_path(dest);
    let result = download_to(url, dest, &part).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

async fn download_to(url: &str, dest: &Path, part: &Path) -> Result<(), ForgeError> {
    let client = http_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| ForgeError::config(format!("fetching {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(ForgeError::config(format!(
            "fetching {url}: HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| ForgeError::config(format!("reading response body from {url}: {e}")))?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(ForgeError::Io)?;
    }
    tokio::fs::write(part, &bytes)
        .await
        .map_err(ForgeError::Io)?;
    tokio::fs::rename(part, dest)
        .await
        .map_err(ForgeError::Io)?;
    Ok(())
}

async fn fetch_and_verify(
    url: &str,
    path: &Path,
    expected_sha256: &str,
) -> Result<bool, ForgeError> {
    download(url, path).await?;
    verify(path, expected_sha256)
}

/// Ensure weights for `needle` are on disk and verified: reuse an already
/// verified file, otherwise fetch (retrying once on checksum mismatch or a
/// transient fetch error, matching the design's "one automatic refetch"
/// guarantee). Never returns a hard error for "no weights available"
/// conditions — an unpinned variant, a network failure, or a checksum that
/// never matches all degrade to `WeightsStatus::Missing` so `forge init`
/// can warn and routers can fall back instead of the process failing.
pub async fn ensure_weights(needle: &NeedleConfig) -> Result<WeightsStatus, ForgeError> {
    let spec = match spec_for(&needle.variant) {
        Ok(spec) => spec,
        Err(e) => {
            return Ok(WeightsStatus::Missing {
                path: best_effort_path(needle),
                reason: e.to_string(),
            });
        }
    };
    // `spec_for` just succeeded for this variant, so `weights_path` cannot
    // fail here.
    let path = weights_path(needle)?;
    // Precedence: an operator-supplied `[needle].weights_sha256` wins over
    // everything (it's a conscious override, paired with a custom
    // `weights_path`/base URL); then the test-only env escape hatch; else
    // the compiled-in pin, the default trust anchor.
    let expected_sha256 = if !needle.weights_sha256.trim().is_empty() {
        needle.weights_sha256.clone()
    } else {
        std::env::var(TEST_SHA256_ENV).unwrap_or_else(|_| spec.sha256.to_string())
    };

    if path.is_file() {
        match verify(&path, &expected_sha256) {
            Ok(true) => return Ok(WeightsStatus::Present(path)),
            Ok(false) => {
                // Truncated/corrupt bytes on disk: don't trust them,
                // refetch below.
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                // Can't even read the existing file (permission denied,
                // transient I/O, ...): this is exactly the kind of
                // degradable condition ensure_weights promises never to
                // hard-error on. Treat it the same as a checksum mismatch
                // — drop it and fall through to fetch; if the path is
                // genuinely unusable (e.g. a directory), the fetch below
                // will fail too and that failure degrades to `Missing`
                // through the normal retry path instead of propagating.
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "could not verify existing needle weights file, refetching"
                );
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    match fetch_and_verify(&spec.url, &path, &expected_sha256).await {
        Ok(true) => return Ok(WeightsStatus::Fetched(path)),
        Ok(false) => {
            let _ = std::fs::remove_file(&path);
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %spec.url, "needle weights fetch attempt 1 failed, retrying once");
        }
    }

    match fetch_and_verify(&spec.url, &path, &expected_sha256).await {
        Ok(true) => Ok(WeightsStatus::Fetched(path)),
        Ok(false) => {
            let _ = std::fs::remove_file(&path);
            Ok(WeightsStatus::Missing {
                path,
                reason: format!(
                    "checksum mismatch after 2 download attempts from {}",
                    spec.url
                ),
            })
        }
        Err(e) => Ok(WeightsStatus::Missing {
            path,
            reason: format!("fetching {} failed after 2 attempts: {e}", spec.url),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use sha2::{Digest, Sha256};

    fn hex_sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    // `#[serial]`: this asserts the *default* resolve URL, so it must not run
    // while one of the `ensure_weights` tests below has `BASE_URL_ENV`
    // pointed at its wiremock server. (Without this it fails intermittently
    // depending on test scheduling.)
    #[test]
    #[serial]
    fn spec_for_full_returns_the_pinned_artifact() {
        let spec = spec_for("full").expect("full is pinned");
        assert_eq!(spec.variant, "full");
        assert_eq!(spec.filename, "needle3.cact");
        assert_eq!(spec.sha256.len(), 64, "sha256 must be 64 hex chars");
        assert_eq!(
            spec.url,
            "https://huggingface.co/Cactus-Compute/needle3/resolve/main/needle3.cact"
        );
    }

    #[test]
    fn spec_for_unpinned_variant_names_available_ones() {
        // "medium" is a config-valid `[needle].variant` (see
        // forge-config's NEEDLE_VARIANTS) but has no separately hosted
        // artifact — see the module doc for why.
        let err = spec_for("medium").expect_err("medium has no pinned artifact");
        let message = err.to_string();
        assert!(message.contains("medium"), "message: {message}");
        assert!(message.contains("full"), "message: {message}");
    }

    #[test]
    fn spec_for_unknown_variant_is_also_a_typed_error() {
        assert!(spec_for("does-not-exist").is_err());
    }

    #[test]
    fn verify_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tmp");
        let missing = dir.path().join("nope.bin");
        assert!(!verify(&missing, &"0".repeat(64)).expect("no io error"));
    }

    #[test]
    fn verify_matches_and_mismatches_correctly() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("w.bin");
        std::fs::write(&path, b"hello weights").expect("write");
        let good = hex_sha256(b"hello weights");
        assert!(verify(&path, &good).expect("verify"));
        assert!(!verify(&path, &"0".repeat(64)).expect("verify"));
    }

    #[test]
    fn verify_returns_err_not_false_for_an_unreadable_path() {
        // A directory can't be re-hashed as file bytes: `File::open`/read
        // fails with something other than `NotFound`. `verify` must
        // surface that as `Err`, not silently report `Ok(false)` (a
        // checksum mismatch) — `ensure_weights` relies on being able to
        // tell "doesn't match" apart from "couldn't even check" so it
        // knows to degrade rather than trust a false negative.
        let dir = tempfile::tempdir().expect("tmp");
        let err = verify(dir.path(), &"0".repeat(64));
        assert!(
            err.is_err(),
            "expected Err for a directory path, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn ensure_weights_refetches_instead_of_hard_erroring_when_existing_file_is_unreadable() {
        // Regression test: ensure_weights's pre-existing-file branch used
        // to do `if verify(&path, &expected_sha256)? { .. }`, which
        // propagated any non-checksum-mismatch `Err` from `verify`
        // (permission denied, transient I/O, ...) straight out of
        // `ensure_weights` — bypassing the delete-and-refetch/Missing
        // degradation every other verify-failure path takes. Simulate
        // that with a real permission error (chmod 0) rather than a
        // directory: `path.is_file()` is false for a directory, so that
        // wouldn't reach the branch under test at all.
        use std::os::unix::fs::PermissionsExt;

        let server = wiremock::MockServer::start().await;
        let body = b"refetched-after-unreadable-existing-file".to_vec();
        let sha = hex_sha256(&body);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("w.bin");
        std::fs::write(&path, b"pre-existing bytes").expect("seed existing file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");

        if std::fs::File::open(&path).is_ok() {
            // Running with privileges that bypass file permissions (e.g.
            // root in some containers): chmod 0 doesn't reproduce the
            // "can't read" condition this test targets. Skip rather than
            // false-failing.
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
            return;
        }

        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::set_var(TEST_SHA256_ENV, &sha);
        }
        let cfg = NeedleConfig {
            variant: "full".to_string(),
            weights_path: path.display().to_string(),
            autofetch: true,
            weights_sha256: String::new(),
        };

        let result = ensure_weights(&cfg).await;

        // Restore permissions unconditionally before any assertion can
        // panic, so tempdir cleanup and other tests are never affected.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var(TEST_SHA256_ENV);
        }

        match result {
            Ok(WeightsStatus::Fetched(_)) => {}
            other => panic!(
                "expected ensure_weights to degrade by refetching rather than hard-erroring, got {other:?}"
            ),
        }
    }

    #[test]
    fn weights_path_honors_override() {
        let cfg = NeedleConfig {
            variant: "full".to_string(),
            weights_path: "/custom/location/w.cact".to_string(),
            autofetch: true,
            weights_sha256: String::new(),
        };
        let path = weights_path(&cfg).expect("override + pinned variant resolves");
        assert_eq!(path, PathBuf::from("/custom/location/w.cact"));
    }

    #[test]
    fn weights_path_rejects_unpinned_variant_even_with_override() {
        // The override only changes *where* the file goes; `medium` still
        // has no known-good checksum to verify it against.
        let cfg = NeedleConfig {
            variant: "medium".to_string(),
            weights_path: "/custom/location/w.bin".to_string(),
            autofetch: true,
            weights_sha256: String::new(),
        };
        assert!(weights_path(&cfg).is_err());
    }

    #[tokio::test]
    #[serial]
    async fn ensure_weights_fetches_verifies_and_is_idempotent() {
        let server = wiremock::MockServer::start().await;
        let body = b"fake-weights".to_vec();
        let sha = hex_sha256(&body);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tmp");
        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::set_var(TEST_SHA256_ENV, &sha);
        }
        let cfg = NeedleConfig {
            variant: "full".to_string(),
            weights_path: dir.path().join("w.bin").display().to_string(),
            autofetch: true,
            weights_sha256: String::new(),
        };

        let first = ensure_weights(&cfg).await.expect("fetches");
        assert!(matches!(first, WeightsStatus::Fetched(_)));
        let second = ensure_weights(&cfg).await.expect("present");
        assert!(matches!(second, WeightsStatus::Present(_)));

        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var(TEST_SHA256_ENV);
        }
    }

    #[tokio::test]
    #[serial]
    async fn config_supplied_checksum_override_is_respected() {
        // `needle.weights_sha256` (an operator-supplied override) must win
        // even though it doesn't match the compiled-in pin for "full", and
        // even with no FORGE_NEEDLE_TEST_SHA256 env var set at all — the
        // config field alone is enough.
        let server = wiremock::MockServer::start().await;
        let body = b"operator-supplied-weights".to_vec();
        let sha = hex_sha256(&body);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tmp");
        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::remove_var(TEST_SHA256_ENV);
        }
        let cfg = NeedleConfig {
            variant: "full".to_string(),
            weights_path: dir.path().join("w.bin").display().to_string(),
            autofetch: true,
            weights_sha256: sha,
        };

        let status = ensure_weights(&cfg)
            .await
            .expect("fetches against the config-supplied checksum");
        assert!(matches!(status, WeightsStatus::Fetched(_)));

        unsafe {
            std::env::remove_var(BASE_URL_ENV);
        }
    }

    #[tokio::test]
    #[serial]
    async fn corrupt_download_refetches_once_then_reports() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"garbage".to_vec()))
            .expect(2)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tmp");
        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::set_var(TEST_SHA256_ENV, "0".repeat(64));
        }
        let cfg = NeedleConfig {
            variant: "full".to_string(),
            weights_path: dir.path().join("w.bin").display().to_string(),
            autofetch: true,
            weights_sha256: String::new(),
        };

        let status = ensure_weights(&cfg)
            .await
            .expect("completes without hard error");
        match status {
            WeightsStatus::Missing { reason, .. } => assert!(reason.contains("checksum")),
            other => panic!("expected Missing, got {other:?}"),
        }

        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var(TEST_SHA256_ENV);
        }
    }

    #[tokio::test]
    #[serial]
    async fn truncated_existing_file_fails_verify_and_is_refetched() {
        let server = wiremock::MockServer::start().await;
        let body = b"fake-weights-full".to_vec();
        let sha = hex_sha256(&body);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("w.bin");
        std::fs::write(&path, b"only-a-few-bytes").expect("write truncated stand-in");
        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::set_var(TEST_SHA256_ENV, &sha);
        }
        let cfg = NeedleConfig {
            variant: "full".to_string(),
            weights_path: path.display().to_string(),
            autofetch: true,
            weights_sha256: String::new(),
        };

        let status = ensure_weights(&cfg)
            .await
            .expect("refetches over the truncated file");
        assert!(matches!(status, WeightsStatus::Fetched(_)));

        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var(TEST_SHA256_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_weights_degrades_to_missing_for_unpinned_variant_without_network() {
        // No env vars touched, no mock server started: if this ever tried
        // to make a network call it would hang/fail against the real
        // internet, so a fast `Missing` here also proves no fetch attempt
        // was made for a variant with no pinned artifact.
        let dir = tempfile::tempdir().expect("tmp");
        let cfg = NeedleConfig {
            variant: "medium".to_string(),
            weights_path: dir.path().join("w.bin").display().to_string(),
            autofetch: true,
            weights_sha256: String::new(),
        };
        let status = ensure_weights(&cfg)
            .await
            .expect("degrades, never hard-errors");
        match status {
            WeightsStatus::Missing { reason, .. } => {
                assert!(reason.contains("medium"), "reason: {reason}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }
}
