//! `trash_thread` and `batch_trash` tools — move Gmail threads to trash.
//!
//! Calls `threads.trash` (20 quota units per thread). Dry-run support via
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

pub(crate) struct TrashThreadInput {
    pub account: String,
    pub thread_id: String,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct TrashThreadOutput {
    pub thread_id: String,
    /// `false` when `dry_run = true`; `true` after a successful Gmail call.
    pub applied: bool,
}

pub(crate) struct BatchTrashInput {
    pub account: String,
    pub thread_ids: Vec<String>,
    pub dry_run: bool,
    pub mode: BatchMode,
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// Move a single Gmail thread to trash.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "trash_thread",
        tool.account = %input.account,
        tool.thread_id = %input.thread_id,
        tool.dry_run = input.dry_run,
    ),
)]
pub(crate) async fn trash_thread<T: RefreshTransport>(
    gmail: &GmailService<T>,
    input: TrashThreadInput,
) -> Result<TrashThreadOutput, Error> {
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
        Decision::DryRun => Ok(TrashThreadOutput {
            thread_id: input.thread_id,
            applied: false,
        }),
        Decision::Apply => {
            let path = format!(
                "/users/{}/threads/{}/trash",
                percent_encode_path_segment(&input.account),
                percent_encode_path_segment(&input.thread_id),
            );
            // threads.trash is a POST with empty body; response contains the trashed thread.
            let _: serde_json::Value = gmail
                .client()
                .authed_post(
                    &input.account,
                    &path,
                    GmailMethod::ThreadsTrash.cost(),
                    &serde_json::Value::Object(serde_json::Map::new()),
                )
                .await?;
            Ok(TrashThreadOutput {
                thread_id: input.thread_id,
                applied: true,
            })
        }
    }
}

/// Move multiple Gmail threads to trash concurrently.
///
/// Dispatches N parallel `threads.trash` calls via [`batch::run_thread_batch`].
/// Never short-circuits: all results are collected regardless of per-item
/// failures. Input ordering is preserved (previously sorted alphabetically —
/// see issue #105).
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "batch_trash",
        tool.account = %input.account,
        tool.count = input.thread_ids.len(),
        tool.dry_run = input.dry_run,
    ),
)]
pub(crate) async fn batch_trash<T: RefreshTransport + Send + Sync + 'static>(
    gmail: Arc<GmailService<T>>,
    input: BatchTrashInput,
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
                trash_thread(
                    &g,
                    TrashThreadInput {
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
    use super::*;
    use std::collections::HashMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use chrono::Duration as ChronoDuration;
    use chrono::Utc;

    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::error::Error;
    use crate::gmail::client::GmailClient;
    use crate::gmail::service::GmailService;

    #[derive(Clone)]
    struct NoRefresh;

    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _uri: &str, _body: String) -> Result<(u16, String), Error> {
            Ok((
                200,
                r#"{"access_token":"TOK","expires_in":3600}"#.to_owned(),
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
            HashMap::from([("personal".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            std::env::temp_dir().join(format!("trash-test-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("trash-test-{}", std::process::id())),
        )
        .unwrap();
        let client = Arc::new(GmailClient::new(base_url, tokens, reqwest::Client::new()));
        Arc::new(GmailService::new(client, None))
    }

    #[tokio::test]
    async fn trash_thread_dry_run_returns_applied_false() {
        let client = make_client("https://unused.example");
        let out = trash_thread(
            &client,
            TrashThreadInput {
                account: "personal".into(),
                thread_id: "tid1".into(),
                dry_run: true,
            },
        )
        .await
        .expect("dry run should not fail");
        assert!(!out.applied);
        assert_eq!(out.thread_id, "tid1");
    }

    #[tokio::test]
    async fn trash_thread_happy_path() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/personal/threads/tid1/trash"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "tid1", "historyId": "99"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let client = make_client(&mock.uri());
        let out = trash_thread(
            &client,
            TrashThreadInput {
                account: "personal".into(),
                thread_id: "tid1".into(),
                dry_run: false,
            },
        )
        .await
        .expect("should succeed");
        assert!(out.applied);
        assert_eq!(out.thread_id, "tid1");
    }

    #[tokio::test]
    async fn batch_trash_dry_run_all_ok() {
        let client = make_client("https://unused.example");
        let out = batch_trash(
            client,
            BatchTrashInput {
                account: "personal".into(),
                thread_ids: vec!["a".into(), "b".into()],
                dry_run: true,
                mode: BatchMode::All,
            },
        )
        .await
        .expect("dry run should not fail");
        match out {
            BatchResponse::All {
                succeeded_count,
                results,
            } => {
                assert_eq!(succeeded_count, 2);
                assert_eq!(results.len(), 2);
                assert!(results.iter().all(|r| r.ok));
            }
            other => panic!("expected All variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_trash_mixed_success_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/personal/threads/tid-ok/trash"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "tid-ok", "historyId": "1"})),
            )
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/users/personal/threads/tid-err/trash"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(
                    serde_json::json!({"error": {"code": 404, "message": "not found"}}),
                ),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let client = make_client(&mock.uri());
        // mode: all returns the full per-item list in input order.
        let out = batch_trash(
            client,
            BatchTrashInput {
                account: "personal".into(),
                thread_ids: vec!["tid-err".into(), "tid-ok".into()],
                dry_run: false,
                mode: BatchMode::All,
            },
        )
        .await
        .expect("batch should complete");
        match out {
            BatchResponse::All {
                succeeded_count,
                results,
            } => {
                assert_eq!(succeeded_count, 1);
                // Input order is preserved (issue #105): tid-err first.
                assert_eq!(results[0].thread_id, "tid-err");
                assert!(!results[0].ok);
                assert!(results[0].error.is_some());
                assert_eq!(results[1].thread_id, "tid-ok");
                assert!(results[1].ok);
            }
            other => panic!("expected All variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_trash_validates_empty_thread_ids() {
        let client = make_client("https://unused.example");
        let err = batch_trash(
            client,
            BatchTrashInput {
                account: "personal".into(),
                thread_ids: vec![],
                dry_run: false,
                mode: BatchMode::FailuresOnly,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }
}
