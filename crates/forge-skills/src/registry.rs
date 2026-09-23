use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_core::{
    ExecRequest, ExecutionProvider, ForgeError, RiskLevel, Skill, SkillMeta, SkillRegistry,
};

use crate::frontmatter;

/// Which discovery root a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    ProjectForge,
    ProjectAgents,
    ProjectClaude,
    UserForge,
    UserAgents,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::ProjectForge => ".forge/skills",
            Self::ProjectAgents => ".agents/skills",
            Self::ProjectClaude => ".claude/skills",
            Self::UserForge => "user config",
            Self::UserAgents => "user agents",
        }
    }
}

/// Filesystem-backed skill registry with progressive disclosure:
/// `list()` reads only the head of each `SKILL.md` (frontmatter),
/// `activate()` reads the full instructions.
pub struct FsSkillRegistry {
    roots: Vec<(SkillSource, PathBuf)>,
    exec: Option<Arc<dyn ExecutionProvider>>,
}

impl FsSkillRegistry {
    /// Standard discovery roots for a project. User roots honor
    /// `XDG_CONFIG_HOME`/`HOME`. Project roots shadow user roots.
    pub fn new(project_root: &Path, exec: Option<Arc<dyn ExecutionProvider>>) -> Self {
        let mut roots = vec![
            (
                SkillSource::ProjectForge,
                project_root.join(".forge").join("skills"),
            ),
            (
                SkillSource::ProjectAgents,
                project_root.join(".agents").join("skills"),
            ),
            (
                SkillSource::ProjectClaude,
                project_root.join(".claude").join("skills"),
            ),
        ];
        let user_forge = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            PathBuf::from(xdg).join("forge").join("skills")
        } else {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("forge")
                .join("skills")
        };
        roots.push((SkillSource::UserForge, user_forge));
        if let Some(home) = std::env::home_dir() {
            roots.push((SkillSource::UserAgents, home.join(".agents").join("skills")));
        }
        Self { roots, exec }
    }

    /// Registry over explicit roots (tests).
    pub fn with_roots(
        roots: Vec<(SkillSource, PathBuf)>,
        exec: Option<Arc<dyn ExecutionProvider>>,
    ) -> Self {
        Self { roots, exec }
    }

    /// Which source root a skill path belongs to.
    pub fn source_of(&self, skill_path: &Path) -> Option<SkillSource> {
        self.roots
            .iter()
            .filter(|(_, root)| skill_path.starts_with(root))
            // Longest root wins when roots nest.
            .max_by_key(|(_, root)| root.as_os_str().len())
            .map(|(source, _)| *source)
    }

    /// All discovered skills (metadata only), project roots shadowing
    /// user roots on name collision.
    fn discover(&self) -> Vec<(SkillSource, SkillMeta)> {
        let mut out: Vec<(SkillSource, SkillMeta)> = Vec::new();
        for (source, root) in &self.roots {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(_) => continue, // missing roots are normal
            };
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let skill_file = dir.join("SKILL.md");
                if !skill_file.is_file() {
                    continue;
                }
                match Self::read_meta(*source, &dir, &skill_file) {
                    Some(meta) => {
                        if !out.iter().any(|(_, m)| m.name == meta.name) {
                            out.push((*source, meta));
                        }
                    }
                    None => {
                        tracing::warn!(path = %skill_file.display(), "skipping unreadable skill");
                    }
                }
            }
        }
        out.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        out
    }

    /// Parse metadata from the head of a SKILL.md only (progressive
    /// disclosure: no full body read).
    fn read_meta(source: SkillSource, dir: &Path, skill_file: &Path) -> Option<SkillMeta> {
        let head = read_head(skill_file, 4096)?;
        let (fields, body) = frontmatter::split(&head);
        let dir_name = dir.file_name()?.to_string_lossy().to_string();
        let name = frontmatter::field(&fields, "name")
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or(dir_name);
        let description = frontmatter::field(&fields, "description")
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| frontmatter::first_prose_line(body))
            .unwrap_or_default();
        let _ = source;
        Some(SkillMeta {
            name,
            description,
            path: skill_file.to_path_buf(),
        })
    }

    /// Extra files in the skill directory, loaded on demand later.
    pub fn references(&self, name: &str) -> Result<Vec<PathBuf>, ForgeError> {
        let meta = self.find(name)?;
        let dir = meta
            .path
            .parent()
            .ok_or_else(|| ForgeError::skill(format!("skill {name:?} has no directory")))?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(ForgeError::Io)?.flatten() {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Run a skill's `test.sh`/`test.py` through the execution provider.
    /// `Ok(None)` means the skill has no test script.
    pub async fn test_skill(
        &self,
        name: &str,
    ) -> Result<Option<forge_core::ExecResult>, ForgeError> {
        let exec = self.exec.as_ref().ok_or_else(|| {
            ForgeError::skill("no execution provider configured for skill testing")
        })?;
        let meta = self.find(name)?;
        let dir = meta
            .path
            .parent()
            .ok_or_else(|| ForgeError::skill(format!("skill {name:?} has no directory")))?;

        let (command, script) = if dir.join("test.sh").is_file() {
            ("sh", dir.join("test.sh"))
        } else if dir.join("test.py").is_file() {
            ("python3", dir.join("test.py"))
        } else {
            return Ok(None);
        };

        let request = ExecRequest {
            command: command.to_string(),
            args: vec![script.to_string_lossy().to_string()],
            cwd: Some(dir.to_path_buf()),
            risk: RiskLevel::Risky,
            inherit_stdio: false,
        };
        exec.execute(request).await.map(Some)
    }

    fn find(&self, name: &str) -> Result<SkillMeta, ForgeError> {
        self.discover()
            .into_iter()
            .find(|(_, meta)| meta.name == name)
            .map(|(_, meta)| meta)
            .ok_or_else(|| ForgeError::skill(format!("unknown skill: {name}")))
    }
}

impl SkillRegistry for FsSkillRegistry {
    fn list(&self) -> Vec<SkillMeta> {
        self.discover().into_iter().map(|(_, meta)| meta).collect()
    }

    fn activate(&self, name: &str) -> Result<Skill, ForgeError> {
        let meta = self.find(name)?;
        let content = std::fs::read_to_string(&meta.path).map_err(ForgeError::Io)?;
        let (_fields, body) = frontmatter::split(&content);
        let instructions = if body.trim().is_empty() {
            content
        } else {
            body.trim_start().to_string()
        };
        Ok(Skill { meta, instructions })
    }

    /// Naive case-insensitive substring match of skill name/description
    /// against the prompt's words.
    fn match_task(&self, prompt: &str) -> Vec<SkillMeta> {
        let prompt = prompt.to_lowercase();
        self.discover()
            .into_iter()
            .filter(|(_, meta)| {
                let name = meta.name.to_lowercase();
                let desc = meta.description.to_lowercase();
                prompt
                    .split_whitespace()
                    .any(|word| word.len() >= 3 && (name.contains(word) || desc.contains(word)))
            })
            .map(|(_, meta)| meta)
            .collect()
    }
}

/// Read at most `limit` bytes from a file (frontmatter-sized reads).
fn read_head(path: &Path, limit: usize) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.by_ref()
        .take(limit as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}
