use forge_core::ForgeError;

use crate::cli::ConfigCommand;
use crate::commands::Context;

pub fn run(ctx: &Context, command: ConfigCommand) -> Result<(), ForgeError> {
    match command {
        ConfigCommand::Show => show(ctx),
        ConfigCommand::Path => path(ctx),
        ConfigCommand::Explain { key } => explain(ctx, &key),
    }
}

fn show(ctx: &Context) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    if ctx.global.json {
        let out = serde_json::json!({
            "config": resolved.config,
            "sources": resolved.sources,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::config(format!("serializing config as JSON: {e}")))?
        );
    } else {
        let text = toml::to_string_pretty(&resolved.config)
            .map_err(|e| ForgeError::config(format!("serializing config as TOML: {e}")))?;
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

fn path(ctx: &Context) -> Result<(), ForgeError> {
    let user = forge_config::Config::user_config_path();
    let project_root = ctx.project_root()?;
    let project = forge_config::Config::project_config_path(&project_root);

    if ctx.global.json {
        let out = serde_json::json!({
            "user": { "path": user, "exists": user.is_file() },
            "project": { "path": project, "exists": project.is_file() },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::config(format!("serializing paths as JSON: {e}")))?
        );
    } else {
        let mark = |p: &std::path::Path| if p.is_file() { "exists" } else { "missing" };
        println!("user:    {} ({})", user.display(), mark(&user));
        println!("project: {} ({})", project.display(), mark(&project));
    }
    Ok(())
}

fn explain(ctx: &Context, key: &str) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    match resolved.explain(key) {
        Some((value, origin)) => {
            if ctx.global.json {
                let out = serde_json::json!({
                    "key": key,
                    "value": value,
                    "origin": origin,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&out)
                        .map_err(|e| ForgeError::config(format!("serializing as JSON: {e}")))?
                );
            } else {
                println!("{key} = {value} (source: {origin})");
            }
            Ok(())
        }
        None => Err(ForgeError::config(format!(
            "unknown configuration key: {key}"
        ))),
    }
}
