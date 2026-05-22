//! `TokenManager` — per-account OAuth token store with refresh and atomic persistence.
//!
//! Implements ADR-0004: proactive expiry-based refresh with a 401 fallback,
//! plus the redacted `Debug` invariants of ADR-0017.

#![allow(clippy::significant_drop_tightening)]

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::Error;

const EXPIRY_BUFFER_SECS: i64 = 60;
const COOLDOWN_INITIAL_SECS: i64 = 1;
const COOLDOWN_MAX_SECS: i64 = 60;

const REDACTED: &str = "<redacted>";

/// Persisted, on-disk shape of a token. Transient state (cooldown, failure
/// counts) is `#[serde(skip)]` so it never round-trips through the token file.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TokenState {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub scopes: Vec<String>,
    pub client_id: String,
    pub client_secret: String,

    #[serde(skip)]
    pub failed_until: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub consecutive_failures: u32,
}

impl fmt::Debug for TokenState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenState")
            .field("access_token", &REDACTED)
            .field("refresh_token", &REDACTED)
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .field("client_id", &self.client_id)
            .field("client_secret", &REDACTED)
            .field("failed_until", &self.failed_until)
            .field("consecutive_failures", &self.consecutive_failures)
            .finish()
    }
}

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

/// Per-account OAuth token store. Hot-reload-safe via the `Arc<ArcSwap<_>>`
/// pattern documented in [ADR-0002]; this struct owns only the inner state.
pub(crate) struct TokenManager<T: RefreshTransport = ReqwestRefreshTransport> {
    states: HashMap<String, Arc<RwLock<TokenState>>>,
    /// `account → client_id` snapshot captured at construction. The
    /// `client_id` doesn't rotate across refreshes for the same OAuth
    /// client, so this stays stable for the daemon's lifetime — letting
    /// callers resolve per-account project context without holding any
    /// async lock. See [`crate::project_quota`].
    client_ids: HashMap<String, String>,
    transport: T,
    token_uri: String,
    tokens_dir: PathBuf,
}

impl<T: RefreshTransport> fmt::Debug for TokenManager<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenManager")
            .field("accounts", &self.states.keys().collect::<Vec<_>>())
            .field("token_uri", &self.token_uri)
            .field("tokens_dir", &self.tokens_dir)
            .finish_non_exhaustive()
    }
}

impl<T: RefreshTransport> TokenManager<T> {
    pub(crate) fn new(
        states: HashMap<String, TokenState>,
        transport: T,
        token_uri: impl Into<String>,
        tokens_dir: impl Into<PathBuf>,
    ) -> Self {
        let client_ids: HashMap<String, String> = states
            .iter()
            .map(|(k, v)| (k.clone(), v.client_id.clone()))
            .collect();
        let states = states
            .into_iter()
            .map(|(k, v)| (k, Arc::new(RwLock::new(v))))
            .collect();
        Self {
            states,
            client_ids,
            transport,
            token_uri: token_uri.into(),
            tokens_dir: tokens_dir.into(),
        }
    }

    /// Look up `account`'s OAuth `client_id` without taking any lock.
    /// Returns the value captured at construction. Used by the per-GCP-project
    /// quota tracker (issue #30) to map account → GCP project number.
    pub(crate) fn client_id(&self, account: &str) -> Option<&str> {
        self.client_ids.get(account).map(String::as_str)
    }

    fn state_for(&self, account: &str) -> Result<&Arc<RwLock<TokenState>>, Error> {
        self.states
            .get(account)
            .ok_or_else(|| Error::AccountNotFound {
                account: account.to_owned(),
            })
    }

    /// Returns a valid access token, refreshing if within
    /// [`EXPIRY_BUFFER_SECS`] of expiry. See ADR-0004 for the locking model.
    pub(crate) async fn access_token(&self, account: &str) -> Result<String, Error> {
        let state = self.state_for(account)?;

        // Fast path — read lock, return current token if not near expiry.
        {
            let s = state.read().await;
            if !is_near_expiry(s.expires_at, Utc::now()) {
                return Ok(s.access_token.clone());
            }
        }

        self.refresh_locked(account, state, false).await
    }

    /// Unconditionally refresh and return the new access token. Used by
    /// the 401 fallback in `GmailClient` and peers.
    pub(crate) async fn force_refresh(&self, account: &str) -> Result<String, Error> {
        let state = self.state_for(account)?;
        self.refresh_locked(account, state, true).await
    }

    #[tracing::instrument(
        skip_all,
        err(Display),
        fields(oauth.account = %account, oauth.force = force),
    )]
    async fn refresh_locked(
        &self,
        account: &str,
        state: &Arc<RwLock<TokenState>>,
        force: bool,
    ) -> Result<String, Error> {
        let mut s = state.write().await;

        // Double-check: another writer may have refreshed while we were waiting.
        if !force && !is_near_expiry(s.expires_at, Utc::now()) {
            return Ok(s.access_token.clone());
        }

        // Honor the cooldown — surface the prior failure without hitting Google.
        let now = Utc::now();
        if let Some(until) = s.failed_until {
            if now < until {
                return Err(Error::AuthRequired {
                    account: account.to_owned(),
                    reason: format!(
                        "refresh in cooldown for {}s (consecutive failures: {})",
                        (until - now).num_seconds().max(0),
                        s.consecutive_failures
                    ),
                });
            }
        }

        let body = build_refresh_body(&s.refresh_token, &s.client_id, &s.client_secret);
        let result = self.transport.post_form(&self.token_uri, body).await;

        match result.and_then(|(status, body)| apply_refresh_response(&s, status, &body, account)) {
            Ok(new_state) => {
                self.persist_atomic(account, &new_state).await?;
                let access = new_state.access_token.clone();
                *s = new_state;
                Ok(access)
            }
            Err(e) => {
                // invalid_grant is terminal — don't burn cooldown on it; the
                // user has to re-auth regardless. All other errors trip the
                // exponential backoff so we don't hammer Google.
                if matches!(e, Error::AuthRequired { .. }) {
                    s.failed_until = None;
                    s.consecutive_failures = 0;
                } else {
                    s.consecutive_failures = s.consecutive_failures.saturating_add(1);
                    let secs = cooldown_secs(s.consecutive_failures);
                    s.failed_until = Some(Utc::now() + Duration::seconds(secs));
                }
                Err(e)
            }
        }
    }

    /// Write `state` to `<tokens_dir>/<account>.json` atomically (tmpfile +
    /// rename). On Unix the final file is chmod 0600. ADR-0017.
    async fn persist_atomic(&self, account: &str, state: &TokenState) -> Result<(), Error> {
        let final_path = self.tokens_dir.join(format!("{account}.json"));
        let tmp_path = self.tokens_dir.join(format!(".{account}.json.tmp"));
        let body = serde_json::to_string_pretty(state).map_err(|e| Error::Parse {
            context: "serialize TokenState".to_owned(),
            source: e,
        })?;
        write_atomic_0600(&tmp_path, &final_path, body.as_bytes()).await
    }
}

fn is_near_expiry(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now + Duration::seconds(EXPIRY_BUFFER_SECS) >= expires_at
}

fn cooldown_secs(consecutive_failures: u32) -> i64 {
    // 1s, 2s, 4s, 8s, ... capped at COOLDOWN_MAX_SECS.
    let shift = consecutive_failures.saturating_sub(1).min(30);
    let secs = COOLDOWN_INITIAL_SECS.saturating_mul(1_i64 << shift);
    secs.min(COOLDOWN_MAX_SECS)
}

fn build_refresh_body(refresh_token: &str, client_id: &str, client_secret: &str) -> String {
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
fn apply_refresh_response(
    prior: &TokenState,
    status: u16,
    body: &str,
    account: &str,
) -> Result<TokenState, Error> {
    if !(200..300).contains(&status) {
        if body.contains("invalid_grant") {
            return Err(Error::AuthRequired {
                account: account.to_owned(),
                reason: format!("refresh_token rejected (invalid_grant): {body}"),
            });
        }
        return Err(Error::upstream("google-oauth", status, body.to_owned()));
    }

    let parsed: TokenResponse = serde_json::from_str(body).map_err(|e| Error::Parse {
        context: "OAuth refresh response".to_owned(),
        source: e,
    })?;

    Ok(TokenState {
        access_token: parsed.access_token,
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| prior.refresh_token.clone()),
        expires_at: Utc::now() + Duration::seconds(parsed.expires_in),
        scopes: prior.scopes.clone(),
        client_id: prior.client_id.clone(),
        client_secret: prior.client_secret.clone(),
        failed_until: None,
        consecutive_failures: 0,
    })
}

async fn write_atomic_0600(tmp: &Path, final_path: &Path, bytes: &[u8]) -> Result<(), Error> {
    tokio::fs::write(tmp, bytes).await?;
    set_mode_0600(tmp).await?;
    tokio::fs::rename(tmp, final_path).await?;
    Ok(())
}

#[cfg(unix)]
async fn set_mode_0600(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(path).await?.permissions();
    perms.set_mode(0o600);
    tokio::fs::set_permissions(path, perms).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_mode_0600(_path: &Path) -> Result<(), Error> {
    // No-op on non-Unix; ADR-0017's permission check is Unix-only.
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn unique_tmp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("gpm-tokens-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

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
        }
    }

    /// Scriptable transport: each call returns the next queued `(status, body)`
    /// (or a network error). Also counts how many times it was invoked.
    struct MockTransport {
        responses: Mutex<Vec<Result<(u16, String), Error>>>,
        calls: AtomicU32,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<(u16, String), Error>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: AtomicU32::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl RefreshTransport for MockTransport {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.responses.lock().expect("mock lock");
            if queue.is_empty() {
                return Err(Error::Internal {
                    context: "MockTransport".into(),
                    source: anyhow::anyhow!("no more responses queued"),
                });
            }
            queue.remove(0)
        }
    }

    fn success_body() -> String {
        r#"{"access_token":"new-access","expires_in":3600}"#.into()
    }

    fn success_body_with_rotation() -> String {
        r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600}"#
            .into()
    }

    // ── TokenState Debug redaction (AC) ───────────────────────────────────────

    #[test]
    fn token_state_debug_redacts_tokens_and_secret() {
        let s = sample_state("aaaa-access-bytes", "bbbb-refresh-bytes", Utc::now());
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("aaaa-access-bytes"),
            "access_token leaked: {dbg}"
        );
        assert!(
            !dbg.contains("bbbb-refresh-bytes"),
            "refresh_token leaked: {dbg}"
        );
        assert!(
            !dbg.contains("very-secret-shhh"),
            "client_secret leaked: {dbg}"
        );
        assert!(dbg.contains("<redacted>"), "no redaction marker: {dbg}");
        assert!(dbg.contains("client-id-abc"));
    }

    #[tokio::test]
    async fn token_manager_debug_redacts() {
        let dir = unique_tmp_dir("dbg-redact");
        let mgr = TokenManager::new(
            HashMap::from([(
                "work".to_owned(),
                sample_state("a-leak-target", "r-leak-target", Utc::now()),
            )]),
            MockTransport::new(vec![]),
            "https://example/token",
            dir,
        );
        let dbg = format!("{mgr:?}");
        assert!(!dbg.contains("a-leak-target"), "leaked via manager: {dbg}");
        assert!(!dbg.contains("r-leak-target"), "leaked via manager: {dbg}");
        assert!(dbg.contains("work"), "account alias missing: {dbg}");
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

    // ── access_token returns cached when not expiring (AC) ────────────────────

    #[tokio::test]
    async fn access_token_returns_cached_when_not_near_expiry() {
        let dir = unique_tmp_dir("cached");
        let state = sample_state("still-valid", "r", Utc::now() + Duration::seconds(3600));
        let mgr = TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            MockTransport::new(vec![]),
            "https://example/token",
            dir,
        );
        let tok = mgr.access_token("work").await.expect("should succeed");
        assert_eq!(tok, "still-valid");
        assert_eq!(mgr.transport.call_count(), 0, "should not refresh");
    }

    #[tokio::test]
    async fn access_token_account_not_found() {
        let dir = unique_tmp_dir("missing");
        let mgr = TokenManager::new(
            HashMap::new(),
            MockTransport::new(vec![]),
            "https://example/token",
            dir,
        );
        let err = mgr.access_token("ghost").await.expect_err("must fail");
        assert!(matches!(err, Error::AccountNotFound { .. }), "got: {err:?}");
    }

    // ── Refresh near expiry persists atomically and updates state (AC) ────────

    #[tokio::test]
    async fn access_token_refreshes_near_expiry_and_persists() {
        let dir = unique_tmp_dir("persist");
        let state = sample_state("stale", "r", Utc::now() + Duration::seconds(10));
        let mgr = TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            MockTransport::new(vec![Ok((200, success_body()))]),
            "https://example/token",
            dir.clone(),
        );
        let tok = mgr.access_token("work").await.expect("should refresh");
        assert_eq!(tok, "new-access");
        assert_eq!(mgr.transport.call_count(), 1);

        let file = dir.join("work.json");
        let written = std::fs::read_to_string(&file).expect("file present");
        let parsed: TokenState = serde_json::from_str(&written).expect("valid json");
        assert_eq!(parsed.access_token, "new-access");
        assert_eq!(parsed.refresh_token, "r");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "wrong mode: {mode:o}");
        }
    }

    // ── force_refresh always refreshes (AC) ───────────────────────────────────

    #[tokio::test]
    async fn force_refresh_refreshes_even_when_token_is_fresh() {
        let dir = unique_tmp_dir("force");
        let state = sample_state("still-valid", "r", Utc::now() + Duration::seconds(3600));
        let mgr = TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            MockTransport::new(vec![Ok((200, success_body()))]),
            "https://example/token",
            dir,
        );
        let tok = mgr
            .force_refresh("work")
            .await
            .expect("should force-refresh");
        assert_eq!(tok, "new-access");
        assert_eq!(mgr.transport.call_count(), 1);
    }

    // ── Concurrency: exactly one refresh fires under contention (AC) ──────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_access_token_double_check_only_one_refresh() {
        let dir = unique_tmp_dir("concurrent");
        let state = sample_state("stale", "r", Utc::now() + Duration::seconds(5));
        let mgr = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            // Only ONE success queued — a second refresh attempt would fail
            // with "no more responses". So the assertion is twofold: only
            // one call, AND both join arms succeed.
            MockTransport::new(vec![Ok((200, success_body()))]),
            "https://example/token",
            dir,
        ));

        let a = mgr.clone();
        let b = mgr.clone();
        let (r1, r2) = tokio::join!(async move { a.access_token("work").await }, async move {
            b.access_token("work").await
        });
        assert_eq!(r1.expect("a ok"), "new-access");
        assert_eq!(r2.expect("b ok"), "new-access");
        assert_eq!(
            mgr.transport.call_count(),
            1,
            "double-check should suppress the second refresh"
        );
    }

    // ── invalid_grant → AuthRequired and does NOT trip cooldown (AC) ──────────

    #[tokio::test]
    async fn invalid_grant_returns_auth_required_no_cooldown() {
        let dir = unique_tmp_dir("invalid-grant");
        let state = sample_state("stale", "bad-refresh", Utc::now() + Duration::seconds(5));
        let mgr = TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            MockTransport::new(vec![Ok((400, r#"{"error":"invalid_grant"}"#.into()))]),
            "https://example/token",
            dir,
        );
        let err = mgr.access_token("work").await.expect_err("must fail");
        assert!(matches!(err, Error::AuthRequired { .. }), "got: {err:?}");

        let s = mgr.states["work"].read().await;
        assert!(
            s.failed_until.is_none(),
            "invalid_grant shouldn't cool down"
        );
        assert_eq!(s.consecutive_failures, 0);
    }

    // ── Non-invalid_grant failure: cooldown grows then resets on success ──────

    #[tokio::test]
    async fn upstream_failure_trips_cooldown_growing_then_resets_on_success() {
        let dir = unique_tmp_dir("cooldown");
        let state = sample_state("stale", "r", Utc::now() + Duration::seconds(5));
        let mgr = TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            MockTransport::new(vec![
                Ok((503, "boom".into())),
                Ok((503, "boom".into())),
                Ok((200, success_body())),
            ]),
            "https://example/token",
            dir,
        );

        let err = mgr
            .force_refresh("work")
            .await
            .expect_err("first must fail");
        assert!(matches!(err, Error::Upstream { .. }));
        {
            let s = mgr.states["work"].read().await;
            assert_eq!(s.consecutive_failures, 1);
            assert!(s.failed_until.expect("cooldown set") > Utc::now());
        }

        // While in cooldown: surfaces AuthRequired without hitting transport.
        let calls_before = mgr.transport.call_count();
        let err2 = mgr.force_refresh("work").await.expect_err("cooldown");
        assert!(
            matches!(err2, Error::AuthRequired { ref reason, .. } if reason.contains("cooldown")),
            "got: {err2:?}"
        );
        assert_eq!(mgr.transport.call_count(), calls_before);

        // Clear cooldown to drive the next failure path.
        {
            let mut s = mgr.states["work"].write().await;
            s.failed_until = None;
        }
        let err3 = mgr
            .force_refresh("work")
            .await
            .expect_err("third must fail");
        assert!(matches!(err3, Error::Upstream { .. }));
        {
            let s = mgr.states["work"].read().await;
            assert_eq!(s.consecutive_failures, 2);
        }

        // Clear cooldown again, then succeed: counters reset.
        {
            let mut s = mgr.states["work"].write().await;
            s.failed_until = None;
        }
        let tok = mgr.force_refresh("work").await.expect("third succeeds");
        assert_eq!(tok, "new-access");
        let s = mgr.states["work"].read().await;
        assert_eq!(s.consecutive_failures, 0);
        assert!(s.failed_until.is_none());
    }
}

// ── Layer 2 wiremock tests for ReqwestRefreshTransport ──────────────────────
//
// The Layer 1 tests above use `MockTransport` (a script of `(status, body)`
// tuples) and bypass `reqwest` entirely. These tests exercise
// `ReqwestRefreshTransport` against a real `wiremock` HTTP server so the
// on-the-wire behavior — header construction, body encoding, status parsing —
// is verified end-to-end. Closes the only L2-untested production code path
// noted in #17.

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod wiremock_tests {
    use super::*;
    use std::collections::HashMap;

    use chrono::Duration as ChronoDuration;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
        let s = mgr.states["work"].read().await;
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
        let s = mgr.states["work"].read().await;
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
