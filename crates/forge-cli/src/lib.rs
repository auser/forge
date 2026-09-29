pub mod bootstrap;
pub mod chat;
pub mod cli;
pub mod commands;
pub mod tracing_setup;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    tracing_setup::init(cli.global.verbose, cli.global.no_color);
    // .env / .env.local load before config resolution and before any
    // provider construction or session-store env snapshot.
    bootstrap::load_dotenv(&cli.global);
    tracing::trace!("command line parsed; dispatching");

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(commands::dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::debug!(error = %err, "command failed");
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
