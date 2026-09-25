pub mod acp_cmd;
pub mod auth_cmd;
pub mod chat_cmd;
pub mod config_cmd;
pub mod doctor;
pub mod graph_cmd;
pub mod init;
pub mod mcp_cmd;
pub mod model_cmd;
pub mod router_cmd;
pub mod run_cmd;
pub mod serve_cmd;
pub mod service;
pub mod session_cmd;
pub mod skill_cmd;

use std::path::PathBuf;

use forge_config::CliOverrides;
use forge_core::{ForgeError, find_project_root};

use crate::cli::{Cli, Command, GlobalOpts, SessionCommand};

/// Per-invocation context derived from global flags.
pub struct Context {
    pub global: GlobalOpts,
}

impl Context {
    fn cli_overrides(&self) -> CliOverrides {
        CliOverrides {
            config_path: self.global.config.clone(),
            model: self.global.model.clone(),
            router: self.global.router.clone(),
            execution: self.global.execution.clone(),
            approval: self.global.approval.clone(),
            local_only: self.global.local_only.then_some(true),
            ..CliOverrides::default()
        }
    }

    /// Directory to start project-root discovery from (`--project` or cwd).
    fn project_start(&self) -> Result<PathBuf, ForgeError> {
        match &self.global.project {
            Some(p) => Ok(p.clone()),
            None => std::env::current_dir().map_err(ForgeError::Io),
        }
    }

    /// Discovered project root (walks up for `.git`/`.forge`, else the start dir).
    pub fn project_root(&self) -> Result<PathBuf, ForgeError> {
        Ok(find_project_root(&self.project_start()?))
    }

    /// Resolved configuration for the discovered project root.
    pub fn resolve_config(&self) -> Result<forge_config::ResolvedConfig, ForgeError> {
        let root = self.project_root()?;
        forge_config::Config::load(Some(&root), &self.cli_overrides())
    }
}

pub async fn dispatch(cli: Cli) -> Result<(), ForgeError> {
    let ctx = Context { global: cli.global };
    let json = ctx.global.json;

    match cli.command {
        // No subcommand: the interactive chat. Deliberately the same code
        // path as `forge chat`, so the two can never drift.
        None => chat_cmd::run(&ctx, chat_cmd::ChatArgs::default()).await,
        Some(Command::Chat {
            prompt,
            continue_session,
            session,
        }) => {
            chat_cmd::run(
                &ctx,
                chat_cmd::ChatArgs {
                    prompt,
                    continue_session,
                    session,
                },
            )
            .await
        }
        Some(Command::Init) => init::run(&ctx),
        Some(Command::Version) => {
            let name = "forge";
            let version = env!("CARGO_PKG_VERSION");
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "name": name, "version": version })
                );
            } else {
                println!("{name} {version}");
            }
            Ok(())
        }
        Some(Command::Doctor) => doctor::run(&ctx).await,
        Some(Command::Auth { command }) => match command {
            crate::cli::AuthCommand::Status => auth_cmd::status(&ctx),
        },
        Some(Command::Config { command }) => config_cmd::run(&ctx, command),

        Some(Command::Run { prompt, max_turns }) => run_cmd::run(&ctx, prompt, max_turns).await,
        Some(Command::Resume { id }) => session_cmd::resume(&ctx, &id).await,
        Some(Command::Cancel { id }) => session_cmd::cancel(&ctx, &id),
        Some(Command::Session { command }) => match command.unwrap_or(SessionCommand::List) {
            SessionCommand::List => session_cmd::list(&ctx),
            SessionCommand::Show { id } => session_cmd::show(&ctx, &id),
            SessionCommand::Fork { id, at } => session_cmd::fork(&ctx, &id, at.as_deref()),
        },
        Some(Command::Model { command }) => model_cmd::run(&ctx, command).await,
        Some(Command::Router { command }) => match command {
            crate::cli::RouterCommand::Serve { host, port } => {
                router_cmd::serve(&ctx, host, port).await
            }
        },
        Some(Command::Graph { command }) => graph_cmd::run(&ctx, command).await,
        Some(Command::Skill { command }) => skill_cmd::run(&ctx, command).await,
        Some(Command::Serve { host, port }) => serve_cmd::run(&ctx, host, port).await,
        Some(Command::Mcp) => mcp_cmd::run(&ctx).await,
        Some(Command::Acp) => acp_cmd::run(&ctx).await,
    }
}
