//! Per-GCP-project daily-quota tracker (issue #30, v0.3 bridge until the
//! `SQLite` cache from ADR-0009 lands).
//!
//! Sits **alongside** the per-account per-minute limiter from
//! [`crate::rate_limit`]; both are AND-gated by [`crate::gmail::client`] —
//! whichever exhausts first returns `Error::RateLimited`. The per-account
//! limiter protects burst behavior; this one protects sustained behavior
//! across an operator's account set sharing one GCP project.
//!
//! Gmail's quota window resets at 00:00 UTC. The counter is in memory only;
//! a daemon restart re-fills the budget (acceptable per ADR-0000 — caching
//! is the long-term fix).

#![allow(clippy::significant_drop_tightening)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, NaiveTime, TimeZone, Utc};

use crate::error::Error;

/// Default per-GCP-project daily quota — Gmail API's documented limit.
/// Override via `[services.gmail.rate_limit].per_project_daily_units`.
pub(crate) const GMAIL_DEFAULT_PROJECT_DAILY_UNITS: u64 = 1_200_000;

/// One project's daily counter. Resets on first call after UTC midnight.
#[derive(Debug)]
struct DailyCounter {
    capacity: u64,
    used: u64,
    day_started: DateTime<Utc>,
}

impl DailyCounter {
    const fn new(capacity: u64, day_started: DateTime<Utc>) -> Self {
        Self {
            capacity,
            used: 0,
            day_started,
        }
    }

    /// Try to consume `cost` units, resetting at UTC midnight first. On
    /// failure returns the wall-clock seconds until the next UTC midnight.
    fn try_consume(&mut self, cost: u64, now: DateTime<Utc>) -> Result<(), u64> {
        if now.date_naive() > self.day_started.date_naive() {
            self.used = 0;
            self.day_started = now;
        }
        if self.used + cost <= self.capacity {
            self.used = self.used.saturating_add(cost);
            return Ok(());
        }
        Err(seconds_to_next_midnight_utc(now))
    }

    #[cfg(test)]
    const fn remaining(&self) -> u64 {
        self.capacity.saturating_sub(self.used)
    }
}

/// Per-GCP-project registry. Lazy-creates a [`DailyCounter`] on first call
/// for a project, just like [`crate::rate_limit::KeyedRateLimiter`].
#[derive(Debug)]
pub(crate) struct ProjectQuotaRegistry {
    capacity: u64,
    counters: Mutex<HashMap<String, DailyCounter>>,
}

impl Default for ProjectQuotaRegistry {
    fn default() -> Self {
        Self::new(GMAIL_DEFAULT_PROJECT_DAILY_UNITS)
    }
}

impl ProjectQuotaRegistry {
    pub(crate) fn new(capacity: u64) -> Self {
        Self {
            capacity,
            counters: Mutex::new(HashMap::new()),
        }
    }

    /// Consume `cost` units against `project_id`'s daily budget. Returns
    /// `Error::RateLimited` with `retry_after = seconds-to-next-UTC-midnight`
    /// on exhaustion. The `account` field carries the supplied alias for
    /// diagnostic clarity.
    pub(crate) fn try_acquire(
        &self,
        project_id: &str,
        account: &str,
        cost: u32,
    ) -> Result<(), Error> {
        self.try_acquire_at(project_id, account, cost, Utc::now())
    }

    fn try_acquire_at(
        &self,
        project_id: &str,
        account: &str,
        cost: u32,
        now: DateTime<Utc>,
    ) -> Result<(), Error> {
        let mut counters = self.counters.lock().map_err(|e| Error::Internal {
            context: "project_quota mutex poisoned".to_owned(),
            source: anyhow::anyhow!("{e}"),
        })?;
        let counter = counters
            .entry(project_id.to_owned())
            .or_insert_with(|| DailyCounter::new(self.capacity, now));
        counter
            .try_consume(u64::from(cost), now)
            .map_err(|secs| Error::RateLimited {
                account: account.to_owned(),
                retry_after: Duration::from_secs(secs),
            })
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn remaining(&self, project_id: &str) -> u64 {
        let counters = self.counters.lock().expect("lock");
        counters
            .get(project_id)
            .map_or(self.capacity, DailyCounter::remaining)
    }
}

/// Extract the GCP project number from a Google OAuth `client_id`. The
/// canonical shape is `{project_number}-{hash}.apps.googleusercontent.com`;
/// the leading digit run is the GCP project number. Returns `None` for
/// any `client_id` that doesn't fit this pattern (e.g. service-account JWTs).
pub(crate) fn project_id_from_client_id(client_id: &str) -> Option<String> {
    let dash = client_id.find('-')?;
    let prefix = &client_id[..dash];
    if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
        Some(prefix.to_owned())
    } else {
        None
    }
}

/// Seconds from `now` to the next UTC midnight (00:00:00). Always ≥ 1 so
/// the caller's exponential-retry doesn't pile up at the boundary.
fn seconds_to_next_midnight_utc(now: DateTime<Utc>) -> u64 {
    let today = now.date_naive();
    let tomorrow = today.succ_opt().unwrap_or(today);
    let midnight = Utc.from_utc_datetime(&tomorrow.and_time(NaiveTime::MIN));
    let delta = (midnight - now).num_seconds();
    u64::try_from(delta.max(1)).unwrap_or(1)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    // ── project_id extraction ───────────────────────────────────────────────

    #[test]
    fn extracts_project_number_from_canonical_client_id() {
        assert_eq!(
            project_id_from_client_id("123456789-abc.apps.googleusercontent.com").as_deref(),
            Some("123456789")
        );
    }

    #[test]
    fn rejects_non_numeric_prefix() {
        assert_eq!(project_id_from_client_id("abc-123.apps"), None);
    }

    #[test]
    fn rejects_no_dash() {
        assert_eq!(project_id_from_client_id("nodash"), None);
        assert_eq!(project_id_from_client_id(""), None);
    }

    // ── DailyCounter math ───────────────────────────────────────────────────

    #[test]
    fn fresh_counter_at_full_capacity() {
        let c = DailyCounter::new(100, at(2026, 5, 17, 10, 0, 0));
        assert_eq!(c.remaining(), 100);
    }

    #[test]
    fn consume_decrements_remaining() {
        let mut c = DailyCounter::new(100, at(2026, 5, 17, 10, 0, 0));
        c.try_consume(30, at(2026, 5, 17, 10, 30, 0)).expect("ok");
        assert_eq!(c.remaining(), 70);
    }

    #[test]
    fn exhaustion_returns_seconds_to_next_midnight() {
        let day = at(2026, 5, 17, 23, 59, 30);
        let mut c = DailyCounter::new(10, day);
        c.try_consume(10, day).expect("drain");
        let err = c.try_consume(1, day).expect_err("rate-limited");
        // 30 seconds until 2026-05-18T00:00:00Z.
        assert_eq!(err, 30);
    }

    #[test]
    fn counter_resets_after_utc_midnight() {
        let mut c = DailyCounter::new(10, at(2026, 5, 17, 23, 0, 0));
        c.try_consume(10, at(2026, 5, 17, 23, 30, 0))
            .expect("drain");
        // Crossing midnight resets.
        c.try_consume(5, at(2026, 5, 18, 0, 1, 0))
            .expect("post-midnight ok");
        assert_eq!(c.remaining(), 5);
    }

    // ── ProjectQuotaRegistry: multi-project / multi-account ─────────────────

    #[test]
    fn multi_account_same_project_shares_budget() {
        let reg = ProjectQuotaRegistry::new(100);
        let now = at(2026, 5, 17, 12, 0, 0);
        reg.try_acquire_at("proj-1", "work", 60, now).expect("ok");
        // Different account, SAME project — must charge the same bucket.
        reg.try_acquire_at("proj-1", "personal", 30, now)
            .expect("ok");
        assert_eq!(reg.remaining("proj-1"), 10);
    }

    #[test]
    fn multi_project_isolates_budgets() {
        let reg = ProjectQuotaRegistry::new(100);
        let now = at(2026, 5, 17, 12, 0, 0);
        reg.try_acquire_at("proj-1", "work", 100, now).expect("ok");
        // Different project — its own bucket, full capacity.
        reg.try_acquire_at("proj-2", "personal", 100, now)
            .expect("ok");
        assert_eq!(reg.remaining("proj-1"), 0);
        assert_eq!(reg.remaining("proj-2"), 0);
    }

    #[test]
    fn registry_returns_rate_limited_on_exhaustion() {
        let reg = ProjectQuotaRegistry::new(100);
        let now = at(2026, 5, 17, 23, 0, 0); // 1h to midnight
        reg.try_acquire_at("proj-1", "work", 100, now)
            .expect("drain");
        let err = reg
            .try_acquire_at("proj-1", "work", 1, now)
            .expect_err("rate-limited");
        match err {
            Error::RateLimited {
                account,
                retry_after,
            } => {
                assert_eq!(account, "work");
                assert_eq!(retry_after, Duration::from_hours(1));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    // ── seconds_to_next_midnight_utc bounds ─────────────────────────────────

    #[test]
    fn seconds_to_midnight_at_one_second_before() {
        let now = at(2026, 5, 17, 23, 59, 59);
        assert_eq!(seconds_to_next_midnight_utc(now), 1);
    }

    #[test]
    fn seconds_to_midnight_at_noon() {
        let now = at(2026, 5, 17, 12, 0, 0);
        // 12 hours = 43200 seconds.
        assert_eq!(seconds_to_next_midnight_utc(now), 43_200);
    }
}
