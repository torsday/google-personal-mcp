//! `list_attachments` tool — enumerate downloadable attachments on a thread.
//!
//! Lighter-weight alternative to `get_thread` when the host already knows the
//! thread ID and just wants the attachment inventory. Per
//! [ADR-0016 §"Tools deferred to v0.3"](../../docs/adr/0016-tool-surface-and-conventions.md)
//! line 40 (SPEC story 28).
//!
//! Implementation: calls `GmailService::get_thread` (which already returns
//! parsed `ParsedAttachment` rows per message) and projects each
//! `(message_id, attachment)` pair into the wire shape. No body bytes are
//! returned — that's the sibling [`download_attachment`] tool's job.
//!
//! Cost: `threads.get(format=FULL)` = 40 quota units (one call, cache-aware).
//!
//! Untrusted content: `filename_untrusted` is wrapped per
//! [ADR-0018](../../docs/adr/0018-email-content-trust.md). Attachment IDs and
//! `mime_type` are server-assigned identifiers (not attacker-controlled
//! enough to require wrapping); `size_bytes` is a Gmail-reported integer.

use serde::Serialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::service::GmailService;
use crate::gmail::threads::ParsedThread;
use crate::gmail::untrusted::UntrustedString;

/// One row of the [`ListAttachmentsOutput::attachments`] array. Shape
/// matches the per-attachment slice of `get_thread`'s
/// `attachment_summaries`, plus the parent `message_id` so the host can
/// pair the row with a follow-up `download_attachment` call without
/// re-fetching the thread.
#[derive(Debug, Serialize)]
pub(crate) struct AttachmentRow {
    pub message_id: String,
    pub attachment_id: String,
    pub filename_untrusted: UntrustedString,
    pub mime_type: String,
    pub size_bytes: u64,
}

/// Tool output. Inventory is a flat array (rather than nested by message)
/// so the host LLM can iterate without an additional join.
#[derive(Debug, Serialize)]
pub(crate) struct ListAttachmentsOutput {
    pub thread_id: String,
    pub attachments: Vec<AttachmentRow>,
}

/// Fetch the thread and project attachment metadata. Bodies are
/// deliberately not returned.
///
/// Errors:
/// - [`Error::InvalidArgument`] for empty `account` or `thread_id`.
/// - Upstream errors from `GmailService::get_thread` (auth failure, 404
///   thread, quota exhaustion, etc).
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "list_attachments", tool.account = %account, tool.thread_id = %thread_id),
)]
pub(crate) async fn list_attachments<T: RefreshTransport + 'static>(
    gmail: &GmailService<T>,
    account: &str,
    thread_id: &str,
) -> Result<ListAttachmentsOutput, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    if thread_id.is_empty() {
        return Err(Error::InvalidArgument {
            field: "thread_id".into(),
            detail: "thread_id must not be empty".into(),
        });
    }

    let parsed: ParsedThread = gmail.get_thread(account, thread_id).await?;
    Ok(project(parsed))
}

fn project(parsed: ParsedThread) -> ListAttachmentsOutput {
    let mut attachments: Vec<AttachmentRow> = Vec::new();
    for msg in parsed.messages {
        for att in msg.attachments {
            attachments.push(AttachmentRow {
                message_id: msg.message_id.clone(),
                attachment_id: att.attachment_id,
                filename_untrusted: UntrustedString::new("FILENAME", att.filename),
                mime_type: att.mime_type,
                size_bytes: att.size_bytes,
            });
        }
    }
    ListAttachmentsOutput {
        thread_id: parsed.thread_id,
        attachments,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::gmail::client::GmailClient;
    use crate::gmail::service::GmailService;
    use crate::gmail::threads::{ParsedAttachment, ParsedMessage, ParsedThread};
    use crate::http::RetryPolicy;

    fn make_thread_with_attachments() -> ParsedThread {
        ParsedThread {
            thread_id: "tid1".into(),
            messages: vec![
                ParsedMessage {
                    message_id: "m1".into(),
                    internal_date_ms: "1717200000000".into(),
                    label_ids: vec!["INBOX".into()],
                    subject: "Hi".into(),
                    from: "a@example.com".into(),
                    to: vec![],
                    cc: vec![],
                    body_text: "body".into(),
                    attachments: vec![ParsedAttachment {
                        attachment_id: "att1".into(),
                        filename: "doc.pdf".into(),
                        mime_type: "application/pdf".into(),
                        size_bytes: 12_345,
                    }],
                },
                ParsedMessage {
                    message_id: "m2".into(),
                    internal_date_ms: "1717200001000".into(),
                    label_ids: vec!["INBOX".into()],
                    subject: "Re: Hi".into(),
                    from: "b@example.com".into(),
                    to: vec![],
                    cc: vec![],
                    body_text: "reply".into(),
                    // Two attachments on the second message — exercise the
                    // multi-attachment branch.
                    attachments: vec![
                        ParsedAttachment {
                            attachment_id: "att2".into(),
                            filename: "image.png".into(),
                            mime_type: "image/png".into(),
                            size_bytes: 4_096,
                        },
                        ParsedAttachment {
                            attachment_id: "att3".into(),
                            filename: "extra.zip".into(),
                            mime_type: "application/zip".into(),
                            size_bytes: 65_536,
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn project_flattens_per_message_attachments_into_one_array() {
        let out = project(make_thread_with_attachments());
        assert_eq!(out.thread_id, "tid1");
        assert_eq!(out.attachments.len(), 3);
        // First row paired with m1.
        assert_eq!(out.attachments[0].message_id, "m1");
        assert_eq!(out.attachments[0].attachment_id, "att1");
        assert_eq!(out.attachments[0].mime_type, "application/pdf");
        assert_eq!(out.attachments[0].size_bytes, 12_345);
        // Second + third paired with m2.
        assert_eq!(out.attachments[1].message_id, "m2");
        assert_eq!(out.attachments[2].message_id, "m2");
    }

    #[test]
    fn project_wraps_filename_as_untrusted() {
        let out = project(make_thread_with_attachments());
        let wrap = out.attachments[0].filename_untrusted.wrap();
        assert!(wrap.contains("UNTRUSTED:FILENAME"), "wrap = {wrap}");
        assert!(wrap.contains("doc.pdf"), "wrap = {wrap}");
    }

    #[test]
    fn project_returns_empty_array_when_no_attachments() {
        let thread = ParsedThread {
            thread_id: "tid".into(),
            messages: vec![ParsedMessage {
                message_id: "m1".into(),
                internal_date_ms: "0".into(),
                label_ids: vec![],
                subject: String::new(),
                from: String::new(),
                to: vec![],
                cc: vec![],
                body_text: String::new(),
                attachments: vec![],
            }],
        };
        let out = project(thread);
        assert_eq!(out.thread_id, "tid");
        assert!(out.attachments.is_empty());
    }

    #[tokio::test]
    async fn rejects_empty_account() {
        let gmail = make_passthrough_service("http://localhost:1");
        let err = list_attachments(&gmail, "", "tid").await.expect_err("err");
        match err {
            Error::InvalidArgument { field, .. } => assert_eq!(field, "account"),
            other => panic!("expected InvalidArgument(account), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_empty_thread_id() {
        let gmail = make_passthrough_service("http://localhost:1");
        let err = list_attachments(&gmail, "work", "").await.expect_err("err");
        match err {
            Error::InvalidArgument { field, .. } => assert_eq!(field, "thread_id"),
            other => panic!("expected InvalidArgument(thread_id), got {other:?}"),
        }
    }

    /// Layer 2 wiremock test: end-to-end call goes through to Gmail's
    /// `threads.get(format=FULL)` and the response is projected into the
    /// flat `attachments` array. Confirms the cache-aware
    /// `GmailService::get_thread` path is wired correctly and the
    /// per-message attachment metadata reaches the tool output.
    #[tokio::test]
    async fn fetches_thread_and_returns_attachment_inventory() {
        let server = MockServer::start().await;
        // `threads.get(format=FULL)` response with a single multipart
        // message containing two attachment parts. Body is enough to
        // exercise `parse_thread` so the projection sees real
        // `ParsedAttachment` rows.
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/tid42$"))
            .and(query_param("format", "FULL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "tid42",
                "messages": [{
                    "id": "msg1",
                    "threadId": "tid42",
                    "internalDate": "1717200000000",
                    "labelIds": ["INBOX"],
                    "sizeEstimate": 65536,
                    "payload": {
                        "mimeType": "multipart/mixed",
                        "headers": [
                            {"name": "Subject", "value": "Files attached"},
                            {"name": "From", "value": "sender@example.com"}
                        ],
                        "parts": [
                            {
                                "mimeType": "text/plain",
                                "headers": [],
                                "body": {"data": "aGVsbG8="}
                            },
                            {
                                "mimeType": "application/pdf",
                                "filename": "report.pdf",
                                "headers": [
                                    {"name": "Content-Disposition",
                                     "value": "attachment; filename=\"report.pdf\""}
                                ],
                                "body": {"attachmentId": "ATT-PDF", "size": 98765}
                            },
                            {
                                "mimeType": "image/png",
                                "filename": "diagram.png",
                                "headers": [
                                    {"name": "Content-Disposition",
                                     "value": "attachment; filename=\"diagram.png\""}
                                ],
                                "body": {"attachmentId": "ATT-PNG", "size": 4321}
                            }
                        ]
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let gmail = make_passthrough_service(&server.uri());
        let out = list_attachments(&gmail, "work", "tid42")
            .await
            .expect("list_attachments");

        assert_eq!(out.thread_id, "tid42");
        assert_eq!(out.attachments.len(), 2, "out: {out:?}");

        // Sorted by Gmail's part order; both rows paired with msg1.
        assert_eq!(out.attachments[0].message_id, "msg1");
        assert_eq!(out.attachments[0].attachment_id, "ATT-PDF");
        assert_eq!(out.attachments[0].mime_type, "application/pdf");
        assert_eq!(out.attachments[0].size_bytes, 98_765);
        let wrap = out.attachments[0].filename_untrusted.wrap();
        assert!(wrap.contains("report.pdf"), "wrap = {wrap}");

        assert_eq!(out.attachments[1].attachment_id, "ATT-PNG");
        assert_eq!(out.attachments[1].size_bytes, 4_321);
    }

    // ── Test fixtures ──────────────────────────────────────────────────────

    struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            Ok((
                200,
                r#"{"access_token":"NEW","expires_in":3600}"#.to_owned(),
            ))
        }
    }

    fn make_passthrough_service(base_url: &str) -> GmailService<NoRefresh> {
        let state = TokenState {
            access_token: "TOKEN".into(),
            refresh_token: "R".into(),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            scopes: vec![],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        };
        let tdir = std::env::temp_dir().join(format!(
            "gpm-listatt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&tdir).expect("mkdir");
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            tdir,
        ));
        let client = Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        );
        GmailService::new(client, None)
    }
}
