//! `forge chat` — the interactive conversation, and what `forge` with no
//! subcommand runs.
//!
//! This is the command *shell*: the things that have to be settled before
//! any transcript exists — the `--json` refusal, and building the
//! `CliHost` (which resolves configuration and constructs the whole
//! runtime, so a chat that cannot start fails here rather than mid-loop).
//! The transcript/prompt loop itself, including the entry banner (design
//! §7, session id and needle wording included), lives in `forge_chat::run`
//! — this module's job ends at handing it a `ChatIo` and a `Start`.

use std::io::IsTerminal;

use forge_chat::{SessionStart, Start};
use forge_core::ForgeError;

use crate::chat::host::CliHost;
use crate::chat::palette::Palette;
use crate::chat::piped_io::PipedIo;
use crate::chat::terminal_io::TerminalIo;
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

    let root = ctx.project_root()?;

    tracing::debug!(
        prompt_words = args.prompt.len(),
        continue_session = args.continue_session,
        session = ?args.session,
        "chat requested"
    );

    // Resolves configuration and builds the whole runtime (model, router,
    // execution, skills, needle) — a chat that cannot start over its own
    // config fails here, before anything has reached stdout, rather than
    // mid-loop.
    let host = CliHost::new(ctx).await?;

    let palette = Palette::detect(ctx.global.no_color);
    let start = Start {
        session: match (args.session, args.continue_session) {
            (Some(id), _) => SessionStart::Named(id),
            (None, true) => SessionStart::Continue,
            (None, false) => SessionStart::Fresh,
        },
        first_prompt: (!args.prompt.is_empty()).then(|| args.prompt.join(" ")),
    };

    // The entry banner (design §7) — session id and needle wording
    // included — is printed by `forge_chat::run` itself (`App::print_banner`,
    // over `ChatIo`), not here: those two facts are not available until
    // the session has started and this `CliHost` has resolved them.
    let code = if std::io::stdin().is_terminal() {
        let history_path = root.join(".forge").join("chat-history");
        let io = TerminalIo::new(palette, history_path)?;
        forge_chat::run(io, host, start).await?
    } else {
        let io = PipedIo::new(palette)?;
        forge_chat::run(io, host, start).await?
    };

    // `forge_chat::run` already calls `ChatIo::shutdown` on every normal
    // exit path itself (`App::drive`, right before returning) — and could
    // not be called again from here regardless, since `io` was moved into
    // `run`. Only the exit code crosses back.
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
