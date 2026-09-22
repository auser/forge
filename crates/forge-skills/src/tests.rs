use std::path::Path;
use std::sync::Arc;

use forge_core::{RiskLevel, SkillRegistry};
use forge_execution::MockExecution;

use super::*;

fn write_skill(root: &Path, dir: &str, content: &str) {
    let path = root.join(dir);
    std::fs::create_dir_all(&path).expect("mkdir");
    std::fs::write(path.join("SKILL.md"), content).expect("write SKILL.md");
}

fn registry(root: &Path) -> FsSkillRegistry {
    FsSkillRegistry::with_roots(
        vec![
            (SkillSource::ProjectForge, root.join(".forge/skills")),
            (SkillSource::UserForge, root.join("user/forge/skills")),
        ],
        None,
    )
}

#[test]
fn discovers_skills_with_frontmatter() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/demo",
        "---\nname: demo\ndescription: Demo skill\n---\n# Demo\n\nFull instructions here.\n",
    );
    let reg = registry(tmp.path());
    let list = reg.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "demo");
    assert_eq!(list[0].description, "Demo skill");
}

#[test]
fn falls_back_to_dir_name_and_first_prose_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/plain",
        "# Plain Skill\n\nDoes plain things.\n",
    );
    let reg = registry(tmp.path());
    let list = reg.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "plain");
    assert_eq!(list[0].description, "Does plain things.");
}

#[test]
fn project_roots_shadow_user_roots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/dup",
        "---\nname: dup\ndescription: project version\n---\nbody\n",
    );
    write_skill(
        tmp.path(),
        "user/forge/skills/dup",
        "---\nname: dup\ndescription: user version\n---\nbody\n",
    );
    write_skill(
        tmp.path(),
        "user/forge/skills/user-only",
        "---\nname: user-only\ndescription: only in user root\n---\nbody\n",
    );
    let reg = registry(tmp.path());
    let list = reg.list();
    assert_eq!(list.len(), 2);
    let dup = list.iter().find(|m| m.name == "dup").expect("dup");
    assert_eq!(dup.description, "project version");
    assert!(list.iter().any(|m| m.name == "user-only"));
}

#[test]
fn activate_loads_full_instructions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/demo",
        "---\nname: demo\ndescription: d\n---\n# Demo\n\nStep 1. Step 2.\n",
    );
    let reg = registry(tmp.path());
    let skill = reg.activate("demo").expect("activate");
    assert!(skill.instructions.contains("Step 1. Step 2."));
    assert!(!skill.instructions.contains("description: d"));
    assert!(matches!(
        reg.activate("nope"),
        Err(forge_core::ForgeError::Skill(_))
    ));
}

#[test]
fn references_lists_extra_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/demo",
        "---\nname: demo\n---\nbody\n",
    );
    std::fs::write(tmp.path().join(".forge/skills/demo/helper.sh"), "echo hi\n").expect("write");
    let reg = registry(tmp.path());
    let refs = reg.references("demo").expect("references");
    assert_eq!(refs.len(), 1);
    assert!(refs[0].ends_with("helper.sh"));
}

#[tokio::test]
async fn test_skill_runs_through_execution_provider() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/demo",
        "---\nname: demo\n---\nbody\n",
    );
    std::fs::write(tmp.path().join(".forge/skills/demo/test.sh"), "exit 0\n").expect("write");

    let exec = Arc::new(MockExecution::new());
    let reg = FsSkillRegistry::with_roots(
        vec![(SkillSource::ProjectForge, tmp.path().join(".forge/skills"))],
        Some(exec.clone()),
    );

    let result = reg.test_skill("demo").await.expect("test run");
    assert!(result.is_some());
    let recorded = exec.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].command, "sh");
    assert!(recorded[0].args[0].ends_with("test.sh"));
    assert_eq!(recorded[0].risk, RiskLevel::Risky);
}

#[tokio::test]
async fn test_skill_returns_none_without_test_script() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/demo",
        "---\nname: demo\n---\nbody\n",
    );
    let reg = FsSkillRegistry::with_roots(
        vec![(SkillSource::ProjectForge, tmp.path().join(".forge/skills"))],
        Some(Arc::new(MockExecution::new())),
    );
    assert!(reg.test_skill("demo").await.expect("ok").is_none());
}

#[test]
fn match_task_finds_relevant_skills() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(
        tmp.path(),
        ".forge/skills/deploy",
        "---\nname: deploy\ndescription: Deploy the service to staging\n---\nbody\n",
    );
    write_skill(
        tmp.path(),
        ".forge/skills/lint",
        "---\nname: lint\ndescription: Run the linter\n---\nbody\n",
    );
    let reg = registry(tmp.path());
    let matches = reg.match_task("please deploy this to staging");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "deploy");
    assert!(reg.match_task("unrelated question").is_empty());
}
