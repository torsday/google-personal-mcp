//! Per-source-IP failed-auth throttle per
//! [ADR-0020 §"Failed-auth treatment"](../../../docs/adr/0020-http-transport-authentication.md).
//!
//! Token bucket per source IP. Each IP starts with [`ThrottleConfig::burst`]
//! tokens; tokens refill at [`ThrottleConfig::rate_per_sec`]/sec, capped at
//! `burst`. Every failed `Authorization` check consumes one token; when no
//! tokens remain, subsequent requests from that IP get HTTP 429 +
//! `Retry-After: 60` **without** running the bearer check (no extra work for
//! the attacker, per the ADR).
//!
//! ## Source-IP, not X-Forwarded-For
//!
//! The IP is the **peer address from the HTTP listener**, not anything from
//! `X-Forwarded-For`. nginx (or whatever fronts the daemon) is the trust
//! boundary; operators who want XFF trust can add it at the proxy. Without
//! that boundary, an attacker could spoof XFF and bypass per-IP throttling.
//!
//! ## In-memory only
//!
//! Daemon restart resets all bucket state. The throttle is a defence-in-depth
//! brake against brute-force from a single IP — not a durable allowlist. A
//! background sweeper ages out idle buckets to keep memory bounded under
//! drive-by scanning.
//!
//! ## Cardinality note
//!
//! The Prometheus counter `gmcp_http_auth_failures_total` is labelled by
//! `source_ip` per the ADR-0020 spec. This is high-cardinality and the
//! downstream alertmanager rule (ADR-0008) is expected to aggregate. If the
//! cardinality becomes a problem for storage, replace with a label-free
//! counter plus structured logs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Operator-facing config; deserialized from `[http.auth.throttle]` in
/// `config.toml`. Defaults match the ADR-0020 reference values.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThrottleConfig {
    /// Token refill rate. ADR default: 1 token/sec.
    #[serde(default = "default_rate_per_sec")]
    pub(crate) rate_per_sec: u32,
    /// Maximum burst — the initial bucket fill, and the cap on refill.
    /// ADR default: 10 tokens. Per-IP — not aggregate.
    #[serde(default = "default_burst")]
    pub(crate) burst: u32,
    /// Idle window. Buckets with no failed-auth activity for this many
    /// seconds get swept out of the map by the background task. Also
    /// used as the `Retry-After` value in the 429 response. ADR
    /// default: 60.
    #[serde(default = "default_window_secs")]
    pub(crate) window_secs: u64,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            rate_per_sec: default_rate_per_sec(),
            burst: default_burst(),
            window_secs: default_window_secs(),
        }
    }
}

const fn default_rate_per_sec() -> u32 {
    1
}
const fn default_burst() -> u32 {
    10
}
const fn default_window_secs() -> u64 {
    60
}

/// Per-IP token-bucket state. Stored inside the [`Throttle`]'s dashmap.
/// `f64` tokens so the refill math can express fractional tokens between
/// integer-second ticks (consumed values are still whole tokens).
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
    /// Last time any code touched this bucket — used by the sweeper to
    /// age out idle entries. Updated on both successful and consumed
    /// failure attempts.
    last_touched: Instant,
}

/// Outcome of [`Throttle::check_and_consume`]. The caller (HTTP
/// middleware) routes the 429 path on `Throttled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThrottleOutcome {
    /// Caller may proceed to the bearer-token check.
    Allowed,
    /// Burst exhausted for this IP — respond `429` with
    /// `Retry-After: window_secs` immediately, do **not** run the
    /// bearer check (ADR-0020: no extra work for the attacker).
    Throttled,
}

/// Per-IP failed-auth throttle.
///
/// `Clone` is cheap (every field is `Arc`-wrapped). The dispatcher
/// stores one `Arc<Throttle>` and hands it to the middleware via axum
/// state.
#[derive(Debug, Clone)]
pub(crate) struct Throttle {
    config: ThrottleConfig,
    buckets: Arc<Mutex<HashMap<IpAddr, Bucket>>>,
}

impl Throttle {
    /// Build a throttle with `config`. No background sweeper is spawned;
    /// callers that want one invoke [`Self::spawn_sweeper`] after wiring
    /// `Throttle` into the router state.
    pub(crate) fn new(config: ThrottleConfig) -> Self {
        Self {
            config,
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Snapshot of the configured `Retry-After` window. Exposed so the
    /// middleware can write the header value without a second config
    /// indirection.
    pub(crate) const fn retry_after_secs(&self) -> u64 {
        self.config.window_secs
    }

    /// Probe the bucket for `ip`. **Does not** consume a token — call
    /// this before running the bearer check; on failure, follow up with
    /// [`Self::record_failure`].
    ///
    /// `now` is parameterized so tests can drive deterministic timing;
    /// production callers pass `Instant::now()`.
    pub(crate) fn check(&self, ip: IpAddr, now: Instant) -> ThrottleOutcome {
        // PoisonError fall-through: if a panic earlier corrupted the
        // map, fall closed by extracting the guard via `into_inner()`
        // rather than letting unwrap panic. In practice a poisoned
        // mutex here would indicate a bug — log nothing, treat as
        // recoverable.
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = buckets.entry(ip).or_insert_with(|| Bucket {
            tokens: f64::from(self.config.burst),
            last_refill: now,
            last_touched: now,
        });
        refill(entry, &self.config, now);
        let tokens = entry.tokens;
        drop(buckets);
        if tokens >= 1.0 {
            ThrottleOutcome::Allowed
        } else {
            ThrottleOutcome::Throttled
        }
    }

    /// Record a failed auth attempt from `ip`. Consumes one token. If
    /// the bucket is already empty this is a no-op for accounting
    /// (saturating subtraction) but still updates `last_touched` so the
    /// sweeper keeps the entry alive while the attacker is hammering.
    pub(crate) fn record_failure(&self, ip: IpAddr, now: Instant) {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = buckets.entry(ip).or_insert_with(|| Bucket {
            tokens: f64::from(self.config.burst),
            last_refill: now,
            last_touched: now,
        });
        refill(entry, &self.config, now);
        entry.tokens = (entry.tokens - 1.0).max(0.0);
        entry.last_touched = now;
        drop(buckets);
    }

    /// Remove buckets idle for longer than `window_secs * 2`. Run
    /// periodically by [`Self::spawn_sweeper`]; also exposed for tests.
    pub(crate) fn sweep_idle(&self, now: Instant) {
        let cutoff = Duration::from_secs(self.config.window_secs.saturating_mul(2));
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buckets.retain(|_, b| now.duration_since(b.last_touched) < cutoff);
    }

    /// Snapshot of the live bucket count. Test-only.
    #[cfg(test)]
    pub(crate) fn bucket_count(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Spawn the background sweeper. Drops idle entries every
    /// `window_secs` seconds. Returns `None` if the config has
    /// `window_secs = 0` (sweeper disabled — operator opt-out).
    pub(crate) fn spawn_sweeper(self: &Arc<Self>) -> Option<SweeperHandle> {
        if self.config.window_secs == 0 {
            return None;
        }
        let throttle = Arc::clone(self);
        let interval = Duration::from_secs(throttle.config.window_secs);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                throttle.sweep_idle(Instant::now());
            }
        });
        Some(SweeperHandle { task: Some(handle) })
    }
}

/// Refill `bucket` based on `now - last_refill`. Caps at
/// `config.burst`. Saturating math protects against extreme clock jumps.
fn refill(bucket: &mut Bucket, config: &ThrottleConfig, now: Instant) {
    let elapsed = now.duration_since(bucket.last_refill);
    if elapsed.is_zero() {
        return;
    }
    // f64 is fine — the bucket is small (capped at `burst`) and the rate
    // is on the order of 1/sec. Loss of precision past 2^53 doesn't
    // matter for this use case.
    #[allow(clippy::cast_precision_loss)]
    let refill = elapsed.as_secs_f64() * f64::from(config.rate_per_sec);
    bucket.tokens = (bucket.tokens + refill).min(f64::from(config.burst));
    bucket.last_refill = now;
}

/// Abort-on-drop handle for the background sweeper. Same shape as
/// [`crate::cache::eviction::EvictionHandle`].
#[must_use = "the sweeper aborts when this handle is dropped"]
pub(crate) struct SweeperHandle {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SweeperHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            tracing::debug!("aborting http-auth throttle sweeper");
            t.abort();
        }
    }
}

impl SweeperHandle {
    #[cfg(test)]
    pub(crate) async fn stop(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = t.await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    fn ip4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn default_throttle() -> Throttle {
        Throttle::new(ThrottleConfig::default())
    }

    /// Burst exhaustion: 10 failures from one IP must leave the 11th
    /// attempt throttled.
    #[test]
    fn burst_of_ten_then_throttled() {
        let t = default_throttle();
        let ip = ip4(192, 0, 2, 1);
        let now = Instant::now();
        for i in 0..10 {
            assert_eq!(t.check(ip, now), ThrottleOutcome::Allowed, "i={i}");
            t.record_failure(ip, now);
        }
        // 11th attempt: bucket should now be empty.
        assert_eq!(t.check(ip, now), ThrottleOutcome::Throttled);
    }

    /// Sliding-window expiry: after exhaustion, waiting `burst / rate`
    /// seconds refills the bucket. Drive deterministically via
    /// `now + Duration`.
    #[test]
    fn sliding_window_refills_at_configured_rate() {
        let t = default_throttle(); // rate 1/sec, burst 10
        let ip = ip4(192, 0, 2, 2);
        let start = Instant::now();
        for _ in 0..10 {
            t.record_failure(ip, start);
        }
        // Right after exhaustion: throttled.
        assert_eq!(t.check(ip, start), ThrottleOutcome::Throttled);
        // 1 second later: 1 token refilled — one more attempt allowed.
        let after_one = start + Duration::from_secs(1);
        assert_eq!(t.check(ip, after_one), ThrottleOutcome::Allowed);
        t.record_failure(ip, after_one);
        // 6s total elapsed = +5s after the after_one refill. At rate
        // 1/sec, that's 5 tokens credited. Bucket = 0 (after the
        // after_one consume) + 5 = 5 tokens. Five consumes before
        // throttle.
        let after_six = start + Duration::from_secs(6);
        for _ in 0..5 {
            assert_eq!(t.check(ip, after_six), ThrottleOutcome::Allowed);
            t.record_failure(ip, after_six);
        }
        // Bucket exhausted at the post-burst rate.
        assert_eq!(t.check(ip, after_six), ThrottleOutcome::Throttled);
    }

    /// Per-IP isolation: one IP burning through its burst does not
    /// affect a different IP's bucket.
    #[test]
    fn per_ip_isolation_attacker_does_not_throttle_others() {
        let t = default_throttle();
        let attacker = ip4(192, 0, 2, 3);
        let bystander = ip4(192, 0, 2, 4);
        let now = Instant::now();

        for _ in 0..15 {
            t.record_failure(attacker, now);
        }
        assert_eq!(t.check(attacker, now), ThrottleOutcome::Throttled);

        // Bystander has never failed; still has a full bucket.
        for _ in 0..10 {
            assert_eq!(t.check(bystander, now), ThrottleOutcome::Allowed);
            t.record_failure(bystander, now);
        }
    }

    /// IPv6 sources route through the same bucket map.
    #[test]
    fn ipv6_sources_are_throttled_independently_of_ipv4() {
        let t = default_throttle();
        let v4 = ip4(192, 0, 2, 5);
        let v6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let now = Instant::now();

        for _ in 0..10 {
            t.record_failure(v6, now);
        }
        assert_eq!(t.check(v6, now), ThrottleOutcome::Throttled);
        assert_eq!(t.check(v4, now), ThrottleOutcome::Allowed);
    }

    /// `sweep_idle` removes buckets idle past `window_secs * 2`; active
    /// (recently-failed) buckets survive.
    #[test]
    fn sweep_removes_idle_buckets_keeps_active_ones() {
        let t = default_throttle(); // window_secs = 60
        let stale = ip4(192, 0, 2, 6);
        let fresh = ip4(192, 0, 2, 7);
        let t0 = Instant::now();
        t.record_failure(stale, t0);
        t.record_failure(fresh, t0);
        assert_eq!(t.bucket_count(), 2);

        // 2x window + 1s past — stale entry exceeds cutoff, fresh
        // entry sees a touch right before sweep.
        let later = t0 + Duration::from_secs(121);
        t.record_failure(fresh, later);
        t.sweep_idle(later);

        assert_eq!(t.bucket_count(), 1);
        // Fresh still has a bucket (must remain active for the throttle
        // to keep brake-pressure on an ongoing attack).
        assert_eq!(t.check(fresh, later), ThrottleOutcome::Allowed);
    }

    /// Bucket refill is bounded at `burst` — a long idle period doesn't
    /// let the attacker stockpile tokens beyond the cap.
    #[test]
    fn refill_caps_at_burst() {
        let t = default_throttle();
        let ip = ip4(192, 0, 2, 8);
        let t0 = Instant::now();
        t.record_failure(ip, t0);
        // One hour later: refill would theoretically credit 3600
        // tokens, but the cap is 10.
        let far_future = t0 + Duration::from_hours(1);
        for _ in 0..10 {
            assert_eq!(t.check(ip, far_future), ThrottleOutcome::Allowed);
            t.record_failure(ip, far_future);
        }
        assert_eq!(t.check(ip, far_future), ThrottleOutcome::Throttled);
    }

    /// `retry_after_secs` surfaces the configured window for the
    /// middleware's `Retry-After` header.
    #[test]
    fn retry_after_secs_matches_config() {
        let t = Throttle::new(ThrottleConfig {
            rate_per_sec: 2,
            burst: 5,
            window_secs: 45,
        });
        assert_eq!(t.retry_after_secs(), 45);
    }

    /// Sweeper handle aborts the task on drop; verifies the
    /// `must_use` contract.
    #[tokio::test]
    async fn sweeper_handle_drops_and_aborts() {
        let t = Arc::new(default_throttle());
        let handle = t.spawn_sweeper().expect("Some");
        // Drop explicitly — the abort should land without panicking.
        handle.stop().await;
    }
}
