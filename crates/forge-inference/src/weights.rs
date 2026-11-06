//! Weights lifecycle for locally-run models: where the files live, how they
//! are fetched, and how their bytes are checked before anything executes
//! them.
//!
//! The shape is the one already proven in `forge-needle/src/weights.rs`:
//! fetch to a `<file>.part` path, verify the SHA-256, rename into place only
//! after the bytes check out, and retry once on a transient failure. A
//! truncated or tampered file is never renamed into its final name, so a
//! file's presence at the final path is itself evidence that it verified.
//!
//! **Nothing here runs a model.** Verification happens before a session is
//! ever constructed; the runtime is handed a file that already hashed to
//! the pinned digest.

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_core::ForgeError;
use sha2::{Digest, Sha256};

use crate::model::{Artifact, ModelSpec};

/// Overrides the base URL model artifacts resolve against. `forge init` and
/// the runtime never set this; tests point it at a `wiremock` server so no
/// test touches the network.
const BASE_URL_ENV: &str = "FORGE_RTEN_WEIGHTS_BASE_URL";

/// Test-only escape hatch: overrides the expected SHA-256 so tests can
/// verify against small fake payloads instead of a real 34 MB artifact.
const TEST_SHA256_ENV: &str = "FORGE_RTEN_TEST_SHA256";

/// Real resolve-URL prefix for the pinned Hugging Face repo.
const DEFAULT_BASE_URL: &str = "https://huggingface.co/Xenova/bge-small-en-v1.5/resolve/main";

/// How long a single download attempt may take before it is treated as
/// failed (the int8 model is 34 MB; generous for slow links).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// Outcome of [`ensure_weights`]. Callers decide severity: `forge init`
/// warns on `Missing`, the runtime reports it and falls back to no
/// semantic search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeightsStatus {
    /// Every file was already on disk and verified; nothing was fetched.
    Present(PathBuf),
    /// One or more files were downloaded (and verified) this call.
    Fetched(PathBuf),
    /// No usable weights; `reason` is human-readable and safe to print
    /// (never includes secrets — only model ids, paths and URLs).
    Missing { path: PathBuf, reason: String },
}

/// Resolve the URL a model's artifact is fetched from.
fn artifact_url(artifact: &Artifact) -> String {
    let base = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    format!("{}/{}", base.trim_end_matches('/'), artifact.repo_path)
}

/// The directory a model's files live in when nothing overrides it:
/// `~/.cache/forge/models/rten/<model-id>/`.
///
/// One directory per model keeps a second model's files from colliding with
/// the first's, and keeps the whole model removable with a single delete.
pub fn default_weights_dir(spec: &ModelSpec) -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("forge")
        .join("models")
        .join("rten")
        .join(spec.id)
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

/// Download `url` into `part`, cleaning the partial file up if the attempt
/// fails. The caller verifies `part` and only then renames it into place,
/// so the final path never holds bytes that have not hashed to the pin —
/// and this function never writes to, or removes, the final path at all.
async fn download(url: &str, part: &Path) -> Result<(), ForgeError> {
    let result = download_to(url, part).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(part).await;
    }
    result
}

async fn download_to(url: &str, part: &Path) -> Result<(), ForgeError> {
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
    if let Some(parent) = part.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(ForgeError::Io)?;
    }
    tokio::fs::write(part, &bytes)
        .await
        .map_err(ForgeError::Io)?;
    Ok(())
}

/// The digest an artifact must have.
///
/// Precedence: the compiled-in pin, unless the test-only env override is
/// set. (Unlike `needle`, there is no operator-facing checksum override:
/// an operator running their own model points `embedding.weights_dir` at
/// it and owns the trust decision outright.)
fn expected_sha256(artifact: &Artifact) -> String {
    std::env::var(TEST_SHA256_ENV).unwrap_or_else(|_| artifact.sha256.to_string())
}

/// Ensure every file of `spec` is on disk in `dir` and verified.
///
/// Never hard-errors for "no weights available" conditions: an unpinned
/// model id, a network failure, or a checksum that never matches all
/// degrade to [`WeightsStatus::Missing`], so `forge init` can warn and the
/// runtime can degrade to no semantic search instead of the process
/// failing.
pub async fn ensure_weights(
    spec: &ModelSpec,
    dir: &Path,
    autofetch: bool,
) -> Result<WeightsStatus, ForgeError> {
    let artifacts = [spec.model, spec.tokenizer];
    let mut fetched_any = false;

    for artifact in artifacts {
        let dest = dir.join(artifact.filename);
        let expected = expected_sha256(&artifact);

        if dest.is_file() {
            match verify(&dest, &expected) {
                Ok(true) => continue,
                // Truncated or tampered bytes on disk: don't trust them, and
                // don't delete them either — a verified refetch replaces the
                // file by atomic rename, and if no refetch happens the
                // caller is told `Missing` rather than losing the file.
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %dest.display(),
                        "could not verify existing model file, refetching"
                    );
                }
            }
        }

        if !autofetch {
            return Ok(WeightsStatus::Missing {
                path: dir.to_path_buf(),
                reason: format!(
                    "{} is not present and autofetch is disabled",
                    artifact.filename
                ),
            });
        }

        let url = artifact_url(&artifact);
        match fetch_and_verify(&url, &dest, &expected).await {
            Ok(true) => {
                fetched_any = true;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, url = %url, "model fetch attempt 1 failed, retrying once");
            }
        }

        match fetch_and_verify(&url, &dest, &expected).await {
            Ok(true) => {
                fetched_any = true;
                continue;
            }
            Ok(false) => {
                return Ok(WeightsStatus::Missing {
                    path: dir.to_path_buf(),
                    reason: format!("checksum mismatch after 2 download attempts from {url}"),
                });
            }
            Err(e) => {
                return Ok(WeightsStatus::Missing {
                    path: dir.to_path_buf(),
                    reason: format!("fetching {url} failed after 2 attempts: {e}"),
                });
            }
        }
    }

    Ok(if fetched_any {
        WeightsStatus::Fetched(dir.to_path_buf())
    } else {
        WeightsStatus::Present(dir.to_path_buf())
    })
}

/// Fetch `url` and install it at `dest` only if its bytes hash to
/// `expected_sha256`. A mismatch leaves `dest` untouched and discards the
/// download, so a file forge did not put there is never destroyed by a
/// failed fetch — `embedding.weights_dir` may point at a directory the
/// operator owns and keeps other files in.
async fn fetch_and_verify(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
) -> Result<bool, ForgeError> {
    let part = part_path(dest);
    download(url, &part).await?;
    if !verify(&part, expected_sha256)? {
        let _ = tokio::fs::remove_file(&part).await;
        return Ok(false);
    }
    tokio::fs::rename(&part, dest)
        .await
        .map_err(ForgeError::Io)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use sha2::{Digest, Sha256};

    fn hex_sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn spec() -> &'static ModelSpec {
        crate::model::spec_for(crate::model::DEFAULT_MODEL_ID).expect("default model is pinned")
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
        // A directory can't be re-hashed as file bytes. `verify` must
        // surface that as `Err`, not silently report `Ok(false)` — the
        // difference between "doesn't match" and "couldn't even check".
        let dir = tempfile::tempdir().expect("tmp");
        assert!(verify(dir.path(), &"0".repeat(64)).is_err());
    }

    #[tokio::test]
    #[serial]
    async fn a_failed_fetch_never_destroys_a_file_forge_did_not_write() {
        // `embedding.weights_dir` is ordinary layered config, so a cloned
        // repo's project config can aim it at a directory the operator owns
        // and keeps other files in — and one pinned artifact is named
        // `tokenizer.json`, which collides with half the ML projects on
        // disk. A checksum mismatch must therefore leave the existing file
        // exactly as it was, not clean it up.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::set_var(TEST_SHA256_ENV, hex_sha256(b"what we actually pinned"));
        }

        let dir = tempfile::tempdir().expect("tmp");
        let target = dir.path().join("someone-elses-models");
        std::fs::create_dir_all(&target).expect("mkdir");
        let precious = target.join(spec().tokenizer.filename);
        std::fs::write(&precious, b"the operator's own tokenizer").expect("write");

        let status = ensure_weights(spec(), &target, true).await.expect("ensure");
        assert!(
            matches!(status, WeightsStatus::Missing { .. }),
            "a mismatch must degrade, not succeed: {status:?}"
        );
        assert_eq!(
            std::fs::read(&precious).expect("the file is still there"),
            b"the operator's own tokenizer",
            "a failed fetch must not touch a file forge did not write"
        );
        assert!(
            !part_path(&precious).exists(),
            "the discarded download must not be left behind"
        );

        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var(TEST_SHA256_ENV);
        }
    }

    #[tokio::test]
    #[serial]
    async fn ensure_weights_fetches_verifies_and_is_idempotent() {
        let server = wiremock::MockServer::start().await;
        let body = b"fake-model".to_vec();
        let sha = hex_sha256(&body);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;

        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            std::env::set_var(TEST_SHA256_ENV, &sha);
        }
        let dir = tempfile::tempdir().expect("tmp");
        let target = dir.path().join("weights");

        let first = ensure_weights(spec(), &target, true).await.expect("ensure");
        assert!(
            matches!(first, WeightsStatus::Fetched(_)),
            "expected Fetched, got {first:?}"
        );
        // Both artifacts were fetched, so both are on disk and verified.
        assert!(target.join(spec().model.filename).is_file());
        assert!(target.join(spec().tokenizer.filename).is_file());

        // Second call: already present and verified, nothing fetched.
        let second = ensure_weights(spec(), &target, true).await.expect("ensure");
        assert!(
            matches!(second, WeightsStatus::Present(_)),
            "expected Present, got {second:?}"
        );

        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var(TEST_SHA256_ENV);
        }
    }

    #[tokio::test]
    #[serial]
    async fn ensure_weights_degrades_when_autofetch_is_disabled() {
        let dir = tempfile::tempdir().expect("tmp");
        let target = dir.path().join("weights");
        let status = ensure_weights(spec(), &target, false)
            .await
            .expect("ensure");
        match status {
            WeightsStatus::Missing { path, reason } => {
                assert_eq!(path, target);
                assert!(reason.contains("autofetch"), "reason: {reason}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn ensure_weights_reports_a_checksum_mismatch_instead_of_trusting_the_bytes() {
        let server = wiremock::MockServer::start().await;
        let body = b"not-the-pinned-bytes".to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        unsafe {
            std::env::set_var(BASE_URL_ENV, server.uri());
            // The pinned digest, deliberately not the digest of the body.
            std::env::set_var(TEST_SHA256_ENV, "0".repeat(64));
        }
        let dir = tempfile::tempdir().expect("tmp");
        let target = dir.path().join("weights");

        let status = ensure_weights(spec(), &target, true).await.expect("ensure");
        match status {
            WeightsStatus::Missing { reason, .. } => {
                assert!(reason.contains("checksum"), "reason: {reason}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        // The mismatched bytes must not be left behind as if they verified.
        assert!(!target.join(spec().model.filename).is_file());

        unsafe {
            std::env::remove_var(BASE_URL_ENV);
            std::env::remove_var(TEST_SHA256_ENV);
        }
    }

    #[test]
    fn default_weights_dir_is_per_model() {
        let a = default_weights_dir(spec());
        let b = default_weights_dir(crate::model::spec_for("bge-small-en-v1.5").expect("pinned"));
        assert_ne!(a, b, "each model gets its own directory");
        assert!(a.ends_with(spec().id), "dir: {}", a.display());
    }
}
