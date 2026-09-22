use std::sync::Arc;
use std::time::Instant;

use forge_config::Config;
use forge_core::{CompletionRequest, ForgeError, Message, ModelProvider};

use crate::cli::ModelCommand;
use crate::commands::Context;

pub async fn run(ctx: &Context, command: ModelCommand) -> Result<(), ForgeError> {
    match command {
        ModelCommand::List => list(ctx),
        ModelCommand::Test { model } => test(ctx, model).await,
    }
}

/// Provider for `name` (default: the configured model), keeping the rest
/// of the resolved configuration (base URL, key env) intact.
fn provider_for(ctx: &Context, name: Option<&str>) -> Result<Arc<dyn ModelProvider>, ForgeError> {
    let resolved = ctx.resolve_config()?;
    let root = ctx.project_root()?;
    let model = name.unwrap_or(&resolved.config.model).to_string();
    forge_providers::model_from_config(
        &Config {
            model,
            ..resolved.config.clone()
        },
        &root,
    )
}

fn list(ctx: &Context) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    let active = provider_for(ctx, None)?;
    let caps = active.capabilities();

    if ctx.global.json {
        let out = serde_json::json!({
            "models": [{
                "name": active.name(),
                "active": true,
                "capabilities": caps,
            }],
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out)
                .map_err(|e| ForgeError::provider(format!("serializing models: {e}")))?
        );
    } else {
        println!(
            "{} (active) — streaming={} tools={} structured_output={} vision={} max_context={}",
            active.name(),
            caps.streaming,
            caps.tools,
            caps.structured_output,
            caps.vision,
            caps.max_context
        );
        if resolved.config.model != "mock-local" && resolved.config.model != "mock" {
            println!("mock-local (built-in, available offline)");
        }
    }
    Ok(())
}

async fn test(ctx: &Context, model: Option<String>) -> Result<(), ForgeError> {
    let provider = provider_for(ctx, model.as_deref())?;
    let started = Instant::now();
    let result = provider
        .complete(CompletionRequest::new(
            provider.name().to_string(),
            vec![Message::user("ping")],
        ))
        .await;
    let latency = started.elapsed();

    match result {
        Ok(response) => {
            if ctx.global.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "model": provider.name(),
                        "ok": true,
                        "latency_ms": latency.as_millis(),
                        "sample": response.content,
                    })
                );
            } else {
                println!(
                    "{}: ok ({} ms) — {}",
                    provider.name(),
                    latency.as_millis(),
                    response.content
                );
            }
            Ok(())
        }
        Err(e) => {
            if ctx.global.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "model": provider.name(),
                        "ok": false,
                        "latency_ms": latency.as_millis(),
                        "error": e.to_string(),
                    })
                );
            } else {
                println!(
                    "{}: FAILED ({} ms) — {e}",
                    provider.name(),
                    latency.as_millis()
                );
            }
            Err(e)
        }
    }
}
