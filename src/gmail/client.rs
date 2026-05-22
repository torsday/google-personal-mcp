//! `GmailClient` — the HTTP wrapper every Gmail-touching tool calls into.
//!
//! Generic over the [`crate::auth::tokens::RefreshTransport`] so tests can
//! inject a token manager that doesn't need a live OAuth endpoint. In
//! production [`ReqwestRefreshTransport`] is used.
//!
//! The 401 fallback is a single re-try: on the first 401 we call
//! [`TokenManager::force_refresh`] and replay the request once. A second 401
//! propagates as [`Error::AuthRequired`] per ADR-0004 §"401 fallback".

use std::sync::Arc;

use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::auth::tokens::{RefreshTransport, ReqwestRefreshTransport, TokenManager};
use crate::error::Error;
use crate::http::{execute_with_retry, RetryPolicy};
use crate::project_quota::{project_id_from_client_id, ProjectQuotaRegistry};
use crate::rate_limit::KeyedRateLimiter;

pub(crate) struct GmailClient<T: RefreshTransport = ReqwestRefreshTransport> {
    base_url: String,
    tokens: Arc<TokenManager<T>>,
    http: reqwest::Client,
    retry: RetryPolicy,
    rate_limiter: Arc<KeyedRateLimiter>,
    /// Per-GCP-project daily-quota tracker (issue #30). `None` skips the
    /// project-level check entirely — useful for tests that don't care.
    project_quota: Option<Arc<ProjectQuotaRegistry>>,
}

impl<T: RefreshTransport> GmailClient<T> {
    /// `base_url` is the Gmail API root with no trailing slash, e.g.
    /// `https://gmail.googleapis.com/gmail/v1` in production or a wiremock
    /// host (`http://127.0.0.1:PORT`) in tests.
    pub(crate) fn new(
        base_url: impl Into<String>,
        tokens: Arc<TokenManager<T>>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            tokens,
            http,
            retry: RetryPolicy::default(),
            rate_limiter: Arc::new(KeyedRateLimiter::default()),
            project_quota: None,
        }
    }

    /// Inject a shared rate limiter — useful when multiple clients should
    /// share the same per-account budget (e.g. Gmail + future Calendar
    /// clients counted against the same Google project quota).
    #[cfg(test)]
    pub(crate) fn with_rate_limiter(mut self, rate_limiter: Arc<KeyedRateLimiter>) -> Self {
        self.rate_limiter = rate_limiter;
        self
    }

    /// Inject a per-GCP-project daily-quota registry (issue #30). Without
    /// this the `GmailClient` enforces only the per-account per-minute budget.
    pub(crate) fn with_project_quota(mut self, registry: Arc<ProjectQuotaRegistry>) -> Self {
        self.project_quota = Some(registry);
        self
    }

    /// Run both quota checks in order: per-project first, then per-account.
    ///
    /// Ordering matters: both checks consume budget on success, so the second
    /// check must never fail after the first has already charged. By checking
    /// the project quota (shared, more likely to be exhausted) first, a denial
    /// returns before the per-account bucket is touched. The per-account bucket
    /// then only deducts when the project quota had room.
    ///
    /// AND-gated — whichever exhausts first returns `Error::RateLimited`.
    fn quota_check(&self, account: &str, cost: u32) -> Result<(), Error> {
        // Check per-project first so a denial does not charge the per-account bucket.
        if let Some(registry) = self.project_quota.as_ref() {
            if let Some(client_id) = self.tokens.client_id(account) {
                if let Some(project_id) = project_id_from_client_id(client_id) {
                    registry.try_acquire(&project_id, account, cost)?;
                }
            }
        }
        self.rate_limiter.try_acquire(account, cost)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Authenticated GET. `cost` is the per-call quota cost from
    /// [`crate::gmail::quota::GmailMethod::cost`]; the rate limiter is
    /// consulted before any network I/O and returns `Error::RateLimited`
    /// immediately on exhaustion.
    #[tracing::instrument(
        skip_all,
        err(Display),
        fields(
            google.service = "gmail",
            google.method = "GET",
            google.endpoint = %endpoint_of(path),
            google.account = %account,
            google.cost = cost,
        ),
    )]
    pub(crate) async fn authed_get<R: DeserializeOwned>(
        &self,
        account: &str,
        path: &str,
        cost: u32,
    ) -> Result<R, Error> {
        self.quota_check(account, cost)?;
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .send_with_401_fallback(account, |token| self.http.get(&url).bearer_auth(token))
            .await?;
        parse_json(resp).await
    }

    /// Authenticated POST. See [`Self::authed_get`] for the `cost` contract.
    #[tracing::instrument(
        skip_all,
        err(Display),
        fields(
            google.service = "gmail",
            google.method = "POST",
            google.endpoint = %endpoint_of(path),
            google.account = %account,
            google.cost = cost,
        ),
    )]
    pub(crate) async fn authed_post<B: Serialize + Sync, R: DeserializeOwned>(
        &self,
        account: &str,
        path: &str,
        cost: u32,
        body: &B,
    ) -> Result<R, Error> {
        self.quota_check(account, cost)?;
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .send_with_401_fallback(account, |token| {
                self.http.post(&url).bearer_auth(token).json(body)
            })
            .await?;
        parse_json(resp).await
    }

    /// Send a request, retrying once on a 401 after a forced token refresh.
    /// `build_request` is called fresh for each attempt — `RequestBuilder` is
    /// single-use, and the 401 retry uses a different (refreshed) token.
    async fn send_with_401_fallback<F>(
        &self,
        account: &str,
        build_request: F,
    ) -> Result<reqwest::Response, Error>
    where
        F: Fn(&str) -> reqwest::RequestBuilder + Send + Sync,
    {
        let token = self.tokens.access_token(account).await?;
        let resp = execute_with_retry("gmail", &self.retry, || async {
            build_request(&token).send().await
        })
        .await;

        match resp {
            Ok(r) => Ok(r),
            // Already-Upstream-shaped errors with status 401 are the trigger
            // for the force-refresh fallback. Everything else flows through
            // unchanged.
            Err(Error::Upstream { status: 401, .. }) => {
                let token = self.tokens.force_refresh(account).await?;
                let resp = execute_with_retry("gmail", &self.retry, || async {
                    build_request(&token).send().await
                })
                .await;
                match resp {
                    Ok(r) => Ok(r),
                    Err(Error::Upstream { status: 401, .. }) => Err(Error::AuthRequired {
                        account: account.to_owned(),
                        reason: "401 after forced refresh — token may be revoked".into(),
                    }),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// Strip the query string from a path so spans group by endpoint, not by
/// per-call query. `/users/me/threads?q=foo` → `/users/me/threads`. Used
/// only for the `google.endpoint` tracing field.
fn endpoint_of(path: &str) -> &str {
    path.find('?').map_or(path, |i| &path[..i])
}

async fn parse_json<R: DeserializeOwned>(resp: reqwest::Response) -> Result<R, Error> {
    let status = resp.status();
    let text = resp.text().await.map_err(Error::Network)?;
    if !status.is_success() {
        return Err(Error::upstream("gmail", status.as_u16(), text));
    }
    serde_json::from_str(&text).map_err(|e| Error::Parse {
        context: "gmail response body".to_owned(),
        source: e,
    })
}

// ── 401 detection helper ─────────────────────────────────────────────────────

/// True iff `status` is 401 Unauthorized. Surfaced as a function so the
/// retry-helper code can switch on it cleanly.
const fn is_unauthorized(status: StatusCode) -> bool {
    status.as_u16() == 401
}

// Quiet the lint about the helper being unused in some build configs — it
// documents intent and is used by tests.
#[cfg(test)]
#[allow(dead_code)]
const _: fn(StatusCode) -> bool = is_unauthorized;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use chrono::{Duration as ChronoDuration, Utc};
    use serde::Deserialize;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use crate::auth::tokens::TokenState;

    fn fresh_state(access: &str) -> TokenState {
        TokenState {
            access_token: access.into(),
            refresh_token: "REFRESH".into(),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            scopes: vec!["https://www.googleapis.com/auth/gmail.modify".into()],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    /// Transport that scripts successive refresh responses. For 401-fallback
    /// tests we queue exactly one success — the post-refresh `access_token` is
    /// "REFRESHED".
    struct MockRefreshTransport {
        responses: Mutex<Vec<Result<(u16, String), Error>>>,
        calls: AtomicU32,
    }

    impl MockRefreshTransport {
        fn new(responses: Vec<Result<(u16, String), Error>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: AtomicU32::new(0),
            }
        }
    }

    impl RefreshTransport for MockRefreshTransport {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.responses.lock().unwrap();
            assert!(!queue.is_empty(), "MockRefreshTransport exhausted");
            queue.remove(0)
        }
    }

    fn make_client(
        base_url: &str,
        initial_token: &str,
        refresh_response: Option<String>,
    ) -> Arc<GmailClient<MockRefreshTransport>> {
        let responses = refresh_response
            .map(|body| vec![Ok((200, body))])
            .unwrap_or_default();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), fresh_state(initial_token))]),
            MockRefreshTransport::new(responses),
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-gc-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("gpm-gc-{}", std::process::id())),
        )
        .unwrap();
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        )
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct EchoBody {
        ok: bool,
    }

    // ── Success path ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn success_path_passes_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .and(header("authorization", "Bearer ACCESS-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "ACCESS-1", None);
        let body: EchoBody = client.authed_get("work", "/v1/me", 1).await.expect("ok");
        assert_eq!(body, EchoBody { ok: true });
    }

    // Responder that returns `first_status` once, then `then_status` with
    // `then_body` for all subsequent requests.
    struct ResponderOnceThen {
        calls: AtomicU32,
        first_status: u16,
        first_body: &'static str,
        first_header: Option<(&'static str, &'static str)>,
        then_status: u16,
        then_body: &'static str,
    }

    impl Respond for ResponderOnceThen {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                let mut t =
                    ResponseTemplate::new(self.first_status).set_body_string(self.first_body);
                if let Some((k, v)) = self.first_header {
                    t = t.insert_header(k, v);
                }
                t
            } else {
                ResponseTemplate::new(self.then_status).set_body_string(self.then_body)
            }
        }
    }

    // ── 401 → force_refresh → retry succeeds (AC) ────────────────────────────

    #[tokio::test]
    async fn unauthorized_triggers_refresh_then_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponderOnceThen {
                calls: AtomicU32::new(0),
                first_status: 401,
                first_body: "expired",
                first_header: None,
                then_status: 200,
                then_body: r#"{"ok":true}"#,
            })
            .mount(&server)
            .await;

        // Refresh transport returns one fresh token.
        let refresh = r#"{"access_token":"REFRESHED","expires_in":3600}"#.to_owned();
        let client = make_client(&server.uri(), "STALE", Some(refresh));

        let body: EchoBody = client
            .authed_get("work", "/v1/me", 1)
            .await
            .expect("should recover from 401");
        assert_eq!(body, EchoBody { ok: true });
    }

    // ── 401 twice → AuthRequired (no infinite loop) ──────────────────────────

    #[tokio::test]
    async fn two_consecutive_401s_become_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(401).set_body_string("revoked"))
            .mount(&server)
            .await;

        let refresh = r#"{"access_token":"REFRESHED","expires_in":3600}"#.to_owned();
        let client = make_client(&server.uri(), "STALE", Some(refresh));

        let err = client
            .authed_get::<EchoBody>("work", "/v1/me", 1)
            .await
            .expect_err("must fail");
        assert!(
            matches!(err, Error::AuthRequired { ref reason, .. } if reason.contains("after forced refresh")),
            "got: {err:?}"
        );
    }

    // ── 429 with Retry-After: retried after the indicated delay ──────────────

    #[tokio::test]
    async fn retry_after_honors_seconds_value() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponderOnceThen {
                calls: AtomicU32::new(0),
                first_status: 429,
                first_body: "slow down",
                first_header: Some(("Retry-After", "0")),
                then_status: 200,
                then_body: r#"{"ok":true}"#,
            })
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "ACCESS", None);
        let started = std::time::Instant::now();
        let body: EchoBody = client.authed_get("work", "/v1/me", 1).await.expect("ok");
        assert_eq!(body, EchoBody { ok: true });
        // Retry-After: 0 → effectively no wait.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "Retry-After: 0 should not stall"
        );
    }

    // ── 5xx exhausts retries and surfaces Upstream ───────────────────────────

    #[tokio::test]
    async fn five_xx_exhausts_retries_then_returns_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "ACCESS", None);
        let err = client
            .authed_get::<EchoBody>("work", "/v1/me", 1)
            .await
            .expect_err("must fail");
        match err {
            Error::Upstream {
                status: 500,
                ref message,
                ..
            } => assert!(message.contains("boom")),
            other => panic!("expected Upstream(500), got {other:?}"),
        }
    }

    // ── 4xx non-retryable: short-circuits immediately ────────────────────────

    #[tokio::test]
    async fn non_retryable_4xx_does_not_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "ACCESS", None);
        let err = client
            .authed_get::<EchoBody>("work", "/v1/me", 1)
            .await
            .expect_err("must fail");
        assert!(
            matches!(err, Error::Upstream { status: 404, .. }),
            "got: {err:?}"
        );
    }

    // ── is_unauthorized helper ──────────────────────────────────────────────

    #[test]
    fn unauthorized_predicate() {
        assert!(is_unauthorized(StatusCode::UNAUTHORIZED));
        assert!(!is_unauthorized(StatusCode::OK));
        assert!(!is_unauthorized(StatusCode::FORBIDDEN));
    }

    // ── Rate limiter wired in (AC) ──────────────────────────────────────────

    /// Builds a client with an injected limiter — separate helper because
    /// the limiter has to be wired through `with_rate_limiter` before the
    /// client is wrapped in an `Arc`.
    fn client_with_limiter(
        base_url: &str,
        initial_token: &str,
        limiter: Arc<KeyedRateLimiter>,
    ) -> Arc<GmailClient<MockRefreshTransport>> {
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), fresh_state(initial_token))]),
            MockRefreshTransport::new(vec![]),
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-gc-rl-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("gpm-gc-rl-{}", std::process::id())),
        )
        .expect("mkdir");
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests())
                .with_rate_limiter(limiter),
        )
    }

    #[tokio::test]
    async fn rate_limit_short_circuits_before_network() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .expect(0)
            .mount(&server)
            .await;

        let client = client_with_limiter(
            &server.uri(),
            "ACCESS",
            Arc::new(KeyedRateLimiter::new(0, 1)),
        );
        let err = client
            .authed_get::<EchoBody>("work", "/v1/me", 1)
            .await
            .expect_err("must rate-limit");
        assert!(
            matches!(err, Error::RateLimited { ref account, .. } if account == "work"),
            "got: {err:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_some_get_rate_limited() {
        // Tight budget: 5 units, cost 1 → exactly 5 of 20 succeed.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;
        let client = client_with_limiter(
            &server.uri(),
            "ACCESS",
            Arc::new(KeyedRateLimiter::new(5, 1)),
        );

        let mut handles = Vec::new();
        for _ in 0..20 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                c.authed_get::<EchoBody>("work", "/v1/me", 1).await
            }));
        }
        let mut ok = 0;
        let mut rate_limited = 0;
        for h in handles {
            match h.await.expect("join") {
                Ok(_) => ok += 1,
                Err(Error::RateLimited { .. }) => rate_limited += 1,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert_eq!(ok, 5, "exactly 5 should succeed (budget 5, cost 1 each)");
        assert_eq!(rate_limited, 15, "the rest should be rate-limited");
    }

    // ── Project-quota integration (issue #30) ────────────────────────────────

    /// Build a client with TWO accounts under the same OAuth project number
    /// (per `project_id_from_client_id` shape) plus a tight project-daily budget.
    fn client_two_accounts_one_project(
        base_url: &str,
        project_daily_budget: u64,
    ) -> Arc<GmailClient<MockRefreshTransport>> {
        fn state(access: &str) -> TokenState {
            TokenState {
                access_token: access.into(),
                refresh_token: "R".into(),
                expires_at: Utc::now() + ChronoDuration::seconds(3600),
                scopes: vec!["scope".into()],
                client_id: "111-aaa.apps.googleusercontent.com".into(), // project 111
                client_secret: "csec".into(),
                failed_until: None,
                consecutive_failures: 0,
                last_refresh_at: None,
            }
        }
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([
                ("work".to_owned(), state("ACCESS-W")),
                ("personal".to_owned(), state("ACCESS-P")),
            ]),
            MockRefreshTransport::new(vec![]),
            "https://example/token",
            std::env::temp_dir().join(format!("gpm-proj-{}", std::process::id())),
        ));
        std::fs::create_dir_all(
            std::env::temp_dir().join(format!("gpm-proj-{}", std::process::id())),
        )
        .expect("mkdir");
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests())
                .with_rate_limiter(Arc::new(KeyedRateLimiter::new(1_000_000, 1_000_000)))
                .with_project_quota(Arc::new(ProjectQuotaRegistry::new(project_daily_budget))),
        )
    }

    #[tokio::test]
    async fn two_accounts_under_one_project_share_daily_budget() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;
        // Project daily budget = 3 units total; each call costs 1.
        let client = client_two_accounts_one_project(&server.uri(), 3);

        // Two calls from "work" + one call from "personal" → 3 units → all succeed.
        for who in ["work", "work", "personal"] {
            client
                .authed_get::<EchoBody>(who, "/v1/me", 1)
                .await
                .expect("ok within shared budget");
        }
        // Fourth call exhausts the project bucket regardless of which account.
        let err = client
            .authed_get::<EchoBody>("personal", "/v1/me", 1)
            .await
            .expect_err("must be rate-limited at project level");
        match err {
            Error::RateLimited {
                account,
                retry_after,
            } => {
                assert_eq!(account, "personal");
                // retry_after = seconds to next UTC midnight; positive,
                // ≤ 86_400 (one day max).
                assert!(retry_after.as_secs() > 0);
                assert!(retry_after.as_secs() <= 86_400);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// Regression test: project-quota denial must not drain the per-account bucket.
    ///
    /// Setup: per-account cap = 10 units; project cap = 3 units.
    /// After 3 successful calls the project quota is exhausted. Five further
    /// calls are denied at the project level. The per-account limiter must
    /// still have ~7 units (10 – 3 successful), not ~2 (10 – 3 – 5 leaked).
    ///
    /// We verify by asking the same limiter Arc to serve 7 units directly
    /// after the test — if the old bug were present that call would fail.
    #[tokio::test]
    async fn project_quota_denial_does_not_drain_per_account_bucket() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;

        // Per-account cap=10 (small enough to detect the leak), project cap=3.
        let limiter = Arc::new(KeyedRateLimiter::new(10, 10));
        let client = {
            // Must use a Google-style client_id so project_id_from_client_id succeeds.
            let mut token = fresh_state("ACCESS");
            token.client_id = "999-test.apps.googleusercontent.com".into();
            let tokens = Arc::new(TokenManager::new(
                HashMap::from([("work".to_owned(), token)]),
                MockRefreshTransport::new(vec![]),
                "https://example/token",
                std::env::temp_dir().join(format!("gpm-qco-{}", std::process::id())),
            ));
            std::fs::create_dir_all(
                std::env::temp_dir().join(format!("gpm-qco-{}", std::process::id())),
            )
            .expect("mkdir");
            Arc::new(
                GmailClient::new(server.uri(), tokens, reqwest::Client::new())
                    .with_retry(RetryPolicy::for_tests())
                    .with_rate_limiter(Arc::clone(&limiter))
                    .with_project_quota(Arc::new(ProjectQuotaRegistry::new(3))),
            )
        };

        // Exhaust project budget (3 calls, cost 1 each).
        for _ in 0..3 {
            client
                .authed_get::<EchoBody>("work", "/v1/me", 1)
                .await
                .expect("within budget");
        }

        // 5 calls denied by project quota (cost 1 each).
        for _ in 0..5 {
            let _ = client.authed_get::<EchoBody>("work", "/v1/me", 1).await;
        }

        // Per-account limiter consumed 3 units (successful calls) plus 0 from
        // the 5 denied calls. It must still have ≥ 7 units available.
        // A try_acquire(7) proves at least 7 remain.
        limiter
            .try_acquire("work", 7)
            .expect("per-account bucket must retain budget after project-quota denials");
    }
}
