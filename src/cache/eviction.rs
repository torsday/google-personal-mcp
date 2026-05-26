//! `Evictor` — per-account LRU eviction background task.
//!
//! Phase 5 of [ADR-0009 §"TTLs and eviction"](../../../docs/adr/0009-caching-with-sqlite-and-history-api.md).
//! One [`Evictor`] instance is shared across the per-account background
//! tasks spawned by `lib::build_cache_wiring`. Each tick of [`Evictor::evict_account`]
//! is idempotent: a no-op when the DB file is under [`Evictor::max_size_bytes`],
//! a bounded delete-cycle plus a `VACUUM` when it isn't.
//!
//! **Why a background task and not eviction-on-write:** eviction takes the
//! write lock and runs `VACUUM`, which can be slow on a large file. Inline
//! eviction would add multi-second latency to whichever unlucky tool call
//! crossed the threshold. The background task amortizes the cost predictably;
//! the size cap is a soft limit, not hard (per ADR §"Why background task").
//!
//! Eviction order (ADR §"TTLs and eviction" lines 217-222):
//!
//! 1. Delete expired `query_cache` rows (`expires_at < now`). Cheap; dead first.
//! 2. Delete oldest `query_cache` rows in batches of 100 (`ORDER BY cached_at ASC`)
//!    until projected size ≤ 90 % of limit.
//! 3. Delete `messages` rows with `deleted_at IS NOT NULL` (`ORDER BY deleted_at ASC`).
//! 4. Delete oldest `messages` rows in batches of 100 (`ORDER BY fetched_at ASC`).
//!    `message_labels` rows cascade via the FK.
//! 5. `VACUUM` once at the end of the cycle to reclaim file pages.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio_rusqlite::Connection;

use crate::cache::Cache;
use crate::error::Error;

/// Soft-delete grace window per ADR-0019: rows tombstoned more than 7
/// days ago always have their bodies purged regardless of
/// `body_max_age_days` — there's no read path that wants the body of a
/// deleted message.
const SOFT_DELETE_GRACE_DAYS: i64 = 7;

const MS_PER_DAY: i64 = 86_400_000;

/// Batch size for the message and query-cache LRU steps. Per ADR-0009
/// §"TTLs and eviction" line 219.
const EVICTION_BATCH_SIZE: i64 = 100;

/// Eviction target as a fraction of `max_size_bytes`. Cycling stops when
/// projected size (`page_count` minus freelist) reaches 90 % of the cap, so
/// a steady-state write rate doesn't immediately re-trigger eviction.
const TARGET_FRACTION_NUM: i64 = 9;
const TARGET_FRACTION_DEN: i64 = 10;

/// One eviction cycle's tallies. Logged at INFO when any work happened;
/// returned to tests so they can assert on the exact deletion counts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct EvictionReport {
    /// `page_count * page_size` measured before the cycle.
    pub(crate) bytes_before: u64,
    /// `page_count * page_size` measured after `VACUUM`. When the cycle
    /// is a no-op (under threshold), equals `bytes_before`.
    pub(crate) bytes_after: u64,
    /// Rows whose bodies were nulled in the ADR-0019 body-purge phase
    /// because their `internal_date` was older than `body_max_age_days`.
    pub(crate) bodies_purged_age: usize,
    /// Rows whose bodies were nulled in the body-purge phase because
    /// they were tombstoned more than the 7-day grace window ago.
    pub(crate) bodies_purged_delete: usize,
    /// Rows removed in step 1 (expired `query_cache`).
    pub(crate) query_cache_expired: usize,
    /// Rows removed in step 2 (oldest `query_cache`).
    pub(crate) query_cache_oldest: usize,
    /// Rows removed in step 3 (tombstoned messages).
    pub(crate) messages_tombstoned: usize,
    /// Rows removed in step 4 (oldest messages, by `fetched_at`).
    pub(crate) messages_oldest: usize,
    /// Total wall-clock time the cycle took (including `VACUUM`).
    pub(crate) duration: Duration,
}

impl EvictionReport {
    /// `true` when the cycle actually evicted rows or shrank the file.
    /// A `false` here means the file was already under the cap and the
    /// cycle is a no-op — callers skip the INFO log on no-ops.
    pub(crate) const fn did_work(&self) -> bool {
        self.bodies_purged_age > 0
            || self.bodies_purged_delete > 0
            || self.query_cache_expired > 0
            || self.query_cache_oldest > 0
            || self.messages_tombstoned > 0
            || self.messages_oldest > 0
    }

    /// Bytes reclaimed by the cycle. Saturates at 0 if the file
    /// somehow grew (e.g. WAL checkpoint races); used only for logging.
    pub(crate) const fn bytes_evicted(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Per-account background-loop driver for the size-based LRU eviction
/// and the ADR-0019 body-purge phase.
///
/// One instance is built in `lib::build_cache_wiring` and shared across
/// per-account `tokio::task`s via `Arc`.
pub(crate) struct Evictor {
    cache: Arc<Cache>,
    interval: Duration,
    max_size_bytes: u64,
    /// ADR-0019 `body_max_age_days`. `0` disables the age-based
    /// body-purge phase; the soft-delete grace-window purge still
    /// runs because deleted bodies past 7 days are always safe to
    /// drop.
    body_max_age_days: u32,
    /// Minimum interval between body-purge phases per account. Per
    /// ADR-0019 the default is 24 h. The eviction tick is faster
    /// (5 min default) — this guards against churning the UPDATE on
    /// every tick.
    purge_interval: Duration,
    /// Per-account last-purge wallclock so the cycle can skip the
    /// body-purge phase between purge_interval-spaced ticks. Lost
    /// across daemon restarts, which is intentional: the next tick
    /// after a restart runs the purge unconditionally.
    last_body_purge: Mutex<HashMap<String, Instant>>,
}

impl Evictor {
    /// Build an evictor. `interval` controls the background cadence;
    /// `Duration::ZERO` indicates "no background task" (the daemon will
    /// still see eviction tools call into [`Self::evict_account`] later).
    /// `body_max_age_days = 0` and `purge_interval = Duration::ZERO`
    /// together disable the body-purge phase.
    pub(crate) fn new(
        cache: Arc<Cache>,
        interval: Duration,
        max_size_bytes: u64,
        body_max_age_days: u32,
        purge_interval: Duration,
    ) -> Self {
        Self {
            cache,
            interval,
            max_size_bytes,
            body_max_age_days,
            purge_interval,
            last_body_purge: Mutex::new(HashMap::new()),
        }
    }

    /// Background-loop cadence.
    pub(crate) const fn interval(&self) -> Duration {
        self.interval
    }

    /// Size cap above which a cycle starts evicting.
    pub(crate) const fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    /// Run exactly one eviction cycle for `account`. Returns the report
    /// regardless of whether the cycle did real work; `report.did_work()`
    /// discriminates. Unknown accounts return an empty `Ok(default)`.
    ///
    /// Phase order (per ADR-0019 §"Cache body age cap"):
    /// 1. Body-purge — runs at most once per `purge_interval`; nulls
    ///    body columns on rows past `body_max_age_days` or
    ///    soft-deleted > 7 days. Bumps `gmcp_cache_bodies_purged_*`.
    /// 2. LRU eviction — the existing ADR-0009 four-step cycle.
    /// 3. `VACUUM` — once at end if either phase did work.
    pub(crate) async fn evict_account(&self, account: &str) -> Result<EvictionReport, Error> {
        let started = Instant::now();

        // Phase 1: body-purge. Runs even when the account is "unknown
        // to cache" (no-ops via Cache::purge_old_bodies), so the
        // accounting is consistent regardless of the cache wiring state.
        let purge = if self.should_run_body_purge(account) {
            let report = self
                .cache
                .purge_old_bodies(account, self.age_cutoff_ms(), Self::delete_cutoff_ms())
                .await?;
            self.record_body_purge_run(account);
            report
        } else {
            crate::cache::queries::BodyPurgeReport::default()
        };

        // Phase 2 + 3: existing LRU eviction + VACUUM. Skip when the
        // account is unknown to this cache.
        let Some(conn) = self.cache.connection(account) else {
            let mut report = EvictionReport {
                bodies_purged_age: purge.age_purged,
                bodies_purged_delete: purge.delete_purged,
                ..EvictionReport::default()
            };
            report.duration = started.elapsed();
            return Ok(report);
        };
        let max = self.max_size_bytes;
        let force_vacuum = purge.age_purged > 0 || purge.delete_purged > 0;
        let mut report = run_eviction_cycle(conn, max, force_vacuum).await?;
        report.bodies_purged_age = purge.age_purged;
        report.bodies_purged_delete = purge.delete_purged;
        report.duration = started.elapsed();

        if report.did_work() {
            tracing::info!(
                account = account,
                bytes_before = report.bytes_before,
                bytes_after = report.bytes_after,
                bytes_evicted = report.bytes_evicted(),
                bodies_purged_age = report.bodies_purged_age,
                bodies_purged_delete = report.bodies_purged_delete,
                query_cache_expired = report.query_cache_expired,
                query_cache_oldest = report.query_cache_oldest,
                messages_tombstoned = report.messages_tombstoned,
                messages_oldest = report.messages_oldest,
                duration_ms = u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
                "cache eviction cycle complete",
            );
        }
        Ok(report)
    }

    /// `body_max_age_days * 86_400_000` ms before now, or `0` when the
    /// age-based purge is disabled. The SQL helper treats `0` as
    /// "skip age step" while still running the soft-delete step.
    fn age_cutoff_ms(&self) -> i64 {
        if self.body_max_age_days == 0 {
            return 0;
        }
        let now = now_ms();
        let days = i64::from(self.body_max_age_days);
        now.saturating_sub(days.saturating_mul(MS_PER_DAY))
    }

    /// `now - 7 days` in ms. Always applies; the soft-delete grace
    /// window is a constant, not a config.
    fn delete_cutoff_ms() -> i64 {
        now_ms().saturating_sub(SOFT_DELETE_GRACE_DAYS.saturating_mul(MS_PER_DAY))
    }

    /// `true` when this tick should run the body-purge phase. The
    /// gate fires when either:
    /// - no purge has run for `account` yet this process lifetime, or
    /// - `purge_interval` has elapsed since the last run.
    ///
    /// Returns `false` when both `body_max_age_days = 0` and
    /// `purge_interval = ZERO` — the operator has fully disabled the
    /// phase. When only `body_max_age_days = 0`, the soft-delete step
    /// still runs (returning `true` lets it fire), because deleted
    /// bodies past the grace window are always safe to drop.
    fn should_run_body_purge(&self, account: &str) -> bool {
        if self.purge_interval.is_zero() {
            return false;
        }
        let last = self
            .last_body_purge
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        last.get(account)
            .is_none_or(|t| t.elapsed() >= self.purge_interval)
    }

    fn record_body_purge_run(&self, account: &str) {
        let mut last = self
            .last_body_purge
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        last.insert(account.to_owned(), Instant::now());
    }

    /// Spawn a `tokio::task` that calls [`Self::evict_account`] every
    /// [`Self::interval`] until the returned [`EvictionHandle`] drops.
    /// Returns `None` when the interval is zero (eviction disabled).
    pub(crate) fn spawn_for(self: &Arc<Self>, account: String) -> Option<EvictionHandle> {
        if self.interval.is_zero() {
            return None;
        }
        let driver = Arc::clone(self);
        let interval = self.interval;
        let acct_for_log = account.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = driver.evict_account(&account).await {
                    tracing::warn!(
                        account = account,
                        error = %e,
                        "cache eviction tick failed; will retry on next interval",
                    );
                }
            }
        });
        Some(EvictionHandle {
            task: Some(handle),
            account: acct_for_log,
        })
    }
}

/// Owned handle to one per-account background eviction task. Aborts the
/// task on drop. Same pattern as
/// [`crate::cache::sync::SyncHandle`].
#[must_use = "the eviction task aborts when this handle is dropped"]
pub(crate) struct EvictionHandle {
    task: Option<tokio::task::JoinHandle<()>>,
    account: String,
}

impl EvictionHandle {
    /// Stop the background eviction and wait for the abort to land.
    #[cfg(test)]
    pub(crate) async fn stop(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = t.await;
        }
    }
}

impl Drop for EvictionHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            tracing::debug!(account = %self.account, "aborting cache eviction task");
            t.abort();
        }
    }
}

/// Execute the four-step eviction algorithm on `conn` against
/// `max_size_bytes`. All deletes run in one transaction so they're either
/// all committed or all rolled back. `VACUUM` runs after commit (`SQLite`
/// refuses `VACUUM` inside a transaction).
///
/// Within the transaction the "effective" size is
/// `(page_count - freelist_count) * page_size`. Raw `page_count` doesn't
/// drop until `VACUUM` runs, so the loop conditions key off the
/// freelist-adjusted value instead.
async fn run_eviction_cycle(
    conn: &Arc<Connection>,
    max_size_bytes: u64,
    force_vacuum: bool,
) -> Result<EvictionReport, Error> {
    let now = now_ms();
    conn.call(move |c| -> rusqlite::Result<EvictionReport> {
        let page_size: i64 = c.query_row("SELECT * FROM pragma_page_size()", [], |r| r.get(0))?;
        let page_count_before: i64 =
            c.query_row("SELECT * FROM pragma_page_count()", [], |r| r.get(0))?;
        let bytes_before_i = page_count_before.saturating_mul(page_size);
        let bytes_before = u64::try_from(bytes_before_i).unwrap_or(0);

        let mut report = EvictionReport {
            bytes_before,
            bytes_after: bytes_before,
            ..EvictionReport::default()
        };

        // Use i64 for the SQL math; max_size_bytes is u64 but in practice
        // capped at 500 MiB by config validation. Saturating cast keeps us
        // safe even at extreme configured values.
        let max_i = i64::try_from(max_size_bytes).unwrap_or(i64::MAX);
        if bytes_before_i <= max_i {
            return Ok(report);
        }

        let target = max_i / TARGET_FRACTION_DEN * TARGET_FRACTION_NUM;

        // ── deletions in one transaction ─────────────────────────────────
        let tx = c.transaction()?;
        // FK pragma is connection-scoped; the explicit set mirrors the
        // pattern in `queries::reseed_account` so the cascade fires
        // reliably for `message_labels`.
        tx.pragma_update(None, "foreign_keys", "ON")?;

        // Step 1: expired query_cache (cheap; dead first).
        report.query_cache_expired =
            tx.execute("DELETE FROM query_cache WHERE expires_at < ?1", [now])?;

        // Step 2: oldest query_cache in batches of EVICTION_BATCH_SIZE.
        loop {
            if used_bytes(&tx, page_size)? <= target {
                break;
            }
            let n = tx.execute(
                "DELETE FROM query_cache WHERE query_hash IN \
                 (SELECT query_hash FROM query_cache ORDER BY cached_at ASC LIMIT ?1)",
                [EVICTION_BATCH_SIZE],
            )?;
            if n == 0 {
                break;
            }
            report.query_cache_oldest = report.query_cache_oldest.saturating_add(n);
        }

        // Step 3: tombstoned messages (deleted_at NOT NULL).
        if used_bytes(&tx, page_size)? > target {
            let n = tx.execute("DELETE FROM messages WHERE deleted_at IS NOT NULL", [])?;
            report.messages_tombstoned = n;
        }

        // Step 4: oldest messages by fetched_at in batches.
        // message_labels FK CASCADE drops the dependent rows.
        loop {
            if used_bytes(&tx, page_size)? <= target {
                break;
            }
            let n = tx.execute(
                "DELETE FROM messages WHERE id IN \
                 (SELECT id FROM messages ORDER BY fetched_at ASC LIMIT ?1)",
                [EVICTION_BATCH_SIZE],
            )?;
            if n == 0 {
                break;
            }
            report.messages_oldest = report.messages_oldest.saturating_add(n);
        }

        tx.commit()?;

        // ── VACUUM (must run outside any transaction) ────────────────────
        // Only worth running if something was actually deleted; an empty
        // VACUUM still rewrites the whole file. `force_vacuum` covers
        // the case where the body-purge phase nulled body columns (no
        // rows removed, but freelist pages now exist) and the LRU
        // phase did nothing — without the force we'd never reclaim the
        // body-column bytes.
        if force_vacuum || report.did_work() {
            c.execute("VACUUM", [])?;
        }

        let page_count_after: i64 =
            c.query_row("SELECT * FROM pragma_page_count()", [], |r| r.get(0))?;
        let bytes_after_i = page_count_after.saturating_mul(page_size);
        report.bytes_after = u64::try_from(bytes_after_i).unwrap_or(0);

        Ok(report)
    })
    .await
    .map_err(|e| match e {
        tokio_rusqlite::Error::Error(inner) => Error::Internal {
            context: "cache::evict".into(),
            source: anyhow::Error::new(inner),
        },
        other => Error::Internal {
            context: "cache::evict".into(),
            source: anyhow::Error::new(other),
        },
    })
}

/// Bytes currently in "use" — `(page_count - freelist_count) * page_size`.
/// Inside a transaction `page_count` is stable (until commit + VACUUM) but
/// `freelist_count` grows as DELETEs free pages, so this is what the loop
/// conditions key off.
fn used_bytes(tx: &rusqlite::Transaction<'_>, page_size: i64) -> rusqlite::Result<i64> {
    let pc: i64 = tx.query_row("SELECT * FROM pragma_page_count()", [], |r| r.get(0))?;
    let fc: i64 = tx.query_row("SELECT * FROM pragma_freelist_count()", [], |r| r.get(0))?;
    Ok(pc.saturating_sub(fc).saturating_mul(page_size))
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::cache::Cache;

    fn tmp() -> TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        d
    }

    async fn open_cache(dir: &TempDir, accounts: &[&str]) -> Arc<Cache> {
        let accounts: Vec<String> = accounts.iter().map(|s| (*s).to_owned()).collect();
        Arc::new(
            Cache::new(dir.path().to_owned(), &accounts, Duration::from_mins(5))
                .await
                .expect("open cache"),
        )
    }

    /// Seed `n` messages with `fetched_at` in ascending order so eviction
    /// has a well-defined oldest-first ordering, and large `body_text`
    /// payloads so the file grows past a few KB quickly.
    async fn seed_messages(conn: &Arc<Connection>, n: usize, body_kib: usize) {
        let body = "x".repeat(body_kib * 1024);
        let total = i64::try_from(n).expect("n fits in i64");
        conn.call(move |c| -> rusqlite::Result<()> {
            let tx = c.transaction()?;
            for i in 0..total {
                let id = format!("m{i:08}");
                let tid = format!("t{i:08}");
                tx.execute(
                    "INSERT INTO threads (id, snippet, history_id, fetched_at) \
                     VALUES (?1, NULL, NULL, ?2)",
                    rusqlite::params![tid, i],
                )?;
                tx.execute(
                    "INSERT INTO messages \
                     (id, thread_id, internal_date, headers_json, body_text, body_html, \
                      snippet, has_attachments, attachments_json, raw_size, fetched_at, deleted_at) \
                     VALUES (?1, ?2, 0, '{}', ?3, NULL, NULL, 0, NULL, NULL, ?4, NULL)",
                    rusqlite::params![id, tid, body, i],
                )?;
            }
            tx.commit()
        })
        .await
        .expect("seed");
    }

    /// Layer 1: an oversized DB shrinks below 90% of the cap after one
    /// eviction cycle. Bodies are ~16 KiB each so 200 messages overshoot a
    /// 1 MiB cap by several × and force the algorithm into Step 4.
    #[tokio::test]
    async fn oversized_db_shrinks_below_target() {
        let dir = tmp();
        let cache = open_cache(&dir, &["work"]).await;
        let conn = cache.connection("work").expect("conn").clone();
        seed_messages(&conn, 200, 16).await;

        let cap: u64 = 1024 * 1024; // 1 MiB cap
        let target = cap * 9 / 10;

        // Sanity: before eviction the file is well over the cap.
        let report = run_eviction_cycle(&conn, cap, false).await.expect("cycle");
        assert!(
            report.bytes_before > cap,
            "test setup must overshoot cap: bytes_before={} cap={cap}",
            report.bytes_before,
        );
        assert!(
            report.bytes_after <= target,
            "post-cycle size {} must be ≤ target {target} (cap={cap})",
            report.bytes_after,
        );
        assert!(report.messages_oldest > 0, "Step 4 must have run");
    }

    /// Under-threshold cycle is a no-op: no rows removed, sizes equal.
    #[tokio::test]
    async fn under_threshold_is_no_op() {
        let dir = tmp();
        let cache = open_cache(&dir, &["work"]).await;
        let conn = cache.connection("work").expect("conn").clone();
        seed_messages(&conn, 4, 1).await;

        let cap: u64 = 100 * 1024 * 1024; // 100 MiB — far above test data
        let report = run_eviction_cycle(&conn, cap, false).await.expect("cycle");
        assert!(!report.did_work(), "report: {report:?}");
        assert_eq!(report.bytes_before, report.bytes_after);
    }

    /// Step 1 evicts only expired `query_cache` rows; unexpired rows
    /// survive even on an oversized DB if dropping the expired ones is
    /// enough to get under the target.
    #[tokio::test]
    async fn expired_query_cache_removed_first() {
        let dir = tmp();
        let cache = open_cache(&dir, &["work"]).await;
        let conn = cache.connection("work").expect("conn").clone();

        // Seed 4 query_cache rows: 2 expired, 2 fresh. Body data is
        // irrelevant for this assertion; we only care about row counts.
        conn.call(|c| -> rusqlite::Result<()> {
            let tx = c.transaction()?;
            for (i, expires) in [(0, 1), (1, 1), (2, i64::MAX), (3, i64::MAX)] {
                tx.execute(
                    "INSERT INTO query_cache \
                     (query_hash, query, max_results, page_token, result_ids_json, cached_at, expires_at) \
                     VALUES (?1, 'q', 10, NULL, '{\"threads\":[],\"next_page_token\":null}', 0, ?2)",
                    rusqlite::params![format!("h{i}"), expires],
                )?;
            }
            tx.commit()
        })
        .await
        .expect("seed query_cache");

        // Use a cap of 1 byte so the algorithm definitely enters the
        // eviction loop; we want to observe that expired rows are
        // deleted by Step 1 specifically.
        let report = run_eviction_cycle(&conn, 1, false).await.expect("cycle");
        assert_eq!(
            report.query_cache_expired, 2,
            "expected 2 expired rows removed; report: {report:?}"
        );
    }

    /// `Evictor::evict_account` returns an empty report for an unknown
    /// account rather than erroring. Mirrors the same defensive shape used
    /// in `Cache::lookup_*` (unknown account → miss/no-op).
    #[tokio::test]
    async fn unknown_account_is_no_op() {
        let dir = tmp();
        let cache = open_cache(&dir, &["work"]).await;
        let evictor = Evictor::new(
            cache,
            Duration::from_mins(5),
            1024 * 1024,
            0,
            Duration::ZERO,
        );
        let mut report = evictor.evict_account("missing").await.expect("ok");
        // `duration` is wall-clock and non-deterministic — zero it
        // before structural comparison to keep this assertion stable
        // across machines.
        report.duration = Duration::ZERO;
        assert_eq!(report, EvictionReport::default());
    }

    /// Step 4 deletes via the `messages` table; `message_labels` rows for
    /// the deleted messages must cascade away (FK `ON DELETE CASCADE`).
    /// Regression guard for the highest-risk SQL in the cycle per
    /// `docs/cache-implementation-plan.md` §"Phase 5 risks": orphan
    /// `message_labels` rows would silently leak label state across
    /// re-fetches.
    #[tokio::test]
    async fn message_labels_cascade_when_messages_evicted() {
        let dir = tmp();
        let cache = open_cache(&dir, &["work"]).await;
        let conn = cache.connection("work").expect("conn").clone();
        seed_messages(&conn, 50, 8).await;
        // Attach 2 labels to every message.
        conn.call(|c| -> rusqlite::Result<()> {
            let tx = c.transaction()?;
            tx.pragma_update(None, "foreign_keys", "ON")?;
            let ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM messages")?;
                let rows: rusqlite::Result<Vec<String>> =
                    stmt.query_map([], |r| r.get::<_, String>(0))?.collect();
                rows?
            };
            for id in &ids {
                tx.execute(
                    "INSERT INTO message_labels (message_id, label_id) VALUES (?1, 'INBOX')",
                    [id],
                )?;
                tx.execute(
                    "INSERT INTO message_labels (message_id, label_id) VALUES (?1, 'UNREAD')",
                    [id],
                )?;
            }
            tx.commit()
        })
        .await
        .expect("seed labels");

        let labels_before: i64 = conn
            .call(|c| c.query_row("SELECT COUNT(*) FROM message_labels", [], |r| r.get(0)))
            .await
            .expect("count");
        assert_eq!(labels_before, 100, "test setup must seed 100 label links");

        // Tiny cap forces Step 4 to run.
        let report = run_eviction_cycle(&conn, 32 * 1024, false)
            .await
            .expect("cycle");
        assert!(report.messages_oldest > 0, "Step 4 must run");

        // No orphan label rows survive: every remaining message_labels row
        // must reference a still-present message.
        let orphans: i64 = conn
            .call(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM message_labels ml \
                     WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = ml.message_id)",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .expect("count orphans");
        assert_eq!(
            orphans, 0,
            "FK cascade must drop label rows for evicted messages"
        );
    }

    /// `spawn_for(0)` returns `None`; `spawn_for(small)` returns `Some`
    /// and the spawned task survives at least one tick.
    #[tokio::test]
    async fn spawn_for_returns_none_when_interval_is_zero() {
        let dir = tmp();
        let cache = open_cache(&dir, &["work"]).await;
        let evictor = Arc::new(Evictor::new(
            cache,
            Duration::ZERO,
            1024 * 1024,
            0,
            Duration::ZERO,
        ));
        assert!(evictor.spawn_for("work".into()).is_none());
    }

    #[tokio::test]
    async fn spawn_for_runs_a_tick() {
        let dir = tmp();
        let cache = open_cache(&dir, &["work"]).await;
        let conn = cache.connection("work").expect("conn").clone();
        seed_messages(&conn, 50, 8).await;

        let evictor = Arc::new(Evictor::new(
            Arc::clone(&cache),
            Duration::from_millis(25),
            32 * 1024, // tiny cap so the first tick evicts
            0,
            Duration::ZERO,
        ));
        let handle = evictor.spawn_for("work".into()).expect("Some");

        // Poll up to 2 s for the message count to drop. We don't depend
        // on an exact final size — only that eviction actually ran.
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(50);
        let initial: i64 = conn
            .call(|c| c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)))
            .await
            .expect("count");
        while waited < Duration::from_secs(2) {
            let now: i64 = conn
                .call(|c| c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)))
                .await
                .expect("count");
            if now < initial {
                handle.stop().await;
                return;
            }
            tokio::time::sleep(step).await;
            waited += step;
        }
        handle.stop().await;
        panic!("background eviction never ran");
    }
}
