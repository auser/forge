use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_config::Config;
use forge_core::{CompletionRequest, ForgeError, Message, ModelProvider};

use crate::cli::ModelCommand;
use crate::commands::Context;

pub async fn run(ctx: &Context, command: ModelCommand) -> Result<(), ForgeError> {
    match command {
        ModelCommand::List { catalogue: false } => list(ctx),
        ModelCommand::List { catalogue: true } => list_catalogue(ctx),
        ModelCommand::Add { id } => add(ctx, &id),
        ModelCommand::Refresh => refresh(ctx).await,
        ModelCommand::Test { model } => test(ctx, model).await,
    }
}

/// Provider for `name` (default: the configured model), keeping the rest
/// of the resolved configuration (base URL, key env) intact.
fn provider_for(ctx: &Context, name: Option<&str>) -> Result<Arc<dyn ModelProvider>, ForgeError> {
    let resolved = ctx.resolve_config()?;
    let root = ctx.project_root()?;
    let mut config = resolved.config.clone();
    config.model = match name {
        Some(name) => name.to_string(),
        None if !config.explicit.contains("model") => forge_providers::automatic_model(&config)
            .unwrap_or_else(|| forge_providers::AUTH_REQUIRED_MODEL.to_string()),
        None => config.model.clone(),
    };
    forge_providers::model_from_config(&config, &root)
}

fn list(ctx: &Context) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    let active = provider_for(ctx, None)?;
    let caps = active.capabilities();
    let eligibility: std::collections::BTreeMap<String, forge_providers::ModelEligibility> =
        forge_providers::model_eligibility(&resolved.config)
            .into_iter()
            .map(|status| (status.name.clone(), status))
            .collect();
    let active_status = eligibility.get(active.name());
    let active_eligible = active_status.is_none_or(|status| status.eligible);
    let active_reason = active_status
        .map(|status| status.reason.as_str())
        .unwrap_or("explicitly selected");

    if ctx.global.json {
        let mut models = vec![serde_json::json!({
            "name": active.name(),
            "active": true,
            "eligible": active_eligible,
            "availability": active_reason,
            "capabilities": caps,
        })];
        for (name, entry) in resolved.config.model_entries() {
            let status = eligibility.get(name);
            models.push(serde_json::json!({
                "name": name,
                "active": false,
                "eligible": status.is_some_and(|status| status.eligible),
                "availability": status.map(|status| status.reason.as_str()).unwrap_or("not evaluated"),
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
            "{} (active, {}: {}) — streaming={} tools={} structured_output={} vision={} max_context={}",
            active.name(),
            if active_eligible {
                "eligible"
            } else {
                "ineligible"
            },
            active_reason,
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
            let status = eligibility.get(name);
            let desc = entry.description.as_deref().unwrap_or("");
            let costs = match entry.costs() {
                Some((input, output)) => format!("cost_in=${input}/1M cost_out=${output}/1M"),
                None => "unpriced".to_string(),
            };
            println!(
                "{name} — {}: {}; {costs} {} {}",
                if status.is_some_and(|status| status.eligible) {
                    "eligible"
                } else {
                    "ineligible"
                },
                status
                    .map(|status| status.reason.as_str())
                    .unwrap_or("not evaluated"),
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

/// `forge model list --catalogue`: the cached OpenRouter catalogue — every
/// brokered model with prices and context lengths. Reads the cache only;
/// an absent cache is a note, not an error (never fetched yet), and a stale
/// one is shown with its age named.
fn list_catalogue(ctx: &Context) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    let ttl = Duration::from_secs(resolved.config.catalogue_ttl_days * 86_400);
    let Some(cached) = forge_config::catalogue::load(&forge_config::catalogue::cache_path(), ttl)
    else {
        println!(
            "no cached OpenRouter catalogue — run `forge model refresh` (or `forge init`) to fetch one"
        );
        return Ok(());
    };
    if let Some(warning) = cached.staleness_warning() {
        eprintln!("warning: {warning}");
    }
    let mut models = cached.catalogue.models.clone();
    models.sort_by(|a, b| a.id.cmp(&b.id));

    if ctx.global.json {
        let entries: Vec<serde_json::Value> = models
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "context_length": m.context_length,
                    "cost_input_per_mtok": m.cost_input_per_mtok,
                    "cost_output_per_mtok": m.cost_output_per_mtok,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "fetched_at": cached.catalogue.fetched_at,
                "stale": cached.is_stale(),
                "models": entries,
            }))
            .map_err(|e| ForgeError::provider(format!("serializing catalogue: {e}")))?
        );
        return Ok(());
    }

    for m in &models {
        let costs = match m.costs() {
            Some((input, output)) => format!("cost_in=${input}/1M cost_out=${output}/1M"),
            None => "unpriced".to_string(),
        };
        let context = m
            .context_length
            .map(|c| format!("max_context={c}"))
            .unwrap_or_default();
        println!("{} — {costs} {context}", m.id);
    }
    println!(
        "({} models, fetched {})",
        models.len(),
        cached.catalogue.fetched_at
    );
    Ok(())
}

/// `forge model add <id>`: declare a catalogue model in the project config.
/// The operator's config stays the sole declaration of what forge may route
/// to — this command makes declaring one accurate rather than expanding the
/// pool itself. `tools` is deliberately NOT written: OpenRouter does not
/// report tool support reliably enough to trust, and guessing wrong means
/// the agent loop hands tools to a model that ignores them.
fn add(ctx: &Context, id: &str) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    if resolved
        .config
        .explicit
        .contains(&forge_config::keys::model_entry(id))
    {
        return Err(ForgeError::config(format!(
            "[models.{id}] is already declared in configuration; edit it by hand"
        )));
    }

    let ttl = Duration::from_secs(resolved.config.catalogue_ttl_days * 86_400);
    let Some(cached) = forge_config::catalogue::load(&forge_config::catalogue::cache_path(), ttl)
    else {
        return Err(ForgeError::config(
            "no cached OpenRouter catalogue — run `forge model refresh` first".to_string(),
        ));
    };
    let Some(model) = cached.catalogue.model(id) else {
        return Err(ForgeError::config(format!(
            "{id:?} is not in the cached catalogue; check `forge model list --catalogue` \
             (or run `forge model refresh`)"
        )));
    };

    let mut entry = toml::Table::new();
    entry.insert(
        "base_url".to_string(),
        toml::Value::String(format!(
            "{}/api/v1",
            forge_providers::openrouter::base_url().trim_end_matches('/')
        )),
    );
    entry.insert(
        "key_env".to_string(),
        toml::Value::String("OPENROUTER_API_KEY".to_string()),
    );
    if let Some(context_length) = model.context_length {
        entry.insert(
            "max_context".to_string(),
            toml::Value::Integer(context_length as i64),
        );
    }
    if let Some(input) = model.cost_input_per_mtok {
        entry.insert("cost_input_per_mtok".to_string(), toml::Value::Float(input));
    }
    if let Some(output) = model.cost_output_per_mtok {
        entry.insert(
            "cost_output_per_mtok".to_string(),
            toml::Value::Float(output),
        );
    }
    let mut models = toml::Table::new();
    models.insert(id.to_string(), toml::Value::Table(entry));
    let mut section = toml::Table::new();
    section.insert("models".to_string(), toml::Value::Table(models));
    let block = toml::to_string(&section)
        .map_err(|e| ForgeError::config(format!("serializing model entry: {e}")))?;

    let root = ctx.project_root()?;
    let path = Config::project_config_path(&root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ForgeError::Io)?;
    }
    let mut contents = std::fs::read_to_string(&path).unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    if !contents.is_empty() {
        contents.push('\n');
    }
    contents.push_str(&block);
    std::fs::write(&path, contents).map_err(ForgeError::Io)?;

    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({ "added": id, "path": path, "tools": null })
        );
    } else {
        println!("added [models.{id}] to {}", path.display());
        println!(
            "note: `tools` left unset — OpenRouter does not report tool support reliably; \
             add `tools = true` yourself if this model supports it"
        );
    }
    Ok(())
}

/// `forge model refresh`: re-fetch the catalogue into the cache. The only
/// place besides `forge init` a fetch ever happens; under `--local-only`
/// the fetch refuses (the catalogue is a network source like any other).
async fn refresh(ctx: &Context) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    let (count, path) = forge_providers::openrouter::refresh_cache(&resolved.config).await?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({ "refreshed": count, "path": path })
        );
    } else {
        println!(
            "refreshed OpenRouter catalogue: {count} models → {}",
            path.display()
        );
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
