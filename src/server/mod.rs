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
mod descriptors;
mod dispatch;

use std::sync::Arc;

use serde_json::Value;

use crate::audit::{AuditEntry, AuditWriter, Verbosity};
use crate::auth::tokens::{ReqwestRefreshTransport, TokenManager};
use crate::config::AccountEntry;
use crate::error::{self, Error};
use crate::gmail::client::GmailClient;

/// The root rmcp service. Holds shared state passed to tool implementations.
///
/// Built once at `serve` startup; cloned-as-Arc handles are handed to each
/// tool dispatch path.
#[derive(Clone)]
pub(crate) struct GoogleServer {
    /// Registered Google accounts from `accounts.toml`. Used by `list_accounts`.
    pub(super) accounts: Arc<Vec<AccountEntry>>,
    pub(super) tokens: Arc<TokenManager<ReqwestRefreshTransport>>,
    pub(super) gmail: Arc<GmailClient<ReqwestRefreshTransport>>,
    /// Best-effort JSONL audit writer per ADR-0011 v0.2 subset.
    pub(super) audit: AuditWriter,
    /// Audit verbosity configured via `[audit] verbose` in config.toml.
    /// `Redacted` by default; `Verbose` only when the operator has opted in.
    pub(super) verbosity: Verbosity,
}

impl GoogleServer {
    /// Construct the server with its component clients pre-wired.
    pub(crate) const fn new(
        accounts: Arc<Vec<AccountEntry>>,
        tokens: Arc<TokenManager<ReqwestRefreshTransport>>,
        gmail: Arc<GmailClient<ReqwestRefreshTransport>>,
        audit: AuditWriter,
        verbosity: Verbosity,
    ) -> Self {
        Self {
            accounts,
            tokens,
            gmail,
            audit,
            verbosity,
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
/// Per ADR-0003, stdio is the only v0.2 transport; HTTP transport is v1.0.
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use rmcp::handler::server::ServerHandler;

    fn fake_server() -> GoogleServer {
        let tokens = Arc::new(TokenManager::new(
            HashMap::new(),
            ReqwestRefreshTransport::new(reqwest::Client::new()),
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-srv-test-{}", std::process::id())),
        ));
        let gmail = Arc::new(GmailClient::new(
            "https://gmail.googleapis.com/gmail/v1",
            tokens.clone(),
            reqwest::Client::new(),
        ));
        let audit = AuditWriter::new(
            std::env::temp_dir().join(format!("gpm-srv-test-{}", std::process::id())),
            crate::config::RotateMode::Monthly,
        );
        GoogleServer::new(Arc::new(vec![]), tokens, gmail, audit, Verbosity::Redacted)
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
        let gmail = Arc::new(GmailClient::new(
            "https://gmail.googleapis.com/gmail/v1",
            tokens.clone(),
            reqwest::Client::new(),
        ));
        let audit = AuditWriter::new(dir.path(), crate::config::RotateMode::Monthly);
        (
            GoogleServer::new(Arc::new(vec![]), tokens, gmail, audit, Verbosity::Redacted),
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
}
