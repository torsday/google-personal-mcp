//! `archive_thread` and `batch_archive` tools — remove the INBOX label from
//! one or many Gmail threads.
//!
//! Calls `threads.modify` with `removeLabelIds: ["INBOX"]` (10 quota units
//! per thread). Dry-run support via
//! [`crate::tools::destructive::DestructiveContext::should_apply`].

use std::sync::Arc;

use serde::Serialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::quota::GmailMethod;
use crate::gmail::service::GmailService;
use crate::http::percent_encode_path_segment;
use crate::tools::batch::{self, BatchMode, BatchResponse};
use crate::tools::destructive::{Decision, DestructiveContext};

// ── Input / output types ──────────────────────────────────────────────────────

pub(crate) struct ArchiveThreadInput {
    pub account: String,
    pub thread_id: String,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ArchiveThreadOutput {
    pub thread_id: String,
    /// `false` when `dry_run = true`; `true` after a successful Gmail call.
    pub applied: bool,
}

pub(crate) struct BatchArchiveInput {
    pub account: String,
    pub thread_ids: Vec<String>,
    pub dry_run: bool,
    pub mode: BatchMode,
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// Archive a single Gmail thread by removing the `INBOX` label.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "archive_thread",
        tool.account = %input.account,
        tool.thread_id = %input.thread_id,
        tool.dry_run = input.dry_run,
    ),
)]
pub(crate) async fn archive_thread<T: RefreshTransport>(
    gmail: &GmailService<T>,
    input: ArchiveThreadInput,
) -> Result<ArchiveThreadOutput, Error> {
    if input.account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    if input.thread_id.is_empty() {
        return Err(Error::InvalidArgument {
            field: "thread_id".into(),
            detail: "thread_id must not be empty".into(),
        });
    }

    match DestructiveContext::should_apply(input.dry_run) {
        Decision::DryRun => Ok(ArchiveThreadOutput {
            thread_id: input.thread_id,
            applied: false,
        }),
        Decision::Apply => {
            let path = format!(
                "/users/{}/threads/{}/modify",
                percent_encode_path_segment(&input.account),
                percent_encode_path_segment(&input.thread_id),
            );
            let body = serde_json::json!({ "removeLabelIds": ["INBOX"] });
            let _: serde_json::Value = gmail
                .client()
                .authed_post(
                    &input.account,
                    &path,
                    GmailMethod::ThreadsModify.cost(),
                    &body,
                )
                .await?;
            Ok(ArchiveThreadOutput {
                thread_id: input.thread_id,
                applied: true,
            })
        }
    }
}

/// Archive multiple Gmail threads concurrently.
///
/// Dispatches N concurrent [`archive_thread`] calls via
/// [`batch::run_thread_batch`] and collects per-item results. Never
/// short-circuits on failure — every id receives an entry in the output.
/// Input ordering is preserved.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "batch_archive",
        tool.account = %input.account,
        tool.count = input.thread_ids.len(),
        tool.dry_run = input.dry_run,
    ),
)]
pub(crate) async fn batch_archive<T: RefreshTransport + Send + Sync + 'static>(
    gmail: Arc<GmailService<T>>,
    input: BatchArchiveInput,
) -> Result<BatchResponse, Error> {
    batch::validate_batch_input(&input.account, &input.thread_ids)?;

    let results = if input.dry_run {
        batch::dry_run_results(input.thread_ids)
    } else {
        let account = Arc::new(input.account);
        batch::run_thread_batch(input.thread_ids, |tid| {
            let g = Arc::clone(&gmail);
            let a = Arc::clone(&account);
            async move {
                archive_thread(
                    &g,
                    ArchiveThreadInput {
                        account: (*a).clone(),
                        thread_id: tid,
                        dry_run: false,
                    },
                )
                .await
                .map(|_| ())
            }
        })
        .await
    };

    Ok(BatchResponse::from_items(results, input.mode))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::error::Error;
    use crate::gmail::client::GmailClient;
    use crate::gmail::service::GmailService;
    use crate::http::RetryPolicy;

    use super::*;

    // ── Minimal no-op refresh transport ──────────────────────────────────────

    #[derive(Clone)]
    struct NoRefresh;

    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            Ok((
                200,
                r#"{"access_token":"NEW","expires_in":3600}"#.to_owned(),
            ))
        }
    }

    fn make_client(base_url: &str) -> Arc<GmailService<NoRefresh>> {
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
            std::env::temp_dir().join(format!("gpm-ar-test-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("gpm-ar-test-{}", std::process::id())),
        )
        .unwrap();
        let client = Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        );
        Arc::new(GmailService::new(client, None))
    }

    // ── archive_thread tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn archive_thread_dry_run_returns_applied_false() {
        // No HTTP expectation — MockServer is started but we expect zero calls.
        let server = MockServer::start().await;
        let client = make_client(&server.uri());
        let out = archive_thread(
            &client,
            ArchiveThreadInput {
                account: "work".into(),
                thread_id: "thread-abc".into(),
                dry_run: true,
            },
        )
        .await
        .expect("dry_run must succeed");
        assert_eq!(out.thread_id, "thread-abc");
        assert!(!out.applied, "dry_run should produce applied=false");
        // Verify no HTTP calls were made.
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "dry_run must not make any HTTP calls"
        );
    }

    #[tokio::test]
    async fn archive_thread_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/work/threads/thread-abc/modify"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"id":"thread-abc","historyId":"42"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let out = archive_thread(
            &client,
            ArchiveThreadInput {
                account: "work".into(),
                thread_id: "thread-abc".into(),
                dry_run: false,
            },
        )
        .await
        .expect("happy path must succeed");
        assert_eq!(out.thread_id, "thread-abc");
        assert!(out.applied, "applied must be true after successful call");
    }

    // ── batch_archive tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_archive_dry_run() {
        let server = MockServer::start().await;
        let client = make_client(&server.uri());
        let out = batch_archive(
            client,
            BatchArchiveInput {
                account: "work".into(),
                thread_ids: vec!["t1".into(), "t2".into()],
                dry_run: true,
                mode: BatchMode::All,
            },
        )
        .await
        .expect("batch dry_run must succeed");
        // mode: all surfaces every per-item row.
        match out {
            BatchResponse::All {
                succeeded_count,
                results,
            } => {
                assert_eq!(succeeded_count, 2);
                assert_eq!(results.len(), 2);
                assert!(results.iter().all(|r| r.ok && r.error.is_none()));
            }
            other => panic!("expected All variant, got {other:?}"),
        }
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "dry_run must not make any HTTP calls"
        );
    }

    #[tokio::test]
    async fn batch_archive_mixed() {
        let server = MockServer::start().await;
        // First thread: 200 OK
        Mock::given(method("POST"))
            .and(path("/users/work/threads/t1/modify"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"id":"t1","historyId":"1"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        // Second thread: 404
        Mock::given(method("POST"))
            .and(path("/users/work/threads/t2/modify"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        // Default mode (failures_only): only the failed id appears in `failures`,
        // and succeeded_count confirms the rest succeeded.
        let out = batch_archive(
            client,
            BatchArchiveInput {
                account: "work".into(),
                thread_ids: vec!["t1".into(), "t2".into()],
                dry_run: false,
                mode: BatchMode::FailuresOnly,
            },
        )
        .await
        .expect("batch must complete even with partial failures");

        match out {
            BatchResponse::FailuresOnly {
                succeeded_count,
                failures,
            } => {
                assert_eq!(succeeded_count, 1, "t1 succeeded");
                assert_eq!(failures.len(), 1, "only the failed id is listed");
                assert_eq!(failures[0].thread_id, "t2");
                assert!(!failures[0].error.is_empty());
            }
            other => panic!("expected FailuresOnly variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_archive_validates_empty_thread_ids() {
        let client = make_client("http://localhost:1");
        let err = batch_archive(
            client,
            BatchArchiveInput {
                account: "work".into(),
                thread_ids: vec![],
                dry_run: false,
                mode: BatchMode::FailuresOnly,
            },
        )
        .await
        .expect_err("empty thread_ids must be rejected");
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "thread_ids"),
            "expected InvalidArgument for thread_ids, got: {err:?}"
        );
    }
}
