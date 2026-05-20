//! `search_threads` — the load-bearing email-search primitive.
//!
//! Implements [ADR-0016 §`search_threads`](../../docs/adr/0016-tool-surface-and-conventions.md):
//! one `threads.list` followed by `max_results` parallel
//! `threads.get(format=metadata, metadataHeaders=From,Subject,Date)` calls,
//! producing a rich `ThreadSummary` per result so the host LLM rarely needs
//! a follow-up fetch.
//!
//! **Cost.** 10 quota units for the list + 40 per result hydrated. At the
//! default `max_results=25` that is `1010` units, ~⅙ of the per-user-per-minute
//! cap. The tool description surfaces the cost explicitly to the host.
//!
//! **Untrusted content.** Every attacker-controllable field — `subject`,
//! `from`, `snippet` — is wrapped per
//! [ADR-0018](../../docs/adr/0018-email-content-trust.md).

use std::sync::Arc;

use chrono::{TimeZone as _, Utc};
use serde::Serialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::threads::{
    get_thread_metadata, list_threads, RawListedThread, ThreadMetadata, ThreadMetadataMessage,
};
use crate::gmail::untrusted::UntrustedString;

/// Hard upper bound on `max_results`. ADR-0016 §`search_threads` caps at 100
/// (4010 quota units per call, ~⅔ of the per-user-per-minute budget).
pub(crate) const MAX_RESULTS_CEILING: u32 = 100;

/// Default `max_results` per ADR-0016.
pub(crate) const DEFAULT_MAX_RESULTS: u32 = 25;

// ── Output types ─────────────────────────────────────────────────────────────

/// One row of the `search_threads` response. Mirrors ADR-0016's
/// `ThreadSummary` shape exactly. All attacker-controlled fields are
/// suffixed `_untrusted` and serialize via [`UntrustedString`].
#[derive(Debug, Serialize)]
pub(crate) struct ThreadSummary {
    pub thread_id: String,
    pub snippet_untrusted: UntrustedString,
    pub history_id: String,
    pub subject_untrusted: UntrustedString,
    pub from_untrusted: UntrustedString,
    /// RFC 3339 UTC; derived from the latest message's `internalDate`.
    pub internal_date: String,
    /// Union of all `labelIds` across the thread, in first-occurrence order.
    pub label_ids: Vec<String>,
    pub message_count: u32,
    /// Sum of per-message `sizeEstimate` in bytes.
    pub size_estimate: u64,
}

/// Listing envelope per ADR-0016 §"Response convention".
#[derive(Debug, Serialize)]
pub(crate) struct SearchThreadsOutput {
    pub items: Vec<ThreadSummary>,
    pub next_page_token: Option<String>,
    /// Always `null` for v0.2 — Gmail does not expose a total estimate on
    /// `threads.list` responses. Kept in the schema for forward-compat.
    pub total_estimate: Option<u64>,
}

// ── Input ────────────────────────────────────────────────────────────────────

/// Owned input arguments. Mirrors the JSON schema 1:1 so the server's
/// arg-extraction layer can build this directly.
#[derive(Debug)]
pub(crate) struct SearchThreadsInput {
    pub account: String,
    pub query: String,
    pub max_results: u32,
    pub page_token: Option<String>,
}

// ── Core logic ───────────────────────────────────────────────────────────────

/// Run a search: `threads.list` → fan-out `threads.get(format=metadata)` →
/// hydrate `ThreadSummary` per result. Failures of individual hydration calls
/// propagate as `Error` — partial-success is not the right semantics for a
/// search-results page where any missing row would silently bias what the
/// host LLM sees.
pub(crate) async fn search_threads<T: RefreshTransport + Send + Sync + 'static>(
    client: Arc<GmailClient<T>>,
    input: SearchThreadsInput,
) -> Result<SearchThreadsOutput, Error> {
    let SearchThreadsInput {
        account,
        query,
        max_results,
        page_token,
    } = input;

    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    if max_results == 0 || max_results > MAX_RESULTS_CEILING {
        return Err(Error::InvalidArgument {
            field: "max_results".into(),
            detail: format!("must be 1..={MAX_RESULTS_CEILING}, got {max_results}"),
        });
    }

    // 1. threads.list
    let listed = list_threads(
        &client,
        &account,
        &query,
        max_results,
        page_token.as_deref(),
    )
    .await?;

    if listed.threads.is_empty() {
        return Ok(SearchThreadsOutput {
            items: vec![],
            next_page_token: listed.next_page_token,
            total_estimate: None,
        });
    }

    // Capture list-order before consuming `listed.threads` in the spawn loop —
    // `JoinSet` returns results in completion order, but the search-results
    // page must mirror Gmail's relevance/recency order from `threads.list`.
    let order: Vec<String> = listed.threads.iter().map(|t| t.id.clone()).collect();

    // Parallel hydration: spawn one `threads.get(format=metadata)` per result.
    // Each task carries forward `snippet` and `history_id` from the list call
    // since they are not repeated in the metadata response.
    let mut set = tokio::task::JoinSet::new();
    for listed_thread in listed.threads {
        let RawListedThread {
            id,
            snippet,
            history_id,
        } = listed_thread;
        let client_clone = Arc::clone(&client);
        let account_clone = account.clone();
        let id_for_task = id.clone();
        set.spawn(async move {
            let result = get_thread_metadata(&client_clone, &account_clone, &id_for_task).await;
            (id_for_task, snippet, history_id, result)
        });
    }

    // Collect results into a map keyed by thread_id, then reorder via `order`.
    // Failure of any hydration call is a hard error — partial pages would
    // silently bias what the host LLM sees.
    let mut by_id: std::collections::HashMap<
        String,
        (String, String, Result<ThreadMetadata, Error>),
    > = std::collections::HashMap::with_capacity(order.len());
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok((tid, snippet, hid, outcome)) => {
                by_id.insert(tid, (snippet, hid, outcome));
            }
            Err(join_err) => {
                tracing::error!(error = %join_err, "search_threads hydration task panicked");
                return Err(Error::Internal {
                    context: "search_threads hydration task panicked".into(),
                    source: anyhow::Error::new(join_err),
                });
            }
        }
    }

    let mut items = Vec::with_capacity(order.len());
    for tid in order {
        let (snippet, hid, outcome) = by_id.remove(&tid).ok_or_else(|| Error::Internal {
            context: format!("search_threads hydration result missing for thread_id `{tid}`"),
            source: anyhow::anyhow!("missing hydration result"),
        })?;
        let metadata = outcome?;
        items.push(build_summary(tid, snippet, hid, &metadata));
    }

    Ok(SearchThreadsOutput {
        items,
        next_page_token: listed.next_page_token,
        total_estimate: None,
    })
}

/// Build a `ThreadSummary` from one listed thread + its metadata hydration.
/// The "latest message" wins for `subject`, `from`, `internal_date` per
/// ADR-0016 §`search_threads`. Latest = max `internalDate`.
fn build_summary(
    thread_id: String,
    snippet: String,
    history_id: String,
    metadata: &ThreadMetadata,
) -> ThreadSummary {
    let message_count = u32::try_from(metadata.messages.len()).unwrap_or(u32::MAX);
    let size_estimate: u64 = metadata.messages.iter().map(|m| m.size_estimate).sum();

    // Union of label_ids preserving first-occurrence order.
    let mut label_ids: Vec<String> = Vec::new();
    for msg in &metadata.messages {
        for lid in &msg.label_ids {
            if !label_ids.contains(lid) {
                label_ids.push(lid.clone());
            }
        }
    }

    // Latest message wins for subject/from/internal_date.
    let latest: Option<&ThreadMetadataMessage> = metadata
        .messages
        .iter()
        .max_by_key(|m| m.internal_date_ms.parse::<i64>().unwrap_or(i64::MIN));

    let (subject, from, internal_date_ms) = latest.map_or_else(
        || (String::new(), String::new(), String::new()),
        |m| {
            (
                m.subject.clone(),
                m.from.clone(),
                m.internal_date_ms.clone(),
            )
        },
    );

    ThreadSummary {
        thread_id,
        snippet_untrusted: UntrustedString::new("SNIPPET", snippet),
        history_id,
        subject_untrusted: UntrustedString::new("SUBJECT", subject),
        from_untrusted: UntrustedString::new("FROM", from),
        internal_date: parse_internal_date(&internal_date_ms),
        label_ids,
        message_count,
        size_estimate,
    }
}

/// Convert Gmail's `internalDate` (Unix ms as string) to RFC 3339 UTC.
/// Identical to `tools::get_thread`'s helper; duplicated rather than
/// extracted to keep this tool's surface self-contained.
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::http::RetryPolicy;

    use super::*;

    // ── Mock transport identical to other tool tests ────────────────────────

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
        };
        let tmpdir = std::env::temp_dir().join(format!(
            "gpm-st-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            tmpdir,
        ));
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        )
    }

    fn b64(s: &str) -> String {
        URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    /// One metadata message in the shape Gmail returns it.
    fn meta_message(
        msg_id: &str,
        thread_id: &str,
        labels: &[&str],
        internal_date_ms: &str,
        size_estimate: u64,
        subject: &str,
        from: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": msg_id,
            "threadId": thread_id,
            "labelIds": labels,
            "internalDate": internal_date_ms,
            "sizeEstimate": size_estimate,
            "payload": {
                "headers": [
                    {"name": "Subject", "value": subject},
                    {"name": "From", "value": from},
                ],
                // No body / parts under format=metadata.
            }
        })
    }

    // ── Happy path: list + hydration ────────────────────────────────────────

    #[tokio::test]
    async fn happy_path_hydrates_each_listed_thread() {
        let server = MockServer::start().await;

        // threads.list → two threads
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "threads": [
                    {"id": "t1", "snippet": "hello there", "historyId": "100"},
                    {"id": "t2", "snippet": "more text",   "historyId": "101"}
                ]
            })))
            .mount(&server)
            .await;

        // threads.get for t1
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t1$"))
            .and(query_param("format", "metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "t1",
                "historyId": "100",
                "messages": [
                    meta_message("m1", "t1", &["INBOX", "UNREAD"], "1717200000000", 2048,
                                 "Hello", "alice@example.com"),
                    meta_message("m2", "t1", &["INBOX"],           "1717200060000", 1024,
                                 "Re: Hello", "bob@example.com"),
                ]
            })))
            .mount(&server)
            .await;

        // threads.get for t2
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t2$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "t2",
                "historyId": "101",
                "messages": [
                    meta_message("m3", "t2", &["INBOX"], "1717300000000", 512,
                                 "Subject2", "carol@example.com"),
                ]
            })))
            .mount(&server)
            .await;

        let _ = b64; // keep helper available for future tests

        let client = make_client(&server.uri());
        let out = search_threads(
            client,
            SearchThreadsInput {
                account: "work".into(),
                query: "from:alice".into(),
                max_results: 25,
                page_token: None,
            },
        )
        .await
        .expect("ok");

        assert_eq!(out.items.len(), 2);
        assert!(out.next_page_token.is_none());

        let t1 = out.items.iter().find(|s| s.thread_id == "t1").unwrap();
        // Latest message (m2) wins.
        assert!(t1.subject_untrusted.wrap().contains("Re: Hello"));
        assert!(t1.from_untrusted.wrap().contains("bob@example.com"));
        assert!(t1.snippet_untrusted.wrap().contains("hello there"));
        assert_eq!(t1.history_id, "100");
        assert_eq!(t1.message_count, 2);
        assert_eq!(t1.size_estimate, 3072);
        // Union of labels (insertion order).
        assert_eq!(
            t1.label_ids,
            vec!["INBOX".to_string(), "UNREAD".to_string()]
        );
        // RFC 3339 for the m2 internal_date_ms.
        assert!(t1.internal_date.starts_with("2024-"));
        assert!(t1.internal_date.contains('T'));
    }

    // ── Empty results ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_results_return_null_next_page_token_no_error() {
        let server = MockServer::start().await;

        // threads.list with no threads — Gmail omits the `threads` field
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resultSizeEstimate": 0
            })))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let out = search_threads(
            client,
            SearchThreadsInput {
                account: "work".into(),
                query: "from:nobody@nowhere".into(),
                max_results: 25,
                page_token: None,
            },
        )
        .await
        .expect("empty result is success");

        assert!(out.items.is_empty());
        assert!(out.next_page_token.is_none());
        assert!(out.total_estimate.is_none());
    }

    // ── Pagination round-trip ───────────────────────────────────────────────

    #[tokio::test]
    async fn page_token_round_trips() {
        let server = MockServer::start().await;

        // First call: respond with one thread and a next_page_token.
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads$"))
            .and(query_param("maxResults", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "threads": [{"id": "t1", "snippet": "s1", "historyId": "100"}],
                "nextPageToken": "PAGE2"
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t1$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "t1", "historyId": "100",
                "messages": [meta_message("m1","t1",&["INBOX"],"1717200000000",100,"S","F")]
            })))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let out = search_threads(
            client,
            SearchThreadsInput {
                account: "work".into(),
                query: String::new(),
                max_results: 1,
                page_token: None,
            },
        )
        .await
        .expect("ok");

        assert_eq!(out.next_page_token.as_deref(), Some("PAGE2"));
        assert_eq!(out.items.len(), 1);
    }

    // ── max_results bounds ──────────────────────────────────────────────────

    #[tokio::test]
    async fn max_results_zero_rejected() {
        let client = make_client("http://localhost:1");
        let err = search_threads(
            client,
            SearchThreadsInput {
                account: "work".into(),
                query: String::new(),
                max_results: 0,
                page_token: None,
            },
        )
        .await
        .expect_err("zero max_results must fail");
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "max_results"),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn max_results_above_ceiling_rejected() {
        let client = make_client("http://localhost:1");
        let err = search_threads(
            client,
            SearchThreadsInput {
                account: "work".into(),
                query: String::new(),
                max_results: MAX_RESULTS_CEILING + 1,
                page_token: None,
            },
        )
        .await
        .expect_err("over ceiling must fail");
        assert!(matches!(
            err,
            Error::InvalidArgument { ref field, .. } if field == "max_results"
        ));
    }

    #[tokio::test]
    async fn empty_account_rejected() {
        let client = make_client("http://localhost:1");
        let err = search_threads(
            client,
            SearchThreadsInput {
                account: String::new(),
                query: String::new(),
                max_results: 25,
                page_token: None,
            },
        )
        .await
        .expect_err("empty account must fail");
        assert!(matches!(
            err,
            Error::InvalidArgument { ref field, .. } if field == "account"
        ));
    }

    // ── Untrusted-wrapping survives serialization ───────────────────────────

    #[tokio::test]
    async fn untrusted_fields_serialize_wrapped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "threads": [{"id":"t1","snippet":"untrusted snippet","historyId":"100"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t1$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"t1","historyId":"100",
                "messages":[meta_message("m1","t1",&["INBOX"],"1717200000000",100,
                                          "evil subject","attacker@evil.example")]
            })))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let out = search_threads(
            client,
            SearchThreadsInput {
                account: "work".into(),
                query: String::new(),
                max_results: 1,
                page_token: None,
            },
        )
        .await
        .expect("ok");

        let json = serde_json::to_string(&out).expect("ser");
        assert!(json.contains("<<<UNTRUSTED:SUBJECT"), "got: {json}");
        assert!(json.contains("<<<UNTRUSTED:FROM"), "got: {json}");
        assert!(json.contains("<<<UNTRUSTED:SNIPPET"), "got: {json}");
        assert!(json.contains("UNTRUSTED>>>"), "got: {json}");
    }

    // ── parse_internal_date is testable directly ────────────────────────────

    #[test]
    fn parse_internal_date_converts() {
        let r = parse_internal_date("1717200000000");
        assert!(r.starts_with("2024-"), "got: {r}");
        assert!(r.contains('T'));
    }

    #[test]
    fn parse_internal_date_empty_is_empty() {
        assert_eq!(parse_internal_date(""), "");
    }
}
