//! `forge chat` — the interactive conversation, and what `forge` with no
//! subcommand runs.
//!
//! This is the command *shell*: the things that have to be settled before
//! any transcript exists (the `--json` refusal, config resolution, the
//! entry banner). The transcript/prompt loop itself lives in the pure
//! `forge-chat` crate and is wired in later; until then this prints the
//! banner, says on stderr that the loop is not here yet, and exits 0.

use forge_core::ForgeError;

use crate::commands::Context;

/// Everything `forge chat` accepts, shared with the no-subcommand path so
/// the two cannot drift. `Default` *is* the no-subcommand invocation: a
/// fresh session, no pre-filled first turn.
#[derive(Clone, Debug, Default)]
pub struct ChatArgs {
    /// Optional first turn; the chat stays interactive afterwards.
    pub prompt: Vec<String>,
    /// Continue the project's most recently active session.
    pub continue_session: bool,
    /// Continue a named session.
    pub session: Option<String>,
}

/// Open the interactive chat.
pub async fn run(ctx: &Context, args: ChatArgs) -> Result<(), ForgeError> {
    // Refused, never reinterpreted: `--json` promises stdout is a single
    // machine-readable value, and a conversation cannot honour that. This
    // comes first, so nothing has reached stdout when it fails.
    if ctx.global.json {
        return Err(ForgeError::config(
            "--json is not supported by the interactive chat; use `forge run --json` for \
             machine-readable output",
        ));
    }

    // Resolved before the banner: a chat must not open over a config it
    // cannot read, and the banner reports what this resolution decided.
    let resolved = ctx.resolve_config()?;
    let root = ctx.project_root()?;

    tracing::debug!(
        prompt_words = args.prompt.len(),
        continue_session = args.continue_session,
        session = ?args.session,
        "chat requested"
    );

    // The entry banner (design §7). Two facts it cannot state yet, both
    // owned by later work rather than guessed at here: the session id
    // (there is no session until the loop starts one) and `brain
    // active`/`brain off (...)`, which comes from the chat host's
    // `Environment::needle` so that it and `forge doctor` cannot tell a
    // user different stories.
    println!("forge {}  {}", env!("CARGO_PKG_VERSION"), root.display());
    println!(
        "model {}  router {}  approval {}",
        resolved.config.model, resolved.config.router, resolved.config.approval
    );
    println!("/help for commands");

    // Diagnostic, so stdout stays the transcript.
    eprintln!("note: the interactive chat is not wired up yet; this is the banner only");
    Ok(())
}
