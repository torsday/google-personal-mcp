#![allow(clippy::significant_drop_tightening)]
//! Per-account token-bucket rate limiter against Gmail's
//! 6,000-quota-units-per-user-per-minute cap.
//!
//! See [issue #25] and the quota cheat-sheet in `CLAUDE.md`. The limiter is
//! **non-blocking**: when a request would exceed the budget,
//! [`KeyedRateLimiter::try_acquire`] returns `Error::RateLimited` immediately
//! with a `retry_after_secs` hint computed from the bucket's refill rate.
//! Callers — the MCP tool layer — propagate that to the model so the host
//! LLM can choose to back off rather than the daemon stalling on its own.
//!
//! Per AC, state is process-local; a daemon restart re-fills every bucket
//! to capacity (acceptable for v0.2). Per-restart bucket growth is bounded
//! by the number of distinct account aliases (handful at most).
//!
//! [issue #25]: https://github.com/torsday/google-personal-mcp/issues/25

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::Error;

/// Gmail's per-user-per-minute quota cap.
pub(crate) const GMAIL_CAPACITY: u32 = 6_000;
/// Refill rate in units per second. `6,000 / 60 = 100`.
pub(crate) const GMAIL_REFILL_PER_SEC: u32 = 100;

/// One bucket. Owns no concurrency primitive itself — wrapped in a mutex by
/// [`KeyedRateLimiter`]. `tokens` is a fractional count in millionths so
/// sub-second refill at 100 units/s is exact.
#[derive(Debug)]
pub(crate) struct TokenBucket {
    capacity_micro: u64,
    refill_per_sec_micro: u64,
    tokens_micro: u64,
    last_refill: Instant,
}

impl TokenBucket {
    pub(crate) fn new(capacity: u32, refill_per_sec: u32) -> Self {
        Self::new_at(capacity, refill_per_sec, Instant::now())
    }

    /// Test-only constructor that takes an explicit `at` for deterministic
    /// refill math. Production code uses [`Self::new`].
    fn new_at(capacity: u32, refill_per_sec: u32, at: Instant) -> Self {
        let capacity_micro = u64::from(capacity).saturating_mul(1_000_000);
        Self {
            capacity_micro,
            refill_per_sec_micro: u64::from(refill_per_sec).saturating_mul(1_000_000),
            tokens_micro: capacity_micro,
            last_refill: at,
        }
    }

    /// Try to consume `cost` whole tokens. On success, returns `Ok(())`.
    /// On failure, returns `Err(retry_after_secs)` rounded **up**.
    fn try_consume(&mut self, cost: u32, now: Instant) -> Result<(), u64> {
        self.refill(now);
        let cost_micro = u64::from(cost).saturating_mul(1_000_000);
        if self.tokens_micro >= cost_micro {
            self.tokens_micro -= cost_micro;
            return Ok(());
        }
        let deficit = cost_micro - self.tokens_micro;
        // ceil(deficit / refill_per_sec_micro) in seconds.
        let secs = deficit.saturating_add(self.refill_per_sec_micro.saturating_sub(1))
            / self.refill_per_sec_micro.max(1);
        Err(secs.max(1))
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        if elapsed.is_zero() {
            return;
        }
        let elapsed_nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        // refill_per_sec_micro * (nanos / 1e9) — done as integer division
        // with a single intermediate that won't overflow for reasonable
        // intervals (years would be required).
        let added_micro = self
            .refill_per_sec_micro
            .saturating_mul(elapsed_nanos / 1_000)
            / 1_000_000;
        self.tokens_micro = self
            .tokens_micro
            .saturating_add(added_micro)
            .min(self.capacity_micro);
        self.last_refill = now;
    }

    /// Visible-for-tests current token count (whole units, rounded down).
    #[cfg(test)]
    fn available(&self) -> u32 {
        u32::try_from(self.tokens_micro / 1_000_000).unwrap_or(u32::MAX)
    }
}

/// Per-account rate limiter. Constructs buckets lazily on first
/// `try_acquire` so adding accounts at runtime is cheap.
#[derive(Debug)]
pub(crate) struct KeyedRateLimiter {
    capacity: u32,
    refill_per_sec: u32,
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl Default for KeyedRateLimiter {
    fn default() -> Self {
        Self::new(GMAIL_CAPACITY, GMAIL_REFILL_PER_SEC)
    }
}

impl KeyedRateLimiter {
    pub(crate) fn new(capacity: u32, refill_per_sec: u32) -> Self {
        Self {
            capacity,
            refill_per_sec,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Try to consume `cost` from `account`'s bucket. Lazily creates the
    /// bucket on first use. Returns `Error::RateLimited` immediately when the
    /// bucket lacks budget — no internal sleeping.
    pub(crate) fn try_acquire(&self, account: &str, cost: u32) -> Result<(), Error> {
        self.try_acquire_at(account, cost, Instant::now())
    }

    fn try_acquire_at(&self, account: &str, cost: u32, now: Instant) -> Result<(), Error> {
        let mut buckets = self.buckets.lock().map_err(|e| Error::Internal {
            context: "rate_limit mutex poisoned".to_owned(),
            source: anyhow::anyhow!("{e}"),
        })?;
        let bucket = buckets
            .entry(account.to_owned())
            .or_insert_with(|| TokenBucket::new(self.capacity, self.refill_per_sec));
        bucket
            .try_consume(cost, now)
            .map_err(|secs| Error::RateLimited {
                account: account.to_owned(),
                retry_after: Duration::from_secs(secs),
            })
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn available(&self, account: &str) -> u32 {
        let buckets = self.buckets.lock().expect("lock");
        buckets
            .get(account)
            .map_or(self.capacity, TokenBucket::available)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // ── TokenBucket: math (AC) ───────────────────────────────────────────────

    #[test]
    fn fresh_bucket_starts_at_capacity() {
        let b = TokenBucket::new(6000, 100);
        assert_eq!(b.available(), 6000);
    }

    #[test]
    fn consume_decrements() {
        let mut b = TokenBucket::new(6000, 100);
        b.try_consume(40, Instant::now()).expect("ok");
        assert_eq!(b.available(), 5960);
    }

    #[test]
    fn refill_adds_tokens_at_configured_rate() {
        let start = Instant::now();
        let mut b = TokenBucket::new_at(6000, 100, start);
        b.try_consume(6000, start).expect("drain ok");
        assert_eq!(b.available(), 0);
        // 5 seconds later → 500 tokens refilled.
        b.refill(start + Duration::from_secs(5));
        assert_eq!(b.available(), 500);
    }

    #[test]
    fn refill_caps_at_capacity() {
        let start = Instant::now();
        let mut b = TokenBucket::new_at(6000, 100, start);
        b.try_consume(10, start).expect("ok");
        b.refill(start + Duration::from_hours(1));
        assert_eq!(b.available(), 6000);
    }

    #[test]
    fn exhaustion_returns_retry_after_secs() {
        let start = Instant::now();
        let mut b = TokenBucket::new_at(100, 10, start);
        b.try_consume(100, start).expect("drain");
        let err = b.try_consume(50, start).expect_err("rate-limited");
        assert_eq!(err, 5);
    }

    #[test]
    fn exhaustion_retry_after_rounds_up_and_minimum_one() {
        let start = Instant::now();
        let mut b = TokenBucket::new_at(100, 10, start);
        b.try_consume(100, start).expect("drain");
        let err = b.try_consume(1, start).expect_err("rate-limited");
        assert_eq!(err, 1);
    }

    // ── KeyedRateLimiter (AC) ────────────────────────────────────────────────

    #[test]
    fn keyed_limiter_isolates_buckets_per_account() {
        let rl = KeyedRateLimiter::new(100, 10);
        rl.try_acquire("work", 80).expect("work ok");
        rl.try_acquire("personal", 80).expect("personal ok");
        assert_eq!(rl.available("work"), 20);
        assert_eq!(rl.available("personal"), 20);
    }

    #[test]
    fn keyed_limiter_returns_rate_limited_on_exhaustion() {
        let rl = KeyedRateLimiter::new(100, 10);
        rl.try_acquire("work", 100).expect("ok");
        let err = rl.try_acquire("work", 50).expect_err("must fail");
        match err {
            Error::RateLimited {
                account,
                retry_after,
            } => {
                assert_eq!(account, "work");
                assert_eq!(retry_after, Duration::from_secs(5));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn keyed_limiter_lazy_creates_bucket_on_first_use() {
        let rl = KeyedRateLimiter::new(100, 10);
        // `available` returns the limiter's capacity for unknown accounts.
        assert_eq!(rl.available("ghost"), 100);
        rl.try_acquire("ghost", 30).expect("ok");
        assert_eq!(rl.available("ghost"), 70);
    }

    // ── Quota table sanity check ─────────────────────────────────────────────

    #[test]
    fn gmail_capacity_constants_match_adr() {
        // CLAUDE.md cheat-sheet: 6,000 per-user-per-minute → 100/sec refill.
        assert_eq!(GMAIL_CAPACITY, 6_000);
        assert_eq!(GMAIL_REFILL_PER_SEC, 100);
        assert_eq!(
            GMAIL_CAPACITY / 60,
            GMAIL_REFILL_PER_SEC,
            "refill rate should equal capacity / 60"
        );
    }
}
