//! Gmail API wrapper for `users.messages.attachments.get`.
//!
//! Returns the raw decoded byte buffer for a single attachment. Gmail
//! delivers the payload as base64url-encoded text on the wire; this module
//! handles the decode so the caller (the `download_attachment` tool, #63)
//! sees a plain `Vec<u8>`.
//!
//! Cost: 5 quota units per call (see [`crate::gmail::quota::GmailMethod::MessagesAttachmentsGet`]).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Deserialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::quota::GmailMethod;
use crate::http::percent_encode_path_segment;

/// Raw `users.messages.attachments.get` response. Gmail returns `size`
/// as an integer (Gmail's reported size in bytes) and `data` as the
/// base64url-encoded payload.
#[derive(Debug, Deserialize)]
struct RawAttachment {
    /// Gmail's reported size in bytes. Matches the `size` reported in
    /// `messages.get`'s nested `payload.parts[].body.size`.
    #[serde(default)]
    size: u64,
    /// Base64url-encoded payload. **Padding may or may not be present in
    /// Gmail's response** — handle both via the `Engine::decode_*`
    /// padding-tolerant variants in [`decode_payload`].
    #[serde(default)]
    data: String,
}

/// Decoded attachment bytes plus Gmail's reported size.
#[derive(Debug)]
pub(crate) struct AttachmentBytes {
    pub bytes: Vec<u8>,
    /// Gmail's reported size. For most callers this equals
    /// `bytes.len()`; we surface Gmail's number so the audit record can
    /// reflect the server's own accounting if the two ever diverge.
    pub size_bytes: u64,
}

/// Fetch one attachment by `(message_id, attachment_id)` for `account`.
///
/// Errors:
/// - [`Error::InvalidArgument`] when any of the three string args is empty.
/// - Upstream HTTP / auth errors from [`GmailClient::authed_get`].
/// - [`Error::Internal`] with `context = "attachments::decode"` when
///   the wire `data` field is not valid base64url.
pub(crate) async fn download<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    message_id: &str,
    attachment_id: &str,
) -> Result<AttachmentBytes, Error> {
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
    if attachment_id.is_empty() {
        return Err(Error::InvalidArgument {
            field: "attachment_id".into(),
            detail: "attachment_id must not be empty".into(),
        });
    }

    let path = format!(
        "/users/{a}/messages/{m}/attachments/{att}",
        a = percent_encode_path_segment(account),
        m = percent_encode_path_segment(message_id),
        att = percent_encode_path_segment(attachment_id),
    );
    let raw: RawAttachment = client
        .authed_get(account, &path, GmailMethod::MessagesAttachmentsGet.cost())
        .await?;
    let bytes = decode_payload(&raw.data)?;
    Ok(AttachmentBytes {
        bytes,
        size_bytes: raw.size,
    })
}

/// Decode Gmail's base64url-encoded `data` field. Gmail typically returns
/// the unpadded URL-safe alphabet (per RFC 4648 §5), but some clients have
/// reported padded responses in the wild. Try the canonical `NO_PAD`
/// decoder first; on `InvalidPadding`, fall back to `URL_SAFE` (which
/// expects padding). Map either failure to [`Error::Internal`].
fn decode_payload(data: &str) -> Result<Vec<u8>, Error> {
    URL_SAFE_NO_PAD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data))
        .map_err(|e| Error::Internal {
            context: "attachments::decode".into(),
            source: anyhow::Error::new(e),
        })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::tokens::{TokenManager, TokenState};
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

    fn make_client(base_url: &str) -> Arc<GmailClient<NoRefresh>> {
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
            "gpm-att-{}-{}",
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
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        )
    }

    #[test]
    fn decode_payload_handles_unpadded_url_safe() {
        // "Hello" in URL_SAFE_NO_PAD is "SGVsbG8".
        let out = decode_payload("SGVsbG8").expect("decode");
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn decode_payload_handles_padded_url_safe() {
        // Gmail-in-the-wild sometimes pads: "Hello" with padding is "SGVsbG8=".
        let out = decode_payload("SGVsbG8=").expect("decode");
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn decode_payload_rejects_garbage() {
        let err = decode_payload("not!!base64").expect_err("err");
        match err {
            Error::Internal { context, .. } => assert_eq!(context, "attachments::decode"),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_round_trips_bytes_and_size() {
        let server = MockServer::start().await;
        // "Hello, world!" → URL_SAFE_NO_PAD → "SGVsbG8sIHdvcmxkIQ".
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m1/attachments/att1$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "size": 13,
                "data": "SGVsbG8sIHdvcmxkIQ"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = make_client(&server.uri());
        let att = download(&client, "work", "m1", "att1").await.expect("ok");
        assert_eq!(att.bytes, b"Hello, world!");
        assert_eq!(att.size_bytes, 13);
    }

    #[tokio::test]
    async fn empty_account_rejected_pre_flight() {
        let client = make_client("http://localhost:1");
        let err = download(&client, "", "m1", "a1").await.expect_err("err");
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "account"));
    }

    #[tokio::test]
    async fn empty_message_id_rejected_pre_flight() {
        let client = make_client("http://localhost:1");
        let err = download(&client, "work", "", "a1").await.expect_err("err");
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "message_id"));
    }

    #[tokio::test]
    async fn empty_attachment_id_rejected_pre_flight() {
        let client = make_client("http://localhost:1");
        let err = download(&client, "work", "m1", "").await.expect_err("err");
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "attachment_id")
        );
    }
}
