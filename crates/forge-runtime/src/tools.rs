//! Tool definitions and dispatch for the agent loop. Every tool maps onto
//! the existing abstractions: file ops and commands go through
//! `ExecutionProvider`, graph queries through `ProjectGraph`. The runtime
//! never touches processes or the filesystem directly.

use std::path::PathBuf;
use std::sync::Arc;

use forge_core::{
    ExecRequest, ExecutionProvider, FileOp, ForgeError, ProjectGraph, RiskLevel, ToolCall,
    ToolDefinition, ToolResult,
};

/// The tool set offered to models with the `tools` capability.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let object = |properties: serde_json::Value, required: &[&str]| {
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    };
    let str_prop = |desc: &str| serde_json::json!({"type": "string", "description": desc});

    vec![
        ToolDefinition::new(
            "read_file",
            "Read a file's contents (project-root-relative path).",
            object(
                serde_json::json!({"path": str_prop("file path")}),
                &["path"],
            ),
        ),
        ToolDefinition::new(
            "write_file",
            "Write (create or overwrite) a file. Project-root-relative path.",
            object(
                serde_json::json!({
                    "path": str_prop("file path"),
                    "content": str_prop("full new file content"),
                }),
                &["path", "content"],
            ),
        ),
        ToolDefinition::new(
            "edit_file",
            "Replace an exact string in a file. Fails when `old` is absent or ambiguous.",
            object(
                serde_json::json!({
                    "path": str_prop("file path"),
                    "old": str_prop("exact text to replace"),
                    "new": str_prop("replacement text"),
                }),
                &["path", "old", "new"],
            ),
        ),
        ToolDefinition::new(
            "delete_file",
            "Delete a file. Destructive.",
            object(
                serde_json::json!({"path": str_prop("file path")}),
                &["path"],
            ),
        ),
        ToolDefinition::new(
            "run_command",
            "Run a shell command. Risk is at least `risky` regardless of the hint; \
             the hint can only raise it to `destructive`.",
            object(
                serde_json::json!({
                    "command": str_prop("command to run"),
                    "args": {"type": "array", "items": {"type": "string"}},
                    "risk": str_prop("risk hint: safe|risky|destructive"),
                }),
                &["command"],
            ),
        ),
        ToolDefinition::new(
            "graph_context",
            "Select the most relevant project files for a query (project graph).",
            object(
                serde_json::json!({"query": str_prop("relevance query")}),
                &["query"],
            ),
        ),
        ToolDefinition::new(
            "graph_grep",
            "Search project graph symbols and imports by pattern.",
            object(
                serde_json::json!({"pattern": str_prop("substring or regex")}),
                &["pattern"],
            ),
        ),
    ]
}

/// The outcome of dispatching one tool call.
pub struct ToolOutcome {
    pub result: ToolResult,
    /// Set when a Write/Edit/Delete actually changed the file.
    pub file_changed: Option<PathBuf>,
}

/// Maps tool calls onto execution and graph providers.
pub struct ToolDispatcher {
    exec: Arc<dyn ExecutionProvider>,
    graph: Option<Arc<dyn ProjectGraph>>,
}

fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing or invalid string argument {key:?}"))
}

/// Map a tool call onto a file operation: `None` when the tool is not a
/// file op at all, `Some(Err)` when its arguments don't type-check. Shared
/// by dispatch and [`minimum_dispatch_risk`] so the two can never disagree
/// about what a call would do.
fn file_op_for(call: &ToolCall) -> Option<Result<FileOp, String>> {
    let args = &call.arguments;
    let path_op = |make: fn(PathBuf) -> FileOp| match arg_str(args, "path") {
        Ok(path) => Some(Ok(make(PathBuf::from(path)))),
        Err(e) => Some(Err(e)),
    };
    match call.name.as_str() {
        "read_file" => path_op(|path| FileOp::Read { path }),
        "delete_file" => path_op(|path| FileOp::Delete { path }),
        "write_file" => Some(match (arg_str(args, "path"), arg_str(args, "content")) {
            (Ok(path), Ok(content)) => Ok(FileOp::Write {
                path: PathBuf::from(path),
                content,
            }),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }),
        "edit_file" => Some(
            match (
                arg_str(args, "path"),
                arg_str(args, "old"),
                arg_str(args, "new"),
            ) {
                (Ok(path), Ok(old), Ok(new)) => Ok(FileOp::Edit {
                    path: PathBuf::from(path),
                    old,
                    new,
                }),
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Err(e),
            },
        ),
        _ => None,
    }
}

/// `run_command`'s risk: the model's hint can only raise the level, never
/// lower it — a shell command is never below `Risky`.
fn command_risk(args: &serde_json::Value) -> RiskLevel {
    if arg_str(args, "risk")
        .unwrap_or_default()
        .eq_ignore_ascii_case("destructive")
    {
        RiskLevel::Destructive
    } else {
        RiskLevel::Risky
    }
}

/// The lowest risk level dispatching `call` could possibly be classified
/// at, or `None` for a call that cannot be dispatched at all (unknown tool,
/// arguments that don't type-check). Answered without dispatching anything,
/// so a caller can decide *whether* to dispatch — the seam the needle fast
/// path uses to stay out of approval territory entirely.
///
/// The rules are not restated here: file ops are handed to the real
/// [`FileOp::risk`], commands to the same [`command_risk`] clamp
/// `dispatch_inner` applies, and the graph tools never reach an
/// `ExecutionProvider` (so nothing can gate them). `FileOp::risk` needs a
/// project root to tell `Risky` from `Destructive`, which the tool layer
/// does not know; the sentinel root below is sound for this contract
/// because `Safe` is the one answer that cannot depend on the root (reads
/// return it before any path is examined) and because under-reporting
/// `Destructive` as `Risky` is exactly the "lowest possible" promise. Only
/// `Some(RiskLevel::Safe)` is therefore a guarantee: it means no approval
/// policy can gate this call (see `NativeExecution::check_approval`, which
/// returns early for `Safe`).
pub(crate) fn minimum_dispatch_risk(call: &ToolCall) -> Option<RiskLevel> {
    if let Some(op) = file_op_for(call) {
        return Some(op.ok()?.risk(std::path::Path::new("")));
    }
    match call.name.as_str() {
        "run_command" => Some(command_risk(&call.arguments)),
        "graph_context" | "graph_grep" => Some(RiskLevel::Safe),
        _ => None,
    }
}

fn arg_string_vec(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl ToolDispatcher {
    pub fn new(exec: Arc<dyn ExecutionProvider>, graph: Option<Arc<dyn ProjectGraph>>) -> Self {
        Self { exec, graph }
    }

    /// Dispatch a tool call. `Err` is reserved for approval pauses
    /// (`ForgeError::ApprovalRequired`); operational failures (bad args,
    /// missing files, unknown tools) become error `ToolResult`s so the
    /// model can react.
    pub async fn dispatch(&self, call: &ToolCall) -> Result<ToolOutcome, ForgeError> {
        self.dispatch_inner(call, false).await
    }

    /// Re-dispatch after an explicit recorded approval: file ops and
    /// commands bypass approval gating.
    pub async fn dispatch_approved(&self, call: &ToolCall) -> Result<ToolOutcome, ForgeError> {
        self.dispatch_inner(call, true).await
    }

    async fn dispatch_inner(
        &self,
        call: &ToolCall,
        approved: bool,
    ) -> Result<ToolOutcome, ForgeError> {
        let args = &call.arguments;
        let invalid = |message: String| {
            Ok(ToolOutcome {
                result: ToolResult::error(call.id.clone(), call.name.clone(), message),
                file_changed: None,
            })
        };

        // File operations.
        let file_op = match file_op_for(call) {
            Some(Ok(op)) => Some(op),
            Some(Err(e)) => return invalid(e),
            None => None,
        };
        if let Some(op) = file_op {
            let path = op.path().to_path_buf();
            let changed = !matches!(op, FileOp::Read { .. });
            let result = if approved {
                self.exec.file_op_approved(op).await
            } else {
                self.exec.file_op(op).await
            };
            return match result {
                Ok(r) => Ok(ToolOutcome {
                    result: ToolResult::ok(
                        call.id.clone(),
                        call.name.clone(),
                        if let Some(content) = r.content {
                            content
                        } else {
                            format!("{} succeeded", call.name)
                        },
                    ),
                    file_changed: if r.changed && changed {
                        Some(path)
                    } else {
                        None
                    },
                }),
                Err(e @ ForgeError::ApprovalRequired { .. }) => Err(e),
                Err(e) => Ok(ToolOutcome {
                    result: ToolResult::error(call.id.clone(), call.name.clone(), e.to_string()),
                    file_changed: None,
                }),
            };
        }

        match call.name.as_str() {
            "run_command" => {
                let command = match arg_str(args, "command") {
                    Ok(c) => c,
                    Err(e) => return invalid(e),
                };
                let risk = command_risk(args);
                let request = ExecRequest {
                    command: command.clone(),
                    args: arg_string_vec(args, "args"),
                    cwd: None,
                    risk,
                    inherit_stdio: false,
                    log_label: None,
                };
                let result = if approved {
                    self.exec.execute_approved(request).await
                } else {
                    self.exec.execute(request).await
                };
                match result {
                    Ok(r) => Ok(ToolOutcome {
                        result: ToolResult::ok(
                            call.id.clone(),
                            call.name.clone(),
                            format!(
                                "exit {}\nstdout:\n{}\nstderr:\n{}",
                                r.exit_code, r.stdout, r.stderr
                            ),
                        ),
                        file_changed: None,
                    }),
                    Err(e @ ForgeError::ApprovalRequired { .. }) => Err(e),
                    Err(e) => Ok(ToolOutcome {
                        result: ToolResult::error(
                            call.id.clone(),
                            call.name.clone(),
                            e.to_string(),
                        ),
                        file_changed: None,
                    }),
                }
            }
            "graph_context" => {
                let query = match arg_str(args, "query") {
                    Ok(q) => q,
                    Err(e) => return invalid(e),
                };
                let Some(graph) = &self.graph else {
                    return invalid("graph unavailable (no project graph built)".to_string());
                };
                let hits = graph.context(&query, 10);
                let text = if hits.is_empty() {
                    "no relevant files found".to_string()
                } else {
                    hits.iter()
                        .map(|h| format!("{} (score {})", h.path, h.score))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok(ToolOutcome {
                    result: ToolResult::ok(call.id.clone(), call.name.clone(), text),
                    file_changed: None,
                })
            }
            "graph_grep" => {
                let pattern = match arg_str(args, "pattern") {
                    Ok(p) => p,
                    Err(e) => return invalid(e),
                };
                let Some(graph) = &self.graph else {
                    return invalid("graph unavailable (no project graph built)".to_string());
                };
                match graph.grep(&pattern) {
                    Ok(matches) => {
                        let text = if matches.is_empty() {
                            "no matches".to_string()
                        } else {
                            matches
                                .iter()
                                .map(|m| format!("{}:{}: {}", m.file.display(), m.line, m.text))
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        Ok(ToolOutcome {
                            result: ToolResult::ok(call.id.clone(), call.name.clone(), text),
                            file_changed: None,
                        })
                    }
                    Err(e) => Ok(ToolOutcome {
                        result: ToolResult::error(
                            call.id.clone(),
                            call.name.clone(),
                            e.to_string(),
                        ),
                        file_changed: None,
                    }),
                }
            }
            other => invalid(format!("unknown tool {other:?}")),
        }
    }
}
