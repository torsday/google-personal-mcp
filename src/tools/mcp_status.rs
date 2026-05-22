//! `mcp_status` introspection tool per [ADR-0014](../../docs/adr/0014-status-introspection-tool.md).
//!
//! v0.x minimal subset: returns per-account auth state (alias, granted
//! scopes, `last_refresh_at`, `auth_state`). Cache stats, build info, and
//! transport detail from ADR-0014's full design are deferred until the
//! corresponding subsystems land.
//!
//! Pure transformation over a [`TokenManager::account_snapshot`] result
//! plus an optional account-alias filter. No I/O, no network.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::auth::tokens::AccountSnapshot;

/// Possible auth states surfaced to consumers. See [`auth_state_from`] for
/// the derivation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthState {
    /// Token valid for at least [`AUTH_OK_BUFFER`] more — calls will use
    /// the cached access token without a refresh round-trip.
    Ok,
    /// Token within the 60s expiry buffer; next call will trigger a
    /// proactive refresh. Distinct from `Expired` so operators see when
    /// the refresh dance is about to fire.
    Expiring,
    /// Past `expires_at`; the next call will refresh.
    Expired,
    /// Daemon recorded a refresh failure with a `failed_until` cooldown
    /// active. Calls return `Error::AuthRequired` until cooldown clears.
    /// A cleared cooldown with residual `consecutive_failures` reports
    /// `Expiring` instead (the daemon will self-recover on the next call).
    AuthRequired,
}

/// Buffer mirroring `EXPIRY_BUFFER_SECS` in `tokens.rs` — we report
/// `Expiring` for tokens whose expiry is within this many seconds.
const AUTH_OK_BUFFER: i64 = 60;

/// Request shape per ADR-0014 §"Tool signature". v0.x cut: only the
/// `account` filter — the other ADR-0014 toggles attach to subsystems
/// that don't exist yet (cache stats, recent errors).
#[derive(Debug, Default)]
pub(crate) struct McpStatusInput<'a> {
    /// Optional alias filter. `None` returns all registered accounts.
    pub account: Option<&'a str>,
}

/// Response envelope.
#[derive(Debug, Serialize)]
pub(crate) struct McpStatusOutput {
    pub schema_version: u32,
    pub version: String,
    pub accounts: Vec<AccountStatus>,
}

/// Per-account status row.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct AccountStatus {
    pub alias: String,
    pub scopes_granted: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub expires_in_seconds: i64,
    pub last_refresh_at: Option<DateTime<Utc>>,
    pub auth_state: AuthState,
}

/// Public schema-version of the response. Bump on breaking shape changes
/// so consumers (Claude Desktop, other LLM clients) can detect mismatch.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Build the `mcp_status` response from a slice of snapshots. Pure; tests
/// pass synthetic snapshots without standing up a `TokenManager`.
pub(crate) fn build_status(snapshots: &[AccountSnapshot], now: DateTime<Utc>) -> McpStatusOutput {
    let accounts = snapshots
        .iter()
        .map(|s| AccountStatus {
            alias: s.alias.clone(),
            scopes_granted: s.scopes.clone(),
            expires_at: s.expires_at,
            expires_in_seconds: (s.expires_at - now).num_seconds(),
            last_refresh_at: s.last_refresh_at,
            auth_state: auth_state_from(s, now),
        })
        .collect();

    McpStatusOutput {
        schema_version: SCHEMA_VERSION,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        accounts,
    }
}

/// Map a [`AccountSnapshot`] + `now` to one of the four `AuthState`
/// values. `AuthRequired` wins over `Expired` (the cooldown is actionable
/// info even if the token also happens to be expired).
pub(crate) fn auth_state_from(s: &AccountSnapshot, now: DateTime<Utc>) -> AuthState {
    if let Some(until) = s.failed_until {
        if now < until {
            return AuthState::AuthRequired;
        }
    }
    if s.consecutive_failures > 0 && s.failed_until.is_none() {
        // Cool-off cleared but failures still recorded → recent transient
        // failure; treat as `Expiring` so the next call refreshes. Distinct
        // from `AuthRequired` since the daemon will self-recover.
        return AuthState::Expiring;
    }
    let remaining = (s.expires_at - now).num_seconds();
    if remaining <= 0 {
        AuthState::Expired
    } else if remaining < AUTH_OK_BUFFER {
        AuthState::Expiring
    } else {
        AuthState::Ok
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone as _};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 17, 12, 0, 0).unwrap()
    }

    fn snap(alias: &str, exp_secs_from_now: i64) -> AccountSnapshot {
        AccountSnapshot {
            alias: alias.into(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.modify".into()],
            expires_at: now() + Duration::seconds(exp_secs_from_now),
            last_refresh_at: Some(now() - Duration::seconds(3600 - exp_secs_from_now)),
            failed_until: None,
            consecutive_failures: 0,
        }
    }

    // ── AuthState derivation (every branch) ─────────────────────────────────

    #[test]
    fn ok_when_expiry_well_in_future() {
        let s = snap("work", 1_800);
        assert_eq!(auth_state_from(&s, now()), AuthState::Ok);
    }

    #[test]
    fn expiring_when_inside_buffer() {
        let s = snap("work", 30);
        assert_eq!(auth_state_from(&s, now()), AuthState::Expiring);
    }

    #[test]
    fn expired_when_past_expires_at() {
        let s = snap("work", -1);
        assert_eq!(auth_state_from(&s, now()), AuthState::Expired);
    }

    #[test]
    fn auth_required_when_in_cooldown() {
        let mut s = snap("work", 1_800);
        s.failed_until = Some(now() + Duration::seconds(30));
        s.consecutive_failures = 1;
        assert_eq!(auth_state_from(&s, now()), AuthState::AuthRequired);
    }

    #[test]
    fn cleared_cooldown_with_residual_failures_is_expiring() {
        let mut s = snap("work", 1_800);
        s.failed_until = None;
        s.consecutive_failures = 1;
        assert_eq!(auth_state_from(&s, now()), AuthState::Expiring);
    }

    // ── build_status shape ──────────────────────────────────────────────────

    #[test]
    fn build_status_includes_schema_version_and_package_version() {
        let out = build_status(&[snap("work", 1_800)], now());
        assert_eq!(out.schema_version, SCHEMA_VERSION);
        assert_eq!(out.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(out.accounts.len(), 1);
        assert_eq!(out.accounts[0].alias, "work");
        assert_eq!(out.accounts[0].auth_state, AuthState::Ok);
        assert_eq!(out.accounts[0].expires_in_seconds, 1_800);
        assert!(out.accounts[0].last_refresh_at.is_some());
    }

    #[test]
    fn build_status_with_empty_snapshots_returns_empty_accounts() {
        let out = build_status(&[], now());
        assert!(out.accounts.is_empty());
        assert_eq!(out.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn build_status_preserves_input_order() {
        let snaps = vec![snap("a", 1_800), snap("b", 1_800), snap("c", 1_800)];
        let out = build_status(&snaps, now());
        let aliases: Vec<_> = out.accounts.iter().map(|a| a.alias.as_str()).collect();
        assert_eq!(aliases, vec!["a", "b", "c"]);
    }

    #[test]
    fn auth_state_serializes_as_snake_case() {
        let snaps = vec![snap("ok", 1_800), {
            let mut s = snap("required", 1_800);
            s.failed_until = Some(now() + Duration::seconds(30));
            s.consecutive_failures = 1;
            s
        }];
        let out = build_status(&snaps, now());
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["accounts"][0]["auth_state"], "ok");
        assert_eq!(json["accounts"][1]["auth_state"], "auth_required");
    }

    #[test]
    fn expires_in_seconds_can_be_negative() {
        let out = build_status(&[snap("work", -3600)], now());
        assert_eq!(out.accounts[0].expires_in_seconds, -3600);
        assert_eq!(out.accounts[0].auth_state, AuthState::Expired);
    }
}
