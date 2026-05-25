//! `download_attachment` tool — fetch the byte payload for one
//! attachment, either streamed back as base64 or written to a local
//! file. Per [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md)
//! line 40 (SPEC story 28).
//!
//! Two delivery modes:
//!
//! - `save_to = Some(path)`: write the decoded bytes to `path` (created
//!   with mode `0600` per [ADR-0017](../../docs/adr/0017-secrets-at-rest.md)),
//!   return `{saved_path, size_bytes}`. Refuses to overwrite an existing
//!   file — the operator must remove it first or pick a different path.
//! - `save_to = None`: return `{data_base64, size_bytes}`. The host LLM
//!   gets the same base64url payload Gmail sent on the wire, so it can
//!   forward to another tool without a re-encode round-trip.
//!
//! Cost: `attachments.get` = 5 quota units per call.
//!
//! Audit: per [ADR-0011 §"Redaction"](../../docs/adr/0011-audit-log.md)
//! row 6, the audit summarizer captures `attachment_id`, `mime_type`,
//! `size_bytes`, and `save_to`. The dispatcher writes one
//! `action = "applied"` row per call; no pre-call `intent` because the
//! Gmail-side operation is non-destructive (read-only).

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Serialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::attachments;
use crate::gmail::service::GmailService;

/// Tool input. `mime_type` is operator-supplied (the host LLM should
/// pass through the value it got from `list_attachments`) — it's
/// recorded in the audit row but not re-validated against Gmail.
#[derive(Debug)]
pub(crate) struct DownloadAttachmentInput {
    pub account: String,
    pub message_id: String,
    pub attachment_id: String,
    /// MIME type carried alongside `attachment_id` from a prior
    /// `list_attachments` or `get_thread` call. Used only for the audit
    /// record; we don't reject mismatches.
    pub mime_type: String,
    /// `Some` → write to this path with mode `0600`. `None` → return
    /// base64-encoded bytes in the response.
    pub save_to: Option<PathBuf>,
}

/// Tool output. Exactly one of `data_base64` or `saved_path` is `Some`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct DownloadAttachmentOutput {
    /// Echoed Gmail-reported size in bytes.
    pub size_bytes: u64,
    /// Echoed for the audit-log correlation.
    pub mime_type: String,
    /// Set when `save_to` was supplied; absolute path of the written file.
    pub saved_path: Option<PathBuf>,
    /// Set when `save_to` was omitted; base64url-encoded bytes (no
    /// padding, matching the wire format Gmail returns).
    pub data_base64: Option<String>,
}

/// Fetch and deliver the attachment.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "download_attachment",
        tool.account = %input.account,
        tool.message_id = %input.message_id,
        tool.attachment_id = %input.attachment_id,
    ),
)]
pub(crate) async fn download_attachment<T: RefreshTransport + 'static>(
    gmail: &GmailService<T>,
    input: DownloadAttachmentInput,
) -> Result<DownloadAttachmentOutput, Error> {
    let att = attachments::download(
        gmail.client(),
        &input.account,
        &input.message_id,
        &input.attachment_id,
    )
    .await?;

    if let Some(path) = input.save_to.as_deref() {
        write_to_disk(path, &att.bytes)?;
        Ok(DownloadAttachmentOutput {
            size_bytes: att.size_bytes,
            mime_type: input.mime_type,
            saved_path: Some(path.to_path_buf()),
            data_base64: None,
        })
    } else {
        Ok(DownloadAttachmentOutput {
            size_bytes: att.size_bytes,
            mime_type: input.mime_type,
            saved_path: None,
            data_base64: Some(URL_SAFE_NO_PAD.encode(&att.bytes)),
        })
    }
}

/// Write `bytes` to `path` atomically-ish: opened with `create_new` so an
/// existing file errors out (we refuse to overwrite — operator picked the
/// path), mode 0600 per ADR-0017. The parent directory must already exist;
/// we don't `mkdir -p` because the operator just supplied the path and
/// silently creating intermediate directories could surprise them.
fn write_to_disk(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // refuses to overwrite an existing file
        .mode(0o600)
        .open(path)
        .map_err(|e| io_to_error(path, e))?;
    f.write_all(bytes).map_err(|e| io_to_error(path, e))?;
    f.sync_all().map_err(|e| io_to_error(path, e))?;
    Ok(())
}

fn io_to_error(path: &Path, e: std::io::Error) -> Error {
    Error::Internal {
        context: format!("download_attachment: write {}", path.display()),
        source: anyhow::Error::new(e),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::gmail::client::GmailClient;
    use crate::gmail::service::GmailService;
    use crate::http::RetryPolicy;

    struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            Ok((
                200,
                r#"{"access_token":"NEW","expires_in":3600}"#.to_owned(),
            ))
        }
    }

    fn make_gmail(base_url: &str) -> GmailService<NoRefresh> {
        let state = TokenState {
            access_token: "T".into(),
            refresh_token: "R".into(),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            scopes: vec![],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        };
        let dir = std::env::temp_dir().join(format!(
            "gpm-dl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            dir,
        ));
        let client = Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        );
        GmailService::new(client, None)
    }

    /// Mount one `attachments.get` mock returning "Hello, world!" so both
    /// modes can share a single setup.
    async fn mount_hello_world(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m1/attachments/att1$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "size": 13,
                "data": "SGVsbG8sIHdvcmxkIQ"
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    /// Layer 2: `save_to = None` returns the base64 payload.
    #[tokio::test]
    async fn save_to_none_returns_base64_payload() {
        let server = MockServer::start().await;
        mount_hello_world(&server).await;
        let gmail = make_gmail(&server.uri());
        let out = download_attachment(
            &gmail,
            DownloadAttachmentInput {
                account: "work".into(),
                message_id: "m1".into(),
                attachment_id: "att1".into(),
                mime_type: "text/plain".into(),
                save_to: None,
            },
        )
        .await
        .expect("ok");
        assert_eq!(out.size_bytes, 13);
        assert_eq!(out.mime_type, "text/plain");
        assert!(out.saved_path.is_none());
        assert_eq!(out.data_base64.as_deref(), Some("SGVsbG8sIHdvcmxkIQ"));
    }

    /// Layer 2: `save_to = Some(path)` writes the file with mode 0600
    /// and returns the path.
    #[tokio::test]
    async fn save_to_some_writes_file_with_mode_600() {
        let server = MockServer::start().await;
        mount_hello_world(&server).await;
        let gmail = make_gmail(&server.uri());

        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("hello.txt");

        let out = download_attachment(
            &gmail,
            DownloadAttachmentInput {
                account: "work".into(),
                message_id: "m1".into(),
                attachment_id: "att1".into(),
                mime_type: "text/plain".into(),
                save_to: Some(path.clone()),
            },
        )
        .await
        .expect("ok");
        assert_eq!(out.saved_path.as_deref(), Some(path.as_path()));
        assert!(out.data_base64.is_none());

        let written = std::fs::read(&path).expect("read");
        assert_eq!(written, b"Hello, world!");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "actual mode = 0{mode:o}");
    }

    /// Existing target path must error rather than overwriting — the
    /// `create_new` open flag is the load-bearing guard.
    #[tokio::test]
    async fn save_to_refuses_to_overwrite_existing_file() {
        let server = MockServer::start().await;
        mount_hello_world(&server).await;
        let gmail = make_gmail(&server.uri());

        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("preexisting.txt");
        std::fs::write(&path, b"do not clobber").expect("seed");

        let err = download_attachment(
            &gmail,
            DownloadAttachmentInput {
                account: "work".into(),
                message_id: "m1".into(),
                attachment_id: "att1".into(),
                mime_type: "text/plain".into(),
                save_to: Some(path.clone()),
            },
        )
        .await
        .expect_err("err");
        match err {
            Error::Internal { context, .. } => {
                assert!(context.contains("download_attachment"), "got {context}");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
        // File still has its original contents.
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes, b"do not clobber");
    }

    /// Argument validation is delegated to `attachments::download`;
    /// confirm the error surfaces through the tool boundary unchanged.
    #[tokio::test]
    async fn empty_account_propagates_invalid_argument() {
        let gmail = make_gmail("http://localhost:1");
        let err = download_attachment(
            &gmail,
            DownloadAttachmentInput {
                account: String::new(),
                message_id: "m1".into(),
                attachment_id: "att1".into(),
                mime_type: "text/plain".into(),
                save_to: None,
            },
        )
        .await
        .expect_err("err");
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "account"));
    }
}
