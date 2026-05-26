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

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::BearerValidator;

/// `WWW-Authenticate` header value per ADR-0020 §"Failed-auth treatment".
/// Realm name matches the binary name so operators can spot daemon
/// origin in client error reports.
const WWW_AUTH_VALUE: &str = r#"Bearer realm="google-personal-mcp""#;

/// Generic error body: same shape for "missing" and "invalid" cases per
/// ADR-0020 §"Response" — distinguishing the two leaks information about
/// whether a header was present.
const UNAUTHORIZED_BODY: &str = r#"{"error":"unauthorized"}"#;

/// Helper exposed for call sites that need to mount the bearer-auth
/// middleware (`from_fn_with_state(validator, bearer_middleware)`).
/// Named `pub(crate)` so the router-mounting code in `server::mod` and
/// the tests in this file's `tests` submodule can share one fn ptr.
///
/// Named `fn` (rather than a closure) so axum's signature-reflection-
/// based extractor wiring picks up the `State<Arc<BearerValidator>>`
/// automatically; the inner pre-check + 401 composition lives in this
/// function and nowhere else.
pub(crate) async fn bearer_middleware(
    State(validator): State<Arc<BearerValidator>>,
    req: Request,
    next: Next,
) -> Response {
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

    // Failed auth: WARN to tracing log per ADR-0020 §"Failed-auth
    // treatment" / ADR-0011 §"What is NOT in this audit log". No
    // audit-log entry; that's reserved for tool invocations on operator
    // data. Source-IP capture lands with #170 (per-source-IP throttle);
    // until then, the path + reason are enough to spot a brute-force
    // pattern in journald.
    let reason = if presented.is_none() {
        "missing"
    } else {
        "invalid"
    };
    tracing::warn!(
        path = %req.uri().path(),
        reason = %reason,
        "http auth failure",
    );

    unauthorized_response()
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
    use crate::http_auth::HttpAuthConfig;
    use axum::body::to_bytes;
    use axum::http::Method;
    use axum::routing::post;
    use axum::Router;
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn validator(tokens: &[&str]) -> Arc<BearerValidator> {
        let cfg = HttpAuthConfig {
            tokens: tokens.iter().map(|t| (*t).to_owned()).collect(),
        };
        Arc::new(BearerValidator::new(&cfg, PathBuf::from("/dev/null")))
    }

    /// Build a router that always returns 200 OK on POST `/mcp` after the
    /// middleware passes. Lets each test assert on whether the inner
    /// handler ran.
    fn app(v: Arc<BearerValidator>) -> Router {
        Router::new()
            .route("/mcp", post(|| async { "inner-ok" }))
            .layer(axum::middleware::from_fn_with_state(v, bearer_middleware))
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
        let v = validator(&["secret"]);
        let resp = app(v).oneshot(request(None)).await.unwrap();
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
        let v = validator(&["secret"]);
        let resp = app(v).oneshot(request(Some("not-secret"))).await.unwrap();
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
        let v = validator(&["correct-horse"]);
        let resp = app(v)
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
        let v = validator(&["old", "new"]);
        for tok in ["old", "new"] {
            let resp = app(Arc::clone(&v))
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
        let v = validator(&["secret"]);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, "Basic c2VjcmV0Og==")
            .body(Body::empty())
            .unwrap();
        let resp = app(v).oneshot(req).await.unwrap();
        let (status, _, _) = body_string(resp).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn case_sensitive_scheme() {
        // "bearer" in lowercase is not "Bearer" — the MCP spec quotes
        // the standard scheme name verbatim. Being strict here surfaces
        // client bugs early rather than papering over them.
        let v = validator(&["secret"]);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, "bearer secret")
            .body(Body::empty())
            .unwrap();
        let resp = app(v).oneshot(req).await.unwrap();
        let (status, _, _) = body_string(resp).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
