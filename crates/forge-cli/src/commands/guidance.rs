use std::path::{Path, PathBuf};

use forge_core::Message;

const CODING_CONTRACT: &str = "\
You are a coding agent working in the user's project. Complete the requested \
change in the project rather than only describing it.

Working rules:
- Inspect the relevant code and project instructions before editing.
- Follow AGENTS.md, AGENT.md, CLAUDE.md, and copilot-instructions.md files; \
  instructions in a file's nearest ancestor take precedence for that subtree.
- Preserve unrelated user changes and existing project conventions.
- Prefer the smallest coherent fix to the rule that caused the problem.
- Use project tools to edit files and run the most relevant formatting, tests, \
  lint, or build checks.
- Treat tool failures as evidence: diagnose and recover when possible.
- Do not claim completion until the requested behavior is implemented and \
  validated. Finish with a concise summary of changes and validation.";

const INSTRUCTION_NAMES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md"];
const MAX_DISCOVERED_FILES: usize = 128;
const MAX_ROOT_INSTRUCTION_BYTES: usize = 64 * 1024;

/// Coding contract plus progressively disclosed project instruction files.
///
/// Root guidance is included because it applies to every file. Nested files
/// are listed by path so the model can read the nearest applicable one before
/// editing without paying their full context cost on every run.
pub fn system_context(root: &Path) -> Vec<Message> {
    let mut messages = vec![Message::system(CODING_CONTRACT)];
    let paths = instruction_paths(root);

    let mut root_guidance = Vec::new();
    for path in &paths {
        let applies_at_root = path.parent() == Some(root)
            || path == &root.join(".github").join("copilot-instructions.md");
        if applies_at_root && let Ok(content) = std::fs::read_to_string(path) {
            let content = truncate_utf8(&content, MAX_ROOT_INSTRUCTION_BYTES);
            let relative = path.strip_prefix(root).unwrap_or(path);
            root_guidance.push(format!("## {}\n{content}", relative.display()));
        }
    }
    if !root_guidance.is_empty() {
        messages.push(Message::system(format!(
            "Project instructions that apply at the repository root:\n\n{}",
            root_guidance.join("\n\n")
        )));
    }

    let nested: Vec<String> = paths
        .iter()
        .filter(|path| {
            path.parent() != Some(root)
                && *path != &root.join(".github").join("copilot-instructions.md")
        })
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .collect();
    if !nested.is_empty() {
        messages.push(Message::system(format!(
            "Nested project instruction files are available at these paths. \
             Read the nearest applicable file before editing in its subtree:\n- {}",
            nested.join("\n- ")
        )));
    }
    messages
}

fn instruction_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_instruction_paths(root, &mut paths);
    let copilot = root.join(".github").join("copilot-instructions.md");
    if copilot.is_file() && !paths.contains(&copilot) {
        paths.push(copilot);
    }
    paths.sort();
    paths.truncate(MAX_DISCOVERED_FILES);
    paths
}

fn collect_instruction_paths(directory: &Path, paths: &mut Vec<PathBuf>) {
    if paths.len() >= MAX_DISCOVERED_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                ".git" | ".forge" | "target" | "node_modules" | "dist" | "build"
            ) {
                continue;
            }
            collect_instruction_paths(&path, paths);
        } else if INSTRUCTION_NAMES.contains(&name.as_ref()) {
            paths.push(path);
        }
        if paths.len() >= MAX_DISCOVERED_FILES {
            return;
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_root_guidance_and_lists_nested_guidance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "root rule").expect("root instructions");
        std::fs::create_dir_all(tmp.path().join("src/api")).expect("mkdir");
        std::fs::write(tmp.path().join("src/api/AGENTS.md"), "nested rule")
            .expect("nested instructions");

        let messages = system_context(tmp.path());
        let text = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("root rule"));
        assert!(text.contains("src/api/AGENTS.md"));
        assert!(
            !text.contains("nested rule"),
            "nested bodies use progressive disclosure"
        );
    }
}
