//! `CalendarClient` — the HTTP wrapper every Calendar-touching tool will call
//! into, per [ADR-0023](../../docs/adr/0023-calendar-service-surface.md).
//!
//! Deliberately mirrors [`crate::gmail::client::GmailClient`] so a contributor
//! can grep "how does Gmail do X" and apply it here: same generic over
//! [`crate::auth::tokens::RefreshTransport`], same `new` shape, same
//! `authed_get` / `authed_post` surface, and the same single-retry 401
//! fallback (force-refresh once, replay once, then `Error::AuthRequired`) per
//! [ADR-0004](../../docs/adr/0004-oauth-token-refresh.md) §"401 fallback".
//!
//! Scaffold module: no Calendar tools call these methods yet — they land in
//! follow-up tickets (#200+). The module-level `allow(dead_code)` covers the
//! surface that those tickets will consume; remove it as tools wire in.
#![allow(dead_code)]

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::auth::tokens::{RefreshTransport, ReqwestRefreshTransport, TokenManager};
use crate::error::Error;
use crate::http::{execute_with_retry, RetryPolicy};
use crate::project_quota::{project_id_from_client_id, ProjectQuotaRegistry};
use crate::rate_limit::KeyedRateLimiter;

/// Production Calendar API root (no trailing slash).
pub(crate) const CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Per-call quota cost for a Calendar API request. The Calendar API meters in
/// flat "queries" (1 per call) against a per-minute/day budget, unlike Gmail's
/// weighted unit model ([ADR-0023](../../docs/adr/0023-calendar-service-surface.md)),
/// so every endpoint charges the shared rate limiter a single unit.
pub(crate) const QUERY_COST: u32 = 1;

pub(crate) struct CalendarClient<T: RefreshTransport = ReqwestRefreshTransport> {
    base_url: String,
    tokens: Arc<TokenManager<T>>,
    http: reqwest::Client,
    retry: RetryPolicy,
    rate_limiter: Arc<KeyedRateLimiter>,
    /// Per-GCP-project daily-quota tracker (issue #30). `None` skips the
    /// project-level check entirely. Shared with the Gmail client when both
    /// count against the same Google project.
    project_quota: Option<Arc<ProjectQuotaRegistry>>,
}

impl<T: RefreshTransport> CalendarClient<T> {
    /// `base_url` is the Calendar API root with no trailing slash, e.g.
    /// [`CALENDAR_API_BASE`] in production or a wiremock host
    /// (`http://127.0.0.1:PORT`) in tests.
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

    /// Share a rate limiter with other clients (e.g. the Gmail client) so a
    /// single per-account / per-project budget covers every Google call.
    pub(crate) fn with_rate_limiter(mut self, rate_limiter: Arc<KeyedRateLimiter>) -> Self {
        self.rate_limiter = rate_limiter;
        self
    }

    /// Inject a per-GCP-project daily-quota registry (issue #30).
    pub(crate) fn with_project_quota(mut self, registry: Arc<ProjectQuotaRegistry>) -> Self {
        self.project_quota = Some(registry);
        self
    }

    #[cfg(test)]
    pub(crate) const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Per-project then per-account quota check, in that order so a project
    /// denial never charges the per-account bucket. Mirrors
    /// [`crate::gmail::client::GmailClient`]'s `quota_check`.
    fn quota_check(&self, account: &str, cost: u32) -> Result<(), Error> {
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

    /// Authenticated GET. `cost` is the per-call quota cost; the rate limiter
    /// is consulted before any network I/O.
    #[tracing::instrument(
        skip_all,
        err(Display),
        fields(
            google.service = "calendar",
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
        let started = std::time::Instant::now();
        let resp = self
            .send_with_401_fallback(account, |token| self.http.get(&url).bearer_auth(token))
            .await;
        record_api_call_metrics(path, "GET", started, resp.as_ref());
        parse_json(resp?).await
    }

    /// Authenticated POST. See [`Self::authed_get`] for the `cost` contract.
    #[tracing::instrument(
        skip_all,
        err(Display),
        fields(
            google.service = "calendar",
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
        let started = std::time::Instant::now();
        let resp = self
            .send_with_401_fallback(account, |token| {
                self.http.post(&url).bearer_auth(token).json(body)
            })
            .await;
        record_api_call_metrics(path, "POST", started, resp.as_ref());
        parse_json(resp?).await
    }

    /// Send a request, retrying once on a 401 after a forced token refresh.
    /// `build_request` is called fresh per attempt — `RequestBuilder` is
    /// single-use and the retry uses the refreshed token.
    async fn send_with_401_fallback<F>(
        &self,
        account: &str,
        build_request: F,
    ) -> Result<reqwest::Response, Error>
    where
        F: Fn(&str) -> reqwest::RequestBuilder + Send + Sync,
    {
        let token = self.tokens.access_token(account).await?;
        let resp = execute_with_retry("calendar", &self.retry, || async {
            build_request(&token).send().await
        })
        .await;

        match resp {
            Ok(r) => Ok(r),
            Err(Error::Upstream { status: 401, .. }) => {
                let token = self.tokens.force_refresh(account).await?;
                let resp = execute_with_retry("calendar", &self.retry, || async {
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
/// per-call query. Used only for the `google.endpoint` tracing field.
fn endpoint_of(path: &str) -> &str {
    path.find('?').map_or(path, |i| &path[..i])
}

/// Bump `gmcp_google_api_calls_total` + duration histogram (ADR-0008 §Metrics)
/// with `service = "calendar"`. Mirrors the Gmail client's recorder.
fn record_api_call_metrics(
    path: &str,
    method: &'static str,
    started: std::time::Instant,
    resp: Result<&reqwest::Response, &Error>,
) {
    let status: Option<u16> = match resp {
        Ok(r) => Some(r.status().as_u16()),
        Err(Error::Upstream { status: s, .. }) => Some(*s),
        Err(Error::RateLimited { .. }) => Some(429),
        Err(_) => None,
    };
    let endpoint = endpoint_of(path).to_owned();
    metrics::counter!(
        crate::observability::metrics::names::GOOGLE_API_CALLS_TOTAL,
        "service" => "calendar",
        "endpoint" => endpoint.clone(),
        "method" => method,
        "status_class" => crate::observability::metrics::status_class(status),
    )
    .increment(1);
    metrics::histogram!(
        crate::observability::metrics::names::GOOGLE_API_CALL_DURATION_SECONDS,
        "service" => "calendar",
        "endpoint" => endpoint,
        "method" => method,
    )
    .record(started.elapsed().as_secs_f64());
}

async fn parse_json<R: DeserializeOwned>(resp: reqwest::Response) -> Result<R, Error> {
    let status = resp.status();
    let text = resp.text().await.map_err(Error::Network)?;
    if !status.is_success() {
        return Err(Error::upstream("calendar", status.as_u16(), text));
    }
    serde_json::from_str(&text).map_err(|e| Error::Parse {
        context: "calendar response body".to_owned(),
        source: e,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

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
            scopes: vec!["https://www.googleapis.com/auth/calendar".into()],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    /// Refresh transport scripting successive token-endpoint responses.
    struct MockRefreshTransport {
        responses: Mutex<Vec<Result<(u16, String), Error>>>,
    }

    impl RefreshTransport for MockRefreshTransport {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            let mut queue = self.responses.lock().unwrap();
            assert!(!queue.is_empty(), "MockRefreshTransport exhausted");
            queue.remove(0)
        }
    }

    fn make_client(
        base_url: &str,
        initial_token: &str,
        refresh_response: Option<String>,
    ) -> Arc<CalendarClient<MockRefreshTransport>> {
        let responses = refresh_response
            .map(|body| vec![Ok((200, body))])
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("gpm-cal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), fresh_state(initial_token))]),
            MockRefreshTransport {
                responses: Mutex::new(responses),
            },
            "https://example/token",
            dir,
        ));
        Arc::new(
            CalendarClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        )
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct EchoBody {
        ok: bool,
    }

    #[tokio::test]
    async fn success_path_passes_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary"))
            .and(header("authorization", "Bearer ACCESS-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "ACCESS-1", None);
        let body: EchoBody = client
            .authed_get("work", "/calendars/primary", 1)
            .await
            .expect("ok");
        assert_eq!(body, EchoBody { ok: true });
    }

    /// Returns `first_status` once, then 200 — drives the 401-refresh-retry.
    struct OnceThen {
        calls: AtomicU32,
        first_status: u16,
    }

    impl Respond for OnceThen {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(self.first_status).set_body_string("expired")
            } else {
                ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
            }
        }
    }

    #[tokio::test]
    async fn unauthorized_triggers_refresh_then_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary"))
            .respond_with(OnceThen {
                calls: AtomicU32::new(0),
                first_status: 401,
            })
            .mount(&server)
            .await;

        let refresh = r#"{"access_token":"REFRESHED","expires_in":3600}"#.to_owned();
        let client = make_client(&server.uri(), "STALE", Some(refresh));
        let body: EchoBody = client
            .authed_get("work", "/calendars/primary", 1)
            .await
            .expect("should recover from 401");
        assert_eq!(body, EchoBody { ok: true });
    }

    #[tokio::test]
    async fn non_success_status_maps_to_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary"))
            .respond_with(ResponseTemplate::new(404).set_body_string("no such calendar"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "ACCESS-1", None);
        let err = client
            .authed_get::<EchoBody>("work", "/calendars/primary", 1)
            .await
            .expect_err("404 must error");
        assert!(
            matches!(err, Error::Upstream { ref service, status: 404, .. } if service == "calendar"),
            "got: {err:?}"
        );
    }
}
