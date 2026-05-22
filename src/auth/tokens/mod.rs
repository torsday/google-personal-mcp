//! `TokenManager` — per-account OAuth token store with refresh and atomic persistence.
//!
//! Implements ADR-0004: proactive expiry-based refresh with a 401 fallback,
//! plus the redacted `Debug` invariants of ADR-0017.

#![allow(clippy::significant_drop_tightening)]

mod manager;
mod persistence;
mod refresh;

pub(crate) use manager::TokenManager;
pub(crate) use refresh::{RefreshTransport, ReqwestRefreshTransport};

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
    /// When this token was last successfully refreshed by *this* daemon
    /// process. `None` until the first successful in-process refresh —
    /// state loaded from disk doesn't carry it because token files predate
    /// the field. Surfaced via `TokenManager::account_snapshot` to power
    /// `mcp_status` (#61).
    #[serde(skip)]
    pub last_refresh_at: Option<DateTime<Utc>>,
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
            .field("last_refresh_at", &self.last_refresh_at)
            .finish()
    }
}

/// Per-account state snapshot for `mcp_status` (#61). Lock-free read-only
/// view — derived once via [`TokenManager::account_snapshot`] under brief
/// read locks, then handed to the tool layer.
#[derive(Debug, Clone)]
pub(crate) struct AccountSnapshot {
    pub alias: String,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub last_refresh_at: Option<DateTime<Utc>>,
    pub failed_until: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample_state(access: &str, refresh: &str) -> TokenState {
        TokenState {
            access_token: access.into(),
            refresh_token: refresh.into(),
            expires_at: Utc::now(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.modify".into()],
            client_id: "client-id-abc".into(),
            client_secret: "very-secret-shhh".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    #[test]
    fn token_state_debug_redacts_tokens_and_secret() {
        let s = sample_state("aaaa-access-bytes", "bbbb-refresh-bytes");
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
}
