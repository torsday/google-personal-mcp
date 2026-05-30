# ADR-0026: Gmail tool surface — Phase 2 expansion (drafts, labels CRUD, filters, untrash, profile, send-as, vacation, forward, permanent delete, body/multipart/forwarded-attachment/rate-cap consolidation)

**Date:** 2026-05-28
**Status:** Accepted, target v1.1
**Depends on:** [ADR-0022](0022-capability-gating.md), [ADR-0016](0016-tool-surface-and-conventions.md)
**Consolidates spikes:** [#181](https://github.com/torsday/google-personal-mcp/issues/181) `get_full_body`, [#182](https://github.com/torsday/google-personal-mcp/issues/182) multipart send, [#185](https://github.com/torsday/google-personal-mcp/issues/185) `parse_forwarded_attachment`, [#186](https://github.com/torsday/google-personal-mcp/issues/186) send rate-cap

---

## Context

The v1.0 Gmail surface ([ADR-0016](0016-tool-surface-and-conventions.md)) covers the load-bearing read/search/triage/send/attachment-download path. After shipping that surface and operating it for real, **what's missing for an agent that should be able to do "any reasonable thing with email"** falls into three categories:

1. **Gaps the surface forgot.** Drafts (compose-and-iterate), label vocabulary management (create/update/delete labels rather than just apply existing ones), trash recovery (`untrash_thread`), single-message retrieval (`get_message`), account profile metadata, send-as identity discovery (operator's multi-identity reality), vacation responder, and permanent delete.
2. **Workflow holes already filed as separate spikes** ([#181](https://github.com/torsday/google-personal-mcp/issues/181), [#182](https://github.com/torsday/google-personal-mcp/issues/182), [#185](https://github.com/torsday/google-personal-mcp/issues/185), [#186](https://github.com/torsday/google-personal-mcp/issues/186)). Each is real, but filed as a separate ADR they'd each pay design overhead; folded together they form one coherent Phase 2 surface.
3. **Server-side filters.** Persistent rules ("auto-archive newsletters from X") — a high-leverage but higher-trust capability that belongs in this ADR because it interacts with every other Gmail tool.

If the gaps shipped piecemeal — one ADR per feature — the corpus would grow ten more ADRs to cover what is structurally one coherent expansion. This ADR consolidates.

The principle from [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — *primitives, not smart tools* — applies throughout. We don't add `summarize_inbox` or `auto_categorize`. We add the API primitives that let host LLMs build those workflows.

## Decision

We add 24 tools, classified per [ADR-0022](0022-capability-gating.md) aspects, all following [ADR-0016](0016-tool-surface-and-conventions.md) conventions. ADR-0016 is amended to include them in the v1.1 baseline.

### Tool inventory — by capability area

#### Drafts (compose-and-iterate workflow)

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `list_drafts` | read | `gmail.readonly` | Paginated; returns `DraftSummary` with subject/to/snippet |
| `get_draft` | read | `gmail.readonly` | Full draft body + headers; `_untrusted` wrapping where applicable |
| `create_draft` | write | `gmail.compose` | Returns draft id; same body shape as `send_email` |
| `update_draft` | write | `gmail.compose` | By draft id; replaces draft body (Gmail's `drafts.update` semantics) |
| `send_draft` | destructive | `gmail.send` | Sends an existing draft; same destructive aspect as `send_email` |
| `delete_draft` | destructive | `gmail.compose` | Permanently removes the draft (no trash); `dry_run` per [ADR-0012](0012-idempotency-and-dry-run.md) |

Drafts get their own aspect on the `gmail.compose` scope (a tighter scope than `gmail.send`) so an operator can enable draft management without authorizing sends. Configuration:

```toml
[services.gmail.tools.send_draft]
enabled = false        # operator allows compose but reviews sends manually
```

Per-tool override (the sanctioned exception named in [ADR-0022 §Per-tool override](0022-capability-gating.md), first used by [ADR-0024](0024-contacts-service-surface.md)) lets `send_draft` be disabled while the rest of the draft suite stays available. This is the operator's escape hatch from the Gmail grandfathered-all-on default (per [ADR-0022](0022-capability-gating.md) §Default posture) — an operator who wants drafts management without authorized sends can opt **out** of `send_draft` specifically while keeping every other destructive Gmail tool intact.

#### Labels CRUD (vocabulary management)

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `create_label` | write | `gmail.labels` | Returns the created label; honors `name`, `message_list_visibility`, `label_list_visibility`, `color` |
| `update_label` | write | `gmail.labels` | Rename or recolor; by label id |
| `delete_label` | destructive | `gmail.labels` | Removes a user label; messages keep their other labels; `dry_run` returns affected-message count |

System labels (`INBOX`, `STARRED`, `IMPORTANT`, etc.) are read-only at Google's API and return typed errors on `update_label`/`delete_label`. `list_labels` (existing) flags system vs user labels via `type` field.

#### Trash recovery + permanent delete

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `untrash_thread` | write | `gmail.modify` | Reverses `trash_thread`; works on threads still within Gmail's 30-day trash window |
| `batch_untrash` | write | `gmail.modify` | Parallel; per [ADR-0016](0016-tool-surface-and-conventions.md) batch conventions |
| `permanent_delete_thread` | destructive | `gmail.modify` | Skips trash; **requires `confirm: "yes-permanent-delete-<thread_id>"` literal** per the [ADR-0019](0019-data-retention-and-purge.md) `purge_account` precedent (and extended in [ADR-0025](0025-drive-service-surface.md) `update_permissions`); `dry_run` per [ADR-0012](0012-idempotency-and-dry-run.md) |

Symmetric with [ADR-0019](0019-data-retention-and-purge.md)/[ADR-0025](0025-drive-service-surface.md): the truly unrecoverable destructive op gets a literal-string confirm guard.

#### Single-message retrieval

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `get_message` | read | `gmail.readonly` | By `(account, message_id)`; returns one message without the surrounding thread |
| `get_full_body` | read | `gmail.readonly` | **Replaces [#181](https://github.com/torsday/google-personal-mcp/issues/181)** — returns full body for messages whose body was truncated in `get_thread`. Required when [ADR-0010](0010-mime-and-encoding.md) truncation hit. Signature: `get_full_body(account: String, message_id: String, part_id: Option<String>) -> FullBodyResponse` — `part_id` selects a specific MIME part when a message has multiple bodies (e.g. text vs html); `None` returns all parts |

Resolution of [#181](https://github.com/torsday/google-personal-mcp/issues/181): the proposed `truncate_at` param on `get_thread` is rejected in favor of a dedicated `get_full_body` tool — additive per [ADR-0015](0015-tool-versioning-policy.md), no risk of changing existing `get_thread` callers, clean cache fit (full bodies live in the cache from [ADR-0009](0009-caching-with-sqlite-and-history-api.md) and rehydrate on demand).

#### Account profile

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `get_account_profile` | read | `gmail.readonly` | Storage usage, total messages, total threads, email address; calls `users.getProfile` |

#### Send-as / identities (read-only in v1)

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `list_send_as_aliases` | read | `gmail.settings.basic` | Returns operator's configured send-as identities (legal@, etc.) |
| `get_send_as_alias` | read | `gmail.settings.basic` | By email address |

Create/update/delete of send-as identities deferred — manipulating identities is rare-touch operator work, and creating a fake send-as identity is a real abuse vector. Defer to follow-up ADR.

#### Vacation responder

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `get_vacation` | read | `gmail.settings.basic` | Returns the configured vacation auto-responder |
| `set_vacation` | write | `gmail.settings.basic` | Enable/disable + set message/start/end |

#### Forward (convenience)

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `forward_thread` | destructive | `gmail.send` | Sends a copy of a thread (or specific messages within it) to recipients; optional `comment_untrusted` precedes the quoted text. Honors `dry_run`. |

`forward_thread` is borderline against ADR-0001's no-smart-tools rule. It earns its keep because the alternative — host reads thread, composes a verbatim re-send, attaches the quoted body — is enough boilerplate that every host re-implements it (and gets it slightly wrong). Cleaner as a primitive than as a thousand host-side variations.

#### Server-side filters

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `list_filters` | read | `gmail.settings.basic` | All filters on the account |
| `create_filter` | write | `gmail.settings.basic` | Criteria + actions; Gmail filters are immutable, so `create_filter` is the only write path |
| `delete_filter` | destructive | `gmail.settings.basic` | By filter id |

Filters are classified **destructive** for `delete_filter` because filter loss can mean important mail flows silently change. `create_filter` is write because a wrong filter can quietly archive mail the operator wants to see — but the recovery (delete the filter) is one tool call.

#### Multipart send + forwarded-attachment parsing + send rate-cap (folded in from existing spikes)

These three resolve their respective spikes ([#182](https://github.com/torsday/google-personal-mcp/issues/182), [#185](https://github.com/torsday/google-personal-mcp/issues/185), [#186](https://github.com/torsday/google-personal-mcp/issues/186)) inside this ADR:

- **Multipart `send_email`** — additive `attachments: Option<Vec<AttachmentSpec>>` parameter on the existing `send_email` (per [ADR-0015](0015-tool-versioning-policy.md), adding an optional parameter is additive; no `_v2`). `AttachmentSpec` accepts inline bytes or a local file path; the latter inherits the [ADR-0021](0021-attachment-download-policy.md) path-traversal/extension guards symmetrically with `download_attachment`. Total attachment size capped at Gmail's 25 MB limit; typed error on overflow.

- **`parse_forwarded_attachment`** — new tool, read aspect. Signature: `parse_forwarded_attachment(account: String, message_id: String, attachment_id: String, max_depth: Option<u32>) -> ParsedMessage`. The attachment is expected to be a `message/rfc822` MIME part — non-rfc822 attachments return a typed `Error::UnsupportedMimeType` ([ADR-0005](0005-error-model.md)). Returns a nested `ParsedMessage` with the same `_untrusted` wrapping discipline as the outer message. **Recursion depth capped at 5** by default (configurable per-call via `max_depth`; the hard ceiling is enforced server-side at `[services.gmail].parse_forwarded_max_depth_ceiling`, default 10). The cap prevents forwarded-within-forwarded DoS.

- **Send rate-cap** — not a tool; a new config block governing `send_email` and `send_draft` and `forward_thread`. Per-account configurable:

```toml
[services.gmail.send_rate_cap]
sends_per_hour      = 30        # 0 = disabled (default; preserves v1.0 behavior)
unique_recipients_per_day = 100 # 0 = disabled
```

When a cap is hit, the typed error variant ([ADR-0005](0005-error-model.md)) is `Error::SendCapExceeded { window, current, limit, hint }`. The cap is a circuit breaker against prompt-injection-induced mass-send, not a deliverability quota — it's distinct from the per-account Gmail rate limiter ([#25](https://github.com/torsday/google-personal-mcp/issues/25)) and the per-source-IP HTTP throttle ([#170](https://github.com/torsday/google-personal-mcp/issues/170)).

### Aspect classification summary

| Aspect | New tools |
| --- | --- |
| read | `list_drafts`, `get_draft`, `get_message`, `get_full_body`, `get_account_profile`, `list_send_as_aliases`, `get_send_as_alias`, `get_vacation`, `list_filters`, `parse_forwarded_attachment` (10) |
| write | `create_draft`, `update_draft`, `create_label`, `update_label`, `untrash_thread`, `batch_untrash`, `set_vacation`, `create_filter` (8) |
| destructive | `send_draft`, `delete_draft`, `delete_label`, `permanent_delete_thread`, `forward_thread`, `delete_filter` (6) |

Plus the additive `attachments` parameter on `send_email` and the new `[services.gmail.send_rate_cap]` config block.

### Scope-to-capability summary

| Scope | Tools requiring it |
| --- | --- |
| `gmail.readonly` | All read tools |
| `gmail.compose` | `create_draft`, `update_draft`, `delete_draft` (compose without send) |
| `gmail.send` | `send_email` (existing — now also gates sends-with-attachments via the additive `attachments` param), `send_draft`, `forward_thread` |
| `gmail.labels` | `create_label`, `update_label`, `delete_label` |
| `gmail.modify` | `untrash_thread`, `batch_untrash`, `permanent_delete_thread` (existing scope, new tools) |
| `gmail.settings.basic` | All settings tools (send-as, vacation, filters) |

`gmail.compose` and `gmail.labels` are **new scopes** for this MCP — added to ADR-0006's `[services.gmail].scopes` default. Existing operators who upgrade re-consent to add them; the consent screen lists the new scopes explicitly per Google's OAuth flow.

### Cache fit

Drafts and labels are read-after-write resources that benefit cleanly from the existing [ADR-0009](0009-caching-with-sqlite-and-history-api.md) cache shape — both are message-id-keyed in Gmail's data model. **Defer cache integration to a follow-up ADR** consistent with how Calendar/Contacts/Drive caching is deferred. The Gmail cache today covers threads and the query result store; extending to drafts and labels is a discrete next step.

Vacation and filter state is operator-config-shape; cache it lazily with a generous TTL (15 minutes) — it changes at human cadence, the API is cheap. Implement as part of the tool, not the cache layer, until a real need surfaces.

### Audit-log treatment

All write/destructive tools emit standard [ADR-0011](0011-audit-log.md) audit records. Two notes:

- `send_draft` audit record references the underlying draft id so the `audit_summary` view can correlate "this draft was the source of that send."
- `update_filter` doesn't exist; `delete_filter` audit captures the deleted filter's criteria + actions in `extra` so a misclick is reconstructible.

### Out of scope (intentionally)

- **Send-as identity create/update/delete.** Manipulating identities is rare and abuse-prone; defer.
- **Snooze.** Not a Gmail REST primitive; UI-layer feature.
- **Auto-forwarding, IMAP/POP, language settings, delegates, CSE.** Operator-config; not normal agent territory.
- **`messages.import` / `messages.insert`.** Bringing externally-received mail INTO Gmail; niche.
- **Watch / push.** Orchestration concern.
- **History.list exposed as a tool.** Cache primitive; not user-facing.
- **Reply / Reply-all helpers.** `send_email` with `thread_id` covers reply; reply-all is host-composed from `get_thread` participants.
- **Mark as read/unread, star/unstar, mark important.** All are `modify_thread_labels` with the appropriate system label — no new tools needed.

## Options Considered

### Drafts: one tool with a `mode` param vs the suite of six

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Single `manage_draft(action: "list" \| "get" \| "create" \| ...)` | Small surface | Confuses the snapshot diff; harder description; aspect classification breaks (it's read AND write AND destructive) |
| **(b) Suite of six tools** (chosen) | Each tool has a clear aspect; description carries semantic weight; snapshot reads naturally | Six tools instead of one |

### Filters: full CRUD vs read-only

| Option | Pros | Cons |
| --- | --- | --- |
| (c) Read-only filters in v1 | Smallest blast radius; persistent-rule installation is high-trust | Forces operator to web UI for any filter change; mismatch with the "agent can do anything reasonable" goal |
| **(d) Full CRUD; gated by aspect** (chosen) | Agents can install workflow rules; capability config gates write/destructive aspect | Mistake-install of a wrong filter is real (mitigation: filter-loss is reversible via `delete_filter`; filter-application is forward-only — only affects new mail) |

### Send-as: read-only vs full CRUD

| Option | Pros | Cons |
| --- | --- | --- |
| **(e) Read-only in v1** (chosen) | Eliminates the "agent creates fake send-as identity to impersonate" abuse vector | Operator who wants to add a send-as via the agent has to use the web UI |
| (f) Full CRUD | Symmetric with drafts/labels | The abuse vector is real and the operator-frequency for creating send-as is rare |

### Forward: tool vs host-composes

| Option | Pros | Cons |
| --- | --- | --- |
| (g) No `forward_thread`; host composes via `get_thread` + `send_email` | Most primitive; matches ADR-0001 strictly | Every host re-implements the quoted-body convention; subtle bugs across implementations |
| **(h) `forward_thread` primitive** (chosen) | One correct quote-and-send; small ADR-0001 carve-out | Adds a tool that wraps two others |

### Send rate-cap: tool vs config

| Option | Pros | Cons |
| --- | --- | --- |
| **(i) Config block; typed error on exceed** (chosen) | Operator sets policy once; consistent across all send paths; no per-call decision | Operator can disable it (default off, must opt in) |
| (j) Per-call rate-cap parameter | Per-call control | Pushes policy choice into every send call; defaults end up arbitrary |

## Consequences

**Positive:**

- The Gmail surface goes from "core triage workflow" to "any reasonable agent workflow with mail."
- Four pending spikes ([#181](https://github.com/torsday/google-personal-mcp/issues/181), [#182](https://github.com/torsday/google-personal-mcp/issues/182), [#185](https://github.com/torsday/google-personal-mcp/issues/185), [#186](https://github.com/torsday/google-personal-mcp/issues/186)) consolidate to one ADR instead of four — less per-spike design overhead.
- Drafts as a separate `gmail.compose` scope means operators can authorize compose-only agents (review-before-send).
- The literal-string confirm on `permanent_delete_thread` carries the [ADR-0019](0019-data-retention-and-purge.md)/[ADR-0025](0025-drive-service-surface.md) precedent uniformly across the corpus's most-dangerous ops.
- Send rate-cap closes the prompt-injection mass-send hole without disrupting normal use.

**Negative:**

- Twenty-four new tools is a meaningful registry expansion; the snapshot diff for ADR-0026 will be the largest in the corpus.
- Two new scopes (`gmail.compose`, `gmail.labels`) mean operators upgrading from v1.0 see a fresh Google consent screen — documented in the v1.1 release notes.
- The per-tool override carve-out from [ADR-0022](0022-capability-gating.md) (originally Contacts-only) now also applies to Gmail's `send_draft` — documented here; ADR-0022 amendment.

**Risks:**

- *Risk:* `create_filter` installs a wrong filter that auto-archives important mail; operator only notices weeks later.
  *Mitigation:* `list_filters` is the audit surface; filter audit records ([ADR-0011](0011-audit-log.md)) capture the criteria + actions; recovery is one `delete_filter`.
- *Risk:* `forward_thread` agents leak private threads to attacker-supplied recipients via prompt injection.
  *Mitigation:* `forward_thread` is destructive aspect; send rate-cap applies; explicit `confirm`-style guard considered and rejected (forward isn't unrecoverable like permanent_delete — the recipient could be asked to delete; the rate-cap is the proportionate defense).
- *Risk:* `send_rate_cap` defaults to disabled (preserves v1.0 behavior) so operators who don't read the v1.1 release notes get no protection.
  *Mitigation:* Release notes call it out prominently; INSTALL.md leads with a recommended value (`sends_per_hour = 30` covers most personal usage).
- *Risk:* Drafts grow the cache footprint (drafts can be large; an operator with many drafts hits cache size limits).
  *Mitigation:* Cache integration is deferred; until then, live-fetch on every draft read. When cached, drafts share the [ADR-0019](0019-data-retention-and-purge.md) body-purge policy.
- *Risk:* `permanent_delete_thread` confirm-string mismatch is harder to recover from than the [ADR-0025](0025-drive-service-surface.md) `update_permissions` case (no `dry_run` preview of what would be deleted).
  *Mitigation:* `dry_run: true` returns thread metadata; explicit recipe in the tool description; same error message shape as Drive's `confirm`.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — primitives not smart tools; `forward_thread` carve-out documented
- [ADR-0005](0005-error-model.md) — `SendCapExceeded` and `ExportRequired` variants
- [ADR-0006](0006-config.md) — `[services.gmail].scopes` set extended; `[services.gmail.send_rate_cap]` block added
- [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — cache extension to drafts/labels deferred to follow-up
- [ADR-0010](0010-mime-and-encoding.md) — `get_full_body` resolves the truncation gap
- [ADR-0011](0011-audit-log.md) — audit treatment of new write/destructive tools
- [ADR-0012](0012-idempotency-and-dry-run.md) — `dry_run` on every destructive tool
- [ADR-0015](0015-tool-versioning-policy.md) — additive `attachments` param on `send_email`; snapshot covers all new tools
- [ADR-0016](0016-tool-surface-and-conventions.md) — amended by this ADR (Phase 2 surface adds to the v1.0 baseline)
- [ADR-0018](0018-email-content-trust.md) — `_untrusted` wrapping on draft body, forwarded-message recursion
- [ADR-0019](0019-data-retention-and-purge.md) — literal-string confirm precedent for `permanent_delete_thread`
- [ADR-0021](0021-attachment-download-policy.md) — `attachments` parameter on `send_email` inherits guard rules
- [ADR-0022](0022-capability-gating.md) — aspect classification + per-tool override for `send_draft`
- [ADR-0025](0025-drive-service-surface.md) — literal-string confirm precedent (Drive's `update_permissions`)
- Spikes consolidated: [#181](https://github.com/torsday/google-personal-mcp/issues/181), [#182](https://github.com/torsday/google-personal-mcp/issues/182), [#185](https://github.com/torsday/google-personal-mcp/issues/185), [#186](https://github.com/torsday/google-personal-mcp/issues/186)
- [Gmail API reference](https://developers.google.com/gmail/api/reference/rest)
