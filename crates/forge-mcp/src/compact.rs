//! An opt-in discovery surface over the existing registry, not a second registry.

use serde_json::{Value, json};

use crate::tools::{ForgeTools, ToolDef, ToolError, ToolOutcome, definitions};

pub(crate) const INSTRUCTIONS: &str = "Discover native tools with forge_tools_search (optional \
query keywords and limit; pass {} for the defaults), retrieve their inputSchema with forge_tools_schema, then call \
forge_tools_invoke with name and arguments. The wrapper never authorizes operations: all \
underlying approval requirements remain in effect. A run with status waiting_for_approval \
requires an explicit answer through forge_run_input via forge_tools_invoke (\"y\" approves).";

pub(crate) fn definitions_compact() -> &'static [ToolDef] {
    &[
        ToolDef {
            name: "forge_tools_search",
            title: "Search Forge tools",
            description: "Find native tools by case-insensitive keywords across name, title and description. All keywords must match; results are sorted by name.",
            input_schema: || {
                json!({
                    "type": "object", "additionalProperties": false,
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 10}
                    }
                })
            },
        },
        ToolDef {
            name: "forge_tools_schema",
            title: "Get Forge tool schema",
            description: "Get the input schema of a native Forge tool before invoking it.",
            input_schema: || {
                json!({
                    "type": "object", "additionalProperties": false,
                    "required": ["name"],
                    "properties": {"name": {"type": "string", "minLength": 1}}
                })
            },
        },
        ToolDef {
            name: "forge_tools_invoke",
            title: "Invoke Forge tool",
            description: "Invoke a native Forge tool, preserving its results and approval requirements. This wrapper never authorizes operations.",
            input_schema: || {
                json!({
                    "type": "object", "additionalProperties": false,
                    "required": ["name", "arguments"],
                    "properties": {
                        "name": {"type": "string", "minLength": 1},
                        "arguments": {"type": "object"}
                    }
                })
            },
        },
    ]
}

fn summary(def: &ToolDef) -> Value {
    json!({"name": def.name, "title": def.title, "description": def.description})
}

pub(crate) async fn call(
    tools: &ForgeTools,
    name: &str,
    args: &Value,
) -> Result<ToolOutcome, ToolError> {
    let allowed: &[&str] = match name {
        "forge_tools_search" => &["query", "limit"],
        "forge_tools_schema" => &["name"],
        "forge_tools_invoke" => &["name", "arguments"],
        _ => return Err(ToolError::UnknownTool(name.to_owned())),
    };
    let invalid = || ToolOutcome::error("invalid_params", "invalid discovery tool arguments");
    let Some(args) = args.as_object() else {
        return Ok(invalid());
    };
    if args.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Ok(invalid());
    }
    if name == "forge_tools_search" {
        let query = match args.get("query") {
            None => "",
            Some(Value::String(query)) => query,
            _ => return Ok(invalid()),
        };
        let limit = match args.get("limit") {
            None => 10,
            Some(value) => match value.as_u64() {
                Some(limit @ 1..=100) => limit as usize,
                _ => return Ok(invalid()),
            },
        };
        let query = query.to_lowercase();
        let keywords: Vec<_> = query.split_whitespace().collect();
        let mut matches: Vec<_> = definitions()
            .iter()
            .filter(|def| {
                let text = format!("{} {} {}", def.name, def.title, def.description).to_lowercase();
                keywords.iter().all(|keyword| text.contains(keyword))
            })
            .collect();
        matches.sort_by_key(|def| def.name);
        let total = matches.len();
        return Ok(ToolOutcome::ok(json!({
            "tools": matches.into_iter().take(limit).map(summary).collect::<Vec<_>>(),
            "total": total
        })));
    }
    let Some(inner) = args.get("name").and_then(Value::as_str) else {
        return Ok(invalid());
    };
    if inner.trim().is_empty() {
        return Ok(invalid());
    }
    if name == "forge_tools_invoke" && !args.get("arguments").is_some_and(Value::is_object) {
        return Ok(invalid());
    }
    let Some(def) = definitions().iter().find(|def| def.name == inner) else {
        return Ok(ToolOutcome::error(
            "unknown_tool",
            format!("unknown tool: {inner}"),
        ));
    };
    if name == "forge_tools_schema" {
        let mut value = summary(def);
        value["inputSchema"] = (def.input_schema)();
        return Ok(ToolOutcome::ok(value));
    }
    // Deliberately bypass meta dispatch. The native runtime owns all approval decisions.
    Ok(match tools.call(inner, &args["arguments"]).await {
        Ok(outcome) => outcome,
        Err(ToolError::UnknownTool(name)) => {
            ToolOutcome::error("unknown_tool", format!("unknown tool: {name}"))
        }
    })
}
