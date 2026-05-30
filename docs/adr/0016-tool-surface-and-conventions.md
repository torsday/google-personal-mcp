# ADR-0016: Tool surface and parameter conventions

**Date:** 2026-05-15
**Status:** Accepted
**Amended by:** [ADR-0027](0027-v1-1-surface-refinements.md) §§2–5 (optional `fields_mask` on read tools; `format` on `get_thread`/`get_message`; flat top-level `_cost_units`/`_cache_hit`/`_upstream_ms` on every response; batch default `mode: "failures_only"`)

---

## Context

Fifteen prior ADRs govern *how* tools behave (errors, retries, audit, dry-run, fan-out, versioning) without ever defining *what* tools exist. The result: the README lists 8 tools, individual ADRs scatter-mention 12+ others (`audit_summary`, `cache_status`, `cache_invalidate`, `list_attachments`, `download_attachment`, `mcp_status`, `list_accounts`, `parse_forwarded_attachment`, hypothetical `get_full_body`), and there is no canonical naming or parameter convention. [ADR-0015](0015-tool-versioning-policy.md) enforces a contract that has never been written down. [ADR-0007](0007-testing-strategy.md)'s snapshot test will be the de facto registry by accident.

A consumer reading the MCP tool list cannot tell whether the ID parameter on a thread tool is `id`, `thread_id`, or `threadId`. They cannot tell whether a paging tool takes `max_results`, `limit`, or `count`. The host LLM has to guess. Inconsistency wastes tokens and produces tool-call errors that are entirely avoidable.

If no decision were made, the v1 tool set would solidify by accumulation — whichever convention each `tools.rs` author picks first.

## Decision

### Tool inventory (v1, Gmail-only)

Exactly these tools are in v1. New tools require an ADR amendment.

| Tool | Kind | Scope | Notes |
| --- | --- | --- | --- |
| `list_accounts` | read | none | Returns aliases registered in `accounts.toml` (per [ADR-0002](0002-multi-account-architecture.md)). |
| `search_threads` | read | gmail.modify | Gmail query syntax. Paginated. Rich per-thread metadata so the host rarely needs a follow-up fetch. |
| `get_thread` | read | gmail.modify | Full thread with all messages. |
| `list_labels` | read | gmail.modify | All labels visible to the account. |
| `archive_thread` | write | gmail.modify | Removes `INBOX` label from one thread. |
| `batch_archive` | write | gmail.modify | Removes `INBOX` from 1..=100 threads in one call. |
| `trash_thread` | write | gmail.modify | Moves one thread to trash (recoverable 30d). |
| `batch_trash` | write | gmail.modify | Moves 1..=100 threads to trash. |
| `modify_thread_labels` | write | gmail.modify | Add and/or remove arbitrary labels on one thread. |
| `batch_modify_thread_labels` | write | gmail.modify | Apply the same `add` / `remove` label sets to 1..=100 threads. |
| `send_email` | write | gmail.send | Plain text body in v1; multipart deferred. Supports `to`, `cc`, `bcc`, threading. |

### Deferred to post-v1 (do not implement until the use case is real)

- `mcp_status` — see [ADR-0014](0014-status-introspection-tool.md). External-consumer concern.
- `audit_summary`, `cache_status`, `cache_invalidate` — operational tools that the operator can answer with `journalctl` and `ls` for v1.
- `list_attachments`, `download_attachment` — attachment handling deferred until a real use case appears.
- Cross-account fan-out (`account = "*"`) — see [ADR-0013](0013-cross-account-fan-out.md). Deferred until the operator has manually fanned out a query enough times to feel the pain.
- Calendar, Contacts, Tasks tools — Phase 2+.

### Naming convention

- Tool names: `snake_case`. Verb-first for actions (`archive_thread`), noun-first only for pure lists (`list_labels`, `list_accounts`).
- Verb vocabulary, in preference order: `list`, `get`, `search`, `create`, `modify`, `archive`, `trash`, `delete`, `send`. No synonyms (`fetch`/`retrieve`/`find` → use `get`/`search`). No `update` (use `modify`).
- Pluralization follows the operation: `list_labels` returns many, `get_thread` returns one. Batch variants are prefixed `batch_` and take an array (`batch_archive`).

### Parameter convention

| Parameter | Type | Rules |
| --- | --- | --- |
| `account` | `String` | **Required on every tool that touches Google.** Never defaults to "the only account" — defaulting masks multi-account bugs. `list_accounts` itself takes no `account`. |
| `query` | `String` | Gmail query syntax (passed through as-is). Never `q`, never `search`. |
| `max_results` | `u32` | Paging size. Never `limit`, `count`, `top`, `page_size`. Default values are tool-specific and documented in the tool description. |
| `page_token` | `Option<String>` | Opaque cursor from a prior call. Mirror Google's name. |
| `thread_id`, `message_id`, `label_id` | `String` | Opaque IDs. Use the resource name in the parameter — never bare `id`. |
| `thread_ids: Vec<String>` | array | Batch IDs. Plural matches the array. |
| `dry_run` | `bool` | Required on write tools per [ADR-0012](0012-idempotency-and-dry-run.md). Default `false`. |
| `add_label_ids`, `remove_label_ids` | `Vec<String>` | On `modify_thread_labels`. Mirror Gmail API field names. |

Booleans default to `false`. Optional parameters use `Option<T>`. Strings are never `""` to mean "absent" — use `Option<String>`.

### Response convention

Each tool returns a typed Rust struct that `serde` serializes to a JSON object with `snake_case` keys. Two response shapes:

- **Single-resource:** the resource directly (e.g. `get_thread` returns a `Thread` object). No wrapper.
- **Listing:** `{ "items": [...], "next_page_token": "..." | null, "total_estimate": null | u64 }`. Always `items`, never `results` / `data` / `threads`.

Errors follow [ADR-0005](0005-error-model.md) and never appear inline in a success response. A partial result (some IDs failed in a batch) is itself a structured success — see batch tools below.

### Batch response convention

Batch tools return per-item success/failure:

```json
{
  "results": [
    { "id": "...", "ok": true },
    { "id": "...", "ok": false, "error": { "code": "NOT_FOUND", "message": "..." } }
  ]
}
```

Never short-circuit on first error in a batch. The caller decides what partial success means.

### Tool description convention

Every tool's doc comment (rendered into the MCP description the host LLM sees):

1. One imperative sentence describing what the tool does.
2. One sentence on side effects (read / write / destructive). Write tools name the Gmail label or operation explicitly ("Removes the `INBOX` label").
3. One sentence on cost when relevant (quota, batch size limits).
4. **For tools that surface email body content:** the standard untrusted-content disclaimer from [ADR-0018](0018-email-content-trust.md).

Keep descriptions under 80 words. The host LLM pays tokens for every one of these on every call.

### Tool granularity rule

Tools are **low-level primitives that mirror Google's API verbs.** A tool that summarizes, classifies, or composes is the consumer's job, not this MCP's. The temptation list — `summarize_thread`, `find_emails_about_meeting`, `draft_reply` — is explicitly out of scope and stays out of scope. See [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) tool philosophy.

### Per-tool schemas (v0.2 anchor)

The schemas below are concrete commitments for v0.2. They override or extend the general conventions above. Untrusted-content suffixes follow [ADR-0018](0018-email-content-trust.md). Subsequent changes follow the additive-only rule that activates at v1.0 ([ADR-0015](0015-tool-versioning-policy.md)).

#### `search_threads`

**Request:**

- `account: String` *(required)*
- `query: String` — Gmail query syntax, passed through to the API
- `max_results: u32` — default 25, max 100
- `page_token: Option<String>`

**Response:** listing envelope. Each `items[i]` is a `ThreadSummary`:

- `thread_id: String` — Gmail's thread `id`
- `snippet_untrusted: String` — Gmail's `snippet`
- `history_id: String` — Gmail's `historyId`
- `subject_untrusted: String` — derived from the latest message's `Subject` header
- `from_untrusted: String` — derived from the latest message's `From` header
- `internal_date: String` — RFC 3339 UTC, derived from the latest message's `internalDate`
- `label_ids: Vec<String>` — derived: union of all `labelIds` across messages in the thread
- `message_count: u32` — derived: count of messages in the thread
- `size_estimate: u64` — derived: sum of message-level `sizeEstimate` in bytes

**Implementation: hydration via `threads.get`.** Gmail's `users.threads.list` returns only `{id, snippet, historyId}` per item — no subject, sender, date, labels, or message count. To deliver the rich `ThreadSummary` above, the implementation issues one `threads.list` call followed by `max_results` parallel `threads.get(format=metadata, metadataHeaders=From,Subject,Date)` calls. Format `metadata` returns message IDs, labels, headers, and top-level fields like `internalDate` and `sizeEstimate`, without bodies — the cheapest format that carries enough to populate ThreadSummary.

**Cost model.** `threads.list` = 10 quota units; `threads.get` = 40 quota units regardless of format. A search with `max_results=25` costs `10 + 25 × 40 = 1010` quota units. Per-user-per-minute cap is 6,000 quota units, so the sustained ceiling is ~6 rich searches/minute per account. This trade favors host-LLM ergonomics over quota economy — the alternative (returning only `{thread_id, snippet, history_id}` from `threads.list` natively) would cost 10 units per search but force a per-result follow-up to learn anything useful, multiplying total cost whenever the host wants per-result detail anyway.

If the cost becomes painful in practice, a future amendment may add `include_metadata: bool` (default `true`) so callers can opt out. The forthcoming cache layer ([ADR-0009](0009-caching-with-sqlite-and-history-api.md), v1.0) is the long-term fix: it amortizes the per-`threads.get` cost across repeated reads of the same thread.

**Excluded fields and why.**

- `has_attachments` is not in ThreadSummary. Determining it cleanly requires `format=full` (same 40-unit cost but returns bodies we don't otherwise need) or a fragile Content-Type heuristic from headers. Host LLMs that need attachment information call `get_thread`. May be promoted to ThreadSummary if a low-cost path is found.
- `unread_count` (per-thread unread message count) is similarly derived from `INBOX`/`UNREAD` label state per message; folded into `label_ids` for v0.2.

#### `get_thread`

**Request:**

- `account: String` *(required)*
- `thread_id: String` *(required)*

**Response:** single-resource envelope (no wrapper) — a `Thread`:

- `thread_id: String`
- `subject_untrusted: String`
- `label_ids: Vec<String>`
- `messages: Vec<Message>`

Each `Message`:

- `message_id: String`
- `from_untrusted: String`
- `to_untrusted: Vec<String>`
- `cc_untrusted: Vec<String>`
- `internal_date: String` — RFC 3339 UTC
- `body_text_untrusted: String` — extracted per [ADR-0010](0010-mime-and-encoding.md)
- `attachment_summaries: Vec<AttachmentSummary>` where each is `{ attachment_id, filename_untrusted, mime_type, size_bytes }`. Actual download is deferred.

#### `list_labels`

**Request:** `account: String` *(required)*.

**Response:** listing envelope. Each `items[i]` is a `Label`:

- `label_id: String`
- `name: String` — user-named or system-named (e.g. `INBOX`, `STARRED`, custom names)
- `kind: "system" | "user"`
- `messages_total: Option<u32>` — when Gmail exposes it
- `messages_unread: Option<u32>`

#### `list_accounts`

**Request:** no parameters.

**Response:** listing envelope. Each `items[i]` is an `Account`:

- `alias: String` — the operator-chosen identifier from `accounts.toml`
- `email: String` — the email address the OAuth grant is bound to
- `enabled: bool` — whether the account is currently enabled in `accounts.toml`

(Per-account *scope state* is deferred to `mcp_status` in v1.0 — [ADR-0014](0014-status-introspection-tool.md).)

#### `archive_thread` / `trash_thread`

Both share the same shape; they differ only in which Gmail label transition they perform.

**Request:**

- `account: String` *(required)*
- `thread_id: String` *(required)*
- `dry_run: bool` *(default `false`)*

**Response:** `{ "thread_id": String, "applied": bool }`. `applied: false` when `dry_run: true` was passed.

#### `modify_thread_labels`

**Request:**

- `account: String` *(required)*
- `thread_id: String` *(required)*
- `add_label_ids: Vec<String>` — may be empty
- `remove_label_ids: Vec<String>` — may be empty
- `dry_run: bool` *(default `false`)*

At least one of `add_label_ids` / `remove_label_ids` must be non-empty; otherwise return `InvalidArgument`.

**Response:** `{ "thread_id": String, "label_ids": Vec<String>, "applied": bool }`. `label_ids` is the post-change label set as Gmail reports it (or the pre-change set when `dry_run: true`).

#### `batch_archive` / `batch_trash` / `batch_modify_thread_labels`

All three share the batch envelope. They differ only in extra parameters and the label transition applied.

**Common request:**

- `account: String` *(required)*
- `thread_ids: Vec<String>` — 1..=100. Gmail's `batchModify` allows 1000; 100 is a saner default for an agent-driven tool and prevents one runaway tool call from touching the whole inbox. The cap is configurable per [ADR-0006](0006-config.md).
- `dry_run: bool` *(default `false`)*

Per-tool extras:

- `batch_archive` — no extras
- `batch_trash` — no extras
- `batch_modify_thread_labels` — `add_label_ids: Vec<String>`, `remove_label_ids: Vec<String>` (same labels applied uniformly to every thread; per-thread customization is N `modify_thread_labels` calls)

**Response:** batch results envelope (per the convention above) with `{ thread_id, ok, error? }` per item. Never short-circuit on first error.

**Implementation note: Gmail has no thread-level batch endpoint.** `users.threads` exposes only single-thread operations (`modify`, `trash`, `delete`, `untrash`); batch endpoints exist only on `users.messages` (`batchModify`, `batchDelete`). The MCP's batch tools are therefore implemented as `N` concurrent calls to the corresponding single-thread endpoint server-side. From the host LLM's perspective this is one tool call returning one batch envelope; from Gmail's perspective it is `N` independent operations subject to the per-user quota and rate limits. Cost: `10 × N` quota units for `batch_archive`/`batch_modify_thread_labels` (each uses `threads.modify` at 10 units) or `20 × N` for `batch_trash` (uses `threads.trash` at 20 units). With `N=100` (default cap) this is up to 2,000 quota units per call — roughly one-third of the per-user-per-minute budget — which is why the cap is conservatively below Gmail's 1,000-id limit on `messages.batchModify`. Concurrent execution honors the per-account rate limiter from [ADR-0006](0006-config.md). The `dry_run: true` path issues no Gmail calls and returns `{thread_id, ok: true}` per id with no side effects.

#### `send_email`

**Request:**

- `account: String` *(required)*
- `to: Vec<String>` — ≥1 recipient
- `cc: Option<Vec<String>>`
- `bcc: Option<Vec<String>>`
- `subject: String` — may be empty (Gmail permits)
- `body_text: String` — plain text only in v0.2; may be empty
- `in_reply_to_thread_id: Option<String>` — when set, the message threads correctly: `In-Reply-To` and `References` headers are populated from the latest message in the target thread, and the Gmail `threadId` field is set so Gmail places the message in that thread
- `dry_run: bool` *(default `false`)*

**Validation:**

- Every address validated for header injection (`\r`, `\n`) per [ADR-0005](0005-error-model.md)'s `HeaderInjection` variant. Validation runs *before* dedup ([ADR-0012](0012-idempotency-and-dry-run.md)).
- `to.len() + cc.unwrap_or_default().len() + bcc.unwrap_or_default().len() ≥ 1` — otherwise `InvalidArgument`.

**Response:**

- `sent_message_id: String` — Gmail's returned `id` for the sent message
- `thread_id: String` — the thread the message landed in (existing thread if `in_reply_to_thread_id` was set; new thread otherwise)
- `dedup_action: "sent" | "deduped" | "would_send"` — per [ADR-0012](0012-idempotency-and-dry-run.md); `would_send` corresponds to `dry_run: true`

**Implementation note: Gmail's `messages.send` takes raw RFC 2822.** The structured request above is serialized internally to a base64url-encoded RFC 2822 message, then submitted as the `raw` field of `users.messages.send`. When `in_reply_to_thread_id` is set, the implementation first issues a `threads.get(format=metadata)` against the target thread to extract the latest message's `Message-Id` for the `In-Reply-To` and `References` headers, then submits with both the proper headers and the API-level `threadId` field — Gmail requires both for correct threading. Cost: `messages.send` = 100 quota units (200 if reply, because of the prefetch).

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Document conventions only; let tools accumulate organically | Minimal upfront work | Drift; the README and ADR-0015 already disagree about what exists |
| **(b) Enumerate v1 inventory + lock conventions** (chosen) | Anchors [ADR-0015](0015-tool-versioning-policy.md)'s snapshot test; gives reviewers a yes/no answer; explicit deferral list keeps scope honest | Inventory needs amendment when a real new tool ships — but that's the whole point of versioning |
| (c) Generate tools from a schema definition (OpenAPI-style) | Single source of truth | Premature; eleven tools don't need a code generator |

We choose (b). The cost of writing this ADR is exactly one ADR; the cost of not writing it is every future "should this be `limit` or `max_results`?" review thread.

## Consequences

**Positive:**

- ADRs 0011, 0013, 0014 stop being implicit tool-inventory documents. Their scattered tool mentions are either deferred (per this ADR's exclusion list) or normalized.
- [ADR-0015](0015-tool-versioning-policy.md)'s snapshot test has a documented baseline to enforce against.
- The host LLM gets consistent parameter names across tools — `account` everywhere, `max_results` everywhere, `*_id` for IDs. Lower tool-call error rate.
- The granularity rule blocks the slow drift toward "summarize" / "smart-find" tools that consumers should own.

**Negative:**

- Existing prototype code uses inconsistent names (`id` not `thread_id` in some places). One-time refactor cost.
- The "account required everywhere" rule is slightly more verbose than defaulting. Worth it.

**Risks:**

- *Risk:* The v1 inventory is wrong — Phase-2 Calendar exposes a tool shape that breaks the conventions.
  *Mitigation:* Amend this ADR when Phase 2 begins; the snapshot test will catch any silent drift before then.
- *Risk:* The deferred list ossifies into "never built." A pre-1.0 user wants `mcp_status` and can't get it.
  *Mitigation:* Deferral is by-design and reversible. Promote any deferred tool to v1.x when a real consumer asks for it.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — tool philosophy ("low-level primitives only")
- [ADR-0005](0005-error-model.md) — error envelope (referenced by response convention)
- [ADR-0007](0007-testing-strategy.md) Layer 4 — snapshot test that enforces this surface
- [ADR-0012](0012-idempotency-and-dry-run.md) — `dry_run` parameter on write tools
- [ADR-0015](0015-tool-versioning-policy.md) — change policy this ADR anchors
- [ADR-0018](0018-email-content-trust.md) — untrusted-content disclaimer convention
