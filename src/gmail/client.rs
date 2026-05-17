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

pub(crate) struct GmailClient<T: RefreshTransport = ReqwestRefreshTransport> {
    base_url: String,
    tokens: Arc<TokenManager<T>>,
    http: reqwest::Client,
    retry: RetryPolicy,
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
        }
    }

    #[cfg(test)]
    pub(crate) const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Authenticated GET. Path is appended to `base_url` verbatim — callers
    /// build the full path (including `?key=value` query strings).
    pub(crate) async fn authed_get<R: DeserializeOwned>(
        &self,
        account: &str,
        path: &str,
    ) -> Result<R, Error> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .send_with_401_fallback(account, |token| self.http.get(&url).bearer_auth(token))
            .await?;
        parse_json(resp).await
    }

    /// Authenticated POST. Body is sent as JSON.
    pub(crate) async fn authed_post<B: Serialize + Sync, R: DeserializeOwned>(
        &self,
        account: &str,
        path: &str,
        body: &B,
    ) -> Result<R, Error> {
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
        let body: EchoBody = client.authed_get("work", "/v1/me").await.expect("ok");
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
            .authed_get("work", "/v1/me")
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
            .authed_get::<EchoBody>("work", "/v1/me")
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
        let body: EchoBody = client.authed_get("work", "/v1/me").await.expect("ok");
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
            .authed_get::<EchoBody>("work", "/v1/me")
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
            .authed_get::<EchoBody>("work", "/v1/me")
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
}
