//! The `silo` command line: launches harnesses, manages workspaces, opens
//! sandboxed user shells, and converts journals into replay test scripts.

use std::process::ExitCode;

use clap::Parser;

mod cli;
mod cmd_harnesses;
mod cmd_replay;
mod cmd_run;
mod cmd_shell;
mod cmd_workspace;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = cli::Cli::parse();
    let result = match cli.command {
        cli::Command::Run(args) => cmd_run::execute(*args).await,
        cli::Command::Workspace { action } => cmd_workspace::execute(action),
        cli::Command::Shell(args) => cmd_shell::execute(args).await,
        cli::Command::ReplayTest(args) => cmd_replay::execute(args),
        cli::Command::Harnesses {
            action: cli::HarnessesAction::List,
        } => cmd_harnesses::list(),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("silo: {error:#}");
            ExitCode::from(1)
        }
    }
}
