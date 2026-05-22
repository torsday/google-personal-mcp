# Cache implementation plan (ADR-0009)

**Date:** 2026-05-22
**Status:** Accepted (spike output for #78)
**References:** [ADR-0009](adr/0009-caching-with-sqlite-and-history-api.md), [ADR-0001](adr/0001-monolithic-google-personal-mcp-architecture.md), [ADR-0002](adr/0002-multi-account-architecture.md), [ADR-0008](adr/0008-observability-and-deployment.md)

---

## Why this doc exists

ADR-0009 specifies the *what* of the caching layer in detail — schema, sync protocol, race prevention, eviction, config. It does not specify the *order in which to build it* or the *integration seam* through which cache lookups bypass HTTP. Before splitting the work across the existing implementation tickets (#79, #80, #81, #82, #83), this spike establishes:

1. The phase order — each phase ships green, none leaves a half-migrated seam.
2. Whether the cache ships always-on or gated.
3. The exact seam at which a tool's call into Gmail consults the cache before hitting HTTP.
4. The injection point on `GoogleServer` for the shared `Arc<Cache>`.
5. ADR amendments worth considering once the implementation surfaces drift from the design.

## Phase order

Each phase below maps to one existing implementation ticket (or recommends a new one), produces a green build, and is independently mergeable.

### Phase 0 — `GmailService` seam introduction (new ticket recommended)

**Why first:** No existing ticket covers introducing the wrapper layer that the cache will eventually hide behind. Doing this as a pure refactor *before* cache code lands means later phases touch one wrapper, not 11 `self.gmail` dispatch sites in [`server.rs`](../src/server.rs) plus 6 tool modules plus [`src/gmail/send_email.rs`](../src/gmail/send_email.rs).

**Deliverable:**

- New file `src/gmail/service.rs` defining `GmailService<T>` — owns `Arc<GmailClient<T>>` and `Option<Arc<Cache>>` (initially `None` until Phase 1 lands; `Cache` is a type alias for `()` or a placeholder type for now).
- `GmailService` exposes the three load-bearing methods: `get_thread`, `list_threads`, `get_thread_metadata`. Each currently delegates straight to the existing free functions in [`src/gmail/threads.rs`](../src/gmail/threads.rs).
- `GoogleServer.gmail` field type flips from `Arc<GmailClient<ReqwestRefreshTransport>>` to `Arc<GmailService<ReqwestRefreshTransport>>`.
- Tool modules `search_threads`, `archive`, `trash`, `modify_labels` (take `Arc<GmailClient<T>>`) and `get_thread`, `list_labels` (take `&GmailClient<T>`) flip to the `GmailService` analogue. `src/gmail/send_email.rs` (three `&GmailClient<T>` call sites) flips similarly.
- The free functions in [`src/gmail/threads.rs`](../src/gmail/threads.rs) stay unchanged — they remain `fn(client: &GmailClient<T>, ...)` and `GmailService` methods call them. This preserves the existing test surface.

**Risks:** None — pure refactor, behavior preserved, fully test-covered by the existing tool tests. Confirms the abstraction holds before cache code raises the stakes.

**Estimated size:** S (1 PR; mostly type churn).

### Phase 1 — deps + schema + migrations (#79)

**Deliverable:**

- Add `rusqlite = { version = "0.x", features = ["bundled"] }` and `tokio_rusqlite` to `Cargo.toml`.
- New module tree `src/cache/` with `mod.rs`, `connection.rs`, `migrations.rs`, `migrations/001_initial.sql`.
- `Cache` struct (one connection per account, lazily opened, `DashMap<String, Arc<tokio_rusqlite::Connection>>`).
- Hand-rolled migration framework as specified in ADR-0009 §"Schema migration mechanism".
- File-permission enforcement: `chmod 0600` on create; verify on open via `perm_check::ensure_mode_0600` (the helper already used for token files per ADR-0017).
- Tests:
  - Migration application from version 0 → 1 (fresh DB).
  - Downgrade refusal (schema_version > known migrations → daemon refuses to start).
  - Permission-mode enforcement (creates file mode-600; rejects mode-644).

**Risks:**

- *Cargo.toml diff size:* `rusqlite` with `bundled` statically links SQLite. This is the right call per ADR-0009 — avoids a system dep — but increases build time noticeably. Document in CHANGELOG.
- *Migration test corpus growth:* the test fixture set must grow with every future migration. Lay the directory layout down now so adding `002_*` is mechanical.

**Out of scope this phase:** Any `Cache::lookup_*` or `Cache::insert_*` methods. The DB file is *empty* after Phase 1.

### Phase 2 — on-demand reads with passthrough (new ticket recommended)

**Why a new ticket:** #79 is scoped tightly to schema + deps. Adding read passthrough alongside it would expand #79's scope. #80 covers the *history sync loop*, which is a different concern from cache-miss-fetch. Phase 2 is the bridge.

**Deliverable:**

- `Cache::lookup_thread(account, thread_id) -> Result<Option<ParsedThread>>` and `Cache::insert_thread(account, &ParsedThread)`.
- `GmailService::get_thread` becomes:

  ```rust
  pub async fn get_thread(&self, account: &str, thread_id: &str) -> Result<ParsedThread, Error> {
      if let Some(cache) = &self.cache {
          if let Some(hit) = cache.lookup_thread(account, thread_id).await? {
              metric!(gmcp_cache_hits_total, account = account, kind = "thread");
              return Ok(hit);
          }
          metric!(gmcp_cache_misses_total, account = account, kind = "thread");
          let fresh = gmail::threads::get_thread(&self.client, account, thread_id).await?;
          cache.insert_thread(account, &fresh).await?;
          return Ok(fresh);
      }
      gmail::threads::get_thread(&self.client, account, thread_id).await
  }
  ```

- Same shape for `list_threads` (via `query_cache`) and `get_thread_metadata`.
- Metrics scaffolding for `gmcp_cache_hits_total` and `gmcp_cache_misses_total` (per ADR-0008). Use simple counters; histograms are out of scope.

**Risks:**

- *Cache returns stale data because history sync isn't yet implemented.* This is the load-bearing risk of shipping Phase 2 before Phase 3. **Mitigation:** the cache defaults to `enabled = false` until Phase 5 (see "Feature-flag decision" below); maintainer-only opt-in during the gap means staleness is a maintainer-known accepted risk, not a user-observable bug.
- *`query_cache` race* — Phase 2 ships *without* race prevention (#81 is Phase 4). Window between cache write and external mutation can serve briefly stale results. Acceptable because cache is off-by-default until Phase 4 lands.

### Phase 3 — `history.list` sync loop + 404 reseed (#80)

**Deliverable:** as specified in ADR-0009 §"Sync protocol" — first-touch via `users.getProfile`, incremental sync via `users.history.list`, 404 reseed path. Background sync task per account spawned in `lib::run_server` after `Cache::new` (one task per registered account, joined to a `CancellationToken` on shutdown).

**Risks:**

- *Background task leaks if shutdown is sloppy.* Wire `CancellationToken` from the start; verify with an `--exit-after 5s` smoke test (the audit log + secret-store paths use this same pattern; reuse).
- *History gap (404) at every restart* if `account_state.last_history_id` is not persisted before shutdown — verify the sync writes are durable per WAL semantics.

### Phase 4 — `historyId` watermark race prevention on `query_cache` (#81)

**Deliverable:** as specified in ADR-0009 §"Race-prevention" — `query_cache.fetched_at_history_id` column added via migration `002_*`; read-path validity check; write-path discard-on-advance. New Prometheus counter `gmcp_cache_write_discarded_total`.

**Risks:**

- *Migration test path expands.* Now the test corpus must verify v1 → v2 schema upgrade applies cleanly to a DB populated by Phase 2.
- *Discard rate noisy in tests.* The metric should be near zero in steady state but is concurrency-sensitive; flake-prone if tests share state. Use per-test `tempfile::TempDir`s.

### Phase 5 — LRU eviction + size cap (#82)

**Deliverable:** as specified in ADR-0009 §"TTLs and eviction" — per-account background task on a default 300s tick. Cascading delete order (expired `query_cache` → cold `query_cache` → soft-deleted messages → cold messages). VACUUM throttled to once per cycle. Logged at INFO.

**Risks:**

- *VACUUM holds an exclusive write lock; concurrent tool calls block briefly.* Document the latency spike; if it's > 50ms p99, file a follow-up to incremental-vacuum instead.
- *Soft delete + cascading FK delete is the highest-risk SQL.* Cover with an integration test that populates 10K rows, triggers eviction, verifies no orphan `message_labels` rows survive.

### Phase 6 — `cache_status` + `cache_invalidate` tools (#83)

**Deliverable:** as specified in ADR-0009 §"New tools (operator-facing, low-stakes)". Both tools register in [`src/server.rs`](../src/server.rs) and follow the existing ADR-0016 conventions (`account: String` required, `_untrusted` suffixes where data crosses the trust boundary, etc.).

**Risks:**

- *ADR-0016 amendment:* the locked tool surface in ADR-0016 v0.2 does not list these. ADR-0016 must be amended (its own §"Open / deferred questions" already names cache-status; promote it).

### Phase 7 — flip default to `enabled = true` (new ticket, gates v1.0)

**Deliverable:** one-line config default change in `src/config.rs` + CHANGELOG entry + migration note in the v1.0 release notes. No code change beyond the default.

**Predicate for cutting this ticket:** Phases 0-6 all merged and exercised in a `cargo nextest` integration test that drives a real cache directory through a 1000-call workload without observed staleness.

## Feature-flag decision

**Decision:** ship with `[cache] enabled = false` as the default through Phases 1-6. Flip to `enabled = true` only in Phase 7 (above) after the full implementation is exercised.

This contradicts ADR-0009 §"Config additions" which states `enabled = true`. The contradiction is intentional and temporary: ADR-0009 documents the *long-term* default; the staged-default during build-out protects users of v0.x from any partial-implementation footgun (e.g. Phase 2 ships before Phase 4's race prevention). The flip in Phase 7 is the ADR-aligned end state.

**Operator escape valve while default is off:** `enabled = true` in `config.toml` and the cache runs in full. Useful for maintainer dogfooding.

**Documentation:** `[cache] enabled` description in `config.toml` example must call out the staged-default during Phases 1-6 ("starts at `false` while caching layer is under development; will default to `true` in v1.0").

## Integration seam (the load-bearing question)

ADR-0009's pseudocode example (§"On-demand reads") shows the cache consulted at the *thread-fetching* layer — `self.cache.with_account(account, |conn| async move { ... })` wraps the Gmail call. This is **not** at the HTTP-client layer.

The right seam matches that pseudocode:

- **`GmailClient`** ([`src/gmail/client.rs`](../src/gmail/client.rs)) stays unchanged. It is intentionally shape-agnostic — `authed_get<R: DeserializeOwned>(path, cost)` knows nothing about threads vs labels vs profiles. The cache is shape-aware (threads, message bodies, label catalog), so wedging it into `authed_get` would either (a) leak shape knowledge into the HTTP layer or (b) require key-by-URL caching that can't honor History API invalidation correctly.
- **New `GmailService`** ([`src/gmail/service.rs`](../src/gmail/service.rs) — Phase 0) is the seam. It owns `Arc<GmailClient<T>>` *and* `Option<Arc<Cache>>`. Tools call `service.get_thread(account, id)`; the service decides whether to consult the cache, and on miss falls through to the existing `gmail::threads::get_thread(&self.client, ...)` function.

This keeps three properties intact:

1. **`GmailClient` testability.** All existing `wiremock` tests in [`src/gmail/client.rs`](../src/gmail/client.rs) stay valid — the HTTP wrapper is unchanged.
2. **`gmail::threads::*` testability.** The free functions in [`src/gmail/threads.rs`](../src/gmail/threads.rs) stay pure (no cache parameter), so existing tests don't need a cache-stub injection.
3. **One place to wire the cache.** Phase 2 and later need only edit `GmailService`; tools and call sites don't change again.

The 11 `self.gmail` dispatch sites in `server.rs`, the 6 tool modules, and `src/gmail/send_email.rs` are migrated once, in Phase 0. After that, the seam is invisible to callers.

## `Arc<Cache>` injection point on `GoogleServer`

`GoogleServer` ([`src/server.rs:477`](../src/server.rs#L477)) currently owns `accounts`, `tokens`, `gmail`, `audit`. Post-Phase-0 it owns `accounts`, `tokens`, `gmail: Arc<GmailService<...>>`, `audit`. **No new field on `GoogleServer` is needed** — the cache is owned by the `GmailService`, which `GoogleServer` already holds.

Cache construction happens in [`src/lib.rs:run_server`](../src/lib.rs#L234), between `TokenManager::new` (line 234) and `GoogleServer::new` (line 267). The shape of the wiring (post-Phase-1):

```rust
// existing
let tokens = Arc::new(TokenManager::new(token_states, ...));
let gmail_client = Arc::new(GmailClient::new(gmail_base, tokens.clone(), http_client));

// new in Phase 1 (deps + schema only)
let cache = if cfg.cache.enabled {
    let dir = cfg.cache.dir.expand_tilde()?;
    Some(Arc::new(Cache::new(dir, &loaded_accounts.accounts)?))
} else {
    None
};

// new in Phase 0 (the seam)
let gmail = Arc::new(GmailService::new(gmail_client, cache));

let server = GoogleServer::new(accounts, tokens, gmail, audit);
```

**The `Arc<TokenManager>` snapshot pattern from [ADR-0002](adr/0002-multi-account-architecture.md)** extends naturally: the cache holds its own `Arc` per account; account hot-reload (when it lands) clones the `Arc` snapshot for in-flight reads. No additional indirection needed.

## ADR-0009 amendments to consider

These are flagged for a future maintainer pass, not written in this spike (per the no-ADR-edits convention in `CLAUDE.md`):

1. **§"Config additions" — staged default for `enabled`.** ADR-0009 specifies `enabled = true`. The implementation plan above defaults to `false` during Phases 1-6. Either (a) add a one-line note to ADR-0009 acknowledging the staged default during build-out, or (b) leave ADR-0009 as the long-term target and rely on this plan to document the transition. Recommend (a) for a single source of truth.

2. **§"Integration seam" (new subsection).** ADR-0009's pseudocode example is in tool-fetching shape but does not name the `GmailService` wrapper. A brief subsection naming the wrapper would make the design as-built easier to read for future contributors.

3. **§"Concurrency model" — `tokio_rusqlite::Connection::call` shape.** ADR-0009's pseudocode uses `self.cache.with_account(account, |conn| async move { ... })`. The actual `tokio_rusqlite` API takes a `FnOnce(&mut rusqlite::Connection) -> Result<R>` closure (non-async). The implementation will wrap this with helper methods like `Cache::lookup_thread` that hide the closure shape. Not an ADR amendment per se — just expect the pseudocode to look slightly different in code.

4. **§"New tools" — ADR-0016 cross-reference.** ADR-0009 introduces `cache_status` and `cache_invalidate` but doesn't note that ADR-0016 must be amended to add them to the locked v1.0 tool inventory. Cross-link the two ADRs.

These are listed for visibility; none is a blocker to starting Phase 0.

## Sequencing summary

| Phase | Ticket | Deliverable | Cumulative size |
| ----- | ------ | ----------- | --------------- |
| 0 | _new_ | `GmailService` seam, pure refactor | S |
| 1 | #79   | Deps + schema + migrations | M |
| 2 | _new_ | On-demand reads + passthrough | M |
| 3 | #80   | `history.list` sync loop + 404 reseed | L |
| 4 | #81   | `historyId` watermark race prevention | M |
| 5 | #82   | LRU eviction + size cap | M |
| 6 | #83   | `cache_status` + `cache_invalidate` tools | S |
| 7 | _new_ | Flip `enabled = true` default | XS |

Two new tickets to file when starting the series: **Phase 0** (seam intro) and **Phase 2** (on-demand reads). Phase 7 is the v1.0-gate ticket and only needs filing once Phase 6 merges.

## Out of scope for this spike

- Writing any of the code above. This doc enumerates *what to write*, not *the code itself*.
- Editing ADR-0009. Amendment proposals are listed but not authored — that is a future maintainer decision.
- Filing the two new tickets (Phase 0, Phase 2). The maintainer files them when starting the series; the existing #79-#83 chain is sequenced around them.
- Estimating wall-clock effort. Each phase is sized t-shirt only.
