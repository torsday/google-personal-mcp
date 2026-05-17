#![allow(dead_code)]

mod auth;
mod config;
mod error;
mod gmail;
mod http;
mod observability;
mod perm_check;
mod project_quota;
mod rate_limit;
mod server;
mod tools;

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::auth::cli::AuthCommand;
use crate::auth::tokens::{ReqwestRefreshTransport, TokenManager};
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::server::{run_stdio, GoogleServer};

/// Top-level CLI surface. `serve` is the default; `auth` covers account setup.
#[derive(Parser, Debug)]
#[command(
    name = "google-personal-mcp",
    about = "Personal Google-services MCP daemon",
    long_about = None,
    version,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP daemon over stdio (default if no subcommand given).
    Serve,
    /// OAuth account management.
    Auth {
        #[command(subcommand)]
        sub: AuthCommand,
    },
}

fn main() -> ExitCode {
    observability::init();

    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => run_serve_blocking(),
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

/// Validate startup posture per ADRs 0006 and 0017, build the `GoogleServer`,
/// and drive the rmcp stdio runtime to EOF on a dedicated tokio runtime.
///
/// Returns `Ok(())` on clean client disconnect; surfaces structured errors
/// for startup-posture failures (`InsecurePermissions`, `Config`) and runtime
/// faults (`Internal`).
fn run_serve_blocking() -> Result<(), Error> {
    let dir = config::config_dir();
    perm_check::check(&perm_check::default_subjects(&dir))?;
    let _accounts = config::Accounts::load(&config::accounts_path(&dir))?;
    let cfg = config::Config::load(&config::config_path(&dir))?;

    // v0.2: token state is not yet hot-loaded from `tokens/<alias>.json`
    // (that wiring lives in the future tools that need authenticated calls).
    // Start with an empty registry; the `auth` subcommand is what populates
    // it, and per ADR-0002 the daemon reads the file lazily.
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(Error::Network)?;
    let tokens = Arc::new(TokenManager::new(
        HashMap::new(),
        ReqwestRefreshTransport::new(http_client.clone()),
        "https://oauth2.googleapis.com/token",
        dir.join("tokens"),
    ));
    let gmail_base = "https://gmail.googleapis.com/gmail/v1";
    let gmail = Arc::new(GmailClient::new(gmail_base, tokens.clone(), http_client));

    // Suppress the unused-field warning until tool tickets wire it in.
    let _ = &cfg;

    let server = GoogleServer::new(tokens, gmail);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Internal {
            context: "tokio runtime build".to_owned(),
            source: anyhow::Error::new(e),
        })?;
    runtime.block_on(run_stdio(server))
}
