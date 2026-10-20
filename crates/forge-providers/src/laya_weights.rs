//! Pinned, on-demand checkpoint provisioning for the native Rust Laya router.
//!
//! Laya needs a directory rather than one blob. The large safetensors file is
//! SHA-256 pinned; the accompanying JSON files are fetched from the same
//! immutable upstream commit. Files are streamed to `.part` paths and renamed
//! only after the download completes, so an interrupted fetch never looks
//! like a usable checkpoint.

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_config::LayaConfig;
use forge_core::ForgeError;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const REVISION: &str = "1c5edc17a7acd8701df6fc341c0d179f1c62c982";
const BASE_URL: &str = "https://huggingface.co/convaiinnovations/laya/resolve";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);

struct Artifact {
    path: &'static str,
    sha256: Option<&'static str>,
}

// The SHA-256 comes from the upstream Hugging Face LFS object. The remaining
// small JSON files are fixed by REVISION; Laya requires these exact paths.
const ARTIFACTS: &[Artifact] = &[
    Artifact {
        path: "model.safetensors",
        sha256: Some("4fa56de72383a9d3efa9cfa78955733c81b9fc8067a587ca4beb82c78107a24e"),
    },
    Artifact {
        path: "encoder/config.json",
        sha256: None,
    },
    Artifact {
        path: "tokenizer/tokenizer.json",
        sha256: None,
    },
    Artifact {
        path: "tokenizer/tokenizer_config.json",
        sha256: None,
    },
    Artifact {
        path: "rl_agent_config.json",
        sha256: None,
    },
];

/// The default location of the pinned typed-decisions checkpoint.
pub fn weights_path(config: &LayaConfig) -> PathBuf {
    if !config.weights_path.trim().is_empty() {
        return PathBuf::from(&config.weights_path);
    }
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("forge")
        .join("models")
        .join("laya")
        .join("typed-decisions")
}

/// Ensure a complete checkpoint is present, fetching the pinned upstream
/// release when allowed. Callers turn errors into their normal router fallback.
pub async fn ensure_weights(config: &LayaConfig) -> Result<PathBuf, ForgeError> {
    let root = weights_path(config);
    for artifact in ARTIFACTS {
        let destination = root.join(artifact.path);
        if present_and_valid(&destination, artifact.sha256)? {
            continue;
        }
        if !config.autofetch {
            return Err(ForgeError::router(format!(
                "native laya checkpoint is missing or invalid at {}; set [laya].autofetch = true \
                 to download the pinned 847 MB checkpoint, or provide a complete weights_path",
                root.display()
            )));
        }
        download_artifact(artifact, &destination).await?;
    }
    Ok(root)
}

fn present_and_valid(path: &Path, expected_sha256: Option<&str>) -> Result<bool, ForgeError> {
    if !path.is_file() {
        return Ok(false);
    }
    let metadata = std::fs::metadata(path).map_err(ForgeError::Io)?;
    if metadata.len() == 0 {
        return Ok(false);
    }
    match expected_sha256 {
        Some(expected) => Ok(file_sha256(path)?.eq_ignore_ascii_case(expected)),
        None => Ok(true),
    }
}

fn file_sha256(path: &Path) -> Result<String, ForgeError> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(ForgeError::Io)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(ForgeError::Io)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

async fn download_artifact(artifact: &Artifact, destination: &Path) -> Result<(), ForgeError> {
    let url = format!("{BASE_URL}/{REVISION}/typed-decisions/{}", artifact.path);
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| ForgeError::router(format!("building native laya downloader: {error}")))?;
    let response = client.get(&url).send().await.map_err(|error| {
        ForgeError::router(format!("downloading native laya checkpoint: {error}"))
    })?;
    if !response.status().is_success() {
        return Err(ForgeError::router(format!(
            "downloading native laya checkpoint from {url}: HTTP {}",
            response.status()
        )));
    }

    let parent = destination.parent().ok_or_else(|| {
        ForgeError::router(format!(
            "native laya checkpoint path has no parent: {}",
            destination.display()
        ))
    })?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(ForgeError::Io)?;
    let part = part_path(destination);
    let result = download_to_part(response, &part, artifact.sha256).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
        return result;
    }
    tokio::fs::rename(&part, destination)
        .await
        .map_err(ForgeError::Io)
}

async fn download_to_part(
    mut response: reqwest::Response,
    part: &Path,
    expected_sha256: Option<&str>,
) -> Result<(), ForgeError> {
    let mut file = tokio::fs::File::create(part)
        .await
        .map_err(ForgeError::Io)?;
    let mut digest = Sha256::new();
    let mut received = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ForgeError::router(format!("reading native laya download: {error}")))?
    {
        received += chunk.len() as u64;
        digest.update(&chunk);
        file.write_all(&chunk).await.map_err(ForgeError::Io)?;
    }
    file.flush().await.map_err(ForgeError::Io)?;
    if received == 0 {
        return Err(ForgeError::router("native laya download was empty"));
    }
    if let Some(expected) = expected_sha256 {
        let actual = format!("{:x}", digest.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(ForgeError::router(format!(
                "native laya checkpoint checksum mismatch (expected {expected}, got {actual})"
            )));
        }
    }
    Ok(())
}

fn part_path(destination: &Path) -> PathBuf {
    let mut path = destination.as_os_str().to_os_string();
    path.push(".part");
    PathBuf::from(path)
}
