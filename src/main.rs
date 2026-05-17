#![allow(dead_code)]

mod auth;
mod config;
mod error;
mod gmail;
mod http;
mod server;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn main() {
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
            std::process::exit(1);
        }
        Some("serve") | None => {
            eprintln!("serve not yet implemented (see issue #24)");
            std::process::exit(1);
        }
        Some(other) => {
            eprintln!("unknown subcommand `{other}`; expected `auth` or `serve`");
            std::process::exit(1);
        }
    }
}
