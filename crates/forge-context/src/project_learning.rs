//! Repository-owned, reviewable learning records.
//!
//! Unlike [`crate::learning`], these records are intentionally checked in.
//! They are small Markdown files with a deliberately narrow front matter
//! grammar so a project can review and edit them without a specialised tool.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const DIRECTORY: &str = ".forge/learnings";
const MAX_FILES: usize = 256;
const MAX_FILE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLearningKind {
    Decision,
    Preference,
    Guardrail,
    Runbook,
}

impl ProjectLearningKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "decision" => Some(Self::Decision),
            "preference" => Some(Self::Preference),
            "guardrail" => Some(Self::Guardrail),
            "runbook" => Some(Self::Runbook),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProjectLearning {
    pub path: PathBuf,
    pub kind: ProjectLearningKind,
    pub touches: Vec<String>,
    pub guidance: String,
}

/// Load valid records only. A malformed optional record must never make an
/// ordinary run unavailable; `forge learn check` reports it explicitly.
pub fn load_project_learnings(root: &Path) -> Vec<ProjectLearning> {
    let directory = root.join(DIRECTORY);
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut records = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .take(MAX_FILES)
        .filter_map(|path| parse(&path, root).ok())
        .collect::<Vec<_>>();
    records.sort_by(|a, b| a.path.cmp(&b.path));
    records
}

/// Select records linked to graph-ranked files, with a conservative lexical
/// fallback for a project that has not built its graph yet.
pub fn relevant_project_learnings(
    records: &[ProjectLearning],
    task: &str,
    graph_paths: &[String],
) -> Vec<ProjectLearning> {
    let task = task.to_ascii_lowercase();
    let tokens = task
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() >= 3)
        .collect::<Vec<_>>();
    let mut out = records
        .iter()
        .filter(|record| {
            record.touches.iter().any(|touch| {
                graph_paths.iter().any(|path| path == touch)
                    || tokens
                        .iter()
                        .any(|token| touch.to_ascii_lowercase().contains(token))
                    || tokens
                        .iter()
                        .any(|token| record.guidance.to_ascii_lowercase().contains(token))
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn write_project_learning(
    root: &Path,
    kind: ProjectLearningKind,
    touches: &[String],
    guidance: &str,
) -> std::io::Result<ProjectLearning> {
    let guidance = guidance.trim();
    if guidance.is_empty()
        || guidance.len() > MAX_FILE_BYTES
        || touches.iter().any(|touch| !valid_touch(touch))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid project learning",
        ));
    }
    let mut clean_touches = touches.to_vec();
    clean_touches.sort();
    clean_touches.dedup();
    let digest = sha256(guidance.as_bytes());
    let relative_path =
        PathBuf::from(DIRECTORY).join(format!("{}-{}.md", kind_name(kind), &digest[..12]));
    let path = root.join(&relative_path);
    std::fs::create_dir_all(path.parent().expect("learning parent"))?;
    let body = format!(
        "---\nkind: {}\ntouches:\n{}---\n\n{}\n",
        kind_name(kind),
        clean_touches
            .iter()
            .map(|touch| format!("  - {touch}\n"))
            .collect::<String>(),
        guidance,
    );
    std::fs::write(&path, body)?;
    Ok(ProjectLearning {
        path: relative_path,
        kind,
        touches: clean_touches,
        guidance: guidance.to_string(),
    })
}

pub fn changed_project_files(root: &Path) -> std::io::Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "--"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn parse(path: &Path, root: &Path) -> Result<ProjectLearning, ()> {
    let bytes = std::fs::read(path).map_err(|_| ())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(());
    }
    let content = std::str::from_utf8(&bytes).map_err(|_| ())?;
    let Some(rest) = content.strip_prefix("---\n") else {
        return Err(());
    };
    let Some((header, guidance)) = rest.split_once("---\n") else {
        return Err(());
    };
    let mut kind = None;
    let mut touches = Vec::new();
    let mut in_touches = false;
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("kind: ") {
            kind = ProjectLearningKind::parse(value.trim());
            in_touches = false;
        } else if line.trim() == "touches:" {
            in_touches = true;
        } else if in_touches && let Some(value) = line.trim().strip_prefix("- ") {
            if valid_touch(value) {
                touches.push(value.to_owned());
            } else {
                return Err(());
            }
        } else {
            return Err(());
        }
    }
    let guidance = guidance.trim();
    let kind = kind.ok_or(())?;
    if guidance.is_empty() || touches.is_empty() {
        return Err(());
    }
    Ok(ProjectLearning {
        path: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
        kind,
        touches,
        guidance: guidance.to_owned(),
    })
}

fn valid_touch(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

fn kind_name(kind: ProjectLearningKind) -> &'static str {
    match kind {
        ProjectLearningKind::Decision => "decision",
        ProjectLearningKind::Preference => "preference",
        ProjectLearningKind::Guardrail => "guardrail",
        ProjectLearningKind::Runbook => "runbook",
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_select_by_graph_link() {
        let temp = tempfile::tempdir().unwrap();
        let record = write_project_learning(
            temp.path(),
            ProjectLearningKind::Guardrail,
            &["src/payments.rs".into()],
            "Do not log payment tokens.",
        )
        .unwrap();
        assert!(temp.path().join(&record.path).is_file());
        let records = load_project_learnings(temp.path());
        assert_eq!(records.len(), 1);
        assert_eq!(
            relevant_project_learnings(
                &records,
                "change the checkout",
                &["src/payments.rs".into()]
            )
            .len(),
            1
        );
    }
}
