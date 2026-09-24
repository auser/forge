use std::io::{BufRead, IsTerminal};
use std::sync::Arc;

use forge_core::ForgeError;
use forge_runtime::RunOptions;

use crate::commands::Context;
use crate::commands::service::build_run_service;

/// `forge run <prompt...>` — run the agent loop, print the final text.
///
/// When stdin is not a terminal, a feeder task forwards stdin lines into
/// the run's input channel so approvals can be answered non-interactively
/// (`echo y | forge run ...`). At stdin EOF the channel is closed and a
/// pending approval fails the run cleanly instead of hanging.
pub async fn run(
    ctx: &Context,
    prompt: Vec<String>,
    max_turns: Option<u32>,
) -> Result<(), ForgeError> {
    let prompt = prompt.join(" ");
    let service = Arc::new(build_run_service(ctx).await?);

    let run_id = forge_session::new_run_id();
    let feeder = spawn_stdin_feeder(&service, &run_id);

    let result = service
        .run_with_options(
            &prompt,
            RunOptions {
                run_id: Some(run_id),
                max_turns,
                ..RunOptions::default()
            },
        )
        .await;

    if let Some(handle) = feeder {
        handle.abort();
    }
    let outcome = result?;

    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome)
                .map_err(|e| ForgeError::session(format!("serializing run outcome: {e}")))?
        );
    } else {
        println!("{}", outcome.text);
    }
    Ok(())
}

/// Forward stdin lines to the run's input channel. Only spawned when
/// stdin is not a terminal (on a TTY the execution provider prompts
/// inline instead). Returns None for TTY stdin.
fn spawn_stdin_feeder(
    service: &Arc<forge_runtime::AgentService>,
    run_id: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let service = Arc::clone(service);
    let run_id = run_id.to_string();
    Some(tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match lock.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if service.send_input(&run_id, line.trim_end()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // stdin exhausted: no more approvals can arrive.
        service.close_input(&run_id);
    }))
}
