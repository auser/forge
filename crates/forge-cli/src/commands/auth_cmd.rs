use forge_core::ForgeError;

use crate::commands::Context;

/// `forge auth status` — report detected credentials: provider, usable
/// models, source, kind. Values are never printed.
pub fn status(ctx: &Context) -> Result<(), ForgeError> {
    let probes = forge_providers::probe_auth();
    if ctx.global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&probes)
                .map_err(|e| ForgeError::provider(format!("serializing auth status: {e}")))?
        );
        return Ok(());
    }
    for probe in &probes {
        let detected = match (&probe.source, &probe.kind) {
            (Some(source), Some(kind)) => format!(
                "{source} ({})",
                match kind {
                    forge_providers::CredentialKind::ApiKey => "api-key",
                    forge_providers::CredentialKind::OAuthToken => "oauth",
                }
            ),
            (Some(source), None) => source.to_string(),
            _ => "not found".to_string(),
        };
        println!(
            "{:<10} {:<16} {}",
            probe.provider,
            probe.usable_models.join(","),
            detected
        );
        if let Some(note) = &probe.note {
            println!("           note: {note}");
        }
    }
    Ok(())
}
