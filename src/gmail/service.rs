//! `GmailService` — the seam at which cache lookup happens.
//!
//! Phase 0 of the cache implementation plan
//! ([docs/cache-implementation-plan.md](../../../docs/cache-implementation-plan.md)).
//! Today this is a pure delegate around [`GmailClient`]; in Phase 2 the
//! load-bearing read methods grow cache-hit / cache-miss logic without
//! touching any of their call sites.
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
//! The cache slot is intentionally `Option<Arc<Cache>>`: in Phase 1 ([#79])
//! the `Cache` type exists but isn't yet wired into `lib::run_server`. Phase 2
//! ([#150]) flips the wiring and gives this struct a `Some(_)` cache to
//! consult.
//!
//! [#79]: https://github.com/torsday/google-personal-mcp/issues/79
//! [#150]: https://github.com/torsday/google-personal-mcp/issues/150

use std::sync::Arc;

use crate::auth::tokens::RefreshTransport;
use crate::cache::Cache;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::threads::{self as threads_api, ParsedThread, RawThreadsList, ThreadMetadata};

/// Cache-aware wrapper around [`GmailClient`]. Tools take this instead of the
/// raw client so cacheable reads route through one place.
pub(crate) struct GmailService<T: RefreshTransport> {
    client: Arc<GmailClient<T>>,
    #[allow(dead_code)] // Wired in Phase 2 (#150); Phase 0 stores `None`.
    cache: Option<Arc<Cache>>,
}

impl<T: RefreshTransport> std::fmt::Debug for GmailService<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailService")
            .field("cache", &self.cache.as_ref().map(|_| "Some(Cache)"))
            .finish_non_exhaustive()
    }
}

impl<T: RefreshTransport> GmailService<T> {
    /// Build a service wrapping `client`, optionally backed by `cache`.
    /// During Phase 0 (#149) `cache` is always `None`; Phase 2 (#150) wires
    /// it.
    pub(crate) const fn new(client: Arc<GmailClient<T>>, cache: Option<Arc<Cache>>) -> Self {
        Self { client, cache }
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

    /// Fetch one thread (`threads.get(format=FULL)`, 40 quota units).
    ///
    /// Phase 0: pure delegate. Phase 2 (#150) adds cache lookup → fall
    /// through to API on miss → write back the result.
    pub(crate) async fn get_thread(
        &self,
        account: &str,
        thread_id: &str,
    ) -> Result<ParsedThread, Error> {
        threads_api::get_thread(&self.client, account, thread_id).await
    }

    /// `threads.list` (10 quota units) — returns Gmail's raw envelope so the
    /// caller can hydrate per-thread metadata separately.
    ///
    /// Phase 0: pure delegate. Phase 2 (#150) will memoize via `query_cache`
    /// keyed on `sha256(query || max_results)`.
    pub(crate) async fn list_threads(
        &self,
        account: &str,
        query: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> Result<RawThreadsList, Error> {
        threads_api::list_threads(&self.client, account, query, max_results, page_token).await
    }

    /// `threads.get(format=metadata)` (40 quota units) — headers + envelope
    /// only, no bodies. Used by `search_threads` to hydrate per-result
    /// metadata.
    ///
    /// Phase 0: pure delegate. Phase 2 (#150) adds per-thread metadata
    /// caching alongside the body cache.
    pub(crate) async fn get_thread_metadata(
        &self,
        account: &str,
        thread_id: &str,
    ) -> Result<ThreadMetadata, Error> {
        threads_api::get_thread_metadata(&self.client, account, thread_id).await
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
}
