//! Tracing setup per [ADR-0008](../docs/adr/0008-observability-and-deployment.md).
//!
//! v0.2 ships the minimum subset — `tracing` + `tracing-subscriber` only.
//! All output is routed to **stderr** because stdout is reserved for the
//! MCP wire protocol over stdio transport (ADR-0003). Log level is
//! controlled by `RUST_LOG`; default is `google_personal_mcp=info,warn`.
//!
//! No Prometheus, no `/healthz`, no OTLP exporter in v0.2 — those are v1.0
//! work tracked separately.

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const DEFAULT_FILTER: &str = "google_personal_mcp=info,warn";

/// Install the global tracing subscriber. Safe to call multiple times in
/// tests — repeated installs are no-ops via `try_init`.
pub(crate) fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_FILTER));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        // Calling more than once must not panic.
        init();
        init();
    }
}
