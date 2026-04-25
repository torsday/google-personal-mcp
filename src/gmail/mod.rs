pub(crate) mod types;

use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use url::Url;

use crate::auth::StoredToken;
use types::{BatchModifyResult, FailedThread, Label, SentMessage, Thread, ThreadSummary};

const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

#[derive(Clone)]
pub(crate) struct GmailClient {
    http: Client,
    token: StoredToken,
}

impl GmailClient {
    pub(crate) fn new(token: StoredToken) -> Self {
        Self {
            http: Client::new(),
            token,
        }
    }

    fn api_url(path: &str) -> String {
        format!("{GMAIL_API}{path}")
    }

    /// GET an endpoint and parse JSON.
    ///
    /// Captures the response body BEFORE checking status so Google's actual
    /// error message (e.g. "Invalid label ID: FOO") is preserved in the error
    /// chain. See ADR-0005 for the rule and the rationale.
    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token.access_token)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("Gmail API GET failed: HTTP {status}: {body}");
        }
        serde_json::from_str(&body).context("parsing Gmail GET response")
    }

    /// POST a JSON body and parse the JSON response.
    ///
    /// Same body-before-status pattern as `get_json`. The `B: Sync` bound is
    /// required so the future is `Send` (needed by the multi-threaded Tokio
    /// runtime; awaiting across `&B` requires `B: Sync`).
    async fn post_json<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        url: String,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.token.access_token)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("Gmail API POST failed: HTTP {status}: {body}");
        }
        serde_json::from_str(&body).context("parsing Gmail POST response")
    }

    /// POST a JSON body and discard the response (only the status matters).
    /// Tolerates 200 OK and 204 No Content as success.
    async fn post_no_body<B: Serialize + Sync>(&self, url: String, body: &B) -> Result<()> {
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.token.access_token)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !matches!(status, StatusCode::OK | StatusCode::NO_CONTENT) {
            let body = resp.text().await.unwrap_or_default();
            bail!("Gmail API POST failed: HTTP {status}: {body}");
        }
        Ok(())
    }

    // ── Threads ──────────────────────────────────────────────────────────────

    pub(crate) async fn search_threads(
        &self,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<ThreadSummary>> {
        let mut url = Url::parse(&Self::api_url("/threads"))?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("maxResults", &max_results.to_string());

        let resp: Value = self.get_json(url).await?;

        let threads = resp["threads"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|t| serde_json::from_value(t.clone()).ok())
            .collect();

        Ok(threads)
    }

    pub(crate) async fn get_thread(&self, thread_id: &str) -> Result<Thread> {
        let mut url = Url::parse(&Self::api_url(&format!("/threads/{thread_id}")))?;
        url.query_pairs_mut().append_pair("format", "full");

        self.get_json(url).await
    }

    /// Archive a thread by removing the INBOX label
    pub(crate) async fn archive_thread(&self, thread_id: &str) -> Result<()> {
        self.modify_thread_labels(thread_id, &[], &["INBOX"]).await
    }

    /// Batch archive multiple threads, returns per-thread results
    pub(crate) async fn batch_archive(&self, thread_ids: &[String]) -> Result<BatchModifyResult> {
        let mut succeeded = vec![];
        let mut failed = vec![];

        for id in thread_ids {
            match self.archive_thread(id).await {
                Ok(()) => succeeded.push(id.clone()),
                Err(e) => failed.push(FailedThread {
                    id: id.clone(),
                    error: e.to_string(),
                }),
            }
        }

        Ok(BatchModifyResult { succeeded, failed })
    }

    pub(crate) async fn modify_thread_labels(
        &self,
        thread_id: &str,
        add: &[&str],
        remove: &[&str],
    ) -> Result<()> {
        let body = serde_json::json!({
            "addLabelIds": add,
            "removeLabelIds": remove,
        });
        self.post_no_body(
            Self::api_url(&format!("/threads/{thread_id}/modify")),
            &body,
        )
        .await
    }

    // ── Labels ────────────────────────────────────────────────────────────────

    pub(crate) async fn list_labels(&self) -> Result<Vec<Label>> {
        let url = Url::parse(&Self::api_url("/labels"))?;
        let resp: Value = self.get_json(url).await?;

        let labels = resp["labels"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|l| serde_json::from_value(l.clone()).ok())
            .collect();

        Ok(labels)
    }

    // ── Messages ──────────────────────────────────────────────────────────────

    /// Send an RFC 2822 message. If `thread_id` is `Some`, the message is
    /// attached to that thread (Gmail uses this to thread replies correctly,
    /// in addition to any `In-Reply-To`/`References` headers in the raw message).
    pub(crate) async fn send_message(
        &self,
        raw_rfc2822: &str,
        thread_id: Option<&str>,
    ) -> Result<SentMessage> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::URL_SAFE.encode(raw_rfc2822.as_bytes());

        let mut body = serde_json::json!({ "raw": encoded });
        if let Some(tid) = thread_id {
            body["threadId"] = Value::String(tid.to_owned());
        }

        self.post_json(Self::api_url("/messages/send"), &body).await
    }

    pub(crate) async fn trash_thread(&self, thread_id: &str) -> Result<()> {
        let url = Self::api_url(&format!("/threads/{thread_id}/trash"));
        // POST with empty JSON body; Gmail expects the body to exist but be empty.
        self.post_no_body(url, &serde_json::json!({})).await
    }
}
