//! Gmail API wrapper for `threads.get(format=FULL)`.
//!
//! Deserializes Gmail's MIME tree directly from the API response (not raw RFC
//! 822 bytes) and extracts headers, body text, and attachment summaries.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Deserialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::quota::GmailMethod;
use crate::http::percent_encode_path_segment;

// ── Gmail API response shapes ─────────────────────────────────────────────────

/// Top-level thread response from `threads.get(format=FULL)`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawThread {
    pub id: String,
    #[serde(default)]
    pub messages: Vec<RawMessage>,
}

/// A single Gmail message (format=FULL or format=METADATA).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawMessage {
    pub id: String,
    /// Unix milliseconds as a string.
    #[serde(default)]
    pub internal_date: String,
    #[serde(default)]
    pub label_ids: Vec<String>,
    /// Per-message byte estimate from Gmail. Populated by both FULL and
    /// METADATA formats. Default `0` when absent (older test fixtures).
    #[serde(default)]
    pub size_estimate: u64,
    pub payload: Option<MessagePart>,
}

/// One node in the Gmail MIME tree.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessagePart {
    pub mime_type: Option<String>,
    #[serde(default)]
    pub headers: Vec<PartHeader>,
    pub body: Option<PartBody>,
    #[serde(default)]
    #[allow(clippy::use_self)] // recursive struct; `Self` not valid in field type position
    pub parts: Vec<MessagePart>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PartHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PartBody {
    pub data: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(rename = "attachmentId")]
    pub attachment_id: Option<String>,
}

// ── Parsed output types ───────────────────────────────────────────────────────

/// Parsed attachment summary extracted from a MIME part.
#[derive(Debug)]
pub(crate) struct ParsedAttachment {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

/// Parsed content of a single Gmail message.
#[derive(Debug)]
pub(crate) struct ParsedMessage {
    pub message_id: String,
    pub internal_date_ms: String,
    pub label_ids: Vec<String>,
    pub subject: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub body_text: String,
    pub attachments: Vec<ParsedAttachment>,
}

/// Parsed thread with all messages.
#[derive(Debug)]
pub(crate) struct ParsedThread {
    pub thread_id: String,
    pub messages: Vec<ParsedMessage>,
}

// ── Listing + metadata-only types (used by `search_threads`) ─────────────────

/// One entry in the raw `threads.list` response — the only three fields Gmail
/// returns at list time.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawListedThread {
    pub id: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub history_id: String,
}

/// Raw `threads.list` response. `next_page_token` is `None` on the final page.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawThreadsList {
    #[serde(default)]
    pub threads: Vec<RawListedThread>,
    pub next_page_token: Option<String>,
}

/// Per-message slice of a metadata-format `threads.get` response — only the
/// fields `search_threads` needs to hydrate a `ThreadSummary`. No body text;
/// no attachments.
#[derive(Debug)]
pub(crate) struct ThreadMetadataMessage {
    pub message_id: String,
    pub internal_date_ms: String,
    pub label_ids: Vec<String>,
    pub size_estimate: u64,
    pub subject: String,
    pub from: String,
}

/// Metadata-format thread used by `search_threads` hydration. Contains only
/// the fields needed for the `ThreadSummary` schema in ADR-0016 §`search_threads`
/// — message bodies are deliberately excluded.
#[derive(Debug)]
pub(crate) struct ThreadMetadata {
    pub thread_id: String,
    pub messages: Vec<ThreadMetadataMessage>,
}

/// Minimal-format thread: IDs and label state only. Used by `get_thread` when
/// `format = "minimal"` — callers that only need to check label state avoid the
/// 40-quota-unit full-content fetch.
#[derive(Debug)]
pub(crate) struct ParsedThreadMinimal {
    pub thread_id: String,
    pub messages: Vec<ParsedMessageMinimal>,
}

/// Per-message slice of a `format=minimal` `threads.get` response.
#[derive(Debug)]
pub(crate) struct ParsedMessageMinimal {
    pub message_id: String,
    pub label_ids: Vec<String>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Fetch a thread by ID using `threads.get(format=FULL)`.
pub(crate) async fn get_thread<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    thread_id: &str,
) -> Result<ParsedThread, Error> {
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

    let path = format!(
        "/users/{a}/threads/{t}?format=FULL",
        a = percent_encode_path_segment(account),
        t = percent_encode_path_segment(thread_id),
    );
    let raw: RawThread = client
        .authed_get(account, &path, GmailMethod::ThreadsGet.cost())
        .await?;

    Ok(parse_thread(raw))
}

/// Fetch a single message by ID via `messages.get` (20 quota units). `format`
/// is forwarded to Gmail's `format` parameter (`full` / `metadata` / `minimal`);
/// `parse_message` tolerates the lighter formats by leaving absent fields empty.
pub(crate) async fn get_message<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    message_id: &str,
    format: &str,
) -> Result<ParsedMessage, Error> {
    let raw = fetch_raw_message(client, account, message_id, format).await?;
    Ok(parse_message(raw))
}

/// Fetch the raw `messages.get(format=FULL)` payload for `message_id` — the
/// input to [`extract_body_parts`] for `get_full_body`.
pub(crate) async fn fetch_raw_message<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    message_id: &str,
    format: &str,
) -> Result<RawMessage, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    if message_id.is_empty() {
        return Err(Error::InvalidArgument {
            field: "message_id".into(),
            detail: "message_id must not be empty".into(),
        });
    }
    let path = format!(
        "/users/{a}/messages/{m}?format={f}",
        a = percent_encode_path_segment(account),
        m = percent_encode_path_segment(message_id),
        f = percent_encode_path_segment(format),
    );
    client
        .authed_get(account, &path, GmailMethod::MessagesGet.cost())
        .await
}

/// Walk a raw message's MIME tree and return its decoded `(text/plain,
/// text/html)` leaf bodies — the raw parts `get_full_body` selects between.
/// Unlike [`parse_message`], which collapses to a single `body_text`, this
/// preserves both representations.
pub(crate) fn extract_body_parts(raw: &RawMessage) -> (Option<String>, Option<String>) {
    let mut text_plain: Option<String> = None;
    let mut text_html: Option<String> = None;
    let mut attachments: Vec<ParsedAttachment> = Vec::new();
    if let Some(payload) = raw.payload.as_ref() {
        walk_part(payload, &mut text_plain, &mut text_html, &mut attachments);
    }
    (text_plain, text_html)
}

/// Find the declared MIME type of the part carrying `attachment_id` within a
/// raw message tree. Walks the full `MessagePart` tree (not just filename-bearing
/// attachments, unlike [`walk_part`]) so a `message/rfc822` part with no filename
/// is still located. Returns `None` when no part references that attachment id.
///
/// Used by `parse_forwarded_attachment` to validate that the referenced
/// attachment is `message/rfc822` *before* downloading and parsing it.
pub(crate) fn find_attachment_mime_type(raw: &RawMessage, attachment_id: &str) -> Option<String> {
    fn walk(part: &MessagePart, attachment_id: &str) -> Option<String> {
        if part.body.as_ref().and_then(|b| b.attachment_id.as_deref()) == Some(attachment_id) {
            return Some(part.mime_type.clone().unwrap_or_default());
        }
        part.parts
            .iter()
            .find_map(|child| walk(child, attachment_id))
    }
    raw.payload
        .as_ref()
        .and_then(|payload| walk(payload, attachment_id))
}

/// Issue `users.threads.list` with optional `q` (Gmail search syntax),
/// `max_results`, and `page_token`. Returns Gmail's raw envelope so the
/// caller can hydrate per-thread metadata separately.
///
/// Cost: 10 quota units (one call). Hydration in `search_threads` adds
/// `max_results × 40` for the parallel `threads.get(format=metadata)` fan-out.
pub(crate) async fn list_threads<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    query: &str,
    max_results: u32,
    page_token: Option<&str>,
) -> Result<RawThreadsList, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }

    // Build query string. `q` and `pageToken` only included when non-empty so
    // an empty search just lists the inbox.
    let mut qs = format!("maxResults={max_results}");
    if !query.is_empty() {
        qs.push_str("&q=");
        qs.push_str(&percent_encode_path_segment(query));
    }
    if let Some(tok) = page_token.filter(|t| !t.is_empty()) {
        qs.push_str("&pageToken=");
        qs.push_str(&percent_encode_path_segment(tok));
    }

    let path = format!(
        "/users/{a}/threads?{qs}",
        a = percent_encode_path_segment(account),
    );
    client
        .authed_get(account, &path, GmailMethod::ThreadsList.cost())
        .await
}

/// Fetch a thread in `format=metadata` with `metadataHeaders=From,Subject,Date`.
/// Returns only the headers + per-message envelope needed to build a
/// `ThreadSummary` — bodies are not requested.
///
/// Cost: 40 quota units regardless of format per Google's documented pricing.
pub(crate) async fn get_thread_metadata<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    thread_id: &str,
) -> Result<ThreadMetadata, Error> {
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

    let path = format!(
        "/users/{a}/threads/{t}?format=metadata\
         &metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date",
        a = percent_encode_path_segment(account),
        t = percent_encode_path_segment(thread_id),
    );
    let raw: RawThread = client
        .authed_get(account, &path, GmailMethod::ThreadsGet.cost())
        .await?;

    Ok(metadata_from_raw(raw))
}

/// Fetch a thread in `format=minimal`. Returns only message IDs and label
/// state — no headers, no body.
///
/// Cost: 40 quota units regardless of format per Google's documented pricing.
pub(crate) async fn get_thread_minimal<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    thread_id: &str,
) -> Result<ParsedThreadMinimal, Error> {
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

    let path = format!(
        "/users/{a}/threads/{t}?format=minimal",
        a = percent_encode_path_segment(account),
        t = percent_encode_path_segment(thread_id),
    );
    let raw: RawThread = client
        .authed_get(account, &path, GmailMethod::ThreadsGet.cost())
        .await?;

    Ok(minimal_from_raw(raw))
}

fn minimal_from_raw(raw: RawThread) -> ParsedThreadMinimal {
    let messages = raw
        .messages
        .into_iter()
        .map(|m| ParsedMessageMinimal {
            message_id: m.id,
            label_ids: m.label_ids,
        })
        .collect();
    ParsedThreadMinimal {
        thread_id: raw.id,
        messages,
    }
}

fn metadata_from_raw(raw: RawThread) -> ThreadMetadata {
    let messages = raw
        .messages
        .into_iter()
        .map(|m| {
            let mut subject = String::new();
            let mut from = String::new();
            if let Some(payload) = &m.payload {
                for h in &payload.headers {
                    match h.name.to_ascii_lowercase().as_str() {
                        "subject" => subject.clone_from(&h.value),
                        "from" => from.clone_from(&h.value),
                        _ => {}
                    }
                }
            }
            ThreadMetadataMessage {
                message_id: m.id,
                internal_date_ms: m.internal_date,
                label_ids: m.label_ids,
                size_estimate: m.size_estimate,
                subject,
                from,
            }
        })
        .collect();
    ThreadMetadata {
        thread_id: raw.id,
        messages,
    }
}

// ── Parsing logic ─────────────────────────────────────────────────────────────

fn parse_thread(raw: RawThread) -> ParsedThread {
    let messages = raw.messages.into_iter().map(parse_message).collect();
    ParsedThread {
        thread_id: raw.id,
        messages,
    }
}

pub(crate) fn parse_message(raw: RawMessage) -> ParsedMessage {
    let mut subject = String::new();
    let mut from = String::new();
    let mut to: Vec<String> = Vec::new();
    let mut cc: Vec<String> = Vec::new();

    // Extract headers from the top-level payload
    if let Some(ref payload) = raw.payload {
        for h in &payload.headers {
            match h.name.to_lowercase().as_str() {
                "subject" => subject.clone_from(&h.value),
                "from" => from.clone_from(&h.value),
                "to" => {
                    to = h
                        .value
                        .split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "cc" => {
                    cc = h
                        .value
                        .split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                _ => {}
            }
        }
    }

    let mut text_plain: Option<String> = None;
    let mut text_html: Option<String> = None;
    let mut attachments: Vec<ParsedAttachment> = Vec::new();

    if let Some(payload) = raw.payload {
        walk_part(&payload, &mut text_plain, &mut text_html, &mut attachments);
    }

    let body_text = match (text_plain, text_html) {
        (Some(t), _) => t,
        (None, Some(h)) => html_to_text(&h),
        (None, None) => String::new(),
    };

    ParsedMessage {
        message_id: raw.id,
        internal_date_ms: raw.internal_date,
        label_ids: raw.label_ids,
        subject,
        from,
        to,
        cc,
        body_text,
        attachments,
    }
}

/// Recursively walk a MIME tree collecting text/plain, text/html, and attachments.
fn walk_part(
    part: &MessagePart,
    text_plain: &mut Option<String>,
    text_html: &mut Option<String>,
    attachments: &mut Vec<ParsedAttachment>,
) {
    let mime = part.mime_type.as_deref().unwrap_or("").to_ascii_lowercase();

    // Check if this is an attachment (has a filename header and non-zero body size)
    let filename = part
        .headers
        .iter()
        .find(|h| h.name.to_lowercase() == "content-disposition")
        .and_then(|h| extract_filename_from_disposition(&h.value))
        .or_else(|| {
            part.headers
                .iter()
                .find(|h| h.name.to_lowercase() == "content-type")
                .and_then(|h| extract_filename_from_content_type(&h.value))
        });

    if let Some(fname) = filename {
        if !fname.is_empty() {
            let size = part.body.as_ref().map_or(0, |b| b.size);
            let attachment_id = part
                .body
                .as_ref()
                .and_then(|b| b.attachment_id.clone())
                .unwrap_or_default();
            attachments.push(ParsedAttachment {
                attachment_id,
                filename: fname,
                mime_type: mime,
                size_bytes: size,
            });
            return; // don't descend into attachment parts
        }
    }

    // Leaf text parts
    if mime == "text/plain" && text_plain.is_none() {
        if let Some(body) = part.body.as_ref() {
            if let Some(decoded) = decode_body_data(body.data.as_deref()) {
                *text_plain = Some(decoded);
            }
        }
    } else if mime == "text/html" && text_html.is_none() {
        if let Some(body) = part.body.as_ref() {
            if let Some(decoded) = decode_body_data(body.data.as_deref()) {
                *text_html = Some(decoded);
            }
        }
    }

    // Recurse into children
    for child in &part.parts {
        walk_part(child, text_plain, text_html, attachments);
    }
}

fn decode_body_data(data: Option<&str>) -> Option<String> {
    let data = data?;
    if data.is_empty() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(data).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100).unwrap_or_default()
}

fn extract_filename_from_disposition(value: &str) -> Option<String> {
    // e.g. "attachment; filename=\"foo.pdf\""
    if !value.to_lowercase().starts_with("attachment") {
        return None;
    }
    extract_param(value, "filename")
}

fn extract_filename_from_content_type(value: &str) -> Option<String> {
    extract_param(value, "name")
}

fn extract_param(header_value: &str, param: &str) -> Option<String> {
    let needle = format!("{param}=");
    for part in header_value.split(';') {
        let part = part.trim();
        if part.to_lowercase().starts_with(&needle) {
            let val = &part[needle.len()..];
            let val = val.trim_matches('"').trim_matches('\'');
            return Some(val.to_owned());
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::error::Error;
    use crate::gmail::client::GmailClient;
    use crate::http::RetryPolicy;

    use super::*;

    // ── Minimal mock transport ────────────────────────────────────────────────

    struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            Ok((
                200,
                r#"{"access_token":"NEW","expires_in":3600}"#.to_owned(),
            ))
        }
    }

    fn make_client(base_url: &str) -> Arc<GmailClient<NoRefresh>> {
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
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-th-test-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("gpm-th-test-{}", std::process::id())),
        )
        .unwrap();
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        )
    }

    fn b64(s: &str) -> String {
        URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    fn thread_response(thread_id: &str, messages: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({
            "id": thread_id,
            "historyId": "100",
            "messages": messages
        })
    }

    fn simple_message(msg_id: &str, subject: &str, from: &str, body: &str) -> serde_json::Value {
        serde_json::json!({
            "id": msg_id,
            "threadId": "tid",
            "labelIds": ["INBOX"],
            "internalDate": "1717200000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    {"name": "Subject", "value": subject},
                    {"name": "From", "value": from}
                ],
                "body": {"data": b64(body), "size": body.len()},
                "parts": []
            }
        })
    }

    // ── Test: single text/plain message ──────────────────────────────────────

    #[tokio::test]
    async fn single_text_plain_message() {
        let server = MockServer::start().await;
        let msgs = vec![simple_message(
            "msg1",
            "Hello",
            "alice@example.com",
            "body text",
        )];
        let body = thread_response("tid1", &msgs);
        Mock::given(method("GET"))
            .and(path_regex("/users/work/threads/tid1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let thread = get_thread(&client, "work", "tid1").await.expect("ok");
        assert_eq!(thread.thread_id, "tid1");
        assert_eq!(thread.messages.len(), 1);
        let msg = &thread.messages[0];
        assert_eq!(msg.message_id, "msg1");
        assert_eq!(msg.subject, "Hello");
        assert_eq!(msg.from, "alice@example.com");
        assert_eq!(msg.body_text, "body text");
    }

    // ── Test: multi-message thread ────────────────────────────────────────────

    #[tokio::test]
    async fn multi_message_thread() {
        let server = MockServer::start().await;
        let msgs = vec![
            simple_message("msg1", "Hello", "alice@example.com", "first"),
            simple_message("msg2", "Re: Hello", "bob@example.com", "second"),
        ];
        let body = thread_response("tid2", &msgs);
        Mock::given(method("GET"))
            .and(path_regex("/users/work/threads/tid2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let thread = get_thread(&client, "work", "tid2").await.expect("ok");
        assert_eq!(thread.messages.len(), 2);
        assert_eq!(thread.messages[0].body_text, "first");
        assert_eq!(thread.messages[1].body_text, "second");
    }

    // ── Test: HTML-only message falls back to html2text ───────────────────────

    #[tokio::test]
    async fn html_only_message_renders_text() {
        let server = MockServer::start().await;
        let html = "<html><body><p>Hello World</p></body></html>";
        let msgs3 = vec![serde_json::json!({
            "id": "msg3",
            "threadId": "tid3",
            "labelIds": ["INBOX"],
            "internalDate": "1717200000000",
            "payload": {
                "mimeType": "text/html",
                "headers": [
                    {"name": "Subject", "value": "HTML test"},
                    {"name": "From", "value": "sender@example.com"}
                ],
                "body": {"data": b64(html), "size": html.len()},
                "parts": []
            }
        })];
        let body = thread_response("tid3", &msgs3);
        Mock::given(method("GET"))
            .and(path_regex("/users/work/threads/tid3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let thread = get_thread(&client, "work", "tid3").await.expect("ok");
        let msg = &thread.messages[0];
        assert!(!msg.body_text.is_empty(), "html2text produced no output");
        assert!(
            msg.body_text.contains("Hello World"),
            "expected 'Hello World' in: {}",
            msg.body_text
        );
    }

    // ── Test: attachment summary extracted ────────────────────────────────────

    #[tokio::test]
    async fn attachment_part_collected() {
        let server = MockServer::start().await;
        let msgs4 = vec![serde_json::json!({
            "id": "msg4",
            "threadId": "tid4",
            "labelIds": ["INBOX"],
            "internalDate": "1717200000000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    {"name": "Subject", "value": "With attachment"},
                    {"name": "From", "value": "sender@example.com"}
                ],
                "body": {"size": 0},
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "headers": [],
                        "body": {"data": b64("see attached"), "size": 12},
                        "parts": []
                    },
                    {
                        "mimeType": "application/pdf",
                        "headers": [
                            {"name": "Content-Disposition", "value": "attachment; filename=\"report.pdf\""}
                        ],
                        "body": {"attachmentId": "att123", "size": 4096},
                        "parts": []
                    }
                ]
            }
        })];
        let body = thread_response("tid4", &msgs4);
        Mock::given(method("GET"))
            .and(path_regex("/users/work/threads/tid4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let thread = get_thread(&client, "work", "tid4").await.expect("ok");
        let msg = &thread.messages[0];
        assert_eq!(msg.body_text, "see attached");
        assert_eq!(msg.attachments.len(), 1);
        let att = &msg.attachments[0];
        assert_eq!(att.filename, "report.pdf");
        assert_eq!(att.attachment_id, "att123");
        assert_eq!(att.size_bytes, 4096);
        assert_eq!(att.mime_type, "application/pdf");
    }

    // ── Test: empty thread_id returns InvalidArgument ─────────────────────────

    #[tokio::test]
    async fn empty_thread_id_returns_invalid_argument() {
        let client = make_client("http://localhost:1");
        let err = get_thread(&client, "work", "")
            .await
            .expect_err("must fail");
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "thread_id"),
            "got: {err:?}"
        );
    }

    // ── Test: empty account returns InvalidArgument ───────────────────────────

    #[tokio::test]
    async fn empty_account_returns_invalid_argument() {
        let client = make_client("http://localhost:1");
        let err = get_thread(&client, "", "tid1")
            .await
            .expect_err("must fail");
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "account"),
            "got: {err:?}"
        );
    }

    // ── Test: thread_id containing `/` reaches the wire percent-encoded ──────
    //
    // Layer 2 wiremock test for issue #106. The wiremock matcher is anchored on
    // the exact `%2F`-encoded path; if `format!` interpolation lets the raw `/`
    // through, the matcher misses, the mock returns 404, and the test fails.

    #[tokio::test]
    async fn thread_id_with_slash_is_percent_encoded_on_the_wire() {
        let server = MockServer::start().await;
        let msgs = vec![simple_message("m", "S", "F", "body")];
        let body = thread_response("foo%2Fbar", &msgs);
        // path_regex matches the path component verbatim — `%2F` literal here.
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/foo%2Fbar$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let thread = get_thread(&client, "work", "foo/bar")
            .await
            .expect("encoded path should match");
        assert_eq!(thread.thread_id, "foo%2Fbar");
    }

    #[tokio::test]
    async fn thread_id_with_question_mark_does_not_smuggle_query() {
        // Without encoding, `thread_id="x?format=raw"` would override the
        // `?format=FULL` query string — proving the encoding is doing real work.
        let server = MockServer::start().await;
        let msgs = vec![simple_message("m", "S", "F", "body")];
        let body = thread_response("evil", &msgs);
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/x%3Fformat%3Draw$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let thread = get_thread(&client, "work", "x?format=raw")
            .await
            .expect("encoded path should match");
        assert_eq!(thread.thread_id, "evil");
    }
}
