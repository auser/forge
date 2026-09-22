use forge_core::ForgeError;

use crate::commands::Context;
use crate::commands::service::build_service;

/// `forge run <prompt...>` — route, complete, print the model's text.
pub async fn run(ctx: &Context, prompt: Vec<String>) -> Result<(), ForgeError> {
    let prompt = prompt.join(" ");
    let service = build_service(ctx)?;
    let outcome = service.run(&prompt).await?;

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
