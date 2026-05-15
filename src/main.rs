mod auth;
mod gmail;
mod tools;

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr — stdout is reserved for MCP stdio transport.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "google_personal_mcp=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map_or("serve", String::as_str);

    if command == "auth" {
        auth::run_auth_flow().await?;
        eprintln!("✓ Authentication successful. Run `google-personal-mcp` to start the server.");
    } else {
        let token = auth::load_token().await?;
        let gmail_client = gmail::GmailClient::new(token);
        let server = tools::GmailServer::new(gmail_client);

        tracing::info!("google-personal-mcp starting on stdio");
        let service = server.serve(stdio()).await?;
        service.waiting().await?;
    }

    Ok(())
}
