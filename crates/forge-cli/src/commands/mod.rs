pub mod acp_cmd;
pub mod auth_cmd;
pub mod chat_cmd;
pub mod config_cmd;
pub mod context_cmd;
pub mod doctor;
pub mod graph_cmd;
pub mod guidance;
pub mod init;
pub mod learn_cmd;
pub mod mcp_cmd;
pub mod model_cmd;
pub mod observer_cmd;
pub mod presets;
pub mod router_cmd;
pub mod run_cmd;
pub mod serve_cmd;
pub mod service;
pub mod session_cmd;
pub mod skill_cmd;
pub mod task_cmd;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use forge_config::CliOverrides;
use forge_core::{ForgeError, find_project_root};

use crate::cli::{Cli, Command, GlobalOpts, GraphCommand, SessionCommand, TaskCommand};

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
    let activity = command_activity(&cli.command, json);

    let result = match cli.command {
        // No subcommand: the interactive chat. Deliberately the same code
        // path as `forge chat`, so the two can never drift.
        None => {
            if init::ensure(&ctx)? {
                init::report_first_run(&ctx)?;
            }
            auth_cmd::ensure_for_chat(&ctx)?;
            chat_cmd::run(&ctx, chat_cmd::ChatArgs::default()).await
        }
        Some(Command::Chat {
            prompt,
            continue_session,
            session,
        }) => {
            if init::ensure(&ctx)? {
                init::report_first_run(&ctx)?;
            }
            auth_cmd::ensure_for_chat(&ctx)?;
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
        Some(Command::Init { preset }) => init::run(&ctx, preset.as_deref()),
        Some(Command::Version { build }) => {
            let name = "forge";
            let version = env!("CARGO_PKG_VERSION");
            let commit = env!("FORGE_BUILD_COMMIT");
            let target = env!("FORGE_BUILD_TARGET");
            if json {
                let mut value = serde_json::json!({ "name": name, "version": version });
                if build {
                    value["commit"] = commit.into();
                    value["target"] = target.into();
                }
                println!("{value}");
            } else if build {
                println!("{name} {version} (commit {commit}, target {target})");
            } else {
                println!("{name} {version}");
            }
            Ok(())
        }
        Some(Command::Doctor { live }) => doctor::run(&ctx, live).await,
        Some(Command::Auth { command }) => match command {
            crate::cli::AuthCommand::Status => auth_cmd::status(&ctx),
            crate::cli::AuthCommand::Login { provider } => auth_cmd::login(&ctx, provider),
        },
        Some(Command::Config { command }) => config_cmd::run(&ctx, command),
        Some(Command::Context { command }) => context_cmd::context(&ctx, command),
        Some(Command::Memory { command }) => context_cmd::memory(&ctx, command),
        Some(Command::Learn { command }) => learn_cmd::run(&ctx, command),
        Some(Command::Observer { command }) => match command {
            crate::cli::ObserverCommand::Status => observer_cmd::status(&ctx),
        },

        Some(Command::Run {
            prompt,
            max_turns,
            skills,
        }) => run_cmd::run(&ctx, prompt, max_turns, skills).await,
        Some(Command::Resume { id }) => session_cmd::resume(&ctx, &id).await,
        Some(Command::Cancel { id }) => session_cmd::cancel(&ctx, &id),
        Some(Command::Session { command }) => match command.unwrap_or(SessionCommand::List) {
            SessionCommand::List => session_cmd::list(&ctx),
            SessionCommand::Show { id } => session_cmd::show(&ctx, &id),
            SessionCommand::Fork { id, at } => session_cmd::fork(&ctx, &id, at.as_deref()),
            SessionCommand::Decisions => session_cmd::decisions(&ctx),
        },
        Some(Command::Task { command }) => match command.unwrap_or(TaskCommand::List) {
            TaskCommand::List => task_cmd::list(&ctx),
            TaskCommand::Show { id } => task_cmd::show(&ctx, &id),
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
        Some(Command::Mcp { compact }) => mcp_cmd::run(&ctx, compact).await,
        Some(Command::Acp) => acp_cmd::run(&ctx).await,
    };

    if let Some(activity) = activity {
        activity.finish(result.is_ok());
    }
    result
}

fn command_activity(command: &Option<Command>, json: bool) -> Option<Activity> {
    if json || !std::io::stderr().is_terminal() {
        return None;
    }
    let label = match command {
        None | Some(Command::Chat { .. } | Command::Version { .. }) => return None,
        Some(Command::Init { .. }) => "initializing project",
        Some(Command::Doctor { .. }) => "checking environment",
        Some(Command::Auth { .. }) => "checking authentication",
        Some(Command::Config { .. }) => "resolving configuration",
        Some(Command::Context { .. }) => "reading context status",
        Some(Command::Memory { .. }) => "reading memory state",
        Some(Command::Learn { .. }) => "analyzing learning evidence",
        Some(Command::Observer { .. }) => "reading observer status",
        Some(Command::Run { .. }) => "running agent",
        Some(Command::Resume { .. }) => "resuming run",
        Some(Command::Cancel { .. }) => "cancelling run",
        Some(Command::Session { .. }) => "reading sessions",
        Some(Command::Task { .. }) => "reading tasks",
        Some(Command::Model { .. }) => "checking models",
        Some(Command::Router { .. }) => "starting router",
        Some(Command::Graph {
            command: GraphCommand::Build { .. },
        }) => "building project graph",
        Some(Command::Graph { .. }) => "querying project graph",
        Some(Command::Skill { .. }) => "loading skills",
        Some(Command::Serve { .. }) => "running server",
        Some(Command::Mcp { .. }) => "running MCP server",
        Some(Command::Acp) => "running ACP server",
    };
    Some(Activity::start(label))
}

struct Activity {
    label: &'static str,
    started: Instant,
    stop: mpsc::Sender<()>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}

impl Activity {
    const HEARTBEAT: Duration = Duration::from_secs(10);

    fn start(label: &'static str) -> Self {
        eprintln!("working: {label}...");
        let started = Instant::now();
        let (stop, stopped) = mpsc::channel();
        let heartbeat = std::thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(Self::HEARTBEAT) {
                eprintln!("still working: {label} ({}s)", started.elapsed().as_secs());
            }
        });
        Self {
            label,
            started,
            stop,
            heartbeat: Some(heartbeat),
        }
    }

    fn finish(mut self, succeeded: bool) {
        let _ = self.stop.send(());
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        let status = if succeeded { "done" } else { "failed" };
        eprintln!(
            "{status}: {} ({:.1}s)",
            self.label,
            self.started.elapsed().as_secs_f32()
        );
    }
}
