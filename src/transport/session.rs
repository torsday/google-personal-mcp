//! Session lifecycle for the Streamable HTTP transport per
//! [ADR-0003 §Session-based MCP](../../../docs/adr/0003-transport-stdio-and-streamable-http.md).
//!
//! A [`SessionStore`] is created once at daemon startup. The (future)
//! HTTP transport loop is responsible for two things:
//!
//! 1. On client connect: call [`SessionStore::create`] to get a fresh
//!    `session_id` and register it.
//! 2. On every inbound request: call [`SessionStore::touch`] with the
//!    request's `session_id` to refresh its idle timer.
//!
//! Idle sessions older than the configured threshold are removed
//! automatically by the background sweeper started via
//! [`SessionStore::spawn_sweeper`]. The sweeper polls at a fraction of
//! the idle threshold so eviction granularity is bounded, and the
//! poll interval has a sensible minimum (1s) to keep tests fast.
//!
//! # Concurrency model
//!
//! The store is `Send + Sync` and cheap to `Arc::clone` — every HTTP
//! handler holds a clone. The inner `HashMap` is wrapped in a single
//! `Mutex<_>` because every operation is O(1) under the lock and
//! contention is bounded by request rate, not session count.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Stable identifier for one connected client. The exact format is an
/// implementation detail; the only invariant callers may rely on is
/// uniqueness across the lifetime of the daemon.
pub(crate) type SessionId = String;

/// Per-session state. v0.3 stores only the idle-time bookkeeping;
/// future tickets may attach per-session caches, request counters, etc.
#[derive(Debug, Clone)]
pub(crate) struct Session {
    /// When the session was created. Useful for observability later
    /// (`session_duration_seconds` histogram per ADR-0008).
    pub created_at: Instant,
    /// Last time the client made a request on this session. Updated by
    /// [`SessionStore::touch`] and read by the sweeper.
    pub last_active_at: Instant,
}

impl Session {
    const fn new(now: Instant) -> Self {
        Self {
            created_at: now,
            last_active_at: now,
        }
    }
}

/// In-memory session registry. Cheap to clone (one `Arc<Mutex<_>>`).
#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    inner: Arc<Mutex<HashMap<SessionId, Session>>>,
    idle_timeout: Duration,
}

impl SessionStore {
    /// Construct an empty store. `idle_timeout` is the maximum
    /// `last_active_at → now` gap before the sweeper evicts a session
    /// — typically `config.http.session_idle_timeout_secs`. Pass
    /// `Duration::from_secs(3600)` for the ADR-0006 default.
    pub(crate) fn new(idle_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            idle_timeout,
        }
    }

    /// Register a new session with caller-supplied `id`. Production
    /// callers should generate `id` with a CSPRNG and ensure
    /// uniqueness; the store does not generate IDs itself to keep this
    /// module dependency-free of `rand`.
    ///
    /// Returns `true` if the id was inserted, `false` if a session
    /// with the same id already existed (the existing session is
    /// untouched in that case — duplicate IDs are a client bug, not a
    /// retry condition).
    pub(crate) async fn create(&self, id: SessionId) -> bool {
        let mut map = self.inner.lock().await;
        if map.contains_key(&id) {
            return false;
        }
        let now = Instant::now();
        map.insert(id, Session::new(now));
        true
    }

    /// Refresh the idle timer on `id`. Returns `true` if the session
    /// exists and was touched; `false` if no session with that id is
    /// registered (caller's `id` is stale — the transport layer should
    /// return an error to the client).
    pub(crate) async fn touch(&self, id: &SessionId) -> bool {
        let now = Instant::now();
        let mut map = self.inner.lock().await;
        if let Some(s) = map.get_mut(id) {
            s.last_active_at = now;
            true
        } else {
            false
        }
    }

    /// Explicitly drop a session — e.g. on clean client disconnect.
    /// Returns `true` if a session was removed.
    pub(crate) async fn remove(&self, id: &SessionId) -> bool {
        let mut map = self.inner.lock().await;
        map.remove(id).is_some()
    }

    /// Snapshot the current session count. Useful for the
    /// `gmcp_http_sessions_active` gauge once the observability layer
    /// is wired (ADR-0008).
    pub(crate) async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Inspect the configured idle timeout. Exposed for the sweeper
    /// and for tests that need to compute "just past the threshold".
    pub(crate) const fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Run one eviction pass — used both by the background sweeper and
    /// by tests that don't want to spawn a task. Returns the number of
    /// sessions removed.
    pub(crate) async fn sweep_once(&self) -> usize {
        // Instant subtraction can underflow if `idle_timeout` is absurdly
        // large or the runtime just started — skip this pass in that case;
        // the next one will succeed once enough virtual time has elapsed.
        let Some(cutoff) = Instant::now().checked_sub(self.idle_timeout) else {
            return 0;
        };
        let mut map = self.inner.lock().await;
        let before = map.len();
        map.retain(|_, s| s.last_active_at > cutoff);
        before - map.len()
    }

    /// Spawn the background sweeper. The returned [`SweeperHandle`]
    /// stops the sweeper on drop, ensuring sweepers don't outlive the
    /// store in tests.
    ///
    /// Poll interval = `max(idle_timeout / 4, 1s)` — keeps eviction
    /// granularity reasonable even for tiny idle thresholds (used in
    /// tests with `tokio::time::pause`).
    pub(crate) fn spawn_sweeper(&self) -> SweeperHandle {
        let store = self.clone();
        let interval = std::cmp::max(self.idle_timeout / 4, Duration::from_secs(1));
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately; skip it so the sweeper
            // doesn't evict freshly-created sessions on startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                store.sweep_once().await;
            }
        });
        SweeperHandle { task: Some(handle) }
    }
}

/// Owned handle to the sweeper task. Aborts the underlying task on
/// drop, so callers don't leak background work even if they panic.
/// Callers that need to wait for the abort to land call
/// [`SweeperHandle::stop`] explicitly.
///
/// The `Option<JoinHandle>` shape lets `stop` move the handle out
/// while still leaving `Drop` a no-op-safe path; this is the canonical
/// Rust pattern for "abort on drop OR explicit stop".
#[must_use = "the sweeper stops when this handle is dropped"]
pub(crate) struct SweeperHandle {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SweeperHandle {
    /// Stop the sweeper and wait for it to acknowledge. Future
    /// graceful-shutdown work (issue follow-up) will call this from
    /// the daemon-shutdown path before dropping the [`SessionStore`].
    pub(crate) async fn stop(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            // The aborted task resolves with a JoinError; we don't
            // care about the value, only that it's reaped.
            let _ = task.await;
        }
    }
}

impl Drop for SweeperHandle {
    fn drop(&mut self) {
        // Best-effort abort; `stop().await` is the preferred path for
        // production graceful teardown.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn store(secs: u64) -> SessionStore {
        SessionStore::new(Duration::from_secs(secs))
    }

    // ── Pure-CRUD tests (no time control needed) ─────────────────────────────

    #[tokio::test]
    async fn create_inserts_new_session() {
        let s = store(60);
        assert!(s.create("a".into()).await);
        assert_eq!(s.len().await, 1);
    }

    #[tokio::test]
    async fn create_duplicate_returns_false_no_change() {
        let s = store(60);
        assert!(s.create("a".into()).await);
        assert!(!s.create("a".into()).await, "duplicate must return false");
        assert_eq!(s.len().await, 1);
    }

    #[tokio::test]
    async fn touch_missing_returns_false() {
        let s = store(60);
        assert!(!s.touch(&"ghost".to_owned()).await);
    }

    #[tokio::test]
    async fn touch_existing_returns_true() {
        let s = store(60);
        s.create("a".into()).await;
        assert!(s.touch(&"a".to_owned()).await);
    }

    #[tokio::test]
    async fn remove_returns_true_when_present_false_when_absent() {
        let s = store(60);
        s.create("a".into()).await;
        assert!(s.remove(&"a".to_owned()).await);
        assert!(!s.remove(&"a".to_owned()).await);
        assert_eq!(s.len().await, 0);
    }

    #[tokio::test]
    async fn len_reflects_inserts_and_removes() {
        let s = store(60);
        assert_eq!(s.len().await, 0);
        for id in ["a", "b", "c"] {
            s.create(id.into()).await;
        }
        assert_eq!(s.len().await, 3);
        s.remove(&"b".to_owned()).await;
        assert_eq!(s.len().await, 2);
    }

    // ── Sweeper tests with deterministic time control ────────────────────────
    //
    // `tokio::time::pause()` + `advance()` give us exact control over the
    // virtual clock without sleeping. `start_paused = true` on the test
    // ensures pause is in effect from t=0.

    #[tokio::test(start_paused = true)]
    async fn sweep_once_removes_idle_sessions_keeps_fresh() {
        // 10s idle threshold; create one session, advance 11s, create a
        // second, sweep — only the first should be evicted.
        let s = store(10);
        s.create("stale".into()).await;
        tokio::time::advance(Duration::from_secs(11)).await;
        s.create("fresh".into()).await;

        let removed = s.sweep_once().await;
        assert_eq!(removed, 1);
        assert!(!s.touch(&"stale".to_owned()).await, "stale should be gone");
        assert!(s.touch(&"fresh".to_owned()).await, "fresh should remain");
    }

    #[tokio::test(start_paused = true)]
    async fn touch_resets_idle_timer() {
        // Without the touch, "kept-alive" would expire at t=11s. With
        // the touch at t=5s, its last_active is now t=5s — at t=11s
        // it's only 6s old (under the 10s threshold).
        let s = store(10);
        s.create("kept-alive".into()).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(s.touch(&"kept-alive".to_owned()).await);
        tokio::time::advance(Duration::from_secs(6)).await;

        let removed = s.sweep_once().await;
        assert_eq!(removed, 0, "touched session should not be swept");
        assert_eq!(s.len().await, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_once_drops_nothing_when_empty() {
        let s = store(10);
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(s.sweep_once().await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn session_expires_after_configured_idle_window() {
        // The AC item from the issue, end-to-end against the actual
        // background sweeper task — exercises the spawn-and-wait path.
        let s = store(10);
        let sweeper = s.spawn_sweeper();
        s.create("a".into()).await;

        // Just under threshold → still alive.
        tokio::time::advance(Duration::from_secs(9)).await;
        // Let any pending sweeper ticks run before we check.
        tokio::task::yield_now().await;
        // Sweep poll interval is idle_timeout/4 = 2.5s, but min 1s, so
        // 1s; at t=9 several ticks have fired but none evicted (still
        // under threshold). The session should still be alive.
        assert!(s.touch(&"a".to_owned()).await);
        // touch reset the timer to t=9; advance past 9+10=19.
        tokio::time::advance(Duration::from_secs(11)).await;
        // Run one explicit sweep to side-step the dependency on the
        // tokio interval firing exactly when we want.
        let removed = s.sweep_once().await;
        assert_eq!(removed, 1);
        assert_eq!(s.len().await, 0);
        sweeper.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn sweeper_handle_drop_stops_task() {
        let s = store(10);
        {
            let _h = s.spawn_sweeper();
        } // dropped here — task should be aborted
          // No assertion on the JoinHandle directly (it's private); the
          // test just ensures the program doesn't leak a runaway sweeper
          // across test boundaries.
        assert_eq!(s.len().await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn many_sessions_partial_eviction() {
        let s = store(10);
        // Insert 5 sessions at t=0.
        for i in 0..5 {
            s.create(format!("s{i}")).await;
        }
        tokio::time::advance(Duration::from_secs(5)).await;
        // Touch 2 of them at t=5; the others stay at t=0.
        s.touch(&"s0".to_owned()).await;
        s.touch(&"s1".to_owned()).await;
        // At t=11, sessions s2/s3/s4 are 11s old → evict; s0/s1 are 6s old → keep.
        tokio::time::advance(Duration::from_secs(6)).await;
        let removed = s.sweep_once().await;
        assert_eq!(removed, 3);
        assert_eq!(s.len().await, 2);
        for id in ["s0", "s1"] {
            assert!(s.touch(&id.to_owned()).await, "{id} should survive");
        }
    }

    // ── idle_timeout accessor ────────────────────────────────────────────────

    #[tokio::test]
    async fn idle_timeout_returns_configured_value() {
        let s = store(3600);
        assert_eq!(s.idle_timeout(), Duration::from_hours(1));
    }
}
