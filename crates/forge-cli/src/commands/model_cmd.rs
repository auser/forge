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
        let mut models = vec![serde_json::json!({
            "name": active.name(),
            "active": true,
            "capabilities": caps,
        })];
        for (name, entry) in resolved.config.model_entries() {
            models.push(serde_json::json!({
                "name": name,
                "active": false,
                "description": entry.description,
                "cost_input_per_mtok": entry.cost_input_per_mtok,
                "cost_output_per_mtok": entry.cost_output_per_mtok,
                "base_url": entry.base_url,
                "capabilities": entry.capabilities_if_known(),
            }));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "models": models }))
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
        // `mock-local` is a test-only provider (forge-providers'
        // `test_mocks`): advertising it as an available model is how users
        // ended up running forge against something that answers
        // "mock response to: …". Listed only when the gate is open, and
        // labelled for what it is.
        if forge_config::test_mocks_allowed()
            && resolved.config.model != "mock-local"
            && resolved.config.model != "mock"
        {
            println!("mock-local (test-only mock, available offline)");
        }
        for (name, entry) in resolved.config.model_entries() {
            let desc = entry.description.as_deref().unwrap_or("");
            println!(
                "{name} — cost_in=${}/1M cost_out=${}/1M {} {}",
                entry.cost_input_per_mtok,
                entry.cost_output_per_mtok,
                entry
                    .base_url
                    .as_deref()
                    .map(|u| format!("base_url={u}"))
                    .unwrap_or_default(),
                desc
            );
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
