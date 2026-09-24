use std::sync::Arc;

use forge_core::{Event, EventKind, ForgeError, SessionStore, SkillRegistry};
use forge_execution::MockExecution;
use forge_session::{JsonlSessionStore, new_run_id};
use forge_skills::FsSkillRegistry;

use crate::cli::SkillCommand;
use crate::commands::Context;
use crate::commands::service::build_execution;

/// Session id used for CLI-side lifecycle events such as skill
/// activation (kept out of per-run sessions).
const CLI_SESSION: &str = "cli";

pub async fn run(ctx: &Context, command: SkillCommand) -> Result<(), ForgeError> {
    match command {
        SkillCommand::List => list(ctx),
        SkillCommand::Show { name } => show(ctx, &name),
        SkillCommand::Test { name } => test(ctx, &name).await,
    }
}

fn registry(ctx: &Context) -> Result<FsSkillRegistry, ForgeError> {
    Ok(FsSkillRegistry::new(&ctx.project_root()?, None))
}

fn list(ctx: &Context) -> Result<(), ForgeError> {
    let registry = registry(ctx)?;
    let skills = registry.list();
    if ctx.global.json {
        let out: Vec<serde_json::Value> = skills
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "description": m.description,
                    "path": m.path,
                    "source": registry
                        .source_of(&m.path)
                        .map(|s| s.label())
                        .unwrap_or("unknown"),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::skill(format!("serializing skills: {e}")))?
        );
    } else if skills.is_empty() {
        println!("no skills discovered");
    } else {
        for m in &skills {
            let source = registry
                .source_of(&m.path)
                .map(|s| s.label())
                .unwrap_or("unknown");
            println!("{} — {} ({source})", m.name, m.description);
        }
    }
    Ok(())
}

/// `forge skill show <name>`: activate (full instructions) and log the
/// `SkillActivated` event to the session store. The registry itself stays
/// log-free; activation logging is the caller's job.
fn show(ctx: &Context, name: &str) -> Result<(), ForgeError> {
    let registry = registry(ctx)?;
    let skill = registry.activate(name)?;

    let store = JsonlSessionStore::new(ctx.project_root()?.join(".forge").join("sessions"));
    store.append(Event::new(
        new_run_id(),
        CLI_SESSION,
        EventKind::SkillActivated {
            name: skill.meta.name.clone(),
            path: skill.meta.path.clone(),
        },
    ))?;

    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "name": skill.meta.name,
                "description": skill.meta.description,
                "path": skill.meta.path,
                "references": registry.references(name)?,
                "instructions": skill.instructions,
            }))
            .map_err(|e| ForgeError::skill(format!("serializing skill: {e}")))?
        );
    } else {
        println!("skill: {}", skill.meta.name);
        println!("description: {}", skill.meta.description);
        println!("path: {}", skill.meta.path.display());
        let references = registry.references(name)?;
        if !references.is_empty() {
            println!("references:");
            for r in &references {
                println!("  {}", r.display());
            }
        }
        println!("---");
        print!("{}", skill.instructions);
        if !skill.instructions.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

/// `forge skill test <name>`: run the skill's test script through the
/// configured execution provider.
async fn test(ctx: &Context, name: &str) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    let root = ctx.project_root()?;

    // Keep a handle to the mock so we can report what would have run.
    // Gated exactly like `build_execution`'s `mock` arm (which the other
    // branch goes through): this reads the same config key, so it must not
    // become a way around the gate.
    let mock = if resolved.config.execution == "mock" {
        forge_config::ensure_test_mocks_allowed("execution = \"mock\"")?;
        Some(MockExecution::new(&root))
    } else {
        None
    };
    let exec: std::sync::Arc<dyn forge_core::ExecutionProvider> = match &mock {
        Some(m) => Arc::new(m.clone()),
        None => build_execution(&resolved.config, &root)?,
    };
    let registry = FsSkillRegistry::new(&root, Some(exec));

    match registry.test_skill(name).await? {
        None => {
            if ctx.global.json {
                println!(
                    "{}",
                    serde_json::json!({ "skill": name, "tested": false, "reason": "no test script" })
                );
            } else {
                println!("skill {name:?} has no test script (test.sh/test.py)");
            }
        }
        Some(result) => {
            if ctx.global.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "skill": name,
                        "tested": true,
                        "exit_code": result.exit_code,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                    }))
                    .map_err(|e| ForgeError::skill(format!("serializing result: {e}")))?
                );
            } else {
                println!("skill {name:?} test: exit {}", result.exit_code);
                if !result.stdout.trim().is_empty() {
                    println!("stdout:\n{}", result.stdout.trim_end());
                }
                if !result.stderr.trim().is_empty() {
                    println!("stderr:\n{}", result.stderr.trim_end());
                }
            }
            if let Some(mock) = &mock {
                for recorded in mock.recorded() {
                    println!(
                        "(mock execution recorded: {} {})",
                        recorded.command,
                        recorded.args.join(" ")
                    );
                }
            }
            if !result.success() {
                return Err(ForgeError::skill(format!(
                    "skill {name:?} test failed with exit {}",
                    result.exit_code
                )));
            }
        }
    }
    Ok(())
}
