//! `list_labels` tool — returns all Gmail labels for an account.
//!
//! Calls `users.labels.list` (1 quota unit) and maps the response
//! per [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md)
//! §`list_labels`.

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::quota::GmailMethod;
use crate::gmail::service::GmailService;
use crate::http::percent_encode_path_segment;

// ── Gmail API response shapes ─────────────────────────────────────────────────

/// Raw response from `users.labels.list`.
#[derive(Debug, Deserialize)]
struct LabelsListResponse {
    #[serde(default)]
    labels: Vec<RawLabel>,
}

/// A single label as returned by the Gmail API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLabel {
    id: String,
    name: String,
    /// `"system"` or `"user"` per the Gmail API.
    #[serde(rename = "type")]
    label_type: String,
    messages_total: Option<u32>,
    messages_unread: Option<u32>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// A single label item in the `list_labels` response.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct LabelItem {
    pub label_id: String,
    /// User-assigned name or well-known system name (e.g. `INBOX`, `STARRED`).
    /// Operator-owned or Google-assigned — treated as trusted per ADR-0018.
    pub name: String,
    /// `"system"` or `"user"`.
    pub kind: String,
    pub messages_total: Option<u32>,
    pub messages_unread: Option<u32>,
}

/// Response envelope per ADR-0016 §`list_labels`.
#[derive(Debug, Serialize)]
pub(crate) struct ListLabelsOutput {
    pub items: Vec<LabelItem>,
}

// ── Core logic ────────────────────────────────────────────────────────────────

fn map_raw_label(r: RawLabel) -> LabelItem {
    LabelItem {
        label_id: r.id,
        name: r.name,
        kind: r.label_type,
        messages_total: r.messages_total,
        messages_unread: r.messages_unread,
    }
}

/// Fetch all labels for `account` and return the listing envelope.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "list_labels", tool.account = %account),
)]
pub(crate) async fn list_labels<T: RefreshTransport>(
    gmail: &GmailService<T>,
    account: &str,
) -> Result<ListLabelsOutput, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }

    let path = format!(
        "/users/{a}/labels",
        a = percent_encode_path_segment(account),
    );
    let resp: LabelsListResponse = gmail
        .client()
        .authed_get(account, &path, GmailMethod::LabelsList.cost())
        .await?;

    let items = resp.labels.into_iter().map(map_raw_label).collect();
    Ok(ListLabelsOutput { items })
}

// ── Pure-logic unit tests (Layer 1 — no I/O) ─────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn raw(id: &str, name: &str, label_type: &str) -> RawLabel {
        RawLabel {
            id: id.into(),
            name: name.into(),
            label_type: label_type.into(),
            messages_total: None,
            messages_unread: None,
        }
    }

    #[test]
    fn map_raw_label_system() {
        let item = map_raw_label(raw("INBOX", "INBOX", "system"));
        assert_eq!(item.label_id, "INBOX");
        assert_eq!(item.name, "INBOX");
        assert_eq!(item.kind, "system");
        assert!(item.messages_total.is_none());
        assert!(item.messages_unread.is_none());
    }

    #[test]
    fn map_raw_label_user_with_counts() {
        let r = RawLabel {
            id: "Label_1".into(),
            name: "Work".into(),
            label_type: "user".into(),
            messages_total: Some(42),
            messages_unread: Some(3),
        };
        let item = map_raw_label(r);
        assert_eq!(item.kind, "user");
        assert_eq!(item.messages_total, Some(42));
        assert_eq!(item.messages_unread, Some(3));
    }

    #[test]
    fn output_serialises_to_expected_shape() {
        let out = ListLabelsOutput {
            items: vec![LabelItem {
                label_id: "INBOX".into(),
                name: "INBOX".into(),
                kind: "system".into(),
                messages_total: Some(10),
                messages_unread: Some(2),
            }],
        };
        let json = serde_json::to_value(&out).expect("serialise");
        assert_eq!(json["items"][0]["label_id"], "INBOX");
        assert_eq!(json["items"][0]["name"], "INBOX");
        assert_eq!(json["items"][0]["kind"], "system");
        assert_eq!(json["items"][0]["messages_total"], 10);
        assert_eq!(json["items"][0]["messages_unread"], 2);
    }

    // ── Layer 2 fan-out integration test (#84) ────────────────────────────────
    //
    // Spin up wiremock, register three accounts in the TokenManager, call
    // `run_fanout` with a closure that invokes `list_labels` for each
    // account. Two accounts succeed (200 + JSON), one returns 503. Assert
    // the resulting FanoutResponse has the right shape and partial-success
    // semantics — failures surface as `outcome: "error"`, not top-level
    // errors.

    mod fanout_layer2 {
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
        use crate::tools::fanout::{self, FanoutOutcome};
        use crate::tools::list_labels;

        struct NoRefresh;
        impl RefreshTransport for NoRefresh {
            async fn post_form(
                &self,
                _token_uri: &str,
                _body: String,
            ) -> Result<(u16, String), Error> {
                Ok((
                    200,
                    r#"{"access_token":"NEW","expires_in":3600}"#.to_owned(),
                ))
            }
        }

        fn state() -> TokenState {
            TokenState {
                access_token: "TOKEN".into(),
                refresh_token: "R".into(),
                expires_at: Utc::now() + ChronoDuration::seconds(3600),
                scopes: vec![],
                client_id: "cid".into(),
                client_secret: "csec".into(),
                failed_until: None,
                consecutive_failures: 0,
                last_refresh_at: None,
            }
        }

        #[tokio::test]
        async fn three_account_fanout_one_failure() {
            let server = MockServer::start().await;

            // Two healthy accounts return one INBOX label each.
            for acct in ["work", "personal"] {
                Mock::given(method("GET"))
                    .and(path(format!("/users/{acct}/labels")))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "labels": [
                            {"id": "INBOX", "name": "INBOX", "type": "system"}
                        ]
                    })))
                    .mount(&server)
                    .await;
            }

            // Third account returns 503 — must surface as a per-account error
            // entry, not a top-level failure.
            Mock::given(method("GET"))
                .and(path("/users/acme/labels"))
                .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
                .mount(&server)
                .await;

            // Three-account TokenManager.
            let tmpdir = std::env::temp_dir().join(format!(
                "gpm-ll-fanout-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::create_dir_all(&tmpdir).unwrap();
            let accounts = HashMap::from([
                ("work".to_owned(), state()),
                ("personal".to_owned(), state()),
                ("acme".to_owned(), state()),
            ]);
            let tokens = Arc::new(TokenManager::new(
                accounts,
                NoRefresh,
                "https://example/token",
                tmpdir,
            ));
            let client = Arc::new(
                GmailClient::new(server.uri(), tokens, reqwest::Client::new())
                    .with_retry(RetryPolicy::for_tests()),
            );
            let gmail = Arc::new(GmailService::new(client, None));

            // Run fan-out: closure clones the Arc into each per-account future.
            let resp = fanout::run_fanout(
                vec!["work".into(), "personal".into(), "acme".into()],
                fanout::FanoutConfig::default(),
                move |acct| {
                    let gmail = Arc::clone(&gmail);
                    async move { list_labels::list_labels(&gmail, &acct).await }
                },
            )
            .await;

            // Envelope shape.
            assert!(resp.fanout);
            assert_eq!(resp.summary.total_accounts, 3);
            assert_eq!(resp.summary.succeeded, 2);
            assert_eq!(resp.summary.failed, 1);

            // Per-account outcomes (sorted alphabetically by alias).
            let aliases: Vec<&str> = resp.accounts.iter().map(|r| r.account.as_str()).collect();
            assert_eq!(aliases, vec!["acme", "personal", "work"]);

            // acme is the failure — outcome=error, kind=Upstream.
            let acme = resp
                .accounts
                .iter()
                .find(|r| r.account == "acme")
                .expect("acme entry");
            match &acme.outcome {
                FanoutOutcome::Error { error } => {
                    assert_eq!(error.kind, "Upstream");
                    assert!(error.message.contains("503"), "got: {}", error.message);
                }
                FanoutOutcome::Success { .. } => panic!("expected error outcome for acme"),
            }

            // work + personal succeeded with one INBOX label each.
            for healthy in ["work", "personal"] {
                let entry = resp
                    .accounts
                    .iter()
                    .find(|r| r.account == healthy)
                    .expect("healthy entry");
                match &entry.outcome {
                    FanoutOutcome::Success { data } => {
                        assert_eq!(data.items.len(), 1);
                        assert_eq!(data.items[0].label_id, "INBOX");
                    }
                    FanoutOutcome::Error { .. } => panic!("expected success for {healthy}"),
                }
            }
        }
    }
}
