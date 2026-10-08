use forge_core::ForgeError;
use std::io::{IsTerminal, Write};

use crate::cli::AuthProvider;
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

/// Delegate subscription authentication to each provider's official CLI.
/// Forge never handles a password, browser callback, refresh token, or
/// device-code secret; it only reads the credential store the provider CLI
/// owns after a successful login.
pub fn login(ctx: &Context, provider: AuthProvider) -> Result<(), ForgeError> {
    if ctx.global.json {
        return Err(ForgeError::config(
            "`forge auth login` is interactive and cannot be combined with --json",
        ));
    }

    let (binary, args, probe_name, install_hint): (&str, &[&str], &str, &str) = match provider {
        AuthProvider::Claude => (
            "claude",
            &["auth", "login"],
            "anthropic",
            "install Claude Code, then rerun `forge auth login claude`",
        ),
        AuthProvider::Codex => (
            "codex",
            &["login", "-c", "cli_auth_credentials_store=\"file\""],
            "codex",
            "install Codex CLI, then rerun `forge auth login codex`",
        ),
        AuthProvider::Kimi => (
            "kimi",
            &["login"],
            "kimi-code",
            "install Kimi Code CLI, then rerun `forge auth login kimi`",
        ),
    };

    eprintln!("Opening {binary} subscription sign-in...");
    let exit = std::process::Command::new(binary)
        .args(args)
        .status()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ForgeError::config(format!("{binary} is not installed; {install_hint}"))
            } else {
                ForgeError::Io(error)
            }
        })?;
    if !exit.success() {
        return Err(ForgeError::config(format!(
            "{binary} login exited with {exit}; no Forge configuration was changed"
        )));
    }

    let probe = forge_providers::probe_auth()
        .into_iter()
        .find(|probe| probe.provider == probe_name);
    match probe {
        Some(probe) if probe.detected => {
            println!(
                "authenticated with {} — Forge can use {}",
                match provider {
                    AuthProvider::Claude => "Claude",
                    AuthProvider::Codex => "Codex",
                    AuthProvider::Kimi => "Kimi",
                },
                probe.usable_models.join(", ")
            );
            Ok(())
        }
        _ => Err(ForgeError::config(format!(
            "{binary} reported a successful login, but Forge could not read its credential store; \
             run `{binary} login` once more, then `forge auth status`"
        ))),
    }
}

/// First interactive chat with no configured or discoverable model: offer the
/// only setup step Forge cannot do on the user's behalf. This runs before
/// rustyline starts, so the provider CLI owns the terminal during its
/// browser/device flow.
pub fn ensure_for_chat(ctx: &Context) -> Result<(), ForgeError> {
    let resolved = ctx.resolve_config()?;
    if resolved.config.explicit.contains("model")
        || forge_providers::automatic_model(&resolved.config).is_some()
        || !std::io::stdin().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        return Ok(());
    }

    eprintln!();
    eprintln!("No working generation model is authenticated.");
    eprintln!("Sign in with a subscription:");
    eprintln!("  1  Claude");
    eprintln!("  2  Codex");
    eprintln!("  3  Kimi");
    eprint!("Choose 1-3, or press Enter to continue without a model: ");
    std::io::stderr().flush().map_err(ForgeError::Io)?;

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(ForgeError::Io)?;
    let provider = match answer.trim().to_ascii_lowercase().as_str() {
        "1" | "claude" => Some(AuthProvider::Claude),
        "2" | "codex" => Some(AuthProvider::Codex),
        "3" | "kimi" => Some(AuthProvider::Kimi),
        "" => None,
        _ => {
            eprintln!("No provider selected; run `forge auth login <claude|codex|kimi>` any time.");
            None
        }
    };
    match provider {
        Some(provider) => login(ctx, provider),
        None => Ok(()),
    }
}
