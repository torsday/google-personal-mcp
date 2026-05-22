//! `modify_thread_labels` and `batch_modify_thread_labels` tools — add and/or
//! remove arbitrary Gmail labels from one or many threads.
//!
//! Calls `threads.modify` with `addLabelIds` / `removeLabelIds` (10 quota
//! units per thread). Dry-run support via
//! [`crate::tools::destructive::DestructiveContext::should_apply`].

use std::sync::Arc;

use serde::Serialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::quota::GmailMethod;
use crate::http::percent_encode_path_segment;
use crate::tools::batch::{self, BatchItem};
use crate::tools::destructive::{Decision, DestructiveContext};

// ── Input / output types ──────────────────────────────────────────────────────

pub(crate) struct ModifyThreadLabelsInput {
    pub account: String,
    pub thread_id: String,
    /// Label IDs to add (may be empty if only removing).
    pub add_label_ids: Vec<String>,
    /// Label IDs to remove (may be empty if only adding).
    pub remove_label_ids: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModifyThreadLabelsOutput {
    pub thread_id: String,
    /// Label IDs as reported by Gmail after the modify call, or empty on `dry_run`.
    pub label_ids: Vec<String>,
    /// `false` when `dry_run = true`; `true` after a successful Gmail call.
    pub applied: bool,
}

pub(crate) struct BatchModifyThreadLabelsInput {
    pub account: String,
    pub thread_ids: Vec<String>,
    pub add_label_ids: Vec<String>,
    pub remove_label_ids: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct BatchModifyThreadLabelsOutput {
    pub results: Vec<BatchItem>,
}

// ── Gmail API response ────────────────────────────────────────────────────────

/// Subset of the Thread resource returned by `threads.modify`.
/// The union of all per-message `labelIds` is surfaced here.
#[derive(serde::Deserialize)]
struct ModifyResponse {
    id: String,
    #[serde(default)]
    messages: Vec<ModifyResponseMessage>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModifyResponseMessage {
    #[serde(default)]
    label_ids: Vec<String>,
}

fn extract_label_ids(resp: &ModifyResponse) -> Vec<String> {
    // Return the label_ids of the first message — representative of the
    // thread's current labelling after the modify call.
    resp.messages
        .first()
        .map(|m| m.label_ids.clone())
        .unwrap_or_default()
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// Modify the labels on a single Gmail thread.
pub(crate) async fn modify_thread_labels<T: RefreshTransport>(
    client: &GmailClient<T>,
    input: ModifyThreadLabelsInput,
) -> Result<ModifyThreadLabelsOutput, Error> {
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
    if input.add_label_ids.is_empty() && input.remove_label_ids.is_empty() {
        return Err(Error::InvalidArgument {
            field: "add_label_ids/remove_label_ids".into(),
            detail: "at least one of add_label_ids or remove_label_ids must be non-empty".into(),
        });
    }

    match DestructiveContext::should_apply(input.dry_run) {
        Decision::DryRun => Ok(ModifyThreadLabelsOutput {
            thread_id: input.thread_id,
            label_ids: vec![],
            applied: false,
        }),
        Decision::Apply => {
            let path = format!(
                "/users/{}/threads/{}/modify",
                percent_encode_path_segment(&input.account),
                percent_encode_path_segment(&input.thread_id),
            );
            let body = serde_json::json!({
                "addLabelIds": input.add_label_ids,
                "removeLabelIds": input.remove_label_ids,
            });
            let resp: ModifyResponse = client
                .authed_post(
                    &input.account,
                    &path,
                    GmailMethod::ThreadsModify.cost(),
                    &body,
                )
                .await?;
            let label_ids = extract_label_ids(&resp);
            Ok(ModifyThreadLabelsOutput {
                thread_id: resp.id,
                label_ids,
                applied: true,
            })
        }
    }
}

/// Apply the same label modification to multiple threads concurrently.
///
/// Dispatches N parallel `threads.modify` calls via
/// [`batch::run_thread_batch`]. Never short-circuits: per-item failures are
/// reported alongside successes. Input ordering is preserved (previously
/// sorted alphabetically — see issue #105).
pub(crate) async fn batch_modify_thread_labels<T: RefreshTransport + Send + Sync + 'static>(
    client: Arc<GmailClient<T>>,
    input: BatchModifyThreadLabelsInput,
) -> Result<BatchModifyThreadLabelsOutput, Error> {
    batch::validate_batch_input(&input.account, &input.thread_ids)?;

    // modify-specific: at least one of add/remove must be non-empty.
    if input.add_label_ids.is_empty() && input.remove_label_ids.is_empty() {
        return Err(Error::InvalidArgument {
            field: "add_label_ids/remove_label_ids".into(),
            detail: "at least one of add_label_ids or remove_label_ids must be non-empty".into(),
        });
    }

    if input.dry_run {
        return Ok(BatchModifyThreadLabelsOutput {
            results: batch::dry_run_results(input.thread_ids),
        });
    }

    let account = Arc::new(input.account);
    let add = Arc::new(input.add_label_ids);
    let remove = Arc::new(input.remove_label_ids);
    let results = batch::run_thread_batch(input.thread_ids, |tid| {
        let c = Arc::clone(&client);
        let a = Arc::clone(&account);
        let add = Arc::clone(&add);
        let remove = Arc::clone(&remove);
        async move {
            modify_thread_labels(
                &c,
                ModifyThreadLabelsInput {
                    account: (*a).clone(),
                    thread_id: tid,
                    add_label_ids: (*add).clone(),
                    remove_label_ids: (*remove).clone(),
                    dry_run: false,
                },
            )
            .await
            .map(|_| ())
        }
    })
    .await;

    Ok(BatchModifyThreadLabelsOutput { results })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use chrono::Duration as ChronoDuration;
    use chrono::Utc;

    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::error::Error;

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
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("personal".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            std::env::temp_dir().join(format!("mod-labels-test-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("mod-labels-test-{}", std::process::id())),
        )
        .unwrap();
        Arc::new(GmailClient::new(base_url, tokens, reqwest::Client::new()))
    }

    fn thread_resp(id: &str, labels: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "historyId": "1",
            "messages": [{"id": "m1", "labelIds": labels}]
        })
    }

    #[tokio::test]
    async fn dry_run_returns_applied_false_no_call() {
        let client = make_client("https://unused.example");
        let out = modify_thread_labels(
            &client,
            ModifyThreadLabelsInput {
                account: "personal".into(),
                thread_id: "tid1".into(),
                add_label_ids: vec!["STARRED".into()],
                remove_label_ids: vec![],
                dry_run: true,
            },
        )
        .await
        .expect("dry run ok");
        assert!(!out.applied);
        assert!(out.label_ids.is_empty());
    }

    #[tokio::test]
    async fn add_label_happy_path() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/personal/threads/tid1/modify"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(thread_resp("tid1", &["INBOX", "STARRED"])),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let client = make_client(&mock.uri());
        let out = modify_thread_labels(
            &client,
            ModifyThreadLabelsInput {
                account: "personal".into(),
                thread_id: "tid1".into(),
                add_label_ids: vec!["STARRED".into()],
                remove_label_ids: vec![],
                dry_run: false,
            },
        )
        .await
        .expect("add label ok");
        assert!(out.applied);
        assert!(out.label_ids.contains(&"STARRED".to_owned()));
    }

    #[tokio::test]
    async fn remove_label_happy_path() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/personal/threads/tid2/modify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(thread_resp("tid2", &["SENT"])))
            .expect(1)
            .mount(&mock)
            .await;

        let client = make_client(&mock.uri());
        let out = modify_thread_labels(
            &client,
            ModifyThreadLabelsInput {
                account: "personal".into(),
                thread_id: "tid2".into(),
                add_label_ids: vec![],
                remove_label_ids: vec!["INBOX".into()],
                dry_run: false,
            },
        )
        .await
        .expect("remove label ok");
        assert!(out.applied);
        assert!(!out.label_ids.contains(&"INBOX".to_owned()));
    }

    #[tokio::test]
    async fn rejects_empty_add_and_remove() {
        let client = make_client("https://unused.example");
        let err = modify_thread_labels(
            &client,
            ModifyThreadLabelsInput {
                account: "personal".into(),
                thread_id: "tid1".into(),
                add_label_ids: vec![],
                remove_label_ids: vec![],
                dry_run: false,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn batch_dry_run_all_ok() {
        let client = make_client("https://unused.example");
        let out = batch_modify_thread_labels(
            client,
            BatchModifyThreadLabelsInput {
                account: "personal".into(),
                thread_ids: vec!["a".into(), "b".into()],
                add_label_ids: vec!["STARRED".into()],
                remove_label_ids: vec![],
                dry_run: true,
            },
        )
        .await
        .expect("batch dry run ok");
        assert!(out.results.iter().all(|r| r.ok));
    }

    #[tokio::test]
    async fn batch_mixed_success_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/personal/threads/ok/modify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(thread_resp("ok", &["STARRED"])))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/users/personal/threads/err/modify"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(
                    serde_json::json!({"error": {"code": 403, "message": "forbidden"}}),
                ),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let client = make_client(&mock.uri());
        let out = batch_modify_thread_labels(
            client,
            BatchModifyThreadLabelsInput {
                account: "personal".into(),
                thread_ids: vec!["err".into(), "ok".into()],
                add_label_ids: vec!["STARRED".into()],
                remove_label_ids: vec![],
                dry_run: false,
            },
        )
        .await
        .expect("batch completes");
        // sorted: err < ok
        assert_eq!(out.results[0].thread_id, "err");
        assert!(!out.results[0].ok);
        assert_eq!(out.results[1].thread_id, "ok");
        assert!(out.results[1].ok);
    }
}
