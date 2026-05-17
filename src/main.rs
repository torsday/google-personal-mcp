#![allow(dead_code)]

mod auth;
mod config;
mod error;
mod gmail;
mod http;
mod perm_check;
mod server;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::auth::cli::AuthCommand;
use crate::error::Error;

/// Top-level CLI surface. `serve` is the default; `auth` covers account setup.
#[derive(Parser, Debug)]
#[command(
    name = "google-personal-mcp",
    about = "Personal Google-services MCP daemon",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP daemon (default if no subcommand given).
    Serve,
    /// OAuth account management.
    Auth {
        #[command(subcommand)]
        sub: AuthCommand,
    },
}

fn main() -> ExitCode {
    // Log to stderr — stdout is reserved for MCP stdio transport.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "google_personal_mcp=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => run_serve(),
        Command::Auth { sub } => sub.run(&config::config_dir()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = ?e, "command failed");
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Validate startup posture per ADRs 0006 and 0017, then hand off to the
/// (not-yet-implemented) MCP server loop.
fn run_serve() -> Result<(), Error> {
    let dir = config::config_dir();
    perm_check::check(&perm_check::default_subjects(&dir))?;
    let _accounts = config::Accounts::load(&config::accounts_path(&dir))?;
    let _config = config::Config::load(&config::config_path(&dir))?;
    eprintln!("startup checks passed; serve loop not yet implemented (see issue #24)");
    Ok(())
}
