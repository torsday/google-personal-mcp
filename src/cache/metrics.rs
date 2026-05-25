//! Hit/miss counters for the on-demand cache (Phase 2 of [ADR-0009]).
//!
//! Two coarse `AtomicU64`s — total hits and total misses — plus a
//! per-event `tracing::debug!` carrying `metric.name`, `account`, and
//! `kind` labels. The atomics are observable from tests; the structured
//! events are the seam the future Prometheus exporter ([#75]) consumes.
//!
//! Counter names follow the ADR-0008 convention:
//!
//! - `gmcp_cache_hits_total{account, kind}`
//! - `gmcp_cache_misses_total{account, kind}`
//! - `gmcp_cache_write_discarded_total{account, kind}` (Phase 4 — [#81])
//!
//! where `kind` is one of `"thread"`, `"thread_metadata"`, `"query"`.
//!
//! [ADR-0009]: ../../docs/adr/0009-caching-with-sqlite-and-history-api.md
//! [#75]: https://github.com/torsday/google-personal-mcp/issues/75
//! [#81]: https://github.com/torsday/google-personal-mcp/issues/81

use std::sync::atomic::{AtomicU64, Ordering};

/// In-process counters incremented on every cache lookup outcome.
///
/// Stored as a field of [`super::Cache`] so any holder of `Arc<Cache>`
/// — chiefly [`crate::gmail::service::GmailService`] — can record hits
/// and misses without threading an extra handle.
#[derive(Debug, Default)]
pub(crate) struct CacheMetrics {
    hits: AtomicU64,
    misses: AtomicU64,
    write_discarded: AtomicU64,
}

impl CacheMetrics {
    /// Record a cache hit and emit the corresponding tracing event.
    pub(crate) fn record_hit(&self, account: &str, kind: &str) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            metric.name = "gmcp_cache_hits_total",
            account = account,
            kind = kind,
            "cache hit",
        );
    }

    /// Record a cache miss and emit the corresponding tracing event.
    pub(crate) fn record_miss(&self, account: &str, kind: &str) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            metric.name = "gmcp_cache_misses_total",
            account = account,
            kind = kind,
            "cache miss",
        );
    }

    /// Record a discarded write — Phase 4 (#81) race-prevention. The
    /// upstream API result was correct at the moment it left Gmail, but
    /// the cache's history watermark advanced past the fetch snapshot
    /// before the write landed; persisting would serve data older than
    /// the cache already knew about. Should be near zero in steady
    /// state — a sustained nonzero rate suggests background sync is
    /// outracing read-path API calls.
    pub(crate) fn record_write_discarded(&self, account: &str, kind: &str) {
        self.write_discarded.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            metric.name = "gmcp_cache_write_discarded_total",
            account = account,
            kind = kind,
            "cache write discarded (stale fetched_at_history_id)",
        );
        metrics::counter!(
            crate::observability::metrics::names::CACHE_WRITE_DISCARDED_TOTAL,
            "account" => account.to_owned(),
            "kind" => kind.to_owned(),
        )
        .increment(1);
    }

    /// Total hits since process start. Test-only; the Prometheus exporter
    /// will read the underlying atomic directly when it lands.
    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Total misses since process start. Test-only.
    #[cfg(test)]
    pub(crate) fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Total discarded writes since process start. Test-only.
    #[cfg(test)]
    pub(crate) fn write_discarded(&self) -> u64 {
        self.write_discarded.load(Ordering::Relaxed)
    }

    /// Lifetime cumulative hit counter — used by `cache_status` (#83).
    pub(crate) fn hits_lifetime(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Lifetime cumulative miss counter — used by `cache_status` (#83).
    pub(crate) fn misses_lifetime(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Lifetime cumulative write-discarded counter — used by `cache_status` (#83).
    pub(crate) fn write_discarded_lifetime(&self) -> u64 {
        self.write_discarded.load(Ordering::Relaxed)
    }
}

/// Coarse process-lifetime snapshot. Built by [`super::Cache::metrics_snapshot`]
/// and surfaced by the operator-facing `cache_status` tool (#83). Per-account
/// and time-windowed shapes ship with the Prometheus exporter (#75).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheMetricsSnapshot {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) write_discarded: u64,
}

impl CacheMetricsSnapshot {
    /// Cumulative hit rate `hits / (hits + misses)`. Returns `None` when
    /// no lookups have happened yet — distinct from `Some(0.0)`, which
    /// means lookups happened and all missed.
    pub(crate) fn hit_rate(&self) -> Option<f64> {
        let total = self.hits.checked_add(self.misses)?;
        if total == 0 {
            return None;
        }
        // Loss of precision at total > 2^53 is acceptable for a rate; the
        // counter would have to overflow within a single process for that
        // to matter.
        #[allow(clippy::cast_precision_loss)]
        Some(self.hits as f64 / total as f64)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn record_hit_bumps_only_hits() {
        let m = CacheMetrics::default();
        m.record_hit("work", "thread");
        m.record_hit("work", "thread");
        assert_eq!(m.hits(), 2);
        assert_eq!(m.misses(), 0);
    }

    #[test]
    fn record_miss_bumps_only_misses() {
        let m = CacheMetrics::default();
        m.record_miss("work", "query");
        assert_eq!(m.hits(), 0);
        assert_eq!(m.misses(), 1);
    }

    #[test]
    fn record_write_discarded_bumps_only_discarded() {
        let m = CacheMetrics::default();
        m.record_write_discarded("work", "query");
        assert_eq!(m.hits(), 0);
        assert_eq!(m.misses(), 0);
        assert_eq!(m.write_discarded(), 1);
    }
}
