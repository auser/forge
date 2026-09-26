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

    /// No subcommand opens the interactive chat (see `Command::Chat`).
    /// This is `Option` for exactly that reason: `forge` alone must not be
    /// a usage error.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Clone, Debug, Default)]
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

    /// Override the configured router (needle|jev|laya|http|static|cheapest).
    // `mock` is deliberately absent from that list: it is a test-only
    // router that `router_from_config` refuses unless FORGE_TEST_MOCKS=1
    // (see `forge_config::test_mocks`). The flag still accepts it — the
    // test harnesses pass it — it is just not advertised to users. A plain
    // comment, not a doc comment, so clap cannot render it in `--help`.
    #[arg(long, global = true, value_name = "ROUTER")]
    pub router: Option<String>,

    /// Override the configured execution provider.
    #[arg(long, global = true, value_name = "PROVIDER")]
    pub execution: Option<String>,

    /// Refuse any model or router endpoint that is not on this machine
    /// (loopback, localhost, or a socket path).
    #[arg(long, global = true)]
    pub local_only: bool,

    /// Override the approval mode (auto|prompt|prompt-dangerous|deny).
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
    /// Interactive chat: a scrolling transcript with slash commands.
    /// `forge` with no subcommand is the same thing.
    Chat {
        /// Optional first turn; the chat stays interactive afterwards.
        #[arg(value_name = "PROMPT")]
        prompt: Vec<String>,
        /// Continue the project's most recently active session.
        #[arg(short = 'c', long = "continue")]
        continue_session: bool,
        /// Continue a named session.
        #[arg(long, value_name = "SESSION_ID")]
        session: Option<String>,
    },

    /// Initialize a project for Forge (idempotent).
    Init,

    /// Run a prompt through the agent.
    Run {
        #[arg(required = true, num_args = 1.., value_name = "PROMPT")]
        prompt: Vec<String>,
        /// Agent-loop turn budget (overrides config `max_turns`).
        #[arg(long, value_name = "N")]
        max_turns: Option<u32>,
    },

    /// Serve the Model Context Protocol over stdio (for editors and agent
    /// harnesses). stdout carries the protocol; logs go to stderr.
    Mcp,

    /// Serve the Agent Client Protocol over stdio, making forge an
    /// in-editor agent (Zed and other ACP clients). stdout carries the
    /// protocol; logs go to stderr.
    Acp,

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

    /// Laya router adapter management.
    Router {
        #[command(subcommand)]
        command: RouterCommand,
    },

    /// Credential detection and auth inspection.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
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
    /// Branch a session into a new one, copying its history.
    Fork {
        #[arg(value_name = "SESSION_ID")]
        id: String,
        /// Cut point: a 1-based log position or a run id (default: the
        /// whole log). A cut inside a run snaps forward to that run's end.
        #[arg(long, value_name = "POSITION_OR_RUN_ID")]
        at: Option<String>,
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
        /// Search the local semantic embedding index instead of literal
        /// text/regex matching (requires needle weights; see `forge init`).
        #[arg(long)]
        semantic: bool,
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
pub enum RouterCommand {
    /// Start the local Laya decision-router adapter (open-source System One
    /// router); use with router = "laya"
    Serve {
        #[arg(long, value_name = "HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, value_name = "PORT", default_value = "8788")]
        port: u16,
    },
}

#[derive(Subcommand)]
pub enum AuthCommand {
    /// Show detected credentials (sources only — never values).
    Status,
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
