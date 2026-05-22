//! `send_email` tool — RFC 2822 composition + thread-aware reply + dedup.
//!
//! Implements [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md)
//! §`send_email`. Reply mode prefetches the target thread's metadata to
//! populate `In-Reply-To` and `References` correctly. Wires through the
//! [`crate::tools::destructive::DestructiveContext`] safety net for
//! `dry_run` and send-deduplication.

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::compose::{compose_raw, ComposeInput};
use crate::gmail::quota::GmailMethod;
use crate::http::percent_encode_path_segment;
use crate::tools::destructive::{DestructiveContext, SendDecision, SendDedupKey};

/// Caller-supplied request shape per ADR-0016 `send_email`.
#[derive(Debug, Deserialize)]
pub(crate) struct SendEmailInput {
    pub account: String,
    /// `From` address. Caller supplies (typically resolved from `accounts.toml`'s
    /// email field by the tool layer above).
    pub from: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub in_reply_to_thread_id: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// Response shape per ADR-0016 §`send_email`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct SendEmailOutput {
    pub sent_message_id: Option<String>,
    pub thread_id: Option<String>,
    /// One of `"sent"`, `"deduped"`, `"would_send"`. See ADR-0012.
    pub dedup_action: String,
}

/// Raw response from `messages.send`.
#[derive(Debug, Deserialize)]
struct MessagesSendResponse {
    id: String,
    #[serde(rename = "threadId")]
    thread_id: Option<String>,
}

/// Body shape for `messages.send` per the API.
#[derive(Debug, Serialize)]
struct MessagesSendBody<'a> {
    raw: &'a str,
    #[serde(skip_serializing_if = "Option::is_none", rename = "threadId")]
    thread_id: Option<&'a str>,
}

/// Prefetch shape: only the metadata we need for reply-header threading.
#[derive(Debug, Deserialize)]
struct ThreadMetadata {
    #[serde(default)]
    messages: Vec<ThreadMessage>,
}

#[derive(Debug, Deserialize)]
struct ThreadMessage {
    #[serde(default)]
    payload: Option<ThreadMessagePayload>,
}

#[derive(Debug, Deserialize)]
struct ThreadMessagePayload {
    #[serde(default)]
    headers: Vec<ThreadMessageHeader>,
}

#[derive(Debug, Deserialize)]
struct ThreadMessageHeader {
    name: String,
    value: String,
}

/// Send one message. Drives validation → dedup-check → optional reply
/// prefetch → compose → Gmail `messages.send` → `record_send`.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "send_email",
        tool.account = %input.account,
        tool.dry_run = input.dry_run,
        tool.in_reply_to_thread_id = ?input.in_reply_to_thread_id,
    ),
)]
pub(crate) async fn send_email<T: RefreshTransport>(
    client: &GmailClient<T>,
    dedup: &DestructiveContext,
    input: &SendEmailInput,
) -> Result<SendEmailOutput, Error> {
    // Validate addresses BEFORE dedup so a bad input never lands in the cache.
    Error::check_header_field("From", &input.from)?;
    for a in input.to.iter().chain(&input.cc).chain(&input.bcc) {
        Error::check_header_field("To/Cc/Bcc", a)?;
    }
    Error::check_header_field("Subject", &input.subject)?;
    if input.to.is_empty() && input.cc.is_empty() && input.bcc.is_empty() {
        return Err(Error::InvalidArgument {
            field: "to".into(),
            detail: "at least one recipient required".into(),
        });
    }

    let dedup_key = SendDedupKey {
        account: &input.account,
        to: &input.to,
        cc: &input.cc,
        bcc: &input.bcc,
        subject: &input.subject,
        body_text: &input.body_text,
        in_reply_to_thread_id: input.in_reply_to_thread_id.as_deref(),
    };

    match dedup.should_send(input.dry_run, &dedup_key).await {
        SendDecision::DryRun => Ok(SendEmailOutput {
            sent_message_id: None,
            thread_id: input.in_reply_to_thread_id.clone(),
            dedup_action: "would_send".into(),
        }),
        SendDecision::Deduped {
            message_id,
            thread_id,
            ..
        } => Ok(SendEmailOutput {
            sent_message_id: Some(message_id),
            thread_id,
            dedup_action: "deduped".into(),
        }),
        SendDecision::Apply => apply_send(client, dedup, input, &dedup_key).await,
    }
}

async fn apply_send<T: RefreshTransport>(
    client: &GmailClient<T>,
    dedup: &DestructiveContext,
    input: &SendEmailInput,
    dedup_key: &SendDedupKey<'_>,
) -> Result<SendEmailOutput, Error> {
    // Reply path: prefetch the thread's latest Message-ID + References.
    let (in_reply_to, references) = if let Some(thread_id) = input.in_reply_to_thread_id.as_deref()
    {
        prefetch_reply_headers(client, &input.account, thread_id).await?
    } else {
        (None, Vec::new())
    };

    let raw = compose_raw(&ComposeInput {
        from: &input.from,
        to: &input.to,
        cc: &input.cc,
        bcc: &input.bcc,
        subject: &input.subject,
        body_text: &input.body_text,
        in_reply_to: in_reply_to.as_deref(),
        references: &references,
    })?;

    let body = MessagesSendBody {
        raw: &raw,
        thread_id: input.in_reply_to_thread_id.as_deref(),
    };
    let resp: MessagesSendResponse = client
        .authed_post(
            &input.account,
            "/users/me/messages/send",
            GmailMethod::MessagesSend.cost(),
            &body,
        )
        .await?;

    dedup
        .record_send(dedup_key, resp.id.clone(), resp.thread_id.clone())
        .await;

    Ok(SendEmailOutput {
        sent_message_id: Some(resp.id),
        thread_id: resp.thread_id,
        dedup_action: "sent".into(),
    })
}

/// Fetch `(In-Reply-To, References)` from the latest message in `thread_id`.
/// Returns `(None, [])` when the thread is empty (shouldn't happen) or when
/// the headers are absent (e.g. a draft).
async fn prefetch_reply_headers<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    thread_id: &str,
) -> Result<(Option<String>, Vec<String>), Error> {
    let path = format!(
        "/users/me/threads/{t}?format=metadata\
         &metadataHeaders=Message-ID&metadataHeaders=References",
        t = percent_encode_path_segment(thread_id),
    );
    let thread: ThreadMetadata = client
        .authed_get(account, &path, GmailMethod::ThreadsGet.cost())
        .await?;
    let Some(last) = thread.messages.last() else {
        return Ok((None, Vec::new()));
    };
    let Some(payload) = last.payload.as_ref() else {
        return Ok((None, Vec::new()));
    };
    let mut message_id = None;
    let mut references_raw = None;
    for h in &payload.headers {
        match h.name.to_ascii_lowercase().as_str() {
            "message-id" => message_id = Some(h.value.clone()),
            "references" => references_raw = Some(h.value.clone()),
            _ => {}
        }
    }
    // References per RFC 5322: prior thread Message-IDs joined by spaces.
    // The new send's References = prior References ++ this Message-ID.
    let mut references: Vec<String> = references_raw
        .map(|r| r.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    if let Some(mid) = message_id.as_deref() {
        references.push(mid.to_owned());
    }
    Ok((message_id, references))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::tokens::{ReqwestRefreshTransport, TokenManager, TokenState};
    use crate::http::RetryPolicy;
    use crate::rate_limit::KeyedRateLimiter;

    fn make_input(dry_run: bool, in_reply_to_thread_id: Option<String>) -> SendEmailInput {
        SendEmailInput {
            account: "work".into(),
            from: "alice@example.com".into(),
            to: vec!["bob@example.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "hi".into(),
            body_text: "body".into(),
            in_reply_to_thread_id,
            dry_run,
        }
    }

    fn fresh_state() -> TokenState {
        TokenState {
            access_token: "ACCESS".into(),
            refresh_token: "REFRESH".into(),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            scopes: vec!["https://www.googleapis.com/auth/gmail.send".into()],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    fn build_client(base_url: &str) -> GmailClient<ReqwestRefreshTransport> {
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), fresh_state())]),
            ReqwestRefreshTransport::new(reqwest::Client::new()),
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-send-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("gpm-send-{}", std::process::id())),
        )
        .expect("mkdir");
        GmailClient::new(base_url, tokens, reqwest::Client::new())
            .with_retry(RetryPolicy::for_tests())
            .with_rate_limiter(Arc::new(KeyedRateLimiter::new(10_000, 100)))
    }

    // ── Validation ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn header_injection_in_to_rejects_before_network() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/me/messages/send"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let client = build_client(&server.uri());
        let dedup = DestructiveContext::default();

        let mut input = make_input(false, None);
        input.to = vec!["victim@example.com\r\nBcc: attacker@evil.com".into()];

        let err = send_email(&client, &dedup, &input)
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::HeaderInjection { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn zero_recipients_rejects() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/me/messages/send"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let client = build_client(&server.uri());
        let dedup = DestructiveContext::default();

        let mut input = make_input(false, None);
        input.to = vec![];

        let err = send_email(&client, &dedup, &input)
            .await
            .expect_err("must fail");
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "to"),
            "got: {err:?}"
        );
    }

    // ── dry_run short-circuits ───────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_does_not_call_gmail() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/me/messages/send"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let client = build_client(&server.uri());
        let dedup = DestructiveContext::default();

        let input = make_input(true, None);
        let out = send_email(&client, &dedup, &input).await.expect("ok");
        assert_eq!(out.dedup_action, "would_send");
        assert!(out.sent_message_id.is_none());
    }

    // ── Send happy path ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn new_thread_send_posts_raw_and_returns_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/me/messages/send"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"id":"msg-1","threadId":"thr-1"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = build_client(&server.uri());
        let dedup = DestructiveContext::default();

        let input = make_input(false, None);
        let out = send_email(&client, &dedup, &input).await.expect("ok");
        assert_eq!(out.dedup_action, "sent");
        assert_eq!(out.sent_message_id.as_deref(), Some("msg-1"));
        assert_eq!(out.thread_id.as_deref(), Some("thr-1"));
    }

    // ── Dedup short-circuit ──────────────────────────────────────────────────

    #[tokio::test]
    async fn second_identical_send_is_deduped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/me/messages/send"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"id":"msg-1","threadId":"thr-1"}"#),
            )
            .expect(1) // only the FIRST call hits the network
            .mount(&server)
            .await;
        let client = build_client(&server.uri());
        let dedup = DestructiveContext::default();

        let input = make_input(false, None);
        let first = send_email(&client, &dedup, &input).await.expect("first ok");
        assert_eq!(first.dedup_action, "sent");

        let second = send_email(&client, &dedup, &input)
            .await
            .expect("second ok");
        assert_eq!(second.dedup_action, "deduped");
        assert_eq!(second.sent_message_id.as_deref(), Some("msg-1"));
    }

    // ── Reply path: prefetches thread metadata ───────────────────────────────

    #[tokio::test]
    async fn reply_prefetches_message_id_and_references() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me/threads/thr-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"messages":[{"payload":{"headers":[
                    {"name":"Message-ID","value":"<a@x>"}
                ]}},{"payload":{"headers":[
                    {"name":"Message-ID","value":"<b@x>"},
                    {"name":"References","value":"<a@x>"}
                ]}}]}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/users/me/messages/send"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"id":"msg-new","threadId":"thr-1"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client(&server.uri());
        let dedup = DestructiveContext::default();
        let input = make_input(false, Some("thr-1".into()));

        let out = send_email(&client, &dedup, &input).await.expect("ok");
        assert_eq!(out.sent_message_id.as_deref(), Some("msg-new"));
        assert_eq!(out.thread_id.as_deref(), Some("thr-1"));
    }
}
