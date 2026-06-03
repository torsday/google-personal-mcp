//! Gmail-domain value types shared across the parser, client, and tool layer.
//!
//! Body and attachment shapes follow [ADR-0010](../../docs/adr/0010-mime-and-encoding.md).
//! Attacker-controllable fields use the `_untrusted` suffix per
//! [ADR-0018](../../docs/adr/0018-email-content-trust.md); the MCP response
//! layer is responsible for the `<<<UNTRUSTED ...>>>` wrapping — internal
//! types carry the raw bytes.

#![allow(clippy::struct_field_names)]

use serde::{Deserialize, Serialize};

/// Selected envelope headers exposed to consumers. The MIME parser surfaces
/// these directly from the raw RFC 822 stream; values are attacker-controlled.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Headers {
    pub from_untrusted: Option<String>,
    pub to_untrusted: Vec<String>,
    pub cc_untrusted: Vec<String>,
    pub bcc_untrusted: Vec<String>,
    pub subject_untrusted: Option<String>,
    pub date_untrusted: Option<String>,
    pub message_id_untrusted: Option<String>,
    pub in_reply_to_untrusted: Option<String>,
    pub references_untrusted: Vec<String>,
}

/// Best-effort, decoded body content for one message.
///
/// `text_untrusted` is the canonical plain-text body — either a `text/plain`
/// part decoded to UTF-8, or the `text/html` part rendered via `html2text`
/// when no plain part exists. `html_untrusted` is the raw `text/html` part
/// (decoded), if any. Both can be present simultaneously.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BodyContent {
    pub text_untrusted: Option<String>,
    pub html_untrusted: Option<String>,
    pub raw_length: usize,
    pub truncated: bool,
}

/// Attachment summary — metadata only. Bytes are fetched separately via
/// `download_attachment` (ADR-0010).
///
/// `attachment_id` is the parser's stable, positional identifier
/// (e.g. `"part-1-2"`) — the index path of the part within the MIME tree.
/// The `GmailClient` is responsible for correlating these to Gmail's
/// API-level `attachmentId` values when both shapes are available.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AttachmentMeta {
    pub attachment_id: String,
    pub filename_untrusted: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

/// One parsed Gmail message: headers, body, attachments. Produced by
/// [`crate::gmail::mime::parse_message`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ParsedMessage {
    pub headers: Headers,
    pub body: BodyContent,
    pub attachments: Vec<AttachmentMeta>,
}

/// A forwarded `message/rfc822` attachment parsed into a tree, produced by
/// [`crate::gmail::mime::parse_forwarded`] for the `parse_forwarded_attachment`
/// tool ([ADR-0026](../../docs/adr/0026-gmail-tool-surface-phase-2.md)).
///
/// `message` is this level's parsed content (same shape as a top-level
/// [`ParsedMessage`]). `forwarded` holds any nested `message/rfc822` parts found
/// *within* this message, each recursively parsed — a forward-within-a-forward.
/// `depth` is the 1-based nesting level (the directly-attached message is 1).
/// Recursion is bounded by the caller's `max_depth` so a deeply self-nested
/// forward cannot exhaust the stack ([ADR-0026] §`parse_forwarded_attachment`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ForwardedMessage {
    pub depth: u32,
    pub message: ParsedMessage,
    pub forwarded: Vec<Self>,
}
