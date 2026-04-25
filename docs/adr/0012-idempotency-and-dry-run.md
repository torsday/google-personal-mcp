# ADR-0012: Dry-run preview + automatic send-deduplication for destructive operations

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

The destructive Gmail operations exposed by this MCP — `send_email`, `archive_thread`, `trash_thread`, `batch_archive`, `modify_thread_labels` — are the highest-blast-radius surface for an LLM agent. Two specific failure modes recur in agentic systems:

1. **Reasoning errors leading to over-broad action.** Model says "archive everything from `noreply@`" and constructs a query that catches `notifications@` too. With no preview, the operation is irreversible (well, archive is recoverable; trash less so; *send* is permanent).
2. **Network blips causing duplicate sends.** Model calls `send_email`, the call appears to fail (timeout, transient 5xx), the model retries, the email is sent twice. Gmail has no native idempotency-key support for `messages.send`. The operator gets duplicate emails, possibly to a customer.

The cost of either failure mode is high: a misplaced send to the wrong recipient is a real-world business problem; a duplicate "I'd like to schedule a meeting" email is at minimum embarrassing. The cost of preventing them is low — both safety nets are simple to implement.

These two safety nets address distinct failures:

- **Dry-run** addresses *intent* errors — the operation was wrong before it executed.
- **Send-dedup** addresses *transport* errors — the operation was correct but executed more than once.

If no decision were made, both classes of failure go undetected by this MCP, putting the burden on the consumer (LLM client) to implement them. Consumers don't.

## Decision

We will add:

1. An optional `dry_run: bool` parameter to every destructive tool.
2. **Automatic** in-process deduplication for `send_email` based on a content hash + sliding window.
3. Structured (per-thread, with reason) outcomes from `batch_archive` regardless of dry-run.

### `dry_run` parameter

Added to: `archive_thread`, `trash_thread`, `batch_archive`, `modify_thread_labels`, `send_email`.

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArchiveThreadParams {
    #[schemars(description = "Thread ID to archive")]
    pub thread_id: String,
    #[schemars(description = "Optional account alias (default account if omitted)")]
    pub account: Option<String>,
    #[schemars(
        description = "If true, return what WOULD be archived without making changes. \
                       Defaults to false."
    )]
    #[serde(default)]
    pub dry_run: bool,
}
```

When `dry_run = true`, the tool:

1. Validates parameters (would return `Error::AccountNotFound` etc. just as a real call would).
2. Resolves what would happen (e.g., for `archive_thread`: confirms the thread exists, reports current label state).
3. Returns a structured "would have done X" response.
4. Does **not** call any Gmail mutating endpoint.
5. Records the call in the audit log with `extra.dry_run = true` and `outcome = "preview"` (per [ADR-0011](0011-audit-log.md)).

For `send_email(dry_run=true)`:
- Compose the RFC 2822 message (validates address syntax, header injection, body length).
- Return the composed message preview: `{ to, subject, body_preview (first 500 chars), thread_id_target, would_send_at: now }`.
- Does NOT contact Gmail's `messages.send`.

For `batch_archive(dry_run=true)`:
- Resolve every thread ID against cache (or `messages.get` if cache miss); confirm exists & current INBOX state.
- Return `{ would_archive: [ids in INBOX], already_archived: [ids not in INBOX], not_found: [ids that don't exist or wrong account] }`.
- Does NOT call `threads.modify`.

### Send deduplication (always on, configurable window, persisted across restart)

For `send_email`, we compute a content hash and reject duplicates within a sliding window. Dedup state is **persisted in the per-account SQLite cache** (per [ADR-0009](0009-caching-with-sqlite-and-history-api.md)), so the protection survives daemon restart. This closes the silent failure gap where a network blip + retry that straddles a daemon restart would otherwise double-send.

Schema (added to the cache DB):

```sql
CREATE TABLE send_dedup (
    hash         TEXT PRIMARY KEY,        -- sha256(account || to || cc_sorted || subject || body || in_reply_to)
    message_id   TEXT NOT NULL,           -- the prior send's message id
    thread_id    TEXT,
    sent_at_ms   INTEGER NOT NULL
);
CREATE INDEX idx_send_dedup_sent_at ON send_dedup (sent_at_ms);
```

Pruning is bounded: every insert deletes rows older than `[idempotency] send_dedup_window_seconds`. Read path checks both an in-memory hot cache (sub-millisecond, common case) and the SQLite table (~1ms, post-restart fallback) — in-memory is populated lazily from SQLite on first lookup.

Why both layers: in-memory catches the high-frequency same-process retry path with zero IO; SQLite catches the rare cross-restart case. The in-memory cache has the same bound (`send_dedup_max_entries`) as before; SQLite is unbounded between prunes but bounded in practice by `send_dedup_window_seconds × send_rate`.

Implementation sketch:

```rust
struct SendDedup {
    /// SHA-256(account || to || cc_sorted || subject || body || in_reply_to)
    /// Hex-encoded.
    fn dedup_hash(account: &str, params: &SendEmailParams) -> String { ... }
}

struct DedupCache {
    /// Map: hash -> (sent_at_epoch_ms, message_id, thread_id)
    /// Pruned in O(N) on each insert; bounded by window so size stays small.
    entries: Mutex<HashMap<String, DedupEntry>>,
    window_ms: u64,
}

impl DedupCache {
    async fn check_or_record(&self, hash: String, send_fn: impl FnOnce() -> Result<SentMessage>)
        -> Result<DedupResult>
    {
        let now_ms = ...;
        let mut entries = self.entries.lock().await;
        self.prune_expired(&mut entries, now_ms);

        if let Some(prev) = entries.get(&hash) {
            tracing::warn!(
                hash = %hash,
                prev_sent_at_ms = prev.sent_at_ms,
                prev_message_id = %prev.message_id,
                "send_email dedup: refusing duplicate send"
            );
            return Ok(DedupResult::Duplicate(prev.clone()));
        }
        // Else: send for real, record on success
        let sent = send_fn().await?;
        entries.insert(hash, DedupEntry {
            sent_at_ms: now_ms,
            message_id: sent.id.clone(),
            thread_id: sent.thread_id.clone(),
        });
        Ok(DedupResult::Sent(sent))
    }
}
```

Hash inputs:
- `account` — different account → different send (intentional)
- `to` — exact match required
- `cc` — sorted (order doesn't change semantics)
- `subject`, `body` — exact
- `in_reply_to` — exact (different threads = different sends)

`bcc` is **not** in the hash because Gmail injects `Bcc:` differently and we don't want it influencing dedup negatively. (Reconsider if a real case shows up.)

### Configuration (`[idempotency]` in `config.toml`, see [ADR-0006](0006-config.md))

```toml
[idempotency]
send_dedup_window_seconds = 60         # 0 to disable; default catches the network-blip retry case
send_dedup_max_entries = 10_000        # bounds memory usage
include_in_reply_to_in_hash = true     # set false if you want "same body, different threads" to also dedup
```

The window default of 60s is the empirically common timeout/retry window for HTTP transports; less than this and we miss the intended case, more than this and a legitimate "send the same announcement to two people" scenario could be incorrectly rejected.

### `DedupResult::Duplicate` response shape

Returned to the caller as a normal success with a clear `dedup` flag:

```json
{
  "id": "1928cba93f0a4a",
  "thread_id": "1928cba90000",
  "dedup": {
    "deduplicated": true,
    "sent_at_iso": "2026-04-25T14:31:48.123Z",
    "reason": "Identical send within 60s window; returning prior message_id"
  }
}
```

The agent can detect `dedup.deduplicated = true` and stop retrying. The audit log records both the original send and the deduplicated suppressed-call (with `extra.deduplicated = true`).

### `batch_archive` always-structured output

Even with `dry_run = false`, the response now includes per-thread outcomes (never just a count):

```json
{
  "succeeded": [
    { "thread_id": "...", "labels_after": ["IMPORTANT"] }
  ],
  "skipped": [
    { "thread_id": "...", "reason": "not_in_inbox", "current_labels": ["..."] }
  ],
  "failed": [
    { "thread_id": "...", "error_kind": "NotFound", "error": "..." }
  ]
}
```

The model gets the same structured information whether dry-run or not. This catches the "model thought 50 threads were archived but actually 12 were already-archived no-ops" misunderstanding.

## Options Considered

### Dry-run shape

| Option | Pros | Cons |
| --- | --- | --- |
| (a) No dry-run | Smaller surface | High-blast-radius operations have no preview path |
| **(b) Per-tool `dry_run: bool` parameter** (chosen) | Caller chooses per call; explicit; uniform shape across tools | Adds a parameter to every destructive tool's schema |
| (c) Dedicated preview tools (`archive_thread_preview`, etc.) | Tool surface separates safe-from-unsafe | Doubles tool count; harder to keep in sync; the "preview" tool drifts from the real one |
| (d) Server-wide "dry-run mode" via config | Simple; safe-by-default deploy | Not per-call; agent can't decide preview-vs-execute on the fly |
| (e) Implicit dry-run when called with insufficient confirmation context | "Smart" | Magical; behavior depends on indirect signals; hard to reason about |

### Send dedup

| Option | Pros | Cons |
| --- | --- | --- |
| (f) No dedup | Simplest | Network-blip retries cause duplicate sends |
| **(g) Automatic in-process content-hash dedup with sliding window** (chosen) | Catches the realistic failure mode; transparent to callers; configurable | Memory-state (lost on restart); does NOT survive process boundaries (HTTP-mode multi-process scenarios — out of scope for v1) |
| (h) Idempotency-key parameter (caller provides) | Caller controls what counts as duplicate; survives across restarts if persisted | Requires caller cooperation; LLM agents don't reliably generate stable keys; defeats the "automatic safety net" goal |
| (i) Persistent dedup state in SQLite (cache DB) | Survives restart | More moving parts; gives false sense of dedup across restarts (tokens or sessions might intentionally re-send); the 60s window is irrelevant to a process restart |
| (j) Use Gmail's `clientReferenceId` (if it existed) | Native | It does not exist for `messages.send` |

We choose (b) and (g). Option (h)'s "caller provides key" is a real idiom (Stripe-style idempotency keys) but doesn't fit the LLM-agent caller model. Option (g)'s short window addresses the actual observed failure mode (transport retry) without surprising the caller in legitimate "I want to send this same message twice" cases (which would space them past the window or differ in some field).

### `batch_archive` shape

| Option | Pros | Cons |
| --- | --- | --- |
| (k) Counts only (status quo) | Minimal payload | Loses per-thread outcome; model can't recover from partial failures |
| **(l) Always-structured per-thread outcome** (chosen) | Model can react to skipped/failed entries | Larger response payload (proportional to batch size) |
| (m) Counts by default, structured if requested | Smaller default response | Two response shapes for the same tool — confusing |

We choose (l). The payload size is proportional to the batch (already small for typical batches; if batches grow huge, the model should be using pagination, not a single batch call).

## Consequences

**Positive:**

- The model can preview destructive operations with a single parameter flip — no separate tool, no awkward dance.
- Send dedup catches the "blip + retry = duplicate" failure mode automatically with no caller cooperation.
- `batch_archive`'s structured output means the model can intelligently report partial outcomes ("archived 47, 3 were already archived, 0 failed").
- Audit log captures `dry_run = true` distinctly from real ops; review-time discrimination is easy.
- Config-tunable dedup window means specific use cases (legit identical sends > 60s apart) work; common case is protected.
- The dedup layer logs at WARN when it kicks in — operator gets a real signal that something tried to double-send.
- All of this is uniformly applied across the destructive surface; no "this tool has dry-run, that one doesn't" inconsistency.

**Negative:**

- Every destructive tool's schema gains a parameter. Tool descriptions get slightly longer.
- The dedup state is in-process; restart loses it. Documented; it's the correct tradeoff for the failure mode addressed.
- `batch_archive` response payload grows with batch size. Acceptable; the model is already paginating large operations via search.
- Implementing `dry_run` for each tool requires the tool to read current state (e.g., what labels does this thread currently have) — extra read calls when previewing. With cache (per [ADR-0009](0009-caching-with-sqlite-and-history-api.md)) these are cheap.
- The `send_email` dry-run does **not** confirm the recipient address is valid (Gmail accepts non-existent addresses and bounces later). Documented.

**Risks:**

- *Risk:* Send dedup wrongly suppresses a legitimate "send the same message twice intentionally" case (e.g., resending an announcement after a fix).
  *Mitigation:* 60s window is deliberately short; intentional resends generally happen minutes/hours apart. Operator can disable via `send_dedup_window_seconds = 0`. Dedup response includes `prev_sent_at` so caller can detect and choose to differ a field (add a `[v2]` suffix to subject) and retry.
- *Risk:* Dry-run gives a false sense of safety because the actual subsequent `dry_run = false` call could be different (different params, race with cache state).
  *Mitigation:* Documented limitation. Dry-run is preview, not promise. For batch operations the dry-run output and actual output use the same code path with different sinks (one writes to Gmail, one returns the would-be result).
- *Risk:* Dedup hash collision (different content hashes to the same value).
  *Mitigation:* SHA-256; collision probability astronomically low.
- *Risk:* Memory footprint of dedup cache grows unboundedly.
  *Mitigation:* `send_dedup_max_entries` (default 10K) bounds it. Pruning happens on each insert.
- *Risk:* The `dry_run` parameter looks like noise on every tool description, harming MCP-client UX.
  *Mitigation:* Tool descriptions are deliberate; the `dry_run` line is consistent across destructive tools and absent on read tools (creating a recognizable pattern).
- *Risk:* `batch_archive` per-thread outcome inflates response size for large batches.
  *Mitigation:* Documented; `[messages] max_batch_response_threads` (default 1000) caps with a structured "...and N more, omitted" trailer.

## References

- [ADR-0002](0002-multi-account-architecture.md) — `account` parameter combined with `dry_run` lets cross-account previews exist
- [ADR-0005](0005-error-model.md) — dry-run still returns `Error::AccountNotFound` etc. (validation runs)
- [ADR-0006](0006-config.md) — `[idempotency]` config section
- [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — cached label state used by dry-run to compute "already in this state" without an API call
- [ADR-0011](0011-audit-log.md) — `extra.dry_run` and `extra.deduplicated` distinguish in audit
- [ADR-0013](0013-cross-account-fan-out.md) — fan-out tools always require explicit per-call dry-run on each account (no implicit cross-account dry-run)
- Gmail [`messages.send` API](https://developers.google.com/workspace/gmail/api/reference/rest/v1/users.messages/send) — confirms no native idempotency-key support
