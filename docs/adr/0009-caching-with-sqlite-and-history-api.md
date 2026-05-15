# ADR-0009: Caching layer — SQLite per account, Gmail History API for incremental sync

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

`google-personal-mcp` will be consumed primarily by LLM agents asking question patterns like "summarize my last 50 emails," "find emails from X this month," or "what did Y email me about last week." A naive stateless implementation answers each of these with the natural N+1 pattern: one `threads.list` call returning N IDs, then N `messages.get` calls fetching each body.

This is bad in three distinct ways:

1. **Quota burn.** Gmail's per-user quota is 250 units/second by default. `messages.get` costs 5 units, `threads.list` costs 10. A "summarize last 50" call costs 10 + 50×5 = 260 units — over the per-second limit, and the user pays this cost every single time the question is asked, even for the same data.
2. **Latency.** 50 sequential HTTP round-trips at ~80ms each ≈ 4 seconds. Even with concurrency the latency tax is real.
3. **Token waste in the consumer.** The LLM consumer re-receives the same thread bodies on each call when the question is iterated.

The good news: **Gmail thread bodies are immutable once a message is delivered.** Subjects, headers, body content — none of these change after send. Labels and inbox/archive state mutate; bodies don't. And Gmail provides the **`users.history.list`** API specifically for incremental sync — returns the deltas (new messages, label changes, deletes) since a `historyId` you supply.

This means we can cache aggressively and stay correct, with periodic cheap deltas to detect what's changed.

The maintainer's stated use case (multi-account personal-data daemon, runs forever on a personal VPS, consumed by knowledge tools) maps perfectly to a local persistent cache. We have disk; we have time; we don't have a fresh Lambda environment.

If no decision were made, every consumer of this MCP pays full quota every call. The MCP is then unusable for the very workflows it was designed for.

## Decision

We will implement a **per-account SQLite cache** with **Gmail History API for incremental invalidation**, using **`rusqlite` + `tokio-rusqlite`**.

### Storage layout

One SQLite database file per account at `~/.config/google-personal-mcp/cache/<account>.db`. WAL mode enabled (concurrent readers don't block during writes). Tokens / config never live in the cache DB; only Gmail data.

### Schema (v1)

```sql
-- Messages: write-once, never modified after fetch.
-- Body content is stable; only label state can change (tracked separately below).
CREATE TABLE messages (
    id              TEXT PRIMARY KEY,
    thread_id       TEXT NOT NULL,
    internal_date   INTEGER NOT NULL,         -- Gmail's internalDate (ms epoch)
    headers_json    TEXT NOT NULL,            -- full headers as JSON for any-name lookup
    body_text       TEXT,                     -- best-effort plain-text (per ADR-0010)
    body_html       TEXT,                     -- raw HTML if present
    snippet         TEXT,
    has_attachments INTEGER NOT NULL,         -- bool 0/1
    attachments_json TEXT,                    -- attachment metadata (no content) per ADR-0010
    raw_size        INTEGER,
    fetched_at      INTEGER NOT NULL,         -- ms epoch — for diagnostics, not invalidation
    deleted_at      INTEGER                   -- nullable; set when History tells us the msg is gone
);

CREATE INDEX idx_messages_thread ON messages (thread_id);
CREATE INDEX idx_messages_date ON messages (internal_date DESC);

-- Per-message label state. Mutates; rebuilt from History deltas.
CREATE TABLE message_labels (
    message_id TEXT NOT NULL,
    label_id   TEXT NOT NULL,
    PRIMARY KEY (message_id, label_id),
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

-- Threads: a thread_id maps to an ordered set of messages.
CREATE TABLE threads (
    id           TEXT PRIMARY KEY,
    snippet      TEXT,
    history_id   INTEGER,                     -- the historyId of the latest event we know
    fetched_at   INTEGER NOT NULL
);

-- Labels: full label catalog per account (cheap to refresh; small).
CREATE TABLE labels (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT,                         -- "system" | "user"
    fetched_at  INTEGER NOT NULL
);

-- Per-account sync state. Single row.
CREATE TABLE account_state (
    rowid              INTEGER PRIMARY KEY CHECK (rowid = 1),
    last_history_id    INTEGER,               -- starting point for next history.list call
    last_full_sync_at  INTEGER,               -- ms epoch of the last full reseed (rare)
    schema_version     INTEGER NOT NULL DEFAULT 1
);

-- Search-result memoization. TTL'd (5min default); invalidated by history events on
-- any matching label / thread.
CREATE TABLE query_cache (
    query_hash      TEXT PRIMARY KEY,         -- sha256(query || max_results)
    query           TEXT NOT NULL,
    max_results     INTEGER NOT NULL,
    page_token      TEXT,                     -- nullable (first page = NULL)
    result_ids_json TEXT NOT NULL,            -- JSON array of thread_ids
    cached_at       INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL          -- cached_at + TTL
);
```

### Sync protocol

**First touch on an account:**

1. Call `users.getProfile` → record current `historyId` in `account_state.last_history_id`.
2. Do **not** backfill historical messages. Bodies are fetched lazily on first read of each thread.
3. Sync labels via `labels.list` → populate `labels` table.

This avoids the catastrophic "sync 100K messages on first run" pattern that other Gmail clients suffer.

**Incremental sync (background task, every 60s while daemon runs, plus on-demand before each list operation):**

1. Call `users.history.list(start_history_id=last_history_id, max_results=500)`.
2. For each `historyId` event, apply:
   - `messageAdded` → mark thread as known-stale (don't fetch body unless requested)
   - `messageDeleted` → set `deleted_at` on the message
   - `labelAdded` / `labelRemoved` → update `message_labels`
3. Update `account_state.last_history_id` to the latest event ID.
4. Invalidate any `query_cache` rows whose results contain affected thread IDs.

**History gap (response is `404 historyNotFound` or `gone`):**

When Gmail's History only retains events for ~7 days. If the daemon was offline longer, a full reseed is needed:

1. Drop `messages`, `threads`, `message_labels`, `query_cache` rows for this account (keep `labels`).
2. Reset `account_state.last_history_id` to current `users.getProfile.historyId`.
3. Future requests fetch on demand.

A full reseed is logged at WARN — operator should see it.

**On-demand reads (`get_thread`, `search_threads`):**

```rust
async fn get_thread(&self, account: &str, thread_id: &str) -> Result<Thread> {
    self.cache.with_account(account, |conn| async move {
        // Catch up on history before serving (cheap; usually no-op)
        self.history_sync(account, conn).await?;

        if let Some(thread) = conn.lookup_thread(thread_id)? {
            metric!(cache_hits_total, account = account, kind = "thread");
            return Ok(thread);
        }
        metric!(cache_misses_total, account = account, kind = "thread");

        // Cache miss: fetch from Gmail, persist, return
        let thread = self.api.get_thread(thread_id).await?;
        conn.insert_thread(&thread)?;
        Ok(thread)
    }).await
}
```

For `search_threads`: hash `(query, max_results, page_token)` → check `query_cache` → on miss, call API → store with TTL.

### Race-prevention: `historyId` watermark on cached queries

A naive implementation has a real race: tool call reads stale data → in parallel, `history.list` returns events that *would have invalidated* those results → events apply to other cache entries → the tool call writes its (now-stale) results into `query_cache`. The cache then serves the stale answer until TTL expires.

To prevent this, **every `query_cache` row records the `historyId` it was current as of, captured at API-fetch time**:

```sql
ALTER TABLE query_cache ADD COLUMN fetched_at_history_id INTEGER NOT NULL;
```

Read path: a `query_cache` hit is **only valid if `fetched_at_history_id >= account_state.last_history_id`** at read time. If a newer `last_history_id` exists (because history sync advanced after this row was written), the row is treated as a miss and re-fetched.

Write path: at fetch time, snapshot `account_state.last_history_id` as `fetch_start_history_id`. After the API call returns, in the same transaction:

1. If `account_state.last_history_id` has not advanced since `fetch_start_history_id` → write the row with `fetched_at_history_id = fetch_start_history_id`. Safe.
2. If `account_state.last_history_id` HAS advanced → discard the result (do not cache); the next call will re-fetch and benefit from updated state.

This is conservative — we may discard a fresh-but-not-actually-stale result — but it eliminates the silent staleness window. Prometheus metric `gmcp_cache_write_discarded_total{account=...}` surfaces the rate (should be near zero in steady state; spikes during heavy concurrent activity).

### Schema migration mechanism

The `account_state.schema_version` column tracks the on-disk schema version. Migrations are **hand-rolled** (not via `refinery` or other migration framework — overkill for our scale), defined as an ordered const slice in `src/cache/migrations.rs`:

```rust
const MIGRATIONS: &[Migration] = &[
    Migration {
        from_version: 0,                    // fresh DB
        to_version: 1,
        sql: include_str!("migrations/001_initial.sql"),
    },
    Migration {
        from_version: 1,
        to_version: 2,
        sql: include_str!("migrations/002_add_thread_history_id.sql"),
    },
    // ...
];
```

On connection open: read `account_state.schema_version` (or treat missing table as version 0); apply each migration with `from_version <= current < to_version` in order; update version atomically per migration. Each migration runs in a transaction; failure rolls back the whole DB to the previous version.

**Compatibility rule:** the daemon refuses to start if it encounters a schema version *higher* than its highest known migration target — this prevents a downgrade from accidentally truncating columns. Operator gets a clear error pointing at the version mismatch and instructs them to upgrade the binary or wipe the cache (`rm ~/.config/google-personal-mcp/cache/<account>.db`).

**Testing:** every migration is exercised by a test that loads the previous version's snapshot fixture and verifies the upgrade. The test corpus grows with each migration — a v1→v2→v3 path is verified end-to-end.

### TTLs and eviction

| Cache class | Invalidation | Notes |
| --- | --- | --- |
| Message bodies | Never (immutable) — only `deleted_at` updated via History | Eviction by LRU on size pressure |
| Message labels | History API (`labelAdded` / `labelRemoved`) | Always live with message |
| Threads | History API + `messageAdded` markers | |
| Search results (`query_cache`) | TTL 5min OR History event affecting any result | Default TTL configurable in `[cache]` |
| Labels | TTL 1 hour, refreshed on demand | Tiny table |

LRU eviction kicks in when DB file exceeds `[cache] max_size_bytes_per_account` (default 500 MB). Eviction policy: oldest `messages.fetched_at` (with `deleted_at` first), `query_cache` first.

**Eviction implementation:** a per-account background task runs every `[cache] eviction_interval_seconds` (default 300 — every 5 minutes). The task:

1. Queries the SQLite file size (`PRAGMA page_count * PRAGMA page_size`).
2. If size < `max_size_bytes_per_account`: no-op, sleep for the interval.
3. If over the limit: enter eviction loop:
   - Delete all rows from `query_cache` with `expires_at < now` (cheap; dead entries first).
   - Delete `query_cache` rows in batches of 100 ordered by `cached_at ASC` until size is back under 90% of limit.
   - If still over: delete `messages` rows with `deleted_at IS NOT NULL` ordered by `deleted_at ASC`.
   - If still over: delete `messages` rows in batches of 100 ordered by `fetched_at ASC`. Cascading FK deletes drop `message_labels` for the same row.
   - After each batch: `VACUUM` the database file (otherwise SQLite reuses pages but the file doesn't shrink). VACUUM is expensive; run at most once per eviction cycle.
4. Log INFO with: bytes-evicted, rows-evicted by table, time taken.

This **must not** run inside the main request path. Eviction holds a write lock on the SQLite file; doing it inline would block tool calls.

**Why background task over eviction-on-write:** eviction-on-write would add unpredictable latency to whichever unfortunate tool call triggers the threshold crossing. Background task amortizes the cost predictably and is bounded; a busy account's writes accumulate briefly before the next eviction tick, which is acceptable (size cap is a soft limit, not hard).

### Cache-aware tool design

Tools become orders of magnitude cheaper. The illustrative call cost for "summarize my last 50 emails":

| Phase | Without cache | With cache (warm) | With cache (cold first call) |
| --- | --- | --- | --- |
| `search_threads` | 10 quota units | ~0 (cached query) or 10 (TTL miss + history.list 2u) | 12 |
| `get_thread × 50` | 250 quota units | 0 (immutable bodies cached) | 250 |
| **Total** | **260** | **~12** (or 0 with fresh cache) | **262** |

That's a 20–∞× reduction in steady state.

### Config additions (`[cache]` section in `config.toml`, see ADR-0006)

```toml
[cache]
enabled = true
dir = "~/.config/google-personal-mcp/cache"            # where the .db files live
max_size_bytes_per_account = 524_288_000      # 500 MiB
query_ttl_seconds = 300                       # search-result TTL
labels_ttl_seconds = 3600                     # label catalog TTL
background_sync_interval_seconds = 60         # 0 to disable background sync
sync_on_read = true                           # catch up history before serving lookups
```

Setting `enabled = false` disables the cache entirely (every call is a passthrough). Useful for debugging and for stateless deployment models.

### Concurrency model

- One `tokio_rusqlite::Connection` per account, wrapped in `Arc`.
- WAL mode (`PRAGMA journal_mode = WAL`) → many readers, one writer at a time.
- Background sync task per account; uses the same connection. No competing connection pool.
- The `Arc<TokenManager>` snapshot pattern (per [ADR-0002](0002-multi-account-architecture.md)) extends to cache connections — in-flight reads survive account hot-reload via the snapshot.

### New tools (operator-facing, low-stakes)

- `cache_status(account?)` — returns size on disk, hit rate (last hour), last sync time, last_history_id per account
- `cache_invalidate(account, scope: "all" | "queries" | "labels")` — manual reset for debug; does NOT delete bodies (those are immutable so they're safe to keep)

`cache_invalidate` deletion of bodies requires editing the SQLite file directly — intentional. We never invalidate bodies through the tool surface; operator uses `rm`.

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) No cache (status quo) | Simplest; no schema; no eviction logic | 10-50× quota burn; latency tax on every repeated query; doesn't fit the "consumed by knowledge tools" use case |
| (b) In-memory LRU only (`moka` or similar) | No persistence concerns; simple | Restart loses cache (annoying for a long-running daemon that nonetheless restarts on update); no incremental sync from delta API |
| **(c) SQLite per account + Gmail History API** (chosen) | Persistent across restarts; correct via History deltas; enables cheap "summarize last N" patterns; foundation for offline-capable tools later | New dep (`rusqlite`, `tokio_rusqlite`); cache invalidation logic; disk usage to manage; first-touch latency for cold cache |
| (d) Mirror all messages locally, full SQL queries against the mirror | Most powerful; can do server-side filtering Gmail's query syntax can't | Massive disk usage for 10+ accounts; full reseed pain; reimplements Gmail search; out of scope for "data source" |
| (e) External cache (Redis / Memcached) | Shared across daemon instances | We don't have multiple daemon instances; adds infra burden; over-engineering |
| (f) Filesystem-tree cache (one JSON file per message) | No SQL; trivial | Filesystem inode pressure with thousands of messages; no transactions; slow listing; concurrent-write hazard |
| (g) Cache only search results; always re-fetch bodies | Smaller surface | Body fetches are most of the quota cost; doesn't move the needle |

We choose (c). The combination of immutable bodies + History API is exactly what SQLite is good for; everything else either undersaves the win (b, e, g) or oversaves and becomes a different product (d, f).

## Consequences

**Positive:**

- 10-50× steady-state quota reduction. The "summarize last N" pattern that LLM agents will hit constantly becomes ~free.
- Sub-second response time for cached queries (vs. multi-second for the API path).
- Background sync keeps the cache fresh without operator action.
- History API is the **right** invalidation primitive (Gmail itself uses it); we're not inventing semantics.
- Per-account isolation: a sync failure on `work` doesn't break `personal` reads.
- Foundation for future offline-capable tools (read-only operations work even when Google is down — at least until the cache TTL expires).
- Lazy initial population means accounts with massive history don't take hours to initialize.
- Operator has a single SQLite file per account they can `rsync` for backups, inspect with `sqlite3` CLI, or wipe with `rm`.

**Negative:**

- Adds `rusqlite` (with `bundled` feature so SQLite is statically linked — no system dep) and `tokio_rusqlite`. Both are mature, stable, low-controversy.
- Disk usage grows over time. LRU + size cap mitigates but doesn't eliminate.
- Cache invalidation logic is tested code that has to be correct. Bugs here mean stale data served to the model — which can lead to confidently wrong answers.
- First fetch of a thread is unchanged (one round trip to Gmail); only repeats are fast.
- History API has a ~7-day retention window. Daemon offline >7 days requires full reseed.
- One more thing for the operator to understand — the `[cache]` config section, the cache directory, the `cache_status` tool.
- Schema migrations (when we add columns or rename) need a migration story. v1 is `schema_version` column; future versions check + apply migrations on startup.

**Risks:**

- *Risk:* Cache returns stale data because History sync got behind / dropped events.
  *Mitigation:* `sync_on_read = true` (default) catches up history before each list operation. Background sync provides defense-in-depth. Worst case: stale label state for one minute. Body content is immutable so can never be "stale."
- *Risk:* SQLite file corruption (interrupted write, disk full, filesystem error).
  *Mitigation:* WAL mode is robust. On corruption detected at startup, log ERROR and rebuild from full sync (preserves account_state if intact, otherwise full reseed). `cache_invalidate(scope=all)` provides operator escape.
- *Risk:* Multiple concurrent writes to the same DB connection deadlock.
  *Mitigation:* `tokio_rusqlite` serializes via a single dedicated thread per connection. WAL allows concurrent reads during writes.
- *Risk:* `query_cache` returns stale results because invalidation missed a History event affecting the result set.
  *Mitigation:* TTL bounds the staleness window (default 5 min). The History sync runs before serving when `sync_on_read = true`. Belt and suspenders.
- *Risk:* `messages.body_text` and `body_html` capture sensitive content; the SQLite DB is then the same kind of sensitive-data target as the Gmail API itself.
  *Mitigation:* DB file mode 600 (enforced at create time + verified at startup). Document operator responsibility (encrypt the disk if appropriate). Cache contents are exactly the same data the operator already lets the daemon access — no new disclosure surface.
- *Risk:* History API behavior changes upstream — Google deprecates `history.list` or alters event semantics.
  *Mitigation:* History API is the foundation of every Gmail sync client (Gmail mobile, Mail.app, etc.); deprecation extremely unlikely. Our schema_version column means we can re-encode events under different semantics if needed without breaking on-disk data.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — defines the long-running daemon model that makes persistent cache feasible
- [ADR-0002](0002-multi-account-architecture.md) — per-account scope; cache files keyed by account alias
- [ADR-0006](0006-config.md) — `[cache]` config section
- [ADR-0008](0008-observability-and-deployment.md) — `gmcp_cache_hits_total`, `gmcp_cache_misses_total`, `gmcp_cache_size_bytes` metrics
- [ADR-0010](0010-mime-and-encoding.md) — body parsing whose output we cache
- [ADR-0014](0014-status-introspection-tool.md) — `mcp_status` surfaces cache stats
- Gmail [`users.history.list` reference](https://developers.google.com/workspace/gmail/api/reference/rest/v1/users.history/list) — the invalidation primitive
- Gmail [API quotas](https://developers.google.com/workspace/gmail/api/reference/quota) — per-method costs that this ADR amortizes
- [`rusqlite`](https://docs.rs/rusqlite), [`tokio_rusqlite`](https://docs.rs/tokio_rusqlite) — implementation
