//! Gmail API wrapper for `users.history.list`.
//!
//! Returns a typed view of the per-class entry arrays
//! ([ADR-0009](../../docs/adr/0009-caching-with-sqlite-and-history-api.md)
//! §"Sync protocol"). Cost: 2 quota units per call.
//!
//! The 404 `historyNotFound` response is signalled by surfacing
//! [`Error::Upstream`] with `status = 404` from the underlying client; the
//! sync driver pattern-matches that to trigger the reseed path.

use serde::Deserialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::quota::GmailMethod;
use crate::http::percent_encode_path_segment;

// ── Raw response shapes (Gmail wire format) ───────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHistoryResponse {
    #[serde(default)]
    history: Vec<RawHistoryRecord>,
    history_id: Option<String>,
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHistoryRecord {
    #[allow(dead_code)] // Gmail returns this per-record id; we don't currently use it.
    #[serde(default)]
    id: String,
    #[serde(default)]
    messages_added: Vec<RawMessageMutation>,
    #[serde(default)]
    messages_deleted: Vec<RawMessageMutation>,
    #[serde(default)]
    labels_added: Vec<RawLabelMutation>,
    #[serde(default)]
    labels_removed: Vec<RawLabelMutation>,
}

#[derive(Debug, Deserialize)]
struct RawMessageMutation {
    message: RawMessageRef,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMessageRef {
    id: String,
    #[serde(default)]
    thread_id: String,
    #[serde(default)]
    label_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLabelMutation {
    message: RawMessageRef,
    #[serde(default)]
    label_ids: Vec<String>,
}

// ── Parsed shape (what the sync driver consumes) ──────────────────────────────

/// Decoded `history.list` response. `history_id` is the response-top-level
/// id Gmail returns even when the `history[]` array is empty (means "no
/// changes since last known id; advance the watermark to here").
#[derive(Debug)]
pub(crate) struct HistoryPage {
    pub history_id: Option<String>,
    pub records: Vec<HistoryRecord>,
    pub next_page_token: Option<String>,
}

/// One record in the `history[]` array, broken out by event class. Empty
/// vecs are common: Gmail emits a record for every change, but each
/// record typically affects only one class.
#[derive(Debug, Default)]
pub(crate) struct HistoryRecord {
    pub messages_added: Vec<MessageRef>,
    pub messages_deleted: Vec<MessageRef>,
    pub labels_added: Vec<LabelChange>,
    pub labels_removed: Vec<LabelChange>,
}

/// Reference to a message that changed. `thread_id` may be empty when the
/// event class doesn't carry it (some Gmail responses include only the
/// message id for `messagesDeleted`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageRef {
    pub message_id: String,
    pub thread_id: String,
}

/// Per-event label add or remove. `label_ids` is the label set that
/// changed for this event (Gmail emits one record per atomic mutation;
/// `label_ids` may contain one or many ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelChange {
    pub message: MessageRef,
    pub label_ids: Vec<String>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Call `users.history.list` for `account` starting at `start_history_id`.
/// `max_results` clamps to the Gmail-documented maximum of 500.
///
/// Errors:
/// - [`Error::InvalidArgument`] if `account` is empty.
/// - [`Error::Upstream { status: 404, .. }`] when Gmail returns
///   `historyNotFound` — the sync driver maps this to the reseed path.
/// - Other HTTP/auth errors pass through.
pub(crate) async fn list_history<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
    start_history_id: &str,
    max_results: u32,
    page_token: Option<&str>,
) -> Result<HistoryPage, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    if start_history_id.is_empty() {
        return Err(Error::InvalidArgument {
            field: "start_history_id".into(),
            detail: "start_history_id must not be empty".into(),
        });
    }

    let clamped = max_results.clamp(1, 500);
    let mut qs = format!(
        "startHistoryId={s}&maxResults={m}",
        s = percent_encode_path_segment(start_history_id),
        m = clamped,
    );
    if let Some(tok) = page_token.filter(|t| !t.is_empty()) {
        qs.push_str("&pageToken=");
        qs.push_str(&percent_encode_path_segment(tok));
    }

    let path = format!(
        "/users/{a}/history?{qs}",
        a = percent_encode_path_segment(account),
    );
    let raw: RawHistoryResponse = client
        .authed_get(account, &path, GmailMethod::HistoryList.cost())
        .await?;
    Ok(parse(raw))
}

fn parse(raw: RawHistoryResponse) -> HistoryPage {
    HistoryPage {
        history_id: raw.history_id,
        next_page_token: raw.next_page_token,
        records: raw.history.into_iter().map(parse_record).collect(),
    }
}

fn parse_record(raw: RawHistoryRecord) -> HistoryRecord {
    HistoryRecord {
        messages_added: raw
            .messages_added
            .into_iter()
            .map(|m| message_ref(m.message))
            .collect(),
        messages_deleted: raw
            .messages_deleted
            .into_iter()
            .map(|m| message_ref(m.message))
            .collect(),
        labels_added: raw.labels_added.into_iter().map(label_change).collect(),
        labels_removed: raw.labels_removed.into_iter().map(label_change).collect(),
    }
}

fn message_ref(raw: RawMessageRef) -> MessageRef {
    MessageRef {
        message_id: raw.id,
        thread_id: raw.thread_id,
    }
}

fn label_change(raw: RawLabelMutation) -> LabelChange {
    LabelChange {
        message: message_ref(raw.message),
        label_ids: raw.label_ids,
    }
}

/// Returns `true` when `err` is the 404 `historyNotFound` signal — the
/// sync driver maps this to the reseed path.
pub(crate) const fn is_history_not_found(err: &Error) -> bool {
    matches!(err, Error::Upstream { status: 404, .. })
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
        let dir = std::env::temp_dir().join(format!(
            "gpm-hist-{}-{}",
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

    #[tokio::test]
    async fn list_history_parses_per_class_arrays() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/history"))
            .and(query_param("startHistoryId", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "history": [
                    {
                        "id": "1001",
                        "messagesAdded": [{"message": {"id": "m1", "threadId": "t1", "labelIds": ["INBOX"]}}],
                        "labelsAdded": [{"message": {"id": "m2", "threadId": "t2"}, "labelIds": ["STARRED"]}]
                    },
                    {
                        "id": "1002",
                        "messagesDeleted": [{"message": {"id": "m3", "threadId": "t3"}}],
                        "labelsRemoved": [{"message": {"id": "m4", "threadId": "t4"}, "labelIds": ["UNREAD", "INBOX"]}]
                    }
                ],
                "historyId": "1002",
                "nextPageToken": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = make_client(&server.uri());
        let page = list_history(&client, "work", "1000", 100, None)
            .await
            .expect("ok");
        assert_eq!(page.history_id.as_deref(), Some("1002"));
        assert!(page.next_page_token.is_none());
        assert_eq!(page.records.len(), 2);

        // Record 0 — one messagesAdded + one labelsAdded
        let r0 = &page.records[0];
        assert_eq!(r0.messages_added.len(), 1);
        assert_eq!(r0.messages_added[0].message_id, "m1");
        assert_eq!(r0.messages_added[0].thread_id, "t1");
        assert!(r0.messages_deleted.is_empty());
        assert_eq!(r0.labels_added.len(), 1);
        assert_eq!(r0.labels_added[0].label_ids, vec!["STARRED".to_owned()]);

        // Record 1 — one messagesDeleted + one labelsRemoved
        let r1 = &page.records[1];
        assert!(r1.messages_added.is_empty());
        assert_eq!(r1.messages_deleted[0].message_id, "m3");
        assert_eq!(r1.labels_removed[0].label_ids.len(), 2);
    }

    #[tokio::test]
    async fn list_history_404_is_history_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/history"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"code": 404, "message": "historyNotFound", "status": "NOT_FOUND"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = make_client(&server.uri());
        let err = list_history(&client, "work", "999999", 100, None)
            .await
            .expect_err("must 404");
        assert!(
            is_history_not_found(&err),
            "expected Upstream{{status: 404}}; got {err:?}",
        );
    }

    #[tokio::test]
    async fn list_history_empty_history_array_carries_advance_id() {
        // Gmail returns historyId even when no events occurred — that's the
        // signal to advance the watermark without doing any cache work.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/history"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "historyId": "1500"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = make_client(&server.uri());
        let page = list_history(&client, "work", "1499", 100, None)
            .await
            .expect("ok");
        assert!(page.records.is_empty());
        assert_eq!(page.history_id.as_deref(), Some("1500"));
    }

    #[tokio::test]
    async fn empty_account_or_start_id_returns_invalid_argument() {
        let client = make_client("http://localhost:1");
        let e1 = list_history(&client, "", "1", 100, None)
            .await
            .expect_err("ok");
        assert!(matches!(e1, Error::InvalidArgument { ref field, .. } if field == "account"));
        let e2 = list_history(&client, "work", "", 100, None)
            .await
            .expect_err("ok");
        assert!(
            matches!(e2, Error::InvalidArgument { ref field, .. } if field == "start_history_id")
        );
    }
}
