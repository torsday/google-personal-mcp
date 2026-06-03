//! `GmailService` — the seam at which cache lookup happens.
//!
//! Phases 0–2 of the cache implementation plan
//! ([docs/cache-implementation-plan.md](../../../docs/cache-implementation-plan.md)).
//! Phase 0 introduced this wrapper as a pure delegate; Phase 2 ([#150])
//! grew the load-bearing read methods into cache-aware code paths
//! without touching any of their call sites.
//!
//! Tools that perform **cacheable reads** (`get_thread`, `list_threads`,
//! `get_thread_metadata`) call the corresponding `GmailService` method. Tools
//! that issue writes, list labels, or hit any other endpoint borrow the
//! underlying client via [`GmailService::client`] and pass it through to the
//! existing free functions in [`crate::gmail::threads`],
//! [`crate::gmail::send_email`], etc. Those free functions keep their
//! `&GmailClient<T>` signatures so every existing wiremock test stays valid
//! without modification.
//!
//! The cache slot is `Option<Arc<Cache>>`: when `None`, every method
//! falls straight through to the HTTP client and no `gmcp_cache_*`
//! metrics fire. `lib::run_server` constructs `Some(_)` only when
//! `[cache] enabled = true` in `config.toml` — the default through the
//! v0.x line is `false` per the implementation plan's "Feature-flag
//! decision".
//!
//! [#150]: https://github.com/torsday/google-personal-mcp/issues/150

use std::sync::Arc;

use crate::auth::tokens::RefreshTransport;
use crate::cache::sync::HistorySync;
use crate::cache::Cache;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::threads::{
    self as threads_api, ParsedThread, ParsedThreadMinimal, RawThreadsList, ThreadMetadata,
};

/// Cache-aware wrapper around [`GmailClient`]. Tools take this instead of the
/// raw client so cacheable reads route through one place.
pub(crate) struct GmailService<T: RefreshTransport> {
    client: Arc<GmailClient<T>>,
    cache: Option<Arc<Cache>>,
    history_sync: Option<Arc<HistorySync<T>>>,
    /// When `true`, every cacheable read calls `HistorySync::sync_account`
    /// before consulting the cache. Cheap when caught up (one
    /// `history.list` call returning empty `history[]`); expensive only
    /// when a real backlog of events exists. Per [ADR-0009] §"Sync
    /// protocol" and the `[cache] sync_on_read` config flag.
    ///
    /// [ADR-0009]: ../../docs/adr/0009-caching-with-sqlite-and-history-api.md
    sync_on_read: bool,
}

impl<T: RefreshTransport> std::fmt::Debug for GmailService<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailService")
            .field("cache", &self.cache.as_ref().map(|_| "Some(Cache)"))
            .field(
                "history_sync",
                &self.history_sync.as_ref().map(|_| "Some(HistorySync)"),
            )
            .field("sync_on_read", &self.sync_on_read)
            .finish_non_exhaustive()
    }
}

// Non-`'static` surface — constructor + passthrough accessors. Tools that
// only consume `client()` / `client_arc()` keep their existing
// `T: RefreshTransport` bounds and don't take on a `'static` bound.
impl<T: RefreshTransport> GmailService<T> {
    /// Build a service wrapping `client`, optionally backed by `cache`.
    /// Use [`Self::with_history_sync`] to attach the Phase 3 sync driver
    /// after construction. `lib::run_server` is the canonical caller;
    /// tests typically pass `None`.
    pub(crate) const fn new(client: Arc<GmailClient<T>>, cache: Option<Arc<Cache>>) -> Self {
        Self {
            client,
            cache,
            history_sync: None,
            sync_on_read: false,
        }
    }

    /// Attach the Phase 3 history-sync driver. When `sync_on_read` is
    /// `true`, every cacheable read calls `HistorySync::sync_account`
    /// before consulting the cache. Returns `self` so callers can chain
    /// off `new(...)`.
    #[must_use]
    pub(crate) fn with_history_sync(
        mut self,
        history_sync: Arc<HistorySync<T>>,
        sync_on_read: bool,
    ) -> Self {
        self.history_sync = Some(history_sync);
        self.sync_on_read = sync_on_read;
        self
    }

    /// Borrow the underlying HTTP client. Tools that hit endpoints with no
    /// cacheable shape (`list_labels`, archive/trash/modify writes,
    /// `send_email`, future `cache_status` reads) call this and pass the
    /// borrow through to the existing free functions.
    pub(crate) fn client(&self) -> &GmailClient<T> {
        &self.client
    }

    /// `Arc<GmailClient<T>>` clone for tools that spawn fan-out tasks and
    /// need to move the client into a `'static` future. Mirrors the existing
    /// `Arc::clone(&self.gmail)` pattern at every dispatch site.
    pub(crate) fn client_arc(&self) -> Arc<GmailClient<T>> {
        Arc::clone(&self.client)
    }

    /// Borrow the optional `Arc<Cache>`. `None` when the operator has
    /// `[cache] enabled = false`. Used by the operator-facing
    /// `cache_status` and `cache_invalidate` tools (#83), which need the
    /// cache handle directly rather than going through the read-path
    /// methods.
    pub(crate) const fn cache(&self) -> Option<&Arc<Cache>> {
        self.cache.as_ref()
    }
}

// `'static` surface — the cache-aware reads. `tokio::spawn` inside
// `HistorySync::sync_account`'s future requires `T: 'static`, and the
// borrow checker propagates that to every method that awaits a future
// referencing `self.history_sync`. Tools that call `get_thread`,
// `list_threads`, or `get_thread_metadata` therefore carry
// `T: RefreshTransport + 'static` themselves (most already do, since
// they also need `Send + Sync` for fan-out).
impl<T: RefreshTransport + 'static> GmailService<T> {
    async fn maybe_sync(&self, account: &str) {
        if !self.sync_on_read {
            return;
        }
        let Some(sync) = self.history_sync.as_ref() else {
            return;
        };
        if let Err(e) = sync.sync_account(account).await {
            // sync_on_read is best-effort: a failed sync degrades to
            // serving the (possibly stale) cached data rather than
            // failing the user's tool call. The background loop will
            // retry on its next tick.
            tracing::warn!(
                account = account,
                error = %e,
                "sync_on_read failed; proceeding with cached data",
            );
        }
    }

    /// Fetch one thread (`threads.get(format=FULL)`, 40 quota units).
    ///
    /// With a cache: hit → return cached `ParsedThread`. Miss → fetch from
    /// Gmail, write back, return. Both branches bump
    /// `gmcp_cache_{hits,misses}_total{kind="thread"}`. With no cache:
    /// passthrough to the free function.
    pub(crate) async fn get_thread(
        &self,
        account: &str,
        thread_id: &str,
    ) -> Result<ParsedThread, Error> {
        self.maybe_sync(account).await;
        if let Some(cache) = &self.cache {
            if let Some(hit) = cache.lookup_thread(account, thread_id).await? {
                cache.metrics().record_hit(account, "thread");
                return Ok(hit);
            }
            cache.metrics().record_miss(account, "thread");
            let fresh = threads_api::get_thread(&self.client, account, thread_id).await?;
            cache.insert_thread(account, &fresh).await?;
            return Ok(fresh);
        }
        threads_api::get_thread(&self.client, account, thread_id).await
    }

    /// `messages.get` (20 quota units) — fetch one message in the requested
    /// `format`. Passthrough: single-message reads are not cached (only full
    /// threads and bodies are), so this never touches the cache.
    pub(crate) async fn get_message(
        &self,
        account: &str,
        message_id: &str,
        format: &str,
    ) -> Result<crate::gmail::threads::ParsedMessage, Error> {
        threads_api::get_message(&self.client, account, message_id, format).await
    }

    /// Fetch a message's `(body_text, body_html)` parts for `get_full_body`,
    /// cache-first. On a cache hit the stored bodies are returned without a
    /// network call; on a miss (or no cache) it falls through to
    /// `messages.get(format=FULL)` and extracts the parts. Read-only on the
    /// cache — never writes back (ADR-0009 body storage is owned by
    /// `insert_thread`). Bumps `gmcp_cache_{hits,misses}_total{kind="message_body"}`.
    pub(crate) async fn get_full_body(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<(Option<String>, Option<String>), Error> {
        self.maybe_sync(account).await;
        if let Some(cache) = &self.cache {
            if let Some(parts) = cache.lookup_message_body(account, message_id).await? {
                cache.metrics().record_hit(account, "message_body");
                return Ok(parts);
            }
            cache.metrics().record_miss(account, "message_body");
        }
        let raw = threads_api::fetch_raw_message(&self.client, account, message_id, "full").await?;
        Ok(threads_api::extract_body_parts(&raw))
    }

    /// `parse_forwarded_attachment` ([ADR-0026](../../docs/adr/0026-gmail-tool-surface-phase-2.md)):
    /// fetch a `message/rfc822` attachment and parse it — and any nested
    /// forwarded messages within it — into a [`ForwardedMessage`] tree, bounded
    /// at `max_depth` levels.
    ///
    /// Two upstream calls, neither cached: `messages.get(format=FULL)` (20 units)
    /// to locate the attachment part and validate its MIME type, then
    /// `messages.attachments.get` to fetch the raw bytes. Returns
    /// [`Error::UnsupportedMimeType`] when the attachment is not
    /// `message/rfc822`, or [`Error::NotFound`] when `attachment_id` matches no
    /// part on the message.
    pub(crate) async fn parse_forwarded_attachment(
        &self,
        account: &str,
        message_id: &str,
        attachment_id: &str,
        max_depth: u32,
    ) -> Result<crate::gmail::types::ForwardedMessage, Error> {
        let raw = threads_api::fetch_raw_message(&self.client, account, message_id, "full").await?;
        match threads_api::find_attachment_mime_type(&raw, attachment_id) {
            None => {
                return Err(Error::NotFound {
                    what: format!("attachment `{attachment_id}` on message `{message_id}`"),
                })
            }
            Some(mime) if !mime.eq_ignore_ascii_case("message/rfc822") => {
                return Err(Error::UnsupportedMimeType {
                    found: mime,
                    expected: "message/rfc822",
                })
            }
            Some(_) => {}
        }
        let attachment =
            crate::gmail::attachments::download(&self.client, account, message_id, attachment_id)
                .await?;
        crate::gmail::mime::parse_forwarded(&attachment.bytes, max_depth)
    }

    /// `threads.list` (10 quota units) — returns Gmail's raw envelope so the
    /// caller can hydrate per-thread metadata separately.
    ///
    /// With a cache: memoize per `(query, max_results, page_token)` tuple
    /// for the configured `query_ttl`. Both branches bump
    /// `gmcp_cache_{hits,misses}_total{kind="query"}`. With no cache:
    /// passthrough.
    pub(crate) async fn list_threads(
        &self,
        account: &str,
        query: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> Result<RawThreadsList, Error> {
        self.maybe_sync(account).await;
        if let Some(cache) = &self.cache {
            if let Some(hit) = cache
                .lookup_query(account, query, max_results, page_token)
                .await?
            {
                cache.metrics().record_hit(account, "query");
                return Ok(hit);
            }
            cache.metrics().record_miss(account, "query");
            // ADR-0009 §"Race-prevention" (Phase 4, #81): snapshot the
            // account watermark BEFORE issuing the upstream API call so
            // `insert_query` can detect — and discard — a write whose
            // watermark has been overtaken by the background sync during
            // the round-trip.
            let snapshot = cache.last_history_id(account).await?.unwrap_or(0);
            let fresh =
                threads_api::list_threads(&self.client, account, query, max_results, page_token)
                    .await?;
            // Discarded writes are not a tool-call failure; the caller
            // still receives the fresh response. The `gmcp_cache_write_
            // discarded_total` counter bumps inside `insert_query` so
            // operators can spot sustained nonzero rates.
            cache
                .insert_query(account, query, max_results, page_token, &fresh, snapshot)
                .await?;
            return Ok(fresh);
        }
        threads_api::list_threads(&self.client, account, query, max_results, page_token).await
    }

    /// `threads.get(format=metadata)` (40 quota units) — headers + envelope
    /// only, no bodies. Used by `search_threads` to hydrate per-result
    /// metadata.
    ///
    /// With a cache: hit → return cached `ThreadMetadata`. Miss → fetch,
    /// write back, return. Both branches bump
    /// `gmcp_cache_{hits,misses}_total{kind="thread_metadata"}`. With no
    /// cache: passthrough.
    pub(crate) async fn get_thread_metadata(
        &self,
        account: &str,
        thread_id: &str,
    ) -> Result<ThreadMetadata, Error> {
        self.maybe_sync(account).await;
        if let Some(cache) = &self.cache {
            if let Some(hit) = cache.lookup_thread_metadata(account, thread_id).await? {
                cache.metrics().record_hit(account, "thread_metadata");
                return Ok(hit);
            }
            cache.metrics().record_miss(account, "thread_metadata");
            let fresh = threads_api::get_thread_metadata(&self.client, account, thread_id).await?;
            cache.insert_thread_metadata(account, &fresh).await?;
            return Ok(fresh);
        }
        threads_api::get_thread_metadata(&self.client, account, thread_id).await
    }

    /// `threads.get(format=minimal)` (40 quota units) — IDs and label state
    /// only. Not cached (minimal format carries no content worth storing).
    pub(crate) async fn get_thread_minimal(
        &self,
        account: &str,
        thread_id: &str,
    ) -> Result<ParsedThreadMinimal, Error> {
        self.maybe_sync(account).await;
        threads_api::get_thread_minimal(&self.client, account, thread_id).await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::http::RetryPolicy;

    use super::*;

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
        let tmpdir = std::env::temp_dir().join(format!(
            "gpm-svc-test-{}-{}",
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

    /// Phase-0 invariant: `get_thread` on the service issues exactly the same
    /// HTTP request the free function would have. No header rewriting; one
    /// request lands on the upstream.
    #[tokio::test]
    async fn get_thread_delegates_to_underlying_client() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "t1",
                "messages": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let service = GmailService::new(client, None);
        let thread = service
            .get_thread("work", "t1")
            .await
            .expect("get_thread delegates");
        assert_eq!(thread.thread_id, "t1");
    }

    /// Phase-0 invariant: `list_threads` delegates verbatim.
    #[tokio::test]
    async fn list_threads_delegates_to_underlying_client() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "threads": [],
                "nextPageToken": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let service = GmailService::new(client, None);
        let raw = service
            .list_threads("work", "from:foo@bar.example", 5, None)
            .await
            .expect("list_threads delegates");
        assert!(raw.threads.is_empty());
    }

    /// Phase-0 invariant: `client()` and `client_arc()` return the same
    /// underlying client the service was constructed with.
    #[tokio::test]
    async fn client_accessor_returns_underlying_client() {
        let server = MockServer::start().await;
        let client = make_client(&server.uri());
        let arc_before = Arc::clone(&client);
        let service = GmailService::new(client, None);
        let borrowed: &GmailClient<_> = service.client();
        assert!(std::ptr::eq(borrowed, &raw const *arc_before));
        let cloned = service.client_arc();
        assert!(Arc::ptr_eq(&cloned, &arc_before));
    }

    // ── Phase-2 cache integration tests ──────────────────────────────────────

    use crate::cache::Cache;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// 5-minute TTL, matches the ADR-0009 default.
    const TEST_TTL: Duration = Duration::from_mins(5);

    async fn make_cache(accounts: &[&str]) -> Arc<Cache> {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        // Leak the tempdir into a stable path so the cache outlives this
        // helper. Each test gets a fresh tmp dir per `tempdir()`; cleanup
        // happens at process exit which is acceptable for a fast test
        // suite.
        let path = tmp.keep();
        let aliases: Vec<String> = accounts.iter().map(|a| (*a).to_owned()).collect();
        Arc::new(
            Cache::new(path, &aliases, TEST_TTL)
                .await
                .expect("cache open"),
        )
    }

    fn thread_body(thread_id: &str) -> serde_json::Value {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let body_b64 = URL_SAFE_NO_PAD.encode(b"the body");
        serde_json::json!({
            "id": thread_id,
            "messages": [{
                "id": "m1",
                "threadId": thread_id,
                "labelIds": ["INBOX"],
                "internalDate": "1717200000000",
                "payload": {
                    "mimeType": "text/plain",
                    "headers": [
                        {"name": "Subject", "value": "hello"},
                        {"name": "From", "value": "alice@example.com"}
                    ],
                    "body": {"data": body_b64, "size": 8},
                    "parts": []
                }
            }]
        })
    }

    /// Cold cache → exactly one HTTP call, miss counter bumps, hit counter
    /// stays at zero, and the returned thread matches the upstream body.
    #[tokio::test]
    async fn get_thread_cache_miss_fetches_and_records_miss() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t-miss"))
            .respond_with(ResponseTemplate::new(200).set_body_json(thread_body("t-miss")))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let cache = make_cache(&["work"]).await;
        let service = GmailService::new(client, Some(Arc::clone(&cache)));

        let thread = service.get_thread("work", "t-miss").await.expect("first");
        assert_eq!(thread.thread_id, "t-miss");
        assert_eq!(cache.metrics().misses(), 1);
        assert_eq!(cache.metrics().hits(), 0);
    }

    /// Layer-2 invariant: with the cache primed, a second `get_thread` for
    /// the same thread MUST NOT issue an HTTP request. The wiremock
    /// `.expect(1)` will panic at server drop time if a second request
    /// arrives.
    #[tokio::test]
    async fn get_thread_warm_cache_does_not_call_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t-warm"))
            .respond_with(ResponseTemplate::new(200).set_body_json(thread_body("t-warm")))
            .expect(1) // <-- exactly one upstream call across both reads
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let cache = make_cache(&["work"]).await;
        let service = GmailService::new(client, Some(Arc::clone(&cache)));

        let first = service.get_thread("work", "t-warm").await.expect("first");
        let second = service.get_thread("work", "t-warm").await.expect("second");
        assert_eq!(first.thread_id, second.thread_id);
        assert_eq!(first.messages.len(), second.messages.len());
        assert_eq!(cache.metrics().misses(), 1);
        assert_eq!(cache.metrics().hits(), 1);
    }

    /// ADR-0019 (#169) Layer-2 invariant: after a body-purge nulls the
    /// body columns, the next `get_thread` MUST issue exactly one
    /// upstream call (rehydration), and the subsequent read MUST hit
    /// the cache again (`purged_at` cleared by the FULL insert).
    /// `.expect(2)` on the wiremock proves "two upstream calls total
    /// across three reads" — initial miss + post-purge rehydrate, no
    /// more.
    #[tokio::test]
    async fn get_thread_rehydrates_after_body_purge() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t-purge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(thread_body("t-purge")))
            .expect(2)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let cache = make_cache(&["work"]).await;
        let service = GmailService::new(client, Some(Arc::clone(&cache)));

        // Read 1: cold cache → fetches from Gmail.
        let _t1 = service.get_thread("work", "t-purge").await.expect("first");
        assert_eq!(cache.metrics().misses(), 1);
        assert_eq!(cache.metrics().hits(), 0);

        // Body-purge passes a far-future age cutoff so the seeded row
        // (internal_date = 1717200000000) is unambiguously past it.
        let report = cache
            .purge_old_bodies("work", i64::MAX, 0)
            .await
            .expect("purge");
        assert_eq!(report.age_purged, 1);
        assert_eq!(cache.metrics().bodies_purged(), 1);

        // Read 2: cache returns None (purged → body_text IS NULL),
        // GmailService refetches from Gmail and writes back.
        let _t2 = service.get_thread("work", "t-purge").await.expect("second");
        assert_eq!(cache.metrics().misses(), 2);
        assert_eq!(cache.metrics().hits(), 0);

        // Read 3: post-rehydration the row has body_text populated and
        // purged_at = NULL — should hit the cache, no upstream call.
        let _t3 = service.get_thread("work", "t-purge").await.expect("third");
        assert_eq!(cache.metrics().hits(), 1, "rehydrated row hits the cache");
        assert_eq!(cache.metrics().misses(), 2);

        // `.expect(2)` on the wiremock enforces the upstream-call cap
        // when the server drops.
    }

    /// `get_full_body` cache HIT: a prior `get_thread` stored message `m1`'s
    /// body, so a `get_full_body("m1")` returns it WITHOUT a `messages.get`
    /// call. No messages.get mock is mounted — a stray call would 404 and fail.
    #[tokio::test]
    async fn get_full_body_cache_hit_uses_stored_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t-fb"))
            .respond_with(ResponseTemplate::new(200).set_body_json(thread_body("t-fb")))
            .expect(1) // only the thread fetch; get_full_body must not hit the API
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let cache = make_cache(&["work"]).await;
        let service = GmailService::new(client, Some(Arc::clone(&cache)));

        // Prime: get_thread stores message m1 (body "the body", no html).
        service.get_thread("work", "t-fb").await.expect("prime");
        let (text, html) = service
            .get_full_body("work", "m1")
            .await
            .expect("full body");
        assert_eq!(text.as_deref(), Some("the body"));
        assert_eq!(html, None);
        assert_eq!(cache.metrics().hits(), 1, "message_body hit");
    }

    /// `get_full_body` cache MISS: an uncached message id falls through to
    /// `messages.get(format=full)` exactly once and extracts the body parts.
    #[tokio::test]
    async fn get_full_body_cache_miss_falls_through_to_api() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let html_b64 = URL_SAFE_NO_PAD.encode(b"<p>hi</p>");
        let text_b64 = URL_SAFE_NO_PAD.encode(b"hi");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m-miss"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "m-miss",
                "internalDate": "1717200000000",
                "payload": {
                    "mimeType": "multipart/alternative",
                    "headers": [],
                    "parts": [
                        {"mimeType": "text/plain", "body": {"data": text_b64, "size": 2}, "parts": []},
                        {"mimeType": "text/html", "body": {"data": html_b64, "size": 9}, "parts": []}
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let cache = make_cache(&["work"]).await;
        let service = GmailService::new(client, Some(Arc::clone(&cache)));

        let (text, html) = service
            .get_full_body("work", "m-miss")
            .await
            .expect("full body");
        assert_eq!(text.as_deref(), Some("hi"));
        assert_eq!(html.as_deref(), Some("<p>hi</p>"));
        assert_eq!(cache.metrics().misses(), 1, "message_body miss");
        assert_eq!(cache.metrics().hits(), 0);
    }

    /// `get_full_body` with NO cache configured: straight passthrough to
    /// `messages.get`, no metrics.
    #[tokio::test]
    async fn get_full_body_no_cache_passthrough() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let text_b64 = URL_SAFE_NO_PAD.encode(b"plain only");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m-nc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "m-nc",
                "internalDate": "1717200000000",
                "payload": {
                    "mimeType": "text/plain",
                    "headers": [],
                    "body": {"data": text_b64, "size": 10},
                    "parts": []
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let service = GmailService::new(client, None);
        let (text, html) = service
            .get_full_body("work", "m-nc")
            .await
            .expect("full body");
        assert_eq!(text.as_deref(), Some("plain only"));
        assert_eq!(html, None);
    }

    /// `list_threads` warm-cache path: distinct `(query, max_results,
    /// page_token)` tuples cache independently. One upstream hit for one
    /// tuple even when called twice.
    #[tokio::test]
    async fn list_threads_warm_cache_does_not_call_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "threads": [
                    {"id": "tid-1", "snippet": "snip", "historyId": "100"}
                ],
                "nextPageToken": null,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let cache = make_cache(&["work"]).await;
        let service = GmailService::new(client, Some(Arc::clone(&cache)));

        let r1 = service
            .list_threads("work", "from:alice", 10, None)
            .await
            .expect("first");
        let r2 = service
            .list_threads("work", "from:alice", 10, None)
            .await
            .expect("second");
        assert_eq!(r1.threads.len(), 1);
        assert_eq!(r2.threads.len(), 1);
        assert_eq!(r2.threads[0].id, "tid-1");
        assert_eq!(r2.threads[0].snippet, "snip");
        assert_eq!(r2.threads[0].history_id, "100");
        assert_eq!(cache.metrics().misses(), 1);
        assert_eq!(cache.metrics().hits(), 1);
    }

    /// `get_thread_metadata` warm-cache path: same shape as `get_thread`,
    /// keyed on `(account, thread_id)`.
    #[tokio::test]
    async fn get_thread_metadata_warm_cache_does_not_call_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/threads/t-meta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "t-meta",
                "messages": [{
                    "id": "m1",
                    "threadId": "t-meta",
                    "labelIds": ["INBOX"],
                    "internalDate": "1717200000000",
                    "sizeEstimate": 2048,
                    "payload": {
                        "mimeType": "text/plain",
                        "headers": [
                            {"name": "Subject", "value": "meta"},
                            {"name": "From", "value": "alice@example.com"}
                        ]
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let cache = make_cache(&["work"]).await;
        let service = GmailService::new(client, Some(Arc::clone(&cache)));

        let m1 = service
            .get_thread_metadata("work", "t-meta")
            .await
            .expect("first");
        let m2 = service
            .get_thread_metadata("work", "t-meta")
            .await
            .expect("second");
        assert_eq!(m1.thread_id, m2.thread_id);
        assert_eq!(m2.messages[0].subject, "meta");
        assert_eq!(cache.metrics().misses(), 1);
        assert_eq!(cache.metrics().hits(), 1);
    }

    // ── parse_forwarded_attachment: two-call fetch + validate + parse ─────────

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    /// A `messages.get(format=full)` body whose payload carries a single
    /// attachment part with the given `mime_type` and attachment id `att1`.
    fn message_with_attachment(mid: &str, mime_type: &str) -> serde_json::Value {
        serde_json::json!({
            "id": mid,
            "labelIds": ["INBOX"],
            "internalDate": "1717200000000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [{"name": "Subject", "value": "carrier"}],
                "body": {"size": 0},
                "parts": [{
                    "mimeType": mime_type,
                    "headers": [
                        {"name": "Content-Disposition", "value": "attachment; filename=\"x\""}
                    ],
                    "body": {"attachmentId": "att1", "size": 100}
                }]
            }
        })
    }

    #[tokio::test]
    async fn parse_forwarded_attachment_happy_path() {
        let server = MockServer::start().await;

        // 1. messages.get → message exposing a message/rfc822 part (att1).
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m1$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(message_with_attachment("m1", "message/rfc822")),
            )
            .expect(1)
            .mount(&server)
            .await;

        // 2. attachments.get → raw RFC 822 bytes of the forwarded message.
        let forwarded_raw = "From: inner@example.com\r\n\
                             To: outer@example.com\r\n\
                             Subject: forwarded subject\r\n\
                             Content-Type: text/plain\r\n\
                             \r\n\
                             forwarded body";
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m1/attachments/att1$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": URL_SAFE_NO_PAD.encode(forwarded_raw.as_bytes()),
                "size": forwarded_raw.len(),
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let service = GmailService::new(client, None);
        let fwd = service
            .parse_forwarded_attachment("work", "m1", "att1", 5)
            .await
            .expect("parses forwarded attachment");
        assert_eq!(fwd.depth, 1);
        assert_eq!(
            fwd.message.headers.subject_untrusted.as_deref(),
            Some("forwarded subject")
        );
        assert_eq!(
            fwd.message.body.text_untrusted.as_deref(),
            Some("forwarded body")
        );
    }

    #[tokio::test]
    async fn parse_forwarded_attachment_rejects_non_rfc822() {
        let server = MockServer::start().await;
        // messages.get reports the attachment is a PDF — no download should happen.
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m1$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(message_with_attachment("m1", "application/pdf")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let service = GmailService::new(client, None);
        let err = service
            .parse_forwarded_attachment("work", "m1", "att1", 5)
            .await
            .expect_err("non-rfc822 must be rejected");
        assert!(
            matches!(err, Error::UnsupportedMimeType { ref found, expected }
                if found == "application/pdf" && expected == "message/rfc822"),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn parse_forwarded_attachment_unknown_id_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/messages/m1$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(message_with_attachment("m1", "message/rfc822")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let service = GmailService::new(client, None);
        let err = service
            .parse_forwarded_attachment("work", "m1", "does-not-exist", 5)
            .await
            .expect_err("unknown attachment id must be NotFound");
        assert!(matches!(err, Error::NotFound { .. }), "got: {err:?}");
    }
}
