//! axum middleware enforcing the bearer-token check from
//! [ADR-0020](../../../docs/adr/0020-http-transport-authentication.md).
//!
//! Sits in front of the rmcp `StreamableHttpService` mounted at `/mcp`
//! by [`crate::server::serve_http_on`]. Returns `401 Unauthorized` with
//! the documented `WWW-Authenticate` header on any missing or invalid
//! `Authorization: Bearer <token>` header; passes through to the inner
//! handler on a successful match.
//!
//! **Routing:** only mounted when the daemon binds non-loopback. Loopback
//! binds (`127.0.0.1`, `::1`) bypass entirely — the OS user boundary is
//! the auth layer for local-only deployments per the ADR. The decision
//! lives in [`crate::lib::run_serve_blocking`]; this module exposes only
//! the layer constructor.
//!
//! **No session interaction:** the auth check happens *before* rmcp's
//! `Mcp-Session-Id` lookup. Unauthenticated requests never reach session
//! establishment, so a 401 cannot leak session-state information about
//! whether a token *would have* matched an existing session.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::throttle::{Throttle, ThrottleOutcome};
use super::BearerValidator;

/// `WWW-Authenticate` header value per ADR-0020 §"Failed-auth treatment".
/// Realm name matches the binary name so operators can spot daemon
/// origin in client error reports.
const WWW_AUTH_VALUE: &str = r#"Bearer realm="google-personal-mcp""#;

/// Generic error body: same shape for "missing" and "invalid" cases per
/// ADR-0020 §"Response" — distinguishing the two leaks information about
/// whether a header was present.
const UNAUTHORIZED_BODY: &str = r#"{"error":"unauthorized"}"#;

/// Bundled axum state for the bearer-auth middleware. Carries both
/// the token validator (#162) and the per-source-IP throttle (#170)
/// so the middleware can short-circuit on throttled IPs without
/// running the constant-time token compare.
///
/// `Clone` is cheap — every field is `Arc`-wrapped.
#[derive(Clone)]
pub(crate) struct AuthState {
    pub(crate) validator: Arc<BearerValidator>,
    pub(crate) throttle: Arc<Throttle>,
}

/// Helper exposed for call sites that need to mount the bearer-auth
/// middleware (`from_fn_with_state(state, bearer_middleware)`).
/// Named `pub(crate)` so the router-mounting code in `server::mod` and
/// the tests in this file's `tests` submodule can share one fn ptr.
///
/// Named `fn` (rather than a closure) so axum's signature-reflection-
/// based extractor wiring picks up the `State<AuthState>` and the
/// `ConnectInfo<SocketAddr>` automatically; the inner pre-check +
/// throttle + 401/429 composition lives in this function and nowhere
/// else.
pub(crate) async fn bearer_middleware(
    State(state): State<AuthState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let peer_ip = peer.ip();

    // ── Throttle pre-check (ADR-0020 §"Failed-auth treatment") ──────
    // If the IP is over its burst budget, reply 429 *immediately*
    // without running the bearer check. ADR rationale: don't burn
    // CPU on attackers, and the constant-time compare leaks no
    // additional information beyond what 429 already signals.
    if state.throttle.check(peer_ip, Instant::now()) == ThrottleOutcome::Throttled {
        tracing::warn!(
            path = %req.uri().path(),
            source_ip = %peer_ip,
            reason = "throttled",
            "http auth failure",
        );
        return throttled_response(state.throttle.retry_after_secs());
    }

    let validator = &state.validator;
    // Extract `Authorization: Bearer <token>` (or note its absence).
    // `to_str().ok()` rejects headers with non-ASCII bytes; the
    // `Authorization` header is ASCII-only per RFC 7235, so any
    // non-ASCII bytes are a treat-as-missing.
    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let presented = auth_header.and_then(|s| s.strip_prefix("Bearer "));

    if let Some(token) = presented {
        if validator.validate(token.as_bytes()) {
            return next.run(req).await;
        }
    }

    // Failed auth: bump the throttle, increment the metric, WARN to
    // tracing. No audit-log entry per ADR-0011 §"What is NOT in this
    // audit log". The source-IP label on the metric is high-cardinality
    // by ADR-0020 design — downstream alertmanager rules aggregate; the
    // throttle's idle-bucket sweep bounds in-memory growth.
    state.throttle.record_failure(peer_ip, Instant::now());
    let reason = if presented.is_none() {
        "missing"
    } else {
        "invalid"
    };
    metrics::counter!(
        crate::observability::metrics::names::HTTP_AUTH_FAILURES_TOTAL,
        "source_ip" => peer_ip.to_string(),
        "reason" => reason,
    )
    .increment(1);
    tracing::warn!(
        path = %req.uri().path(),
        source_ip = %peer_ip,
        reason = %reason,
        "http auth failure",
    );

    unauthorized_response()
}

/// Compose the 429 response with `Retry-After: <seconds>`. Body is
/// minimal — operators consume `Retry-After`, not the body.
fn throttled_response(retry_after_secs: u64) -> Response {
    metrics::counter!(
        crate::observability::metrics::names::HTTP_AUTH_FAILURES_TOTAL,
        "reason" => "throttled",
    )
    .increment(1);
    let body = r#"{"error":"too_many_requests"}"#;
    let retry_value = retry_after_secs.to_string();
    let mut resp = Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response());
    resp.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_value).unwrap_or(HeaderValue::from_static("60")),
    );
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// Compose the 401 response. Pulled out so tests can assert the exact
/// shape without re-running the full middleware path.
fn unauthorized_response() -> Response {
    let mut resp = Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(Body::from(UNAUTHORIZED_BODY))
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response());
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(WWW_AUTH_VALUE),
    );
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::http_auth::throttle::ThrottleConfig;
    use crate::http_auth::HttpAuthConfig;
    use axum::body::to_bytes;
    use axum::http::Method;
    use axum::routing::post;
    use axum::Router;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn validator(tokens: &[&str]) -> Arc<BearerValidator> {
        let cfg = HttpAuthConfig {
            tokens: tokens.iter().map(|t| (*t).to_owned()).collect(),
        };
        Arc::new(BearerValidator::new(&cfg, PathBuf::from("/dev/null")))
    }

    fn auth_state(tokens: &[&str]) -> AuthState {
        AuthState {
            validator: validator(tokens),
            throttle: Arc::new(Throttle::new(ThrottleConfig::default())),
        }
    }

    /// Build a router that always returns 200 OK on POST `/mcp` after the
    /// middleware passes. Lets each test assert on whether the inner
    /// handler ran. The router is wired with `ConnectInfo<SocketAddr>`
    /// via a manual `Extension` insert because `oneshot` bypasses
    /// `into_make_service_with_connect_info`.
    fn app(state: AuthState) -> Router {
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51_234);
        Router::new()
            .route("/mcp", post(|| async { "inner-ok" }))
            .layer(axum::middleware::from_fn_with_state(
                state,
                bearer_middleware,
            ))
            // Inject the ConnectInfo extension so the middleware's
            // `ConnectInfo<SocketAddr>` extractor resolves. Production
            // wiring uses `into_make_service_with_connect_info::<SocketAddr>()`.
            .layer(axum::Extension(ConnectInfo(peer)))
    }

    async fn body_string(resp: Response) -> (StatusCode, String, axum::http::HeaderMap) {
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&body).into_owned();
        (status, text, headers)
    }

    fn request(token: Option<&str>) -> Request {
        let mut b = Request::builder().method(Method::POST).uri("/mcp");
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn missing_header_returns_401_with_www_authenticate() {
        let resp = app(auth_state(&["secret"]))
            .oneshot(request(None))
            .await
            .unwrap();
        let (status, body, headers) = body_string(resp).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, UNAUTHORIZED_BODY);
        assert_eq!(
            headers.get(header::WWW_AUTHENTICATE).unwrap(),
            WWW_AUTH_VALUE
        );
        // Content-Type set so well-behaved clients render the JSON
        // payload rather than guessing.
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let resp = app(auth_state(&["secret"]))
            .oneshot(request(Some("not-secret")))
            .await
            .unwrap();
        let (status, body, headers) = body_string(resp).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, UNAUTHORIZED_BODY);
        // Wrong-token response must be byte-identical to missing-header
        // response — distinguishing them leaks information about whether
        // the header parsed.
        assert_eq!(
            headers.get(header::WWW_AUTHENTICATE).unwrap(),
            WWW_AUTH_VALUE
        );
    }

    #[tokio::test]
    async fn correct_token_passes_through_to_inner_handler() {
        let resp = app(auth_state(&["correct-horse"]))
            .oneshot(request(Some("correct-horse")))
            .await
            .unwrap();
        let (status, body, _) = body_string(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "inner-ok");
    }

    #[tokio::test]
    async fn either_rotation_token_passes() {
        // ADR-0020 §Rotation: during a rotation window, both old and
        // new tokens must validate.
        let state = auth_state(&["old", "new"]);
        for tok in ["old", "new"] {
            let resp = app(state.clone())
                .oneshot(request(Some(tok)))
                .await
                .unwrap();
            let (status, _, _) = body_string(resp).await;
            assert_eq!(status, StatusCode::OK, "token `{tok}` should pass");
        }
    }

    #[tokio::test]
    async fn wrong_scheme_returns_401() {
        // Basic auth header (or any non-Bearer scheme) is treated as
        // missing — the daemon does not negotiate alternative auth
        // schemes per ADR-0020.
        let state = auth_state(&["secret"]);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, "Basic c2VjcmV0Og==")
            .body(Body::empty())
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        let (status, _, _) = body_string(resp).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn case_sensitive_scheme() {
        // "bearer" in lowercase is not "Bearer" — the MCP spec quotes
        // the standard scheme name verbatim. Being strict here surfaces
        // client bugs early rather than papering over them.
        let state = auth_state(&["secret"]);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, "bearer secret")
            .body(Body::empty())
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        let (status, _, _) = body_string(resp).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// 429 path: drive the throttle past burst, then assert that the
    /// next request gets 429 + Retry-After: 60 instead of running the
    /// bearer compare.
    #[tokio::test]
    async fn burst_exhaustion_returns_429_with_retry_after() {
        let state = auth_state(&["secret"]);
        // Spend the burst (10) on wrong-token requests.
        for _ in 0..10 {
            let resp = app(state.clone())
                .oneshot(request(Some("wrong")))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        // 11th request — even with the correct token — gets 429.
        // (The throttle short-circuits before the bearer compare.)
        let resp = app(state).oneshot(request(Some("secret"))).await.unwrap();
        let (status, body, headers) = body_string(resp).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.contains("too_many_requests"), "body = {body}");
        assert_eq!(
            headers.get(header::RETRY_AFTER).unwrap(),
            "60",
            "Retry-After must match throttle's window_secs",
        );
    }
}
