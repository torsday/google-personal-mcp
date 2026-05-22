//! Tracing setup per [ADR-0008](../docs/adr/0008-observability-and-deployment.md).
//!
//! v0.x ships the structured-spans subset — `tracing` + `tracing-subscriber`.
//! All output is routed to **stderr** because stdout is reserved for the
//! MCP wire protocol over stdio transport (ADR-0003). Log level is
//! controlled by `RUST_LOG`; default is `google_personal_mcp=info,warn`.
//!
//! Span coverage (per ADR-0008 §Logging):
//! - One span per MCP tool call (`tool.name`, `tool.account`, etc.) —
//!   added via `#[tracing::instrument]` on each tool entry point under
//!   `src/tools/`.
//! - One span per outbound Gmail HTTP request (`google.service`,
//!   `google.endpoint`, `google.method`, `google.account`, `google.cost`)
//!   — attached to `GmailClient::authed_get` / `authed_post`.
//! - One span per OAuth refresh (`oauth.account`, `oauth.force`) —
//!   attached to `TokenManager::refresh_locked`.
//!
//! No Prometheus, no `/healthz`, no OTLP exporter, no JSON log toggle in
//! v0.x — those are v1.0 work tracked separately.

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
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn init_is_idempotent() {
        // Calling more than once must not panic.
        init();
        init();
    }

    /// `tracing_subscriber::fmt::MakeWriter` over a shared byte buffer so
    /// tests can introspect everything the subscriber emits.
    #[derive(Clone)]
    struct BufWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl io::Write for BufWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.buf.lock().expect("lock").extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Per ADR-0008 redaction rules: spans on the token-refresh and Gmail
    /// HTTP paths must record account aliases and endpoints but **never**
    /// access tokens or refresh tokens. This test runs a real refresh
    /// through the production code path against a wiremock endpoint,
    /// captures all subscriber output, and asserts the secret bytes never
    /// surface.
    #[tokio::test]
    async fn refresh_span_redacts_token_bytes() {
        use crate::auth::tokens::{ReqwestRefreshTransport, TokenManager, TokenState};
        use chrono::{Duration as ChronoDuration, Utc};
        use std::collections::HashMap;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const SECRET_ACCESS: &str = "SUPER-SECRET-ACCESS-TOKEN-AABBCC";
        const SECRET_REFRESH: &str = "SUPER-SECRET-REFRESH-TOKEN-XYZ123";

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = BufWriter { buf: buf.clone() };
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::TRACE)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"access_token":"FRESH-{SECRET_ACCESS}","expires_in":3600}}"#
            )))
            .mount(&server)
            .await;

        let state = TokenState {
            access_token: SECRET_ACCESS.into(),
            refresh_token: SECRET_REFRESH.into(),
            expires_at: Utc::now() + ChronoDuration::seconds(5),
            scopes: vec!["scope.test".into()],
            client_id: "111-abc.apps.googleusercontent.com".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gpm-redact-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mgr = TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            ReqwestRefreshTransport::new(reqwest::Client::new()),
            format!("{}/token", server.uri()),
            dir,
        );

        // Drive the refresh path. The result doesn't matter for this test;
        // the span output is what's being inspected.
        let _ = mgr.access_token("work").await;

        drop(guard);
        let captured = String::from_utf8_lossy(&buf.lock().expect("lock")).to_string();

        assert!(
            !captured.contains(SECRET_ACCESS),
            "access_token leaked into span output:\n{captured}"
        );
        assert!(
            !captured.contains(SECRET_REFRESH),
            "refresh_token leaked into span output:\n{captured}"
        );
        assert!(
            !captured.contains(&format!("FRESH-{SECRET_ACCESS}")),
            "fresh access_token leaked into span output:\n{captured}"
        );
        // Conversely, the non-secret context SHOULD appear so an operator
        // can actually diagnose what happened.
        assert!(
            captured.contains("oauth.account") || captured.contains("work"),
            "expected oauth span context in output:\n{captured}"
        );
    }
}
