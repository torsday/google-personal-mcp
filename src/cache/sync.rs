//! `HistorySync` — drives `users.history.list` against [`super::Cache`].
//!
//! Phase 3 of [ADR-0009 §"Sync protocol"](../../../docs/adr/0009-caching-with-sqlite-and-history-api.md).
//! One [`HistorySync`] instance is shared by the per-account background
//! tasks (spawned in `lib::run_serve_blocking`) and by
//! [`crate::gmail::service::GmailService`] when `sync_on_read = true`.
//! Each [`HistorySync::sync_account`] call is idempotent: a no-op when no
//! history events occurred since the last watermark, a cheap UPDATE +
//! advance when they did, and a full account reseed on a 404
//! `historyNotFound`.
//!
//! The driver owns a clone of `Arc<GmailClient<T>>` and `Arc<Cache>`; it
//! issues no other side effects.

use std::sync::Arc;
use std::time::Duration;

use crate::auth::tokens::RefreshTransport;
use crate::cache::queries::{HistoryDelta, LabelChangeDelta, MessageRefDelta};
use crate::cache::Cache;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::history::{self, is_history_not_found, HistoryRecord};
use crate::gmail::profile;

/// Per-call cap on `history.list` pages. Gmail caps at 500; we keep the
/// request size at the limit so a single tick catches up as much as
/// possible.
const HISTORY_MAX_RESULTS: u32 = 500;

/// Hard cap on history pages walked per `sync_account` call. Protects
/// against runaway loops if Gmail pages a backlog of millions of events
/// (extremely unlikely with the 7-day historyId TTL, but a guard rail
/// against a bug somewhere in the pagination state machine).
const MAX_HISTORY_PAGES_PER_CALL: u32 = 100;

/// Background-loop and on-demand driver for the history-API sync. See
/// the module docs for the contract.
pub(crate) struct HistorySync<T: RefreshTransport> {
    cache: Arc<Cache>,
    client: Arc<GmailClient<T>>,
    interval: Duration,
}

impl<T: RefreshTransport + 'static> HistorySync<T> {
    /// Build a sync driver. `interval` controls the background-loop
    /// cadence; pass `Duration::ZERO` to indicate "no background loop"
    /// (callers can still invoke [`Self::sync_account`] directly for
    /// `sync_on_read`).
    pub(crate) const fn new(
        cache: Arc<Cache>,
        client: Arc<GmailClient<T>>,
        interval: Duration,
    ) -> Self {
        Self {
            cache,
            client,
            interval,
        }
    }

    /// Background-loop cadence.
    pub(crate) const fn interval(&self) -> Duration {
        self.interval
    }

    /// Run exactly one sync pass for `account`. Cheap when caught up
    /// (one `history.list` call returning an empty `history[]`);
    /// expensive only when a real backlog of events exists or a reseed
    /// is required.
    ///
    /// First-touch (no `last_history_id` recorded): seeds the watermark
    /// from `getProfile`. **No backfill** — bodies are fetched lazily on
    /// the first on-demand read of each thread.
    ///
    /// 404 `historyNotFound`: reseeds the account (drops cached rows,
    /// re-seeds the watermark) and logs at WARN.
    pub(crate) async fn sync_account(&self, account: &str) -> Result<(), Error> {
        match self.cache.last_history_id(account).await? {
            None => self.first_touch(account).await,
            Some(start) => self.incremental(account, start).await,
        }
    }

    async fn first_touch(&self, account: &str) -> Result<(), Error> {
        let profile = profile::get_profile(&self.client, account).await?;
        let id = parse_history_id(&profile.history_id, "getProfile")?;
        self.cache.set_last_history_id(account, id).await?;
        tracing::info!(
            account = account,
            history_id = id,
            "cache first-touch: seeded last_history_id from getProfile",
        );
        Ok(())
    }

    async fn incremental(&self, account: &str, start: i64) -> Result<(), Error> {
        let start_str = start.to_string();
        let mut page_token: Option<String> = None;
        let mut pages_walked: u32 = 0;
        let mut latest_id: i64 = start;
        let mut had_mutations = false;

        loop {
            pages_walked = pages_walked.saturating_add(1);
            if pages_walked > MAX_HISTORY_PAGES_PER_CALL {
                tracing::warn!(
                    account = account,
                    pages_walked,
                    "history.list pagination cap hit; remaining events deferred to next tick",
                );
                break;
            }

            let page = match history::list_history(
                &self.client,
                account,
                &start_str,
                HISTORY_MAX_RESULTS,
                page_token.as_deref(),
            )
            .await
            {
                Ok(p) => p,
                Err(e) if is_history_not_found(&e) => {
                    return self.reseed(account).await;
                }
                Err(e) => return Err(e),
            };

            for record in page.records {
                if record_has_mutations(&record) {
                    had_mutations = true;
                }
                self.cache
                    .apply_history_record(account, encode_delta(record))
                    .await?;
            }

            if let Some(id_str) = page.history_id.as_deref() {
                latest_id = parse_history_id(id_str, "history.list")?;
            }

            match page.next_page_token {
                Some(tok) if !tok.is_empty() => page_token = Some(tok),
                _ => break,
            }
        }

        if had_mutations {
            self.cache.invalidate_all_queries(account).await?;
        }
        if latest_id != start {
            self.cache.set_last_history_id(account, latest_id).await?;
        }
        Ok(())
    }

    async fn reseed(&self, account: &str) -> Result<(), Error> {
        let profile = profile::get_profile(&self.client, account).await?;
        let new_id = parse_history_id(&profile.history_id, "getProfile (reseed)")?;
        self.cache.reseed_account(account, new_id).await?;
        tracing::warn!(
            account = account,
            new_history_id = new_id,
            "cache reseed: historyId not found upstream — \
             all cached messages, threads, message_labels, and query_cache rows dropped",
        );
        Ok(())
    }
}

impl<T: RefreshTransport + 'static> HistorySync<T> {
    /// Spawn a `tokio::task` that calls [`Self::sync_account`] every
    /// [`Self::interval`] until the returned [`SyncHandle`] drops.
    /// Returns `None` when the interval is zero (sync disabled).
    pub(crate) fn spawn_for(self: &Arc<Self>, account: String) -> Option<SyncHandle> {
        if self.interval.is_zero() {
            return None;
        }
        let driver = Arc::clone(self);
        let interval = self.interval;
        let acct_for_log = account.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately — run a sync right away so a
            // newly-started daemon doesn't wait `interval` before the
            // first catch-up.
            loop {
                ticker.tick().await;
                if let Err(e) = driver.sync_account(&account).await {
                    tracing::warn!(
                        account = account,
                        error = %e,
                        "cache sync tick failed; will retry on next interval",
                    );
                }
            }
        });
        Some(SyncHandle {
            task: Some(handle),
            account: acct_for_log,
        })
    }
}

/// Owned handle to one per-account background sync task. Aborts the
/// task on drop. Same pattern as
/// [`crate::transport::session::SweeperHandle`].
#[must_use = "the sync task aborts when this handle is dropped"]
pub(crate) struct SyncHandle {
    task: Option<tokio::task::JoinHandle<()>>,
    account: String,
}

impl SyncHandle {
    /// Stop the background sync and wait for the abort to land.
    #[cfg(test)]
    pub(crate) async fn stop(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = t.await;
        }
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            tracing::debug!(account = %self.account, "aborting cache sync task");
            t.abort();
        }
    }
}

const fn record_has_mutations(r: &HistoryRecord) -> bool {
    !r.messages_added.is_empty()
        || !r.messages_deleted.is_empty()
        || !r.labels_added.is_empty()
        || !r.labels_removed.is_empty()
}

fn encode_delta(r: HistoryRecord) -> HistoryDelta {
    HistoryDelta {
        messages_added: r
            .messages_added
            .into_iter()
            .map(|m| MessageRefDelta {
                message_id: m.message_id,
                thread_id: m.thread_id,
            })
            .collect(),
        messages_deleted: r
            .messages_deleted
            .into_iter()
            .map(|m| MessageRefDelta {
                message_id: m.message_id,
                thread_id: m.thread_id,
            })
            .collect(),
        labels_added: r
            .labels_added
            .into_iter()
            .map(|c| LabelChangeDelta {
                message_id: c.message.message_id,
                thread_id: c.message.thread_id,
                label_ids: c.label_ids,
            })
            .collect(),
        labels_removed: r
            .labels_removed
            .into_iter()
            .map(|c| LabelChangeDelta {
                message_id: c.message.message_id,
                thread_id: c.message.thread_id,
                label_ids: c.label_ids,
            })
            .collect(),
    }
}

fn parse_history_id(s: &str, context: &'static str) -> Result<i64, Error> {
    s.parse::<i64>().map_err(|e| Error::Internal {
        context: format!("sync: parse {context} historyId"),
        source: anyhow::Error::new(e),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::tokens::{TokenManager, TokenState};
    use crate::cache::Cache;
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

    fn tmp() -> TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        d
    }

    async fn open_cache(dir: &TempDir) -> Arc<Cache> {
        Arc::new(
            Cache::new(
                dir.path().to_owned(),
                &["work".to_owned()],
                Duration::from_mins(5),
            )
            .await
            .expect("open"),
        )
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
        let tdir = std::env::temp_dir().join(format!(
            "gpm-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&tdir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            tdir,
        ));
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        )
    }

    /// First-touch path: `last_history_id` is NULL → call `getProfile`
    /// once → write the watermark. No `history.list` call expected.
    #[tokio::test]
    async fn first_touch_seeds_watermark_via_get_profile() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/profile$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "alice@example.com",
                "historyId": "777"
            })))
            .expect(1)
            .mount(&server)
            .await;
        // No history.list mock — first-touch must not call it.

        let dir = tmp();
        let cache = open_cache(&dir).await;
        let client = make_client(&server.uri());
        let sync = HistorySync::new(Arc::clone(&cache), client, Duration::from_mins(1));

        sync.sync_account("work").await.expect("first touch");
        assert_eq!(
            cache.last_history_id("work").await.expect("read"),
            Some(777)
        );
    }

    /// Incremental path: empty `history[]` advances the watermark and
    /// does not mutate the cache.
    #[tokio::test]
    async fn incremental_empty_history_advances_watermark() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/history"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "historyId": "2000"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tmp();
        let cache = open_cache(&dir).await;
        cache.set_last_history_id("work", 1000).await.expect("seed");
        let client = make_client(&server.uri());
        let sync = HistorySync::new(Arc::clone(&cache), client, Duration::from_mins(1));

        sync.sync_account("work").await.expect("incremental");
        assert_eq!(
            cache.last_history_id("work").await.expect("read"),
            Some(2000)
        );
    }

    /// Incremental path with mutations: applies labelsAdded and
    /// advances the watermark. Verifies `query_cache` invalidation by
    /// seeding a query row pre-sync and checking it's gone after.
    #[tokio::test]
    async fn incremental_with_mutations_applies_and_invalidates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/history"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "history": [{
                    "id": "1001",
                    "labelsAdded": [{
                        "message": {"id": "m1", "threadId": "t1"},
                        "labelIds": ["STARRED"]
                    }]
                }],
                "historyId": "1001"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tmp();
        let cache = open_cache(&dir).await;
        cache.set_last_history_id("work", 1000).await.expect("seed");
        // Seed a query_cache row to verify invalidation.
        cache
            .insert_query(
                "work",
                "before",
                10,
                None,
                &crate::gmail::threads::RawThreadsList {
                    threads: vec![],
                    next_page_token: None,
                },
                1000,
            )
            .await
            .expect("seed query");

        let client = make_client(&server.uri());
        let sync = HistorySync::new(Arc::clone(&cache), client, Duration::from_mins(1));
        sync.sync_account("work").await.expect("incremental");

        assert_eq!(
            cache.last_history_id("work").await.expect("read"),
            Some(1001),
            "watermark advances",
        );
        assert!(
            cache
                .lookup_query("work", "before", 10, None)
                .await
                .expect("lookup")
                .is_none(),
            "query_cache invalidated after history mutation",
        );
    }

    /// 404 historyNotFound path: drops all cached rows, calls getProfile
    /// for the new watermark, logs WARN.
    #[tokio::test]
    async fn history_404_triggers_reseed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/history"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"code": 404, "message": "historyNotFound", "status": "NOT_FOUND"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/profile$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "alice@example.com",
                "historyId": "5555"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tmp();
        let cache = open_cache(&dir).await;
        cache
            .set_last_history_id("work", 999_999)
            .await
            .expect("seed stale watermark");
        // Seed some data — reseed must drop it.
        cache
            .insert_query(
                "work",
                "x",
                5,
                None,
                &crate::gmail::threads::RawThreadsList {
                    threads: vec![],
                    next_page_token: None,
                },
                999_999,
            )
            .await
            .expect("seed query");

        let client = make_client(&server.uri());
        let sync = HistorySync::new(Arc::clone(&cache), client, Duration::from_mins(1));
        sync.sync_account("work").await.expect("reseed");

        assert_eq!(
            cache.last_history_id("work").await.expect("read"),
            Some(5555),
            "watermark re-seeded from getProfile",
        );
        assert!(
            cache
                .lookup_query("work", "x", 5, None)
                .await
                .expect("lookup")
                .is_none(),
            "reseed dropped query_cache rows",
        );
    }

    /// Background loop tick path: `spawn_for` returns Some when interval > 0;
    /// the task ticks at least once and increments the watermark.
    #[tokio::test]
    async fn spawn_for_runs_a_tick() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/profile$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "alice@example.com",
                "historyId": "42"
            })))
            // The background task may tick more than once before the test
            // checks. Allow any number; we only need ≥1.
            .mount(&server)
            .await;
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let client = make_client(&server.uri());
        // Short interval so the first tick fires quickly.
        let sync = Arc::new(HistorySync::new(
            Arc::clone(&cache),
            client,
            Duration::from_millis(50),
        ));
        let handle = sync.spawn_for("work".into()).expect("interval > 0 → Some");

        // Poll up to 2s for the watermark to land.
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(50);
        while waited < Duration::from_secs(2) {
            if let Some(id) = cache.last_history_id("work").await.expect("read") {
                assert_eq!(id, 42);
                handle.stop().await;
                return;
            }
            tokio::time::sleep(step).await;
            waited += step;
        }
        handle.stop().await;
        panic!("watermark never written by background task");
    }

    /// Interval = 0 disables the background loop.
    #[tokio::test]
    async fn spawn_for_returns_none_when_interval_is_zero() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let client = make_client("http://localhost:1");
        let sync = Arc::new(HistorySync::new(cache, client, Duration::ZERO));
        assert!(sync.spawn_for("work".into()).is_none());
    }
}
