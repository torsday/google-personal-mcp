//! `get_thread` tool — fetches a Gmail thread with optional format control.
//!
//! Supports three formats per [ADR-0027 §3]:
//! - `"full"` (default): headers + body + attachments (40 quota units)
//! - `"metadata"`: headers only, no body (40 quota units)
//! - `"minimal"`: IDs and label state only (40 quota units)
//!
//! All attacker-controlled fields are wrapped as `UntrustedString` per ADR-0018.
//!
//! [ADR-0027 §3]: ../../docs/adr/0027-v1-1-surface-refinements.md

use chrono::{TimeZone as _, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::service::GmailService;
use crate::gmail::threads::{
    ParsedAttachment, ParsedThread, ParsedThreadMinimal, ThreadMetadata,
};
use crate::gmail::untrusted::UntrustedString;

// ── Format enum ───────────────────────────────────────────────────────────────

/// Controls how much of the thread is fetched from Gmail and returned.
/// Passed through to Gmail's `threads.get` `format` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThreadFormat {
    /// Full thread: headers, body, attachments. Default.
    #[default]
    Full,
    /// Headers + structure only; no body text or attachments.
    Metadata,
    /// IDs and label state only; no headers, body, or attachments.
    Minimal,
}

// ── Output types ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct AttachmentSummaryOutput {
    pub attachment_id: String,
    pub filename_untrusted: UntrustedString,
    pub mime_type: String,
    pub size_bytes: u64,
}

/// Full-format message output: headers + body + attachments.
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

/// Full-format thread output.
#[derive(Debug, Serialize)]
pub(crate) struct GetThreadOutput {
    pub thread_id: String,
    pub subject_untrusted: UntrustedString,
    pub label_ids: Vec<String>,
    pub messages: Vec<MessageOutput>,
}

/// Metadata-format message output: headers, date, labels — no body.
#[derive(Debug, Serialize)]
pub(crate) struct MetadataMessageOutput {
    pub message_id: String,
    pub from_untrusted: UntrustedString,
    pub internal_date: String,
    pub label_ids: Vec<String>,
}

/// Metadata-format thread output: headers + structure, no body.
#[derive(Debug, Serialize)]
pub(crate) struct GetThreadMetadataOutput {
    pub thread_id: String,
    pub subject_untrusted: UntrustedString,
    pub label_ids: Vec<String>,
    pub messages: Vec<MetadataMessageOutput>,
}

/// Minimal-format message output: ID and label state only.
#[derive(Debug, Serialize)]
pub(crate) struct MinimalMessageOutput {
    pub message_id: String,
    pub label_ids: Vec<String>,
}

/// Minimal-format thread output: IDs and label state only.
#[derive(Debug, Serialize)]
pub(crate) struct GetThreadMinimalOutput {
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub messages: Vec<MinimalMessageOutput>,
}

/// Untagged enum so each variant serializes as its inner type directly.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum GetThreadResponse {
    Full(GetThreadOutput),
    Metadata(GetThreadMetadataOutput),
    Minimal(GetThreadMinimalOutput),
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// Fetch and map a Gmail thread to tool output, respecting the requested format.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "get_thread", tool.account = %account, tool.thread_id = %thread_id),
)]
pub(crate) async fn get_thread<T: RefreshTransport + 'static>(
    gmail: &GmailService<T>,
    account: &str,
    thread_id: &str,
    format: ThreadFormat,
) -> Result<GetThreadResponse, Error> {
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

    match format {
        ThreadFormat::Full => {
            let parsed: ParsedThread = gmail.get_thread(account, thread_id).await?;
            Ok(GetThreadResponse::Full(map_thread_full(parsed)))
        }
        ThreadFormat::Metadata => {
            let meta: ThreadMetadata = gmail.get_thread_metadata(account, thread_id).await?;
            Ok(GetThreadResponse::Metadata(map_thread_metadata(meta)))
        }
        ThreadFormat::Minimal => {
            let minimal: ParsedThreadMinimal = gmail.get_thread_minimal(account, thread_id).await?;
            Ok(GetThreadResponse::Minimal(map_thread_minimal(minimal)))
        }
    }
}

fn map_thread_full(parsed: ParsedThread) -> GetThreadOutput {
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

fn map_thread_metadata(meta: ThreadMetadata) -> GetThreadMetadataOutput {
    let subject = meta
        .messages
        .first()
        .map(|m| m.subject.clone())
        .unwrap_or_default();

    let mut label_ids: Vec<String> = Vec::new();
    for msg in &meta.messages {
        for lid in &msg.label_ids {
            if !label_ids.contains(lid) {
                label_ids.push(lid.clone());
            }
        }
    }

    let messages = meta
        .messages
        .into_iter()
        .map(|m| MetadataMessageOutput {
            message_id: m.message_id,
            from_untrusted: UntrustedString::new("FROM", m.from),
            internal_date: parse_internal_date(&m.internal_date_ms),
            label_ids: m.label_ids,
        })
        .collect();

    GetThreadMetadataOutput {
        thread_id: meta.thread_id,
        subject_untrusted: UntrustedString::new("SUBJECT", subject),
        label_ids,
        messages,
    }
}

fn map_thread_minimal(minimal: ParsedThreadMinimal) -> GetThreadMinimalOutput {
    let mut label_ids: Vec<String> = Vec::new();
    for msg in &minimal.messages {
        for lid in &msg.label_ids {
            if !label_ids.contains(lid) {
                label_ids.push(lid.clone());
            }
        }
    }

    let messages = minimal
        .messages
        .into_iter()
        .map(|m| MinimalMessageOutput {
            message_id: m.message_id,
            label_ids: m.label_ids,
        })
        .collect();

    GetThreadMinimalOutput {
        thread_id: minimal.thread_id,
        label_ids,
        messages,
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
    use crate::gmail::threads::{
        ParsedAttachment, ParsedMessage, ParsedMessageMinimal, ParsedThread, ParsedThreadMinimal,
        ThreadMetadata, ThreadMetadataMessage,
    };

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

    // ── Full format (Layer 1) ─────────────────────────────────────────────────

    #[test]
    fn map_thread_full_extracts_subject_from_first_message() {
        let thread = ParsedThread {
            thread_id: "tid".into(),
            messages: vec![make_parsed_message("Hello", "alice@example.com", "body")],
        };
        let out = map_thread_full(thread);
        assert_eq!(out.thread_id, "tid");
        assert!(out.subject_untrusted.wrap().contains("Hello"));
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
        let out = map_thread_full(thread);
        assert!(out.label_ids.contains(&"INBOX".to_owned()));
        assert!(out.label_ids.contains(&"STARRED".to_owned()));
        let inbox_count = out.label_ids.iter().filter(|l| *l == "INBOX").count();
        assert_eq!(inbox_count, 1);
    }

    // ── Metadata format (Layer 1) ─────────────────────────────────────────────

    #[test]
    fn map_thread_metadata_extracts_subject_and_wraps_from() {
        let meta = ThreadMetadata {
            thread_id: "tid-meta".into(),
            messages: vec![ThreadMetadataMessage {
                message_id: "msg-m1".into(),
                internal_date_ms: "1717200000000".into(),
                label_ids: vec!["INBOX".into()],
                size_estimate: 512,
                subject: "Metadata subject".into(),
                from: "sender@example.com".into(),
            }],
        };
        let out = map_thread_metadata(meta);
        assert_eq!(out.thread_id, "tid-meta");
        assert!(out.subject_untrusted.wrap().contains("Metadata subject"));
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].message_id, "msg-m1");
        assert!(out.messages[0]
            .from_untrusted
            .wrap()
            .contains("UNTRUSTED:FROM"));
        assert!(out.messages[0].internal_date.starts_with("2024-"));
    }

    #[test]
    fn map_thread_metadata_serializes_without_body_fields() {
        let meta = ThreadMetadata {
            thread_id: "tid".into(),
            messages: vec![ThreadMetadataMessage {
                message_id: "msg1".into(),
                internal_date_ms: "1717200000000".into(),
                label_ids: vec!["INBOX".into()],
                size_estimate: 256,
                subject: "Subj".into(),
                from: "a@b.com".into(),
            }],
        };
        let out = map_thread_metadata(meta);
        let json = serde_json::to_value(&out).unwrap();
        // body_text_untrusted and attachment_summaries must be absent
        assert!(json["messages"][0].get("body_text_untrusted").is_none());
        assert!(json["messages"][0].get("attachment_summaries").is_none());
    }

    // ── Minimal format (Layer 1) ──────────────────────────────────────────────

    #[test]
    fn map_thread_minimal_returns_ids_and_labels_only() {
        let minimal = ParsedThreadMinimal {
            thread_id: "tid-min".into(),
            messages: vec![ParsedMessageMinimal {
                message_id: "msg-min1".into(),
                label_ids: vec!["INBOX".into(), "UNREAD".into()],
            }],
        };
        let out = map_thread_minimal(minimal);
        assert_eq!(out.thread_id, "tid-min");
        assert!(out.label_ids.contains(&"INBOX".to_owned()));
        assert_eq!(out.messages[0].message_id, "msg-min1");
        // Serialized form must not contain any header or body fields
        let json = serde_json::to_value(&out).unwrap();
        assert!(json["messages"][0].get("from_untrusted").is_none());
        assert!(json["messages"][0].get("body_text_untrusted").is_none());
        assert!(json["messages"][0].get("subject_untrusted").is_none());
    }

    // ── ThreadFormat deserialization (Layer 1) ────────────────────────────────

    #[test]
    fn thread_format_default_is_full() {
        assert_eq!(ThreadFormat::default(), ThreadFormat::Full);
    }

    #[test]
    fn thread_format_deserializes_all_variants() {
        let full: ThreadFormat = serde_json::from_str(r#""full""#).unwrap();
        let meta: ThreadFormat = serde_json::from_str(r#""metadata""#).unwrap();
        let min: ThreadFormat = serde_json::from_str(r#""minimal""#).unwrap();
        assert_eq!(full, ThreadFormat::Full);
        assert_eq!(meta, ThreadFormat::Metadata);
        assert_eq!(min, ThreadFormat::Minimal);
    }

    #[test]
    fn thread_format_rejects_unknown_value() {
        let result = serde_json::from_str::<ThreadFormat>(r#""raw""#);
        assert!(result.is_err(), "unknown format must not deserialize");
    }

    // ── Misc ──────────────────────────────────────────────────────────────────

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
}
