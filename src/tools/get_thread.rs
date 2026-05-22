//! `get_thread` tool — fetches a Gmail thread with full message content.
//!
//! Calls `threads.get(format=FULL)` (40 quota units) and returns the thread
//! with per-message headers, body text, and attachment summaries. All
//! attacker-controlled fields are wrapped as `UntrustedString` per ADR-0018.

use chrono::{TimeZone as _, Utc};
use serde::Serialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::threads::{get_thread as fetch_thread, ParsedAttachment, ParsedThread};
use crate::gmail::untrusted::UntrustedString;

// ── Output types ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct AttachmentSummaryOutput {
    pub attachment_id: String,
    pub filename_untrusted: UntrustedString,
    pub mime_type: String,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessageOutput {
    pub message_id: String,
    pub from_untrusted: UntrustedString,
    pub to_untrusted: Vec<UntrustedString>,
    pub cc_untrusted: Vec<UntrustedString>,
    /// RFC 3339 UTC timestamp converted from Gmail's Unix milliseconds.
    pub internal_date: String,
    pub body_text_untrusted: UntrustedString,
    pub attachment_summaries: Vec<AttachmentSummaryOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GetThreadOutput {
    pub thread_id: String,
    pub subject_untrusted: UntrustedString,
    pub label_ids: Vec<String>,
    pub messages: Vec<MessageOutput>,
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// Fetch and map a Gmail thread to tool output.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "get_thread", tool.account = %account, tool.thread_id = %thread_id),
)]
pub(crate) async fn get_thread<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    thread_id: &str,
) -> Result<GetThreadOutput, Error> {
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

    let parsed: ParsedThread = fetch_thread(client, account, thread_id).await?;
    Ok(map_thread(parsed))
}

fn map_thread(parsed: ParsedThread) -> GetThreadOutput {
    // Subject comes from the first message; empty if no messages.
    let subject = parsed
        .messages
        .first()
        .map(|m| m.subject.clone())
        .unwrap_or_default();

    // label_ids union across all messages (preserving first-occurrence order).
    let mut label_ids: Vec<String> = Vec::new();
    for msg in &parsed.messages {
        for lid in &msg.label_ids {
            if !label_ids.contains(lid) {
                label_ids.push(lid.clone());
            }
        }
    }

    let messages = parsed.messages.into_iter().map(map_message).collect();

    GetThreadOutput {
        thread_id: parsed.thread_id,
        subject_untrusted: UntrustedString::new("SUBJECT", subject),
        label_ids,
        messages,
    }
}

fn map_message(parsed: crate::gmail::threads::ParsedMessage) -> MessageOutput {
    let internal_date = parse_internal_date(&parsed.internal_date_ms);

    let to_untrusted = parsed
        .to
        .into_iter()
        .map(|v| UntrustedString::new("TO", v))
        .collect();

    let cc_untrusted = parsed
        .cc
        .into_iter()
        .map(|v| UntrustedString::new("CC", v))
        .collect();

    let attachment_summaries = parsed.attachments.into_iter().map(map_attachment).collect();

    MessageOutput {
        message_id: parsed.message_id,
        from_untrusted: UntrustedString::new("FROM", parsed.from),
        to_untrusted,
        cc_untrusted,
        internal_date,
        body_text_untrusted: UntrustedString::new("BODY", parsed.body_text),
        attachment_summaries,
    }
}

fn map_attachment(att: ParsedAttachment) -> AttachmentSummaryOutput {
    AttachmentSummaryOutput {
        attachment_id: att.attachment_id,
        filename_untrusted: UntrustedString::new("FILENAME", att.filename),
        mime_type: att.mime_type,
        size_bytes: att.size_bytes,
    }
}

/// Convert Gmail's `internalDate` (Unix ms as string) to RFC 3339 UTC.
fn parse_internal_date(ms_str: &str) -> String {
    ms_str
        .parse::<i64>()
        .ok()
        .and_then(|ms| {
            let secs = ms / 1000;
            let nanos = u32::try_from((ms % 1000) * 1_000_000).ok()?;
            Utc.timestamp_opt(secs, nanos).single()
        })
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::gmail::threads::{ParsedAttachment, ParsedMessage, ParsedThread};

    fn make_parsed_message(subject: &str, from: &str, body: &str) -> ParsedMessage {
        ParsedMessage {
            message_id: "msg1".into(),
            internal_date_ms: "1717200000000".into(),
            label_ids: vec!["INBOX".into()],
            subject: subject.into(),
            from: from.into(),
            to: vec!["bob@example.com".into()],
            cc: vec![],
            body_text: body.into(),
            attachments: vec![],
        }
    }

    #[test]
    fn map_thread_extracts_subject_from_first_message() {
        let thread = ParsedThread {
            thread_id: "tid".into(),
            messages: vec![make_parsed_message("Hello", "alice@example.com", "body")],
        };
        let out = map_thread(thread);
        assert_eq!(out.thread_id, "tid");
        assert!(out.subject_untrusted.wrap().contains("Hello"));
    }

    #[test]
    fn parse_internal_date_converts_millis_to_rfc3339() {
        let result = parse_internal_date("1717200000000");
        assert!(result.starts_with("2024-"), "got: {result}");
        assert!(
            result.contains('T'),
            "expected RFC3339 T separator: {result}"
        );
    }

    #[test]
    fn parse_internal_date_empty_returns_empty() {
        assert_eq!(parse_internal_date(""), "");
    }

    #[test]
    fn map_message_wraps_untrusted_fields() {
        let parsed = make_parsed_message("Subj", "sender@example.com", "body content");
        let out = map_message(parsed);
        assert!(out.from_untrusted.wrap().contains("UNTRUSTED:FROM"));
        assert!(out.body_text_untrusted.wrap().contains("body content"));
    }

    #[test]
    fn attachment_mapped_correctly() {
        let att = ParsedAttachment {
            attachment_id: "att1".into(),
            filename: "doc.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 1024,
        };
        let out = map_attachment(att);
        assert_eq!(out.attachment_id, "att1");
        assert!(out.filename_untrusted.wrap().contains("doc.pdf"));
        assert_eq!(out.mime_type, "application/pdf");
        assert_eq!(out.size_bytes, 1024);
    }

    #[test]
    fn label_ids_union_deduped_across_messages() {
        let mut msg2 = make_parsed_message("Re: Hello", "bob@example.com", "reply");
        msg2.message_id = "msg2".into();
        msg2.label_ids = vec!["INBOX".into(), "STARRED".into()];

        let thread = ParsedThread {
            thread_id: "tid".into(),
            messages: vec![
                make_parsed_message("Hello", "alice@example.com", "body"),
                msg2,
            ],
        };
        let out = map_thread(thread);
        assert!(out.label_ids.contains(&"INBOX".to_owned()));
        assert!(out.label_ids.contains(&"STARRED".to_owned()));
        // INBOX should appear only once
        let inbox_count = out.label_ids.iter().filter(|l| *l == "INBOX").count();
        assert_eq!(inbox_count, 1);
    }
}
