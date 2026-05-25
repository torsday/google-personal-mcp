//! `TokenManager` — per-account OAuth token store. Owns the per-account
//! `Arc<RwLock<TokenState>>` registry, brokers refreshes against a
//! [`RefreshTransport`], and persists updated state atomically.
//!
//! Locking model is documented in [ADR-0004]: read-lock fast path → drop
//! → write-lock with double-check before calling the transport.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use tokio::sync::RwLock;

use crate::error::Error;

use super::persistence::write_atomic_0600;
use super::refresh::{
    apply_refresh_response, build_refresh_body, cooldown_secs, is_near_expiry, RefreshTransport,
    ReqwestRefreshTransport,
};
use super::{AccountSnapshot, TokenState};

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

    /// Snapshot per-account state for the `mcp_status` tool (#61). Acquires
    /// a read lock per account briefly; no refresh side effects. When
    /// `account` is `Some`, returns only that account (or empty if unknown);
    /// when `None`, returns every registered account ordered by alias.
    pub(crate) async fn account_snapshot(&self, account: Option<&str>) -> Vec<AccountSnapshot> {
        let mut aliases: Vec<&String> = account.map_or_else(
            || self.states.keys().collect(),
            |a| self.states.keys().filter(|k| k.as_str() == a).collect(),
        );
        aliases.sort();
        let mut out = Vec::with_capacity(aliases.len());
        for alias in aliases {
            // Safe: `alias` came from `self.states.keys()` above.
            if let Some(lock) = self.states.get(alias) {
                let s = lock.read().await;
                out.push(AccountSnapshot {
                    alias: alias.clone(),
                    scopes: s.scopes.clone(),
                    expires_at: s.expires_at,
                    last_refresh_at: s.last_refresh_at,
                    failed_until: s.failed_until,
                    consecutive_failures: s.consecutive_failures,
                });
            }
        }
        out
    }

    /// Test-only read accessor used by sibling-module tests (notably
    /// `refresh::wiremock_tests`) that need to verify post-refresh internal
    /// state without going through `account_snapshot` (which doesn't expose
    /// `refresh_token`). Not exposed outside the `tokens` module.
    #[cfg(test)]
    pub(super) async fn state_read(&self, account: &str) -> Option<TokenState> {
        let lock = self.states.get(account)?;
        let s = lock.read().await;
        Some(s.clone())
    }

    fn state_for(&self, account: &str) -> Result<&Arc<RwLock<TokenState>>, Error> {
        self.states
            .get(account)
            .ok_or_else(|| Error::AccountNotFound {
                account: account.to_owned(),
            })
    }

    /// Returns a valid access token, refreshing if within the expiry buffer.
    /// See ADR-0004 for the locking model.
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

        let final_result =
            result.and_then(|(status, body)| apply_refresh_response(&s, status, &body, account));
        let outcome_label = refresh_outcome_label(&final_result);
        metrics::counter!(
            crate::observability::metrics::names::TOKEN_REFRESHES_TOTAL,
            "account" => account.to_owned(),
            "outcome" => outcome_label,
        )
        .increment(1);
        match final_result {
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

/// Map a refresh attempt's outcome to the `outcome` label used by
/// `gmcp_token_refreshes_total`. Per ADR-0008: `success` /
/// `invalid_grant` (terminal — user must re-auth) / `network` /
/// `upstream` (Google returned non-success that wasn't `invalid_grant`).
const fn refresh_outcome_label(result: &Result<TokenState, Error>) -> &'static str {
    match result {
        Ok(_) => "success",
        Err(Error::AuthRequired { .. }) => "invalid_grant",
        Err(Error::Network(_)) => "network",
        Err(Error::Upstream { .. }) => "upstream",
        Err(_) => "other",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use chrono::DateTime;

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
            last_refresh_at: None,
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

    // ── TokenManager Debug redaction (AC) ─────────────────────────────────────

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
