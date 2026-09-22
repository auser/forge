use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "forge",
    version,
    about = "Forge: a single-binary agentic coding harness"
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Default)]
pub struct GlobalOpts {
    /// Increase diagnostics verbosity (-v info, -vv debug, -vvv trace); diagnostics go to stderr.
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Explicit additional config file, layered after the project config.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Project directory (defaults to the current working directory).
    #[arg(long, global = true, value_name = "PATH")]
    pub project: Option<PathBuf>,

    /// Override the configured model.
    #[arg(long, global = true, value_name = "MODEL")]
    pub model: Option<String>,

    /// Override the configured router.
    #[arg(long, global = true, value_name = "ROUTER")]
    pub router: Option<String>,

    /// Override the configured execution provider.
    #[arg(long, global = true, value_name = "PROVIDER")]
    pub execution: Option<String>,

    /// Restrict to local providers only.
    #[arg(long, global = true)]
    pub local_only: bool,

    /// Override the approval mode (auto|prompt|deny).
    #[arg(long, global = true, value_name = "MODE")]
    pub approval: Option<String>,

    /// Machine-readable JSON on stdout (and nothing else on stdout).
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable ANSI colors in diagnostics.
    #[arg(long = "no-color", global = true)]
    pub no_color: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize a project for Forge (idempotent).
    Init,

    /// Run a prompt through the agent.
    Run {
        #[arg(required = true, num_args = 1.., value_name = "PROMPT")]
        prompt: Vec<String>,
    },

    /// Start the REST/SSE server.
    Serve {
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
    },

    /// Resume a previous run or session.
    Resume {
        #[arg(value_name = "RUN_OR_SESSION_ID")]
        id: String,
    },

    /// Cancel a running run or session.
    Cancel {
        #[arg(value_name = "RUN_OR_SESSION_ID")]
        id: String,
    },

    /// Inspect sessions (defaults to `list`).
    Session {
        #[command(subcommand)]
        command: Option<SessionCommand>,
    },

    /// Project graph operations.
    Graph {
        #[command(subcommand)]
        command: GraphCommand,
    },

    /// Skill discovery and activation.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },

    /// Model provider inspection.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },

    /// Configuration inspection.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Check environment and configuration health.
    Doctor,

    /// Print name and version.
    Version,
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List known sessions.
    List,
    /// Show one session's events.
    Show {
        #[arg(value_name = "ID")]
        id: String,
    },
}

#[derive(Subcommand)]
pub enum GraphCommand {
    /// Build or incrementally refresh the graph.
    Build,
    /// Check graph freshness.
    Check,
    /// Print a structural map.
    Map,
    /// Search indexed file contents.
    Grep {
        #[arg(value_name = "PATTERN")]
        pattern: String,
    },
    /// Find callers of a symbol.
    Callers {
        #[arg(value_name = "SYMBOL")]
        symbol: String,
    },
    /// Show the blast radius of a file.
    Blast {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Select graph-aware context for a query.
    Context {
        #[arg(value_name = "QUERY")]
        query: String,
    },
}

#[derive(Subcommand)]
pub enum SkillCommand {
    /// List discovered skills (metadata only).
    List,
    /// Show a skill's full instructions.
    Show {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Test a skill end to end.
    Test {
        #[arg(value_name = "NAME")]
        name: String,
    },
}

#[derive(Subcommand)]
pub enum ModelCommand {
    /// List configured/available models.
    List,
    /// Test a model (default: the configured model).
    Test {
        #[arg(value_name = "MODEL")]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print the resolved configuration.
    Show,
    /// Print the user and project config file paths.
    Path,
    /// Explain the winning value and origin of a key.
    Explain {
        #[arg(value_name = "KEY")]
        key: String,
    },
}
