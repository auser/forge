use std::path::{Path, PathBuf};

/// Walk up from `start` looking for a `.git` or `.forge` marker directory;
/// fall back to `start` itself when no marker is found.
pub fn find_project_root(start: &Path) -> PathBuf {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    let mut dir: Option<&Path> = Some(start.as_path());
    while let Some(current) = dir {
        if current.join(".git").exists() || current.join(".forge").exists() {
            return current.to_path_buf();
        }
        dir = current.parent();
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_start_when_no_marker() {
        let dir = std::env::temp_dir().join("forge-core-test-no-marker");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let found = find_project_root(&dir);
        assert_eq!(found, dir.canonicalize().expect("canonicalize"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walks_up_to_marker() {
        let base = std::env::temp_dir().join("forge-core-test-marker");
        let nested = base.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::create_dir_all(base.join(".forge")).expect("mkdir .forge");
        let found = find_project_root(&nested);
        assert_eq!(found, base.canonicalize().expect("canonicalize"));
        std::fs::remove_dir_all(&base).ok();
    }
}
