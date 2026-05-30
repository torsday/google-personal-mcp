# ADR-0027: v1.1 surface refinements — compact untrust, response masks, per-call cost feedback, batch defaults, mcp_status filtering, and the activation-trigger amendment to ADR-0015

**Date:** 2026-05-30
**Status:** Accepted, target v1.1
**Amends:** [ADR-0015](0015-tool-versioning-policy.md), [ADR-0016](0016-tool-surface-and-conventions.md), [ADR-0018](0018-email-content-trust.md), [ADR-0014](0014-status-introspection-tool.md), [ADR-0008](0008-observability-and-deployment.md)

---

## Context

The repo tagged `v1.0.0` on 2026-05-30 but has **not yet been publicly advertised** — no announcement, no MCP-catalog listing, no README stability badge, no recruitment of external users. Per [ADR-0015 §v1 scope](0015-tool-versioning-policy.md) — *"Tag v1.0 the day you advertise the repo publicly; until then, break things as needed"* — the additive-only policy's spirit is bound to *external consumers being able to pin*, and no external consumer can. The tag's existence is necessary infrastructure (it activates the snapshot test as the public contract) but not sufficient for the policy to bite.

This open window — between the tag and public advertisement — is the right moment to make surface decisions that *would* be hard to change under additive-only constraint but are correct on the merits. The bar is **"what's actually best"**, not **"what's minimally extending v1.0.0."**

The six decisions below were filtered against two scrutinies: *would this be the right call if we were starting v1.0 from scratch?* and *does the default behavior serve the common case, or have we optimized for the lean case at the common case's expense?* Two candidate decisions (service-prefix tool naming; required `fields_mask` on all reads) didn't survive that scrutiny and are deliberately not in this ADR — see Options Considered.

A seventh decision — the activation trigger of the [ADR-0015](0015-tool-versioning-policy.md) policy — is a clarification, not a change. It codifies the interpretation we're already operating under so future contributors don't re-litigate it.

## Decision

### 1. Compact `_untrusted` delimiter — amends ADR-0018

[ADR-0018](0018-email-content-trust.md) wraps every attacker-controllable value in `<<<UNTRUSTED:KIND value UNTRUSTED>>>` where `KIND` names the kind of untrusted content (e.g., `SUBJECT`, `FROM`, `FILENAME`). This ADR replaces that with:

```
«u»value«/u»
```

— 5 characters of overhead per wrapped value instead of ~25+ (the `KIND` and the `UNTRUSTED` words double the marker length without adding signal). `«` (U+00AB) and `»` (U+00BB) are guillemets — single Unicode codepoints that don't collide with any byte sequence in legitimate email body, file name, or event field content (they appear in French and other European text but always as standalone marks, never in `«u»...«/u»` shape).

**Why drop `KIND`:** the field name already encodes it. `subject_untrusted` is unambiguous about what kind of untrusted content it carries; `KIND: SUBJECT` was duplicative. The host LLM was never expected to dispatch on `KIND` programmatically; it was structural redundancy.

**Self-documenting property preserved:** the inline marker still appears at the value read site. A host LLM scanning a thread response sees `«u»From the IT team «/u»` and knows it's looking at attacker-influenced text without needing to correlate with a separate manifest.

**Wrapper guarantee unchanged:** every field name still suffixed `_untrusted`; every attacker-controllable value still wrapped. The trust contract is identical; only the marker bytes change.

### 2. Optional `fields_mask` on read tools — amends ADR-0016

Every read tool gains an optional `fields_mask: Option<Vec<String>>` parameter. When omitted, the response shape is the current rich default (`ThreadSummary`'s full field set on `search_threads`, full `Event` on `get_event`, etc.). When supplied, only the named fields are returned.

This is **opt-in lean** — the common case (caller doesn't bother specifying) stays rich and convenient; sophisticated callers can ask for `["id", "subject_untrusted"]` and pay tokens only for what they actually use.

The field names are the response shape's keys. `fields_mask: ["subject_untrusted", "from_untrusted"]` returns `{subject_untrusted, from_untrusted, _cost_units, _cache_hit, ...}` (the per-call cost fields below are always present regardless of mask).

Forbidden: `fields_mask: []` (use unset for the default; explicit empty is a typed error). Forbidden: requesting fields that don't exist on the response shape (typed error naming the closest valid match).

### 3. `format` parameter on `get_thread` / `get_message` — amends ADR-0016

```rust
get_thread(account, thread_id, format: ThreadFormat = "full", ...)
get_message(account, message_id, format: MessageFormat = "full", ...)
```

`ThreadFormat` / `MessageFormat`: `"full" | "metadata" | "minimal"` — passed through to Gmail's underlying API. `"metadata"` returns headers + structure but no body; `"minimal"` returns only IDs and label state. Default `"full"` matches the common case (caller asked for *this* thread, presumably wants its contents).

This is **opt-in lean** symmetric with `fields_mask`. An agent that's scanning many threads for a pattern uses `"metadata"`; the typical "show me this thread's messages" call stays unchanged.

### 4. Per-call cost feedback fields on every response — amends ADR-0016 + ADR-0008

Every tool response includes three flat top-level fields:

```rust
{
  ..., // tool-specific fields
  _cost_units: u32,    // Gmail quota units consumed by this call (0 if cache hit)
  _cache_hit: bool,    // true if served entirely from cache
  _upstream_ms: u32,   // upstream API latency (0 if cache hit)
}
```

These are **always present**, even on cache hits (where two of three are 0 / true). Naming uses the existing `_meta`-ish underscore-prefix convention but flat at the response root — one less level of nesting for the host LLM to navigate.

**Why flat, not nested:** the alternative `_meta: { cost_units: 41, cache_hit: false }` adds a level of structure for negligible organizational gain; flat keeps the read site short.

**Cost of always-present:** ~30 bytes per response. Trivial vs the agent's downstream optimization gain — agents learn which calls are expensive without the operator having to lecture, and the cache hit ratio becomes observable at the response level rather than only at `mcp_status`.

### 5. Batch result default `mode: "failures_only"` — amends ADR-0016

Batch tools (`batch_archive`, `batch_trash`, `batch_modify_thread_labels`, `batch_untrash`, `batch_delete_events`, `batch_delete_contacts`, etc.) gain a `mode: BatchMode = "failures_only"` parameter:

- `"failures_only"` (default): response contains `failures: Vec<{thread_id, error}>` listing only items that failed. On full success, `failures: []`.
- `"all"`: response contains per-item results (current v1.0 behavior). For audit-trail use.
- `"summary"`: response contains counts only (`succeeded: N, failed: M`) plus the first 5 failure details. For "ran a 1000-item batch, just tell me how it went" callers.

**Why default to `"failures_only"`:** the common case for a batch operation is **everything succeeded**. The agent doesn't need 200 `{thread_id, success: true}` entries to know that; an empty `failures` array conveys it tersely. When things go wrong, failures get the detail they deserve.

**Backward compat:** none required (per the freedom window). `mode: "all"` exists as the explicit opt-in for the v1.0 shape.

### 6. `mcp_status` per-account filtering — amends ADR-0014

```rust
mcp_status(account: Option<String>) -> StatusResponse
```

When `account` is supplied, the response is scoped to just that account's capability matrix, token state, audit-tail, and cache state. When omitted, the response is the full multi-account view (current v1.0 behavior).

**Why this matters:** with the v1.1 capability matrix from [ADR-0022](0022-capability-gating.md), a multi-account operator gets a response with `accounts × services × aspects` entries (e.g., 10 × 4 × 3 = 120 matrix cells), most of which are irrelevant when the caller's question is "what can I do on my work account right now?"

### 7. ADR-0015 activation-trigger clarification

[ADR-0015](0015-tool-versioning-policy.md) §"v1 scope" reads as ambiguous between two possible activation triggers:

- (a) **The v1.0 git tag** — strict reading of *"The additive-only policy applies from v1.0 onward."*
- (b) **Public advertisement** — strict reading of *"Tag v1.0 the day you advertise the repo publicly; until then, break things as needed."*

This ADR clarifies that **the trigger is (b) — public advertisement** — and that "public advertisement" means *at least one* of:

- an announcement post / commit on a public channel
- a stability badge in the README (e.g., "v1.0 stable API")
- an MCP-catalog listing
- a documented external pin (someone clones the v1.0.0 tag and depends on the tool surface)

Until any of these happens, the v1.x version line can rev with breaking changes (`v1.1.0` can break `v1.0.0`'s tool surface, etc.) without violating the ADR-0015 contract. The snapshot test still catches *unintentional* drift; intentional breakage in v1.x is allowed and documented in CHANGELOG. After the advertisement trigger fires, ADR-0015's full additive-only policy enforces; breaking changes require a major version bump per semver.

This is **not** a substantive change to ADR-0015 — it's the interpretation the ADR's reasoning already supports. The amendment exists to record the precise trigger so it doesn't have to be re-decided.

## Options Considered

### Compact untrust delimiter

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Status quo: `<<<UNTRUSTED:KIND value UNTRUSTED>>>` | Self-documenting; KIND helps a host LLM categorize | Verbose; KIND is duplicative of the `_untrusted` field-name suffix |
| (b) Top-level manifest: `_untrusted_fields: [...]` + raw values | Smallest markers (just a list) | Requires the host LLM to correlate manifest with field values; defeats inline-at-read-site signal; argued against in ADR-0018 §Risks |
| **(c) `«u»value«/u»` — drop KIND, single-codepoint delimiters** (chosen) | Self-documenting property preserved; ~80% smaller marker; no correlation burden | Unicode-savvy hosts only (every modern LLM and parser handles guillemets cleanly) |
| (d) ASCII-only short form: `[u]value[/u]` | Pure ASCII | `[u]` collides with HTML / Markdown / BBCode in legitimate email bodies — false positives |

### `fields_mask` design

| Option | Pros | Cons |
| --- | --- | --- |
| (e) Required on every read tool | Forces every caller to be token-aware | Penalizes the common "just give me the standard fields" case; agent has to know every field name to make any read call |
| **(f) Optional with rich default** (chosen) | Common case unchanged; sophisticated callers opt into lean responses | The default keeps the existing rich shape; "lean by default" only happens when caller asks for it |
| (g) Required, with a `["*"]` shortcut for "everything" | Slightly less aggressive than (e); `*` is the escape hatch | The shortcut becomes the de facto common-case value; same UX cost as (e) in practice |

### `format` default for `get_thread` / `get_message`

| Option | Pros | Cons |
| --- | --- | --- |
| (h) Default `"metadata"` (lean by default) | Saves ~80% of bytes on get_thread calls that only need headers | Wrong default for the common case — calling `get_thread` on a specific id is usually *because you want that thread's content* |
| **(i) Default `"full"` (current rich), opt-in lean** (chosen) | Default matches the common case; agents that want metadata-only say so | Bytes are paid until the agent learns to use `"metadata"` |

### Per-call cost feedback shape

| Option | Pros | Cons |
| --- | --- | --- |
| (j) Nested under `_meta: { cost_units, cache_hit, upstream_ms }` | One key at the root; tidy | Extra level of nesting for the host LLM to navigate; no real organizational gain |
| **(k) Flat top-level `_cost_units` / `_cache_hit` / `_upstream_ms`** (chosen) | Short read site; underscore prefix already signals "metadata about this response, not response content" | Three keys at the root instead of one |
| (l) Surface cost only via `mcp_status` (don't add to per-response) | No per-response payload change | Agents have to make a *separate call* to learn what the prior call cost; defeats self-optimization at the loop level |

### Batch default mode

| Option | Pros | Cons |
| --- | --- | --- |
| (m) Default `"all"` (current behavior — per-item results) | Audit-trail complete; no surprise on what shipped | Full-success batches return N entries the agent doesn't read; common case noisy |
| **(n) Default `"failures_only"`** (chosen) | Common case (all-success) returns near-empty; failures get detail; explicit `"all"` for audit needs | A caller who didn't read the docs might miss that successes aren't itemized — typed response field `succeeded_count: N` always present, even in `"failures_only"`, mitigates |

### `mcp_status` filtering

| Option | Pros | Cons |
| --- | --- | --- |
| (o) Per-call without filter (current) | Simplest call shape | At ~120 cells per call for multi-account operators, response gets large |
| **(p) Optional `account: Option<String>` filter** (chosen) | Common single-account query stays terse; multi-account view available when needed | One extra optional parameter |

### Service-prefix tool naming (REJECTED)

| Option | Pros | Cons |
| --- | --- | --- |
| **(q) Keep current unprefixed names** (chosen) | Names already unique (`delete_thread` ≠ `delete_event` ≠ `delete_file`); ADR-0015 already advises prefix only on collision | None |
| (r) Prefix all tools: `gmail_search_threads`, `calendar_list_events`, etc. | Hair more discoverable by service grouping | ~6 chars × ~60 tools = ~360 chars / ~90 tokens of system-prompt overhead; payoff doesn't justify cost; ADR-0015 explicitly recommends against until needed for disambiguation |

### Tool-description aspect literal (REJECTED for this ADR)

Including `[read]` / `[write]` / `[destructive]` at the start of every tool description was considered. Rejected: aspects are already discoverable from tool descriptions (a tool that says "modifies" / "deletes" / "sends" makes its aspect obvious); the literal adds ~5 chars × N tools without a concrete agent-behavior improvement. Revisit if a measurable disambiguation problem emerges.

## Consequences

**Positive:**

- The `_untrusted` delimiter change alone saves ~10–30% of response bytes on Gmail and Calendar responses with many untrusted-wrapped fields, with no semantic change.
- Sophisticated agents that adopt `fields_mask` and `format: "metadata"` cut their token budget materially without any operator-side configuration.
- Per-call cost feedback closes the agent-self-optimization loop: an agent that calls `get_thread(format: "full")` once and sees `_cost_units: 41` learns to use `"metadata"` for the next 20 scans without an operator lecture.
- Default `mode: "failures_only"` on batches matches the actual common case (full success) — the agent gets a near-empty response confirming everything worked, rather than 200 redundant items.
- `mcp_status(account: X)` makes the multi-account capability matrix actually usable for single-account queries.
- The ADR-0015 activation-trigger clarification codifies what we already think; future contributors don't have to re-decide whether breaking changes are allowed pre-advertisement.

**Negative:**

- Five separate response-shape changes (untrust delimiter, three new optional params, three new always-present fields, new batch default, new optional mcp_status param) — the snapshot diff against v1.0 will be substantial.
- The v1.0.0 tag's response shapes are now historical; anyone who cloned `v1.0.0` and pinned will see breakage on upgrade. We have no external consumers per the §Context, so the cost is bounded; this is the freedom-window decision the user made deliberately.
- `«u»value«/u»` requires the host LLM (or any downstream parser) to handle Unicode guillemets. Modern LLMs handle them cleanly; older / locale-restricted parsers may not. Documented risk.

**Risks:**

- *Risk:* The cost-feedback fields (`_cost_units` etc.) reveal upstream API costs to the host LLM, which could be used adversarially by a prompt-injection chain to learn quota-exhaustion thresholds.
  *Mitigation:* The cost units are already public information (Google's Gmail API docs publish the per-method unit costs); the upstream_ms is observable from latency. Exposing them per call adds no new attacker capability.
- *Risk:* `«u»value«/u»` delimiter collides with legitimate use of guillemets in French email bodies, causing the host LLM to misinterpret real text as untrusted-content boundaries.
  *Mitigation:* The collision requires the exact sequence `«u»` (open guillemet immediately followed by ASCII `u` immediately followed by closing guillemet) in legitimate text — vanishingly improbable. Documented; a verification test in Layer 1 fuzzes legitimate text against the delimiter to confirm zero false positives in a corpus of French / German / Russian emails.
- *Risk:* Agents that read `_cache_hit: true` decide to skip refreshing data they should have refreshed, leading to staleness bugs.
  *Mitigation:* `_cache_hit` is read-only signaling; the cache's own consistency model ([ADR-0009](0009-caching-with-sqlite-and-history-api.md) historyId watermark) is what governs freshness. The field's purpose is observability, not authority.
- *Risk:* `mode: "failures_only"` default on batches surprises agents expecting per-item results, leading to misinterpretation ("everything succeeded" mistakenly inferred from missing data).
  *Mitigation:* `succeeded_count: N` is always present in batch responses regardless of mode — the explicit count distinguishes "0 succeeded, 0 failed" from "200 succeeded, 0 failed."
- *Risk:* The ADR-0015 activation-trigger clarification is misread as a license to break things forever — *"we haven't advertised yet"* used as an indefinite escape hatch.
  *Mitigation:* The clarification names a concrete trigger (the four conditions listed) and explicitly states post-trigger the full policy enforces. Future contributors can verify trigger fire by checking the listed conditions.

## References

- [ADR-0008](0008-observability-and-deployment.md) — amended (per-call cost feedback intersects observability)
- [ADR-0014](0014-status-introspection-tool.md) — amended (`mcp_status` filterability)
- [ADR-0015](0015-tool-versioning-policy.md) — clarified (activation trigger = public advertisement, not v1.0 tag)
- [ADR-0016](0016-tool-surface-and-conventions.md) — amended (`fields_mask`, `format`, batch mode, per-call meta fields)
- [ADR-0018](0018-email-content-trust.md) — amended (compact `«u»...«/u»` delimiter, drops `KIND`)
- [ADR-0022](0022-capability-gating.md) — `mcp_status` capability matrix is the response shape this ADR's filtering parameter scopes
- [ADR-0023](0023-calendar-service-surface.md), [ADR-0024](0024-contacts-service-surface.md), [ADR-0025](0025-drive-service-surface.md), [ADR-0026](0026-gmail-tool-surface-phase-2.md) — service surfaces inherit all conventions above from this amendment
