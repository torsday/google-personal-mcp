# ADR-0019: Data retention and purge — cache age cap, audit deletion opt-in, "right to forget"

**Date:** 2026-05-22
**Status:** Accepted, deferred to v1.0

---

## Context

[ADR-0009](0009-caching-with-sqlite-and-history-api.md) introduces a per-account SQLite cache whose only invalidation primitives are the Gmail History API (event-driven invalidation of mutable label state, and `deleted_at` on `messages` when Gmail itself reports a delete) and LRU eviction triggered when the per-account DB file exceeds `[cache] max_size_bytes_per_account` (default 500 MB).

Both are reactive: they fire on Gmail-side events or local size pressure. Neither answers two operator-facing questions:

- *"I haven't touched mail from 2023 in years. Why is two years of body text still on my disk?"* — LRU only evicts under size pressure; an account that never crosses 500 MB never purges. Low-traffic accounts can hold years of body content indefinitely.
- *"I want to stop storing this account's body content as of today."* — the only current escape is `rm ~/.config/google-personal-mcp/cache/<account>.db`, which loses cache infrastructure entirely.

[ADR-0011](0011-audit-log.md) introduces an append-only JSONL audit log under `~/.config/google-personal-mcp/audit/`, rotated monthly / weekly / daily / by-size, with **no automatic deletion**. The operator manages retention via `find -mtime +N -delete`. This is correct for the audit log's "can't be reconstructed" property — but it leaves operators with deletion-mandated compliance requirements writing their own scripts, with no daemon-side guarantee they don't race the rotation writer.

There is also a cross-cutting GDPR-style "right to forget" gap. When the operator removes an account (say, a shared workspace they no longer have access to), there's no single command that says "purge every trace of this account from cache + tokens + registry." Individual files can be `rm`'d, but the steps are undocumented and easy to half-do.

If no decision were made:

- Cache disk usage on low-traffic, long-lived accounts grows monotonically with no operator-facing remedy short of deleting the whole DB.
- Operators with deletion-mandated regimes have no daemon-supported audit retention path; they implement it themselves and may race the writer.
- "Right to forget" for an account removal is an undocumented multi-step shell operation.

This ADR is **Accepted, deferred to v1.0** because the cache itself ([ADR-0009](0009-caching-with-sqlite-and-history-api.md)) is unimplemented as of v0.x — retention semantics that gate on cache structure must follow the cache. Audit-side retention can land any time; documenting both axes here keeps the retention story coherent.

## Decision

We add three retention primitives:

1. **Cache: time-based body purge** alongside the existing LRU. Configurable max age per body. Defaults OFF (no time cap) to match v0.x behavior; deployment docs recommend a non-zero value (e.g. `90d`) for new installs.
2. **Audit: opt-in automatic deletion** of rotated files older than N days. Configurable. Defaults OFF — preserves [ADR-0011](0011-audit-log.md)'s "no automatic deletion" promise as the safe default.
3. **A `purge_account` tool** that drops the account's cache DB, tokens, and registry entry atomically and writes a single audit record. The audit log itself is **not** modified.

These compose with the existing primitives; they never replace them.

**v1 scope.** Nothing in this ADR ships in v0.x. The cache primitives gate on [ADR-0009](0009-caching-with-sqlite-and-history-api.md)'s cache layer (v1.0). The audit `delete_after_days` config knob *could* be wired in v0.x against the already-shipped audit log, but is deferred to v1.0 so that retention policy is consistent across cache and audit at first introduction. `purge_account` similarly depends on the cache being present to be meaningful.

### Cache: body age cap

Extend the [ADR-0009](0009-caching-with-sqlite-and-history-api.md) `messages` schema with a nullable column:

```sql
ALTER TABLE messages ADD COLUMN purged_at INTEGER;  -- ms epoch; set when body was time-purged
```

`purged_at` is distinct from `deleted_at` (Gmail-side delete, populated by `history.list`) and `fetched_at` (cache age). It means "we still know this message exists by ID, but we no longer hold the body locally — fetch from Gmail on next read."

Extend the [ADR-0006](0006-config.md) `[cache]` section:

```toml
[cache]
# ...existing keys per ADR-0009...
body_max_age_days = 0           # 0 = disabled; > 0 = purge bodies older than N days
purge_interval_seconds = 86400  # how often the body-purge sweep runs (default daily)
```

The per-account eviction background task (defined in [ADR-0009](0009-caching-with-sqlite-and-history-api.md)) gains a body-purge phase, run before the LRU phase:

1. If `body_max_age_days > 0`:
   ```sql
   UPDATE messages
      SET body_text = NULL, body_html = NULL, snippet = NULL,
          attachments_json = NULL, purged_at = :now
    WHERE internal_date < (:now - :max_age_ms)
      AND purged_at IS NULL;
   ```
2. Soft-deleted rows are purged on a tighter floor: any row with `deleted_at IS NOT NULL AND deleted_at < (:now - 7*86400_000)` has its body nulled regardless of `body_max_age_days`. A Gmail-side-deleted message that's been gone a week has no value sitting in cache.
3. `VACUUM` runs at the end of the eviction cycle, once per sweep, after both the body-purge and LRU phases. Never twice.

The metadata row (id, thread_id, internal_date, headers_json, has_attachments) **stays**. Dropping the metadata would force a full thread re-list to rediscover the message; keeping it lets the cache rehydrate a body on demand at one quota call. Body bytes dominate the on-disk footprint; metadata is small.

### Audit: opt-in automatic deletion

Extend the [ADR-0006](0006-config.md) `[audit]` section (atop the already-shipped keys per [ADR-0011](0011-audit-log.md)):

```toml
[audit]
# ...existing keys per ADR-0011...
delete_after_days = 0   # 0 = disabled (default); > 0 = delete rotated files older than N days
```

When `delete_after_days > 0`, a daily background task identifies candidate rotated files by **matching the rotation filename pattern** (per [ADR-0011](0011-audit-log.md)'s filename table — `audit-YYYY-MM.log`, `audit-YYYY-Wnn.log`, etc.), parses the period from the filename, computes the file's age from that, and `unlink(2)`s any file older than the threshold. The currently-open rotation file is identified by pattern (not by mtime) and **always excluded**, even if its computed period age happens to be ≥ threshold.

Defaults to **0 (disabled)** because:

- Per [ADR-0011](0011-audit-log.md), the audit log is "the only data that cannot be reconstructed." Automatic deletion is destructive and irreversible.
- Most operators will not have compliance requirements that mandate deletion. The minority that do will opt in deliberately.
- A non-zero default would silently violate operator expectations during an upgrade.

Deletion events emit a structured tracing log line ([ADR-0008](0008-observability-and-deployment.md)) at INFO: `audit_retention_purge file=<path> age_days=<n>`. The daemon does **not** write an audit-log record about the audit-log deletion (would force creating the next rotation purely to record its own forgetting).

### `purge_account` tool — "right to forget"

A new operator-facing tool extending [ADR-0016](0016-tool-surface-and-conventions.md)'s surface:

```rust
purge_account(account: String, dry_run: bool, confirm: String) -> PurgeResult
```

Effect (when not `dry_run`):

1. Drop `~/.config/google-personal-mcp/cache/<account>.db` if it exists.
2. Drop `~/.config/google-personal-mcp/tokens/<account>.json` — operator must re-`auth add` to re-enable this account.
3. Remove the account entry from `~/.config/google-personal-mcp/accounts.toml` (per [ADR-0002](0002-multi-account-architecture.md)).
4. Write **one** audit record: `tool: "purge_account"`, `outcome: "success"`, `extra: { account, cache_db_existed: bool, token_existed: bool, registry_entry_existed: bool }`. This record persists — it is evidence that the purge happened.
5. **Audit log files are not modified.** Records about the purged account remain in their rotation files. This is intentional: tampering with the audit log even for "good" reasons breaks the [ADR-0011](0011-audit-log.md) trust model.

`confirm` must be the literal string `"yes-purge-<account>"`. The model cannot trip this accidentally; the operator pasted that string deliberately, account name embedded.

Per [ADR-0011](0011-audit-log.md)'s `ToolMetadata` trait: `is_destructive() = true` (the audit pre-fsync invariant applies, so the intent is durable on disk before any file is touched). `requires_explicit_account() = true`. The tool is **not** eligible for [ADR-0013](0013-cross-account-fan-out.md) fan-out — `account = "*"` is rejected. Per-account, deliberate, slow path.

Idempotency: a `purge_account` against an already-absent account succeeds (returns `cache_db_existed: false, token_existed: false, registry_entry_existed: false`) and still writes one audit record. Re-running is safe.

`dry_run = true` reports what *would* be deleted without touching disk; still writes an audit record (with `outcome: "dry_run"` per [ADR-0012](0012-idempotency-and-dry-run.md)).

### Gmail-side delete propagation

[ADR-0009](0009-caching-with-sqlite-and-history-api.md) already populates `messages.deleted_at` via the History API's `messagesDeleted[]` events. This ADR adds:

- `deleted_at`-respect in the body-purge phase above: soft-deleted bodies are purged at 7 days regardless of `body_max_age_days`.
- Two new counters on [ADR-0009](0009-caching-with-sqlite-and-history-api.md)'s `cache_status` tool: `bodies_purged_total` and `bodies_purged_due_to_delete_total` (last hour). The operator can confirm Gmail-side deletes are propagating.

The cache does **not** invalidate `query_cache` rows just because a downstream message was body-purged. `query_cache` invalidation is governed by the `fetched_at_history_id` watermark per [ADR-0009](0009-caching-with-sqlite-and-history-api.md). A body-purged message in a cached search result simply triggers a re-fetch on `get_thread` — which is exactly what an LRU-evicted row would do. The two phenomena are indistinguishable from the read path.

### What this ADR does NOT do

- It does not retroactively delete bodies older than the configured threshold on first upgrade; the purge task discovers them on its first sweep. No special migration path.
- It does not introduce `audit_purge` as a tool. Audit deletion is operator-initiated either via config (`delete_after_days`) or `rm`. **Never** via a model-callable tool. Per [ADR-0011](0011-audit-log.md), the model cannot rewrite its own history.
- It does not address backups. Operator's backup strategy is unchanged ([ADR-0011](0011-audit-log.md) §Backup and durability).
- It does not address per-thread purge. `cache_invalidate(account, scope: "all" | "queries" | "labels")` per [ADR-0009](0009-caching-with-sqlite-and-history-api.md) does not currently support thread-granular invalidation; a follow-up ADR can add that scope if a real use case emerges. `purge_account` covers the account-level boundary.
- It does not chain audit records cryptographically. [ADR-0011](0011-audit-log.md) deferred that; this ADR does not revisit it.

## Options Considered

### Cache retention shape

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Status quo: LRU only | Simplest; no new code | Low-traffic accounts hold body content indefinitely; no operator-facing "purge old content" lever |
| (b) Time-based total deletion (drop the row entirely) | Aggressive; smallest on-disk footprint | Forces full thread re-list to rediscover messages; expensive on next access |
| **(c) Time-based body-only purge, keep metadata** (chosen) | Body is the bulk of disk usage; metadata is tiny; rehydration is one quota call | Slightly more complex read path (body-null handling) |
| (d) Time-purge running through the LRU path | Reuses one code path | LRU and time-purge have different signals; conflating them risks over-eviction during traffic spikes |

We choose (c). Body bytes are the dominant on-disk cost; metadata-only retention bridges the "we know it exists, ask Gmail for the content" pattern that already governs cache misses. Read paths already handle that case.

### Audit retention default

| Option | Pros | Cons |
| --- | --- | --- |
| **(e) Default 0 (disabled) — operator opts in** (chosen) | Safe: never silently deletes irreplaceable data; preserves [ADR-0011](0011-audit-log.md)'s promise | Compliance-bound operators must read the docs and configure |
| (f) Default to a long period (e.g. 365 days) | Reasonable for most cases | Surprise behavior on upgrade for operators who weren't aware automatic deletion existed |
| (g) Reject the feature; force `find -delete` | Smallest daemon surface | Operator-written `find` scripts can race the rotation writer; provides no scaffolding for the compliance use case |

We choose (e). [ADR-0011](0011-audit-log.md)'s "no automatic deletion" promise is preserved as the default; the feature exists for operators who genuinely need it.

### "Right to forget" surface

| Option | Pros | Cons |
| --- | --- | --- |
| (h) Document the manual steps; no tool | Smallest surface area; nothing new to maintain | Multi-step; easy to half-do (forget the token file or the registry entry) |
| **(i) `purge_account` tool with `confirm:` guard** (chosen) | One atomic operation; auditable via the record it writes; idempotent | Adds a destructive tool surface; mitigated by literal-string `confirm:` guard and `requires_explicit_account()` |
| (j) `purge_account` that *also* rewrites the audit log | Truly removes all traces | Breaks [ADR-0011](0011-audit-log.md)'s tamper-resistance promise; not worth it |

We choose (i). The audit log retains the historical record correctly; the cache + token + registry are removed atomically; the operator-imposed `confirm:` literal eliminates the "model accidentally called this" failure mode.

## Consequences

**Positive:**

- Operator gains documented retention levers for both cache and audit, both opt-in by default. Existing v0.x behavior is unchanged for operators who never touch the new config.
- "Right to forget" becomes a one-tool operation rather than a multi-step shell exercise.
- Cache body purge is metadata-preserving, so post-purge access is graceful (one quota call to rehydrate) rather than punitive (full thread re-list).
- Compliance-bound operators (deletion-mandate regimes) get daemon-supported audit retention without writing scripts that might race the writer.
- The audit log's tamper-resistance promise from [ADR-0011](0011-audit-log.md) is preserved — `purge_account` does not modify audit files.
- `cache_status` gains body-purge diagnostics, surfacing whether retention is working as configured.

**Negative:**

- Three new config knobs to document and test (`cache.body_max_age_days`, `cache.purge_interval_seconds`, `audit.delete_after_days`).
- One new SQL column (`purged_at`) — a v1→v2 cache migration per [ADR-0009](0009-caching-with-sqlite-and-history-api.md)'s migration mechanism. Trivial: add a nullable column. No backfill.
- `purge_account` is a new destructive tool; its existence enlarges the destructive surface. The `confirm:` literal-string guard plus `is_destructive() / requires_explicit_account()` metadata is the structural defense.
- Body-purge sweeps add SQLite write activity to the per-account eviction task. Negligible at default sweep interval (daily) for personal-scale data; operators with very large caches and aggressive `body_max_age_days` could see noticeable I/O on each sweep. Documented; not solved here.

**Risks:**

- *Risk:* `delete_after_days` background task races the rotation writer at a period boundary and deletes a file that's just been newly opened.
  *Mitigation:* Audit retention task identifies candidate files by matching the rotation filename pattern ([ADR-0011](0011-audit-log.md) filename table) and computes file age from the **period in the filename**, not from mtime. The currently-open rotation file is always excluded by pattern. mtime is never the primary signal.

- *Risk:* Operator sets `body_max_age_days = 1` (or similar very-aggressive value) on a heavy-traffic account and quietly burns Gmail quota re-hydrating bodies they're actively using.
  *Mitigation:* Documented as an operator footgun. The cost is one extra Gmail quota call per re-read, not data loss. `cache_status.bodies_purged_total` exposes the purge rate so operators can detect the misconfiguration.

- *Risk:* `purge_account` is called by a misbehaving or compromised model.
  *Mitigation:* `confirm: "yes-purge-<account>"` literal-string guard. Even reaching the tool surface, the model has to produce the exact string with the account name embedded. The host application is expected to expose `purge_account` behind an additional human-confirmation gate; documented in `README` under destructive operations.

- *Risk:* Body purge is conflated with Gmail-side deletion, and downstream tools treat a body-null message as deleted.
  *Mitigation:* `purged_at` and `deleted_at` are distinct columns. `get_thread` distinguishes "body absent due to purge → re-fetch transparently" from "message is Gmail-side deleted → propagate to caller". Tests cover both paths when the cache implementation lands.

- *Risk:* Operators expect `purge_account` to delete audit records too and are confused when historical records remain.
  *Mitigation:* `purge_account` documentation (tool description, README, response shape) is explicit: audit records persist by design; the audit log is the evidence trail, not data subject to forgetting.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — single-operator daemon trust model
- [ADR-0002](0002-multi-account-architecture.md) — per-account scope; `purge_account` removes from the registry
- [ADR-0006](0006-config.md) — `[cache]` and `[audit]` config sections
- [ADR-0008](0008-observability-and-deployment.md) — tracing log target for retention events; `cache_status` metrics
- [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — amended by this ADR (adds `purged_at` column, body-purge phase to the eviction task, two `cache_status` counters)
- [ADR-0011](0011-audit-log.md) — amended by this ADR (adds `delete_after_days` to `[audit]`; preserves the "no default automatic deletion" promise)
- [ADR-0012](0012-idempotency-and-dry-run.md) — `purge_account` accepts `dry_run`
- [ADR-0013](0013-cross-account-fan-out.md) — `purge_account` is **not** fan-out-eligible
- [ADR-0016](0016-tool-surface-and-conventions.md) — `purge_account` follows the tool conventions there
- Issue [#88](https://github.com/torsday/google-personal-mcp/issues/88) — origin (open-questions queue, [ADR-0000](0000-adr-process.md))
