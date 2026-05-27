//! MCP server runtime per [ADR-0001](../docs/adr/0001-monolithic-google-personal-mcp-architecture.md)
//! and the stdio transport from [ADR-0003](../docs/adr/0003-transport-stdio-and-streamable-http.md).
//!
//! `GoogleServer` is the root `rmcp::ServerHandler` implementation. It owns
//! the per-service clients (token manager, Gmail HTTP wrapper) and will hold
//! the audit log and dedup cache once those land (#21, #11).
//!
//! Tool routing is manual `list_tools` / `call_tool` dispatch — the
//! `#[tool_router]` macro path is reserved for future services.
//!
//! Module layout (see #91):
//!
//! - [`mod.rs`](self) — `GoogleServer` struct, ctor, `write_destructive_intent`, `run_stdio`
//! - [`descriptors`] — tool descriptor constants + `registered_tools()`
//! - [`dispatch`] — `impl ServerHandler` (the `call_tool` dispatch match)
//! - [`args`] — tool-argument extraction helpers + `ok_result`

mod args;
pub(crate) mod deprecation;
mod descriptors;
mod dispatch;

use std::sync::Arc;

use serde_json::Value;

use crate::audit::{AuditEntry, AuditWriter, Verbosity};
use crate::auth::tokens::{ReqwestRefreshTransport, TokenManager};
use crate::config::AccountEntry;
use crate::error::{self, Error};
use crate::gmail::service::GmailService;

/// The root rmcp service. Holds shared state passed to tool implementations.
///
/// Built once at `serve` startup; cloned-as-Arc handles are handed to each
/// tool dispatch path.
#[derive(Clone)]
pub(crate) struct GoogleServer {
    /// Registered Google accounts from `accounts.toml`. Used by `list_accounts`.
    pub(super) accounts: Arc<Vec<AccountEntry>>,
    pub(super) tokens: Arc<TokenManager<ReqwestRefreshTransport>>,
    pub(super) gmail: Arc<GmailService<ReqwestRefreshTransport>>,
    /// Best-effort JSONL audit writer per ADR-0011 v0.2 subset.
    pub(super) audit: AuditWriter,
    /// Audit verbosity configured via `[audit] verbose` in config.toml.
    /// `Redacted` by default; `Verbose` only when the operator has opted in.
    pub(super) verbosity: Verbosity,
    /// Tool-deprecation registry per [ADR-0015](../../docs/adr/0015-tool-versioning-policy.md).
    /// Empty in production pre-1.0; populated in tests via the `with_*`
    /// constructor so the dispatcher's deprecation-WARN branch can be
    /// exercised without committing a real deprecation.
    pub(super) deprecations: Arc<deprecation::Registry>,
    /// Filesystem paths used by the `purge_account` tool (#166). Built
    /// at startup from `cfg.cache.dir` + the daemon's `config_dir()`.
    pub(super) purge_paths: Arc<crate::tools::purge_account::PurgePaths>,
}

impl GoogleServer {
    /// Construct the server with its component clients pre-wired. Uses the
    /// production deprecation registry (empty pre-1.0); use
    /// [`Self::with_deprecations`] in tests that need to exercise the
    /// deprecation dispatch path.
    pub(crate) fn new(
        accounts: Arc<Vec<AccountEntry>>,
        tokens: Arc<TokenManager<ReqwestRefreshTransport>>,
        gmail: Arc<GmailService<ReqwestRefreshTransport>>,
        audit: AuditWriter,
        verbosity: Verbosity,
        purge_paths: crate::tools::purge_account::PurgePaths,
    ) -> Self {
        Self {
            accounts,
            tokens,
            gmail,
            audit,
            verbosity,
            deprecations: Arc::new(deprecation::production()),
            purge_paths: Arc::new(purge_paths),
        }
    }

    /// Test-only constructor that injects a populated deprecation
    /// registry. Production code uses [`Self::new`], which always wires
    /// the empty `deprecation::production()` registry. Gated behind
    /// `#[cfg(test)]` so the fixture can never accidentally land in
    /// production.
    #[cfg(test)]
    pub(crate) fn with_deprecations(
        accounts: Arc<Vec<AccountEntry>>,
        tokens: Arc<TokenManager<ReqwestRefreshTransport>>,
        gmail: Arc<GmailService<ReqwestRefreshTransport>>,
        audit: AuditWriter,
        verbosity: Verbosity,
        deprecations: deprecation::Registry,
    ) -> Self {
        Self {
            accounts,
            tokens,
            gmail,
            audit,
            verbosity,
            deprecations: Arc::new(deprecations),
            // Tests using `with_deprecations` don't exercise the
            // purge path; supply harmless placeholder paths.
            purge_paths: Arc::new(crate::tools::purge_account::PurgePaths {
                config_dir: std::path::PathBuf::from("/tmp/.gpm-test-stub"),
                cache_dir: std::path::PathBuf::from("/tmp/.gpm-test-stub/cache"),
            }),
        }
    }

    /// Write+fsync a pre-call "intent" audit record for a destructive
    /// tool per [ADR-0011 lines 83-86](../docs/adr/0011-audit-log.md)
    /// (#66). Returns an `ErrorData` mapped from `Error::Internal` if
    /// the audit write fails — caller propagates, **refusing the op**.
    ///
    /// Skipped for `dry_run` calls: there's no API call to crash
    /// during, and the existing post-call audit record captures the
    /// dry-run shape on its own.
    pub(super) fn write_destructive_intent(
        &self,
        account: &str,
        tool: &'static str,
        params_summary: Value,
        dry_run: bool,
    ) -> Result<(), rmcp::ErrorData> {
        if dry_run {
            return Ok(());
        }
        let entry = AuditEntry {
            timestamp: chrono::Utc::now(),
            account: account.to_owned(),
            tool: tool.to_owned(),
            params_summary,
            action: "intent".to_owned(),
            result: "pending".to_owned(),
        };
        self.audit
            .write_synced(&entry)
            .map_err(|e| error::to_mcp_error(&e))
    }
}

/// Run the MCP daemon over stdio until the client disconnects (stdin EOF).
/// Per ADR-0003 this is one of the two supported transports — the other
/// is [`run_http`].
///
/// Stdout is reserved for the MCP wire protocol; the caller is responsible
/// for routing all `tracing` output to stderr (see [`crate::observability`]).
pub(crate) async fn run_stdio(server: GoogleServer) -> Result<(), Error> {
    use rmcp::ServiceExt;
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await.map_err(|e| Error::Internal {
        context: "rmcp::serve(stdio)".to_owned(),
        source: anyhow::Error::new(e),
    })?;
    service.waiting().await.map_err(|e| Error::Internal {
        context: "rmcp service.waiting()".to_owned(),
        source: anyhow::Error::new(e),
    })?;
    Ok(())
}

/// Run the MCP daemon over Streamable HTTP per
/// [ADR-0003](../../docs/adr/0003-transport-stdio-and-streamable-http.md).
///
/// Binds `addr` (e.g. `127.0.0.1:8765`) and serves the rmcp
/// [`StreamableHttpService`](rmcp::transport::streamable_http_server::StreamableHttpService)
/// at `/mcp`. Every inbound MCP request shares the same
/// [`GoogleServer`] handle via cheap `Arc` clones — the service
/// factory passed to rmcp clones the handle per session so accounts,
/// tokens, cache, and audit writer are identical across transports.
///
/// When `validator` is `Some`, an `Authorization: Bearer <token>`
/// middleware is layered in front of `/mcp` per ADR-0020. The caller
/// (`lib::run_serve_blocking`) supplies `None` for loopback binds and a
/// constructed validator for non-loopback binds; loopback binds skip
/// the auth check entirely because the OS user boundary is the auth
/// layer for local-only deployments.
///
/// `axum::serve` is configured for graceful shutdown via the
/// `StreamableHttpServerConfig::cancellation_token`. The token is
/// owned by this function; cancellation is left for a future
/// signal-handler ticket — the daemon's working assumption is "runs
/// until the process is killed" per ADR-0001.
///
/// Errors:
/// - [`Error::Config`] if the listener cannot bind `addr`.
/// - [`Error::Internal`] for axum serve faults.
pub(crate) async fn run_http(
    server: GoogleServer,
    addr: &str,
    validator: Option<Arc<crate::http_auth::BearerValidator>>,
    throttle: Option<Arc<crate::http_auth::throttle::Throttle>>,
) -> Result<(), Error> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| Error::Config {
            path: addr.to_owned(),
            message: format!("HTTP transport could not bind: {e}"),
        })?;
    let bound = listener
        .local_addr()
        .map_or_else(|_| addr.to_owned(), |a| a.to_string());
    tracing::info!(
        addr = %bound,
        bearer_auth = validator.is_some(),
        throttle = throttle.is_some(),
        "HTTP transport listening at /mcp",
    );
    serve_http_on(listener, server, validator, throttle).await
}

/// Inner half of [`run_http`] — accepts a pre-bound listener so tests
/// can supply an ephemeral-port socket and the production caller can
/// supply the operator-configured `addr`. Both share the same axum +
/// rmcp wiring beyond this point.
pub(crate) async fn serve_http_on(
    listener: tokio::net::TcpListener,
    server: GoogleServer,
    validator: Option<Arc<crate::http_auth::BearerValidator>>,
    throttle: Option<Arc<crate::http_auth::throttle::Throttle>>,
) -> Result<(), Error> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use tokio_util::sync::CancellationToken;

    let cancel = CancellationToken::new();
    let config = StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone());
    let server_arc = Arc::new(server);
    let service: StreamableHttpService<GoogleServer, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let server_arc = Arc::clone(&server_arc);
                move || Ok((*server_arc).clone())
            },
            Arc::new(LocalSessionManager::default()),
            config,
        );

    // Mount the rmcp service at `/mcp`; layer the bearer-auth middleware
    // in front of it when the caller supplied a validator. The layer
    // applies only to routes nested below this Router, so it doesn't
    // affect any sibling routes added in the future.
    let mut mcp_router = axum::Router::new().nest_service("/mcp", service);
    if let (Some(v), Some(t)) = (validator, throttle) {
        let state = crate::http_auth::middleware::AuthState {
            validator: v,
            throttle: t,
        };
        mcp_router = mcp_router.layer(axum::middleware::from_fn_with_state(
            state,
            crate::http_auth::middleware::bearer_middleware,
        ));
    }
    // `into_make_service_with_connect_info::<SocketAddr>()` is the
    // load-bearing wiring that makes the `ConnectInfo<SocketAddr>`
    // extractor in `bearer_middleware` resolve to the peer address.
    // Without it the middleware compiles but every request 500s at
    // extractor failure.
    let make_service = mcp_router.into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, make_service)
        .with_graceful_shutdown(async move { cancel.cancelled_owned().await })
        .await
        .map_err(|e| Error::Internal {
            context: "axum::serve (HTTP transport)".to_owned(),
            source: anyhow::Error::new(e),
        })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use rmcp::handler::server::ServerHandler;

    use crate::gmail::client::GmailClient;

    fn fake_server() -> GoogleServer {
        let tokens = Arc::new(TokenManager::new(
            HashMap::new(),
            ReqwestRefreshTransport::new(reqwest::Client::new()),
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-srv-test-{}", std::process::id())),
        ));
        let client = Arc::new(GmailClient::new(
            "https://gmail.googleapis.com/gmail/v1",
            tokens.clone(),
            reqwest::Client::new(),
        ));
        let gmail = Arc::new(GmailService::new(client, None));
        let audit = AuditWriter::new(
            std::env::temp_dir().join(format!("gpm-srv-test-{}", std::process::id())),
            crate::config::RotateMode::Monthly,
        );
        GoogleServer::new(
            Arc::new(vec![]),
            tokens,
            gmail,
            audit,
            Verbosity::Redacted,
            crate::tools::purge_account::PurgePaths {
                config_dir: std::path::PathBuf::from("/tmp/.gpm-test-stub"),
                cache_dir: std::path::PathBuf::from("/tmp/.gpm-test-stub/cache"),
            },
        )
    }

    #[test]
    fn get_info_returns_named_implementation() {
        let server = fake_server();
        let info = server.get_info();
        assert_eq!(info.server_info.name, env!("CARGO_PKG_NAME"));
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability not advertised"
        );
        assert!(
            info.instructions
                .as_deref()
                .unwrap_or("")
                .contains("_untrusted"),
            "instructions should mention the untrusted-field convention"
        );
    }

    fn assert_bounds<T: Send + Sync + 'static>() {}
    fn assert_handler<H: ServerHandler>(_: &H) {}

    #[test]
    fn google_server_is_send_sync_static() {
        assert_bounds::<GoogleServer>();
        assert_handler(&fake_server());
    }

    #[test]
    fn instructions_warn_about_untrusted() {
        let server = fake_server();
        let info = server.get_info();
        let i = info.instructions.expect("instructions present");
        assert!(i.contains("_untrusted"));
    }

    #[test]
    fn server_can_be_cloned() {
        let server = fake_server();
        let cloned = server.clone();
        assert_eq!(
            server.get_info().server_info.name,
            cloned.get_info().server_info.name
        );
    }

    // ── Destructive-op fsync refusal (#66, ADR-0011) ─────────────────────────

    /// Build a server whose audit dir is pre-created read-only so any
    /// `write_synced` call fails with EACCES — simulating the disk-full
    /// / file-handle-exhaustion failure mode the trust property guards
    /// against. Returns the audit dir path so the caller can verify
    /// nothing was written after the failure.
    #[cfg(unix)]
    fn fake_server_with_unwritable_audit() -> (GoogleServer, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir(&audit_dir).unwrap();
        let mut perms = std::fs::metadata(&audit_dir).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&audit_dir, perms).unwrap();

        let tokens = Arc::new(TokenManager::new(
            HashMap::new(),
            ReqwestRefreshTransport::new(reqwest::Client::new()),
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-srv-test-ro-{}", std::process::id())),
        ));
        let client = Arc::new(GmailClient::new(
            "https://gmail.googleapis.com/gmail/v1",
            tokens.clone(),
            reqwest::Client::new(),
        ));
        let gmail = Arc::new(GmailService::new(client, None));
        let audit = AuditWriter::new(dir.path(), crate::config::RotateMode::Monthly);
        (
            GoogleServer::new(
                Arc::new(vec![]),
                tokens,
                gmail,
                audit,
                Verbosity::Redacted,
                crate::tools::purge_account::PurgePaths {
                    config_dir: std::path::PathBuf::from("/tmp/.gpm-test-stub"),
                    cache_dir: std::path::PathBuf::from("/tmp/.gpm-test-stub/cache"),
                },
            ),
            dir,
        )
    }

    #[cfg(unix)]
    #[test]
    fn destructive_intent_failure_refuses_op() {
        // Acceptance: audit-write failure → destructive op refusal.
        // We call the helper that every destructive dispatch arm goes
        // through; if it returns Err, the `?` in the arm short-circuits
        // before Gmail is ever contacted.
        use std::os::unix::fs::PermissionsExt;
        let (server, dir) = fake_server_with_unwritable_audit();
        let result = server.write_destructive_intent(
            "personal",
            "archive_thread",
            crate::audit::summarize_archive_thread("thr-1", false),
            /* dry_run = */ false,
        );
        // Restore perms before assertions (TempDir Drop needs to clean up).
        let audit_dir = dir.path().join("audit");
        let mut perms = std::fs::metadata(&audit_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&audit_dir, perms).ok();

        let err = result.expect_err("must refuse destructive op on audit failure");
        // INTERNAL_ERROR is rmcp's mapping for Error::Internal.
        assert_eq!(err.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn destructive_intent_skipped_for_dry_run() {
        // dry_run = true: no API call to crash during, so no pre-fsync
        // burden. The post-call best-effort write still captures the
        // dry-run shape. This test guards against a regression where
        // someone makes the pre-fsync unconditional.
        let server = fake_server();
        // Audit dir doesn't even exist yet — would fail on a real
        // write — but dry_run path doesn't touch it.
        let result = server.write_destructive_intent(
            "personal",
            "archive_thread",
            crate::audit::summarize_archive_thread("thr-1", true),
            /* dry_run = */ true,
        );
        assert!(result.is_ok(), "dry_run path must short-circuit cleanly");
    }

    // ── Layer 2: HTTP transport handles one MCP request end-to-end ──────────

    /// Spawn `serve_http_on` on an ephemeral 127.0.0.1 port and return
    /// the bound URL. The server task is detached; it stops when the
    /// test runtime tears down.
    async fn spawn_http_server() -> String {
        // Existing test: no bearer auth (mirrors a loopback bind in
        // production, where the validator is None per ADR-0020).
        spawn_http_server_with(None).await
    }

    async fn spawn_http_server_with(
        validator: Option<Arc<crate::http_auth::BearerValidator>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = fake_server();
        // Bundle a default-config throttle when the test passes a
        // validator (production wiring; see lib::run_serve_blocking).
        // Loopback-only tests that omit the validator also omit the
        // throttle — matches the production invariant.
        let throttle = validator.as_ref().map(|_| {
            Arc::new(crate::http_auth::throttle::Throttle::new(
                crate::http_auth::throttle::ThrottleConfig::default(),
            ))
        });
        tokio::spawn(async move {
            let _ = serve_http_on(listener, server, validator, throttle).await;
        });
        format!("http://{addr}/mcp")
    }

    /// MCP `initialize` is the first message in every session; if the
    /// HTTP transport handles it, the transport itself is correctly
    /// wired through to `GoogleServer::get_info`. This is the
    /// acceptance-criterion "one MCP tool call end-to-end" test.
    #[tokio::test]
    async fn http_transport_handles_initialize_handshake() {
        let url = spawn_http_server().await;
        let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"layer2-test","version":"0.0.1"}}}"#;

        let resp = reqwest::Client::new()
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(init_body)
            .send()
            .await
            .expect("POST initialize");
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let body = resp.text().await.expect("body");
        assert_eq!(status, 200, "initialize must succeed (body: {body:?})");

        // rmcp's stateful default returns an SSE stream; locate the
        // first `data: {...}` JSON-RPC envelope. The MCP standard
        // permits either application/json or text/event-stream framing
        // — we accept whichever the server chose.
        // rmcp's stateful SSE stream emits a priming `data:` line first
        // followed by the JSON-RPC payload — skip empty `data:` frames.
        let json_blob = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .find(|s| !s.trim().is_empty())
            .unwrap_or_else(|| body.trim());
        let parsed: Value = serde_json::from_str(json_blob).unwrap_or_else(|e| {
            panic!("response is JSON-RPC (ct={content_type}, body={body:?}): {e}")
        });
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1, "response id echoes the request id");
        let server_info = &parsed["result"]["serverInfo"];
        assert_eq!(
            server_info["name"],
            env!("CARGO_PKG_NAME"),
            "serverInfo flows from GoogleServer::get_info",
        );
        assert_eq!(server_info["version"], env!("CARGO_PKG_VERSION"));
    }

    // ── Layer 2: bearer-auth middleware end-to-end (ADR-0020 / #162) ────────

    /// Spawn an http transport with a bearer validator and POST the same
    /// `initialize` body used by the no-auth test above:
    ///   - no `Authorization` header → 401 with `WWW-Authenticate`
    ///   - wrong token → 401
    ///   - correct token → 200 (the inner rmcp `initialize` handler runs)
    ///
    /// This is the acceptance-criterion "every HTTP request validates
    /// `Authorization: Bearer <token>` **before** session lookup" test.
    #[tokio::test]
    async fn http_transport_enforces_bearer_when_validator_supplied() {
        use crate::http_auth::{BearerValidator, HttpAuthConfig};
        use std::path::PathBuf;

        let cfg = HttpAuthConfig {
            tokens: vec!["the-real-token".to_owned()],
        };
        let validator = Arc::new(BearerValidator::new(&cfg, PathBuf::from("/dev/null")));
        let url = spawn_http_server_with(Some(Arc::clone(&validator))).await;

        let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"layer2-auth-test","version":"0.0.1"}}}"#;
        let client = reqwest::Client::new();

        // ── 1. No Authorization header ──────────────────────────────────────
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(init_body)
            .send()
            .await
            .expect("POST without auth");
        assert_eq!(resp.status(), 401, "missing-header must be 401");
        let www = resp
            .headers()
            .get("www-authenticate")
            .map(|v| v.to_str().unwrap_or("").to_owned())
            .unwrap_or_default();
        assert!(
            www.contains("Bearer") && www.contains("google-personal-mcp"),
            "WWW-Authenticate must advertise Bearer realm: {www:?}",
        );
        let body = resp.text().await.unwrap_or_default();
        assert!(
            body.contains(r#""error""#) && body.contains("unauthorized"),
            "401 body must be JSON unauthorized envelope: {body:?}",
        );

        // ── 2. Wrong token ──────────────────────────────────────────────────
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", "Bearer not-the-token")
            .body(init_body)
            .send()
            .await
            .expect("POST with wrong auth");
        assert_eq!(resp.status(), 401, "wrong-token must be 401");

        // ── 3. Correct token: rmcp initialize handler runs ──────────────────
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", "Bearer the-real-token")
            .body(init_body)
            .send()
            .await
            .expect("POST with correct auth");
        assert_eq!(resp.status(), 200, "correct token must reach inner handler");
    }
}
