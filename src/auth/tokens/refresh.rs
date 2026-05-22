//! OAuth refresh state machine.
//!
//! - [`RefreshTransport`] — HTTP boundary, abstracted so Layer 1 unit tests
//!   exercise the full state machine without a real server.
//! - [`ReqwestRefreshTransport`] — production implementation over `reqwest`.
//! - [`apply_refresh_response`] — pure transformation of `(prior, response)`
//!   into the new `TokenState` or a typed error. Split out so every branch is
//!   testable.
//! - [`is_near_expiry`] / [`cooldown_secs`] — the two scalar predicates the
//!   manager uses to decide whether to refresh and how long to back off.

use std::future::Future;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use crate::error::Error;

use super::TokenState;

pub(super) const EXPIRY_BUFFER_SECS: i64 = 60;
pub(super) const COOLDOWN_INITIAL_SECS: i64 = 1;
pub(super) const COOLDOWN_MAX_SECS: i64 = 60;

/// Raw success response from Google's OAuth token endpoint.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

/// HTTP boundary for the refresh request. Abstracted so Layer 1 unit tests
/// can verify the full refresh state machine without an HTTP server. The
/// production implementation is [`ReqwestRefreshTransport`]; wiremock-backed
/// integration tests come later via issue #17.
pub(crate) trait RefreshTransport: Send + Sync {
    /// POST `body` as `application/x-www-form-urlencoded` to `token_uri`.
    /// Returns `(status_code, response_body)` on success at the transport
    /// layer; HTTP-level errors (DNS, TLS, timeouts) surface as `Error::Network`.
    fn post_form(
        &self,
        token_uri: &str,
        body: String,
    ) -> impl Future<Output = Result<(u16, String), Error>> + Send;
}

pub(crate) struct ReqwestRefreshTransport {
    http: reqwest::Client,
}

impl ReqwestRefreshTransport {
    pub(crate) const fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl RefreshTransport for ReqwestRefreshTransport {
    async fn post_form(&self, token_uri: &str, body: String) -> Result<(u16, String), Error> {
        let resp = self
            .http
            .post(token_uri)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(Error::Network)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(Error::Network)?;
        Ok((status, text))
    }
}

pub(super) fn is_near_expiry(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now + Duration::seconds(EXPIRY_BUFFER_SECS) >= expires_at
}

pub(super) fn cooldown_secs(consecutive_failures: u32) -> i64 {
    // 1s, 2s, 4s, 8s, ... capped at COOLDOWN_MAX_SECS.
    let shift = consecutive_failures.saturating_sub(1).min(30);
    let secs = COOLDOWN_INITIAL_SECS.saturating_mul(1_i64 << shift);
    secs.min(COOLDOWN_MAX_SECS)
}

/// Extract the OAuth `error_description` field from a JSON error body.
/// Returns `None` if `body` isn't JSON or the field is missing. Used
/// instead of splicing the full body into `Error::AuthRequired.reason`
/// per ADR-0017 §"Logging hygiene" (#103) — the description is a
/// human-meant string, never carries a token.
fn parse_oauth_error_description(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("error_description")?.as_str().map(str::to_owned)
}

pub(super) fn build_refresh_body(
    refresh_token: &str,
    client_id: &str,
    client_secret: &str,
) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .append_pair("client_id", client_id)
        .append_pair("client_secret", client_secret)
        .finish()
}

/// Pure transformation from `(prior state, http response)` into a new
/// `TokenState` or a typed error. Split out so tests can drive every branch
/// without a transport.
pub(super) fn apply_refresh_response(
    prior: &TokenState,
    status: u16,
    body: &str,
    account: &str,
) -> Result<TokenState, Error> {
    if !(200..300).contains(&status) {
        if body.contains("invalid_grant") {
            // ADR-0017 §"Logging hygiene" (#103): the response body may
            // contain a fresh access_token on partial-refresh paths.
            // Never splice it into `reason` — parse out the structured
            // `error_description` if present and reference *that*.
            let detail = parse_oauth_error_description(body);
            let reason = detail.map_or_else(
                || "refresh_token rejected (invalid_grant)".to_owned(),
                |d| format!("refresh_token rejected (invalid_grant): {d}"),
            );
            return Err(Error::AuthRequired {
                account: account.to_owned(),
                reason,
            });
        }
        return Err(Error::upstream("google-oauth", status, body.to_owned()));
    }

    let parsed: TokenResponse = serde_json::from_str(body).map_err(|e| Error::Parse {
        context: "OAuth refresh response".to_owned(),
        source: e,
    })?;

    let now = Utc::now();
    Ok(TokenState {
        access_token: parsed.access_token,
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| prior.refresh_token.clone()),
        expires_at: now + Duration::seconds(parsed.expires_in),
        scopes: prior.scopes.clone(),
        client_id: prior.client_id.clone(),
        client_secret: prior.client_secret.clone(),
        failed_until: None,
        consecutive_failures: 0,
        last_refresh_at: Some(now),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample_state(access: &str, refresh: &str, expires_at: DateTime<Utc>) -> TokenState {
        TokenState {
            access_token: access.into(),
            refresh_token: refresh.into(),
            expires_at,
            scopes: vec!["https://www.googleapis.com/auth/gmail.modify".into()],
            client_id: "client-id-abc".into(),
            client_secret: "very-secret-shhh".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    fn success_body() -> String {
        r#"{"access_token":"new-access","expires_in":3600}"#.into()
    }

    fn success_body_with_rotation() -> String {
        r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600}"#
            .into()
    }

    // ── apply_refresh_response (pure) ─────────────────────────────────────────

    #[test]
    fn apply_refresh_keeps_old_refresh_when_not_rotated() {
        let prior = sample_state("old-access", "keep-me", Utc::now());
        let new =
            apply_refresh_response(&prior, 200, &success_body(), "work").expect("should succeed");
        assert_eq!(new.access_token, "new-access");
        assert_eq!(new.refresh_token, "keep-me");
        assert_eq!(new.consecutive_failures, 0);
        assert!(new.failed_until.is_none());
    }

    #[test]
    fn apply_refresh_picks_up_rotated_refresh_token() {
        let prior = sample_state("old-access", "old-refresh", Utc::now());
        let new = apply_refresh_response(&prior, 200, &success_body_with_rotation(), "work")
            .expect("should succeed");
        assert_eq!(new.refresh_token, "rotated-refresh");
    }

    #[test]
    fn apply_refresh_invalid_grant_maps_to_auth_required() {
        let prior = sample_state("old", "bad-refresh", Utc::now());
        let err = apply_refresh_response(
            &prior,
            400,
            r#"{"error":"invalid_grant","error_description":"Bad refresh"}"#,
            "work",
        )
        .expect_err("must fail");
        assert!(
            matches!(err, Error::AuthRequired { ref account, .. } if account == "work"),
            "got: {err:?}"
        );
    }

    #[test]
    fn apply_refresh_other_4xx_is_upstream() {
        let prior = sample_state("old", "old", Utc::now());
        let err = apply_refresh_response(&prior, 403, r#"{"error":"forbidden"}"#, "work")
            .expect_err("must fail");
        assert!(
            matches!(err, Error::Upstream { status: 403, .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn apply_refresh_5xx_is_upstream() {
        let prior = sample_state("old", "old", Utc::now());
        let err = apply_refresh_response(&prior, 503, "service unavailable", "work")
            .expect_err("must fail");
        assert!(
            matches!(err, Error::Upstream { status: 503, .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn apply_refresh_parse_failure_is_parse_error() {
        let prior = sample_state("old", "old", Utc::now());
        let err =
            apply_refresh_response(&prior, 200, "not json at all", "work").expect_err("must fail");
        assert!(matches!(err, Error::Parse { .. }), "got: {err:?}");
    }

    // ── is_near_expiry boundary ───────────────────────────────────────────────

    #[test]
    fn near_expiry_within_buffer() {
        let now = Utc::now();
        let exp = now + Duration::seconds(30);
        assert!(is_near_expiry(exp, now));
    }

    #[test]
    fn not_near_expiry_outside_buffer() {
        let now = Utc::now();
        let exp = now + Duration::seconds(120);
        assert!(!is_near_expiry(exp, now));
    }

    // ── cooldown_secs growth ──────────────────────────────────────────────────

    #[test]
    fn cooldown_secs_grows_exponentially_and_caps() {
        assert_eq!(cooldown_secs(0), 1);
        assert_eq!(cooldown_secs(1), 1);
        assert_eq!(cooldown_secs(2), 2);
        assert_eq!(cooldown_secs(3), 4);
        assert_eq!(cooldown_secs(4), 8);
        assert_eq!(cooldown_secs(7), COOLDOWN_MAX_SECS);
        assert_eq!(cooldown_secs(100), COOLDOWN_MAX_SECS);
    }
}

// ── Layer 2 wiremock tests for ReqwestRefreshTransport ──────────────────────
//
// The Layer 1 tests above exercise the pure-logic helpers and use a script
// transport (in `manager.rs`). These tests exercise `ReqwestRefreshTransport`
// against a real `wiremock` HTTP server so the on-the-wire behavior — header
// construction, body encoding, status parsing — is verified end-to-end.
// Closes the only L2-untested production code path noted in #17.

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod wiremock_tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use chrono::Duration as ChronoDuration;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::tokens::TokenManager;

    fn stale_state() -> TokenState {
        TokenState {
            access_token: "STALE".into(),
            refresh_token: "REFRESH-OLD".into(),
            // 5s out — within the 60s buffer, so access_token() will refresh.
            expires_at: Utc::now() + ChronoDuration::seconds(5),
            scopes: vec!["scope.test".into()],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    fn unique_tmp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gpm-tokens-wm-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn build_manager(token_uri: &str, label: &str) -> TokenManager<ReqwestRefreshTransport> {
        TokenManager::new(
            HashMap::from([("work".to_owned(), stale_state())]),
            ReqwestRefreshTransport::new(reqwest::Client::new()),
            token_uri.to_owned(),
            unique_tmp_dir(label),
        )
    }

    // ── Success refresh: real HTTP → updated access_token ───────────────────

    #[tokio::test]
    async fn refresh_success_via_reqwest_updates_state_and_persists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=REFRESH-OLD"))
            .and(body_string_contains("client_id=cid"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"access_token":"FRESH","expires_in":3600}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mgr = build_manager(&format!("{}/token", server.uri()), "success");
        let tok = mgr.access_token("work").await.expect("refresh ok");
        assert_eq!(tok, "FRESH");
        // Refresh token persists unchanged when not rotated.
        let s = mgr.state_read("work").await.expect("known account");
        assert_eq!(s.refresh_token, "REFRESH-OLD");
        assert!(s.expires_at > Utc::now() + ChronoDuration::seconds(60));
    }

    // ── invalid_grant maps to AuthRequired ──────────────────────────────────

    #[tokio::test]
    async fn invalid_grant_response_maps_to_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":"invalid_grant","error_description":"Token revoked"}"#,
            ))
            .mount(&server)
            .await;

        let mgr = build_manager(&format!("{}/token", server.uri()), "invalid-grant");
        let err = mgr.access_token("work").await.expect_err("must fail");
        assert!(
            matches!(err, Error::AuthRequired { ref account, .. } if account == "work"),
            "got: {err:?}"
        );
    }

    // ── Refresh-token rotation: new refresh_token replaces old ──────────────

    #[tokio::test]
    async fn refresh_token_rotation_replaces_prior_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"FRESH","refresh_token":"REFRESH-NEW","expires_in":3600}"#,
            ))
            .mount(&server)
            .await;

        let mgr = build_manager(&format!("{}/token", server.uri()), "rotate");
        mgr.access_token("work").await.expect("refresh ok");
        let s = mgr.state_read("work").await.expect("known account");
        assert_eq!(s.refresh_token, "REFRESH-NEW");
    }

    // ── 5xx upstream → Error::Upstream with body captured ───────────────────

    #[tokio::test]
    async fn upstream_5xx_returns_upstream_error_with_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream is down"))
            .mount(&server)
            .await;

        let mgr = build_manager(&format!("{}/token", server.uri()), "5xx");
        let err = mgr.access_token("work").await.expect_err("must fail");
        match err {
            Error::Upstream {
                status: 503,
                ref message,
                ..
            } => assert!(message.contains("upstream is down")),
            other => panic!("expected Upstream(503), got {other:?}"),
        }
    }
}
