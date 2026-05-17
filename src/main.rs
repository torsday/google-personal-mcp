#![allow(dead_code)]

mod auth;
mod config;
mod error;
mod gmail;
mod http;
mod perm_check;
mod server;

use std::process::ExitCode;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::error::Error;

fn main() -> ExitCode {
    // Log to stderr — stdout is reserved for MCP stdio transport.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "google_personal_mcp=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(String::as_str);

    match subcommand {
        Some("auth") => {
            eprintln!("auth subcommands not yet implemented (see issue #4, #27)");
            ExitCode::from(1)
        }
        Some("serve") | None => match run_serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = ?e, "startup failed");
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        Some(other) => {
            eprintln!("unknown subcommand `{other}`; expected `auth` or `serve`");
            ExitCode::from(1)
        }
    }
}

/// Validate startup posture per ADRs 0006 and 0017, then hand off to the
/// (not-yet-implemented) MCP server loop. Posture failures refuse to start
/// — see [`perm_check`] for the exact rules and the
/// `GOOGLE_PERSONAL_MCP_SKIP_PERM_CHECK=1` escape hatch.
fn run_serve() -> Result<(), Error> {
    let dir = config::config_dir();

    perm_check::check(&perm_check::default_subjects(&dir))?;
    let _accounts = config::Accounts::load(&config::accounts_path(&dir))?;
    let _config = config::Config::load(&config::config_path(&dir))?;

    // Posture is valid; hand off to the server. Wiring the MCP loop is
    // tracked separately by issue #24.
    eprintln!("startup checks passed; serve loop not yet implemented (see issue #24)");
    Ok(())
}
