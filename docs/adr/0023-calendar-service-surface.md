# ADR-0023: Calendar service surface — full read/write/destructive under the capability gate

**Date:** 2026-05-28
**Status:** Accepted, target v1.1
**Depends on:** [ADR-0022](0022-capability-gating.md)

---

## Context

[ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) commits to Calendar as the next service module after Gmail. The maintainer wants the full Google Calendar v3 surface — list/read events, freebusy queries, create/update/respond, and delete — gated by the capability model from [ADR-0022](0022-capability-gating.md) so operators can run Calendar read-only, read+write-but-not-destructive, or fully open on a per-account basis.

Calendar is *not* a copy of Gmail with different nouns:

- **Query model is time-range, not full-text.** `events.list` is parameterized by `timeMin` / `timeMax` plus an optional `q` for free-text. Gmail's powerful `from:`/`to:`/`label:` operators have no direct analog.
- **Recurrence is first-class.** Events can repeat, and `update`/`delete` distinguish "this single instance" from "this and all following" from "the whole series." This is the load-bearing distinction Gmail doesn't have.
- **Attendees are an externally-visible side effect.** Creating/updating an event with attendees sends invitations (or doesn't, depending on `sendUpdates`). That makes some writes externally-observable in a way Gmail label changes are not.
- **Multiple calendars per account.** The "primary" calendar coexists with shared, subscribed, and secondary calendars; every event lives in exactly one calendar.

If no decision were made, Calendar tools would either copy Gmail conventions inappropriately (a `query` parameter that means something different) or invent fresh conventions that drift from [ADR-0016](0016-tool-surface-and-conventions.md). This ADR pins down the surface, the scope-to-capability mapping, the recurrence/notification semantics, and the trust posture before any tool ships.

## Decision

We adopt a 10-tool Calendar surface that follows [ADR-0016](0016-tool-surface-and-conventions.md) conventions (`account: String` required, `_untrusted` suffixes on attacker-controllable fields, `dry_run` on destructive ops, `batch_` prefix for batch shapes), classified per [ADR-0022](0022-capability-gating.md) aspects, and mapped onto Google's Calendar scopes.

### Tool inventory

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `list_calendars` | read | `calendar.readonly` (calendar list metadata; **not** included in `calendar.events.readonly`) | Returns all calendars the account can see; primary flagged explicitly |
| `list_events` | read | `calendar.events.readonly` | Time-range + optional `q`; recurrence expansion controlled by `single_events: bool` (default `true`); pagination via `page_token` |
| `get_event` | read | `calendar.events.readonly` | By `(calendar_id, event_id)`; for recurring events, optional `instance_id` returns one occurrence |
| `query_freebusy` | read | `calendar.readonly` | Multi-calendar availability over a time window; high-value scheduling primitive |
| `create_event` | write | `calendar.events` | Returns the created event; honors `send_updates: "all" \| "external_only" \| "none"` (MCP default `none`; see §Notification semantics) |
| `update_event` | write | `calendar.events` | Requires `etag` for optimistic concurrency; recurring-event scope param: `"single" \| "following" \| "series"` |
| `respond_to_event` | write | `calendar.events` | Sets attendee `responseStatus` to `"accepted" \| "declined" \| "tentative"`; optional `comment_untrusted` is forwarded verbatim |
| `delete_event` | destructive | `calendar.events` | Recurring-event scope param same as update; `dry_run: bool` returns what would be deleted; `send_updates` controls cancellation notifications |
| `batch_modify_events` | write | `calendar.events` | Parallel `update_event` across `event_refs: Vec<EventRef>`; per-item etag preserved |
| `batch_delete_events` | destructive | `calendar.events` | Parallel `delete_event`; `dry_run` aggregates; order-preserving per [ADR-0016](0016-tool-surface-and-conventions.md) |

The `respond_to_event` separation from `update_event` is deliberate: responding to an invite is a high-frequency, low-stakes write that doesn't merit the etag round-trip a general update does. Single-purpose tool, single quota call.

### Scope-to-capability mapping

Per [ADR-0022](0022-capability-gating.md), each tool gates on `scope_granted(account) ∩ aspect_enabled(account, calendar, aspect)`.

| Config aspect | Tools enabled | Scopes implied |
| --- | --- | --- |
| read | `list_*`, `get_event`, `query_freebusy` | `calendar.readonly` or `calendar.events.readonly` |
| write | `create_event`, `update_event`, `respond_to_event`, `batch_modify_events` | `calendar.events` (write subset of `calendar`) |
| destructive | `delete_event`, `batch_delete_events` | `calendar.events` |

`calendar` (full read+write on calendars themselves — create, delete, share) is **not** requested by default; this ADR's surface needs only `calendar.events` (or its readonly counterpart) for the events surface. Calendar-management tools (create_calendar, delete_calendar, manage_acl) are out of scope for this ADR; if needed later, a follow-up ADR adds them under their own capability scope.

### Untrusted-content posture

Per [ADR-0018](0018-email-content-trust.md), every field that originates outside the operator's own action wraps in `<<<UNTRUSTED:KIND ... UNTRUSTED>>>`. For Calendar specifically:

| Field | Trust | Why |
| --- | --- | --- |
| `summary_untrusted` (title) | untrusted | Invites from anyone with the operator's email can set this |
| `description_untrusted` | untrusted | Same |
| `location_untrusted` | untrusted | Same; commonly carries URLs |
| `attendees[].display_name_untrusted`, `email_untrusted` | untrusted | Attacker-supplied on inbound invites |
| `attendees[].comment_untrusted` | untrusted | Free text in responses |
| `creator_untrusted`, `organizer_untrusted` | untrusted | Attacker-controllable on received invites |
| `conference_data.entry_points[].uri_untrusted` | untrusted | Hangouts/Meet/Zoom links from invites; user must verify before joining |
| `internal_date`, `etag`, recurrence rule (`rrule`), `start`/`end` time | trusted | Google-side metadata, not free-form attacker text |
| `id`, `calendar_id`, `status` enum | trusted | Opaque/typed |

The wrapping convention forwards verbatim — the MCP does not sanitize, classify, or rewrite per [ADR-0018](0018-email-content-trust.md).

### Query model

`list_events` takes:

```rust
list_events(
  account: String,
  calendar_id: String,                       // "primary" or a calendar id from list_calendars
  time_min: String,                          // ISO-8601; required
  time_max: String,                          // ISO-8601; required
  q: Option<String>,                         // free-text, forwarded verbatim to Calendar's `q` parameter
  single_events: bool,                       // default true; expand recurrences into instances
  order_by: Option<OrderBy>,                 // "start_time" (default when single_events) | "updated"
  max_results: u32,                          // default 250, max 2500
  page_token: Option<String>,
) -> ListEventsResponse
```

`time_min` and `time_max` are required — unbounded listing is a footgun (a calendar with 10 years of history can return 100K events). The cap is **explicit and operator-facing**.

`query_freebusy` takes a list of calendar ids and a time window, returns busy intervals per calendar. No event details — just busy/free, matching the underlying API's privacy posture.

### Recurrence semantics

`update_event` and `delete_event` on a recurring event accept a `recurrence_scope` parameter that the MCP **maps onto Google Calendar's underlying recurrence model** (which encodes the same three choices implicitly via *which resource id you address* — the parent event vs an instance id — and, for "this and following," via a recurrence-rule split):

- `"single"` — modify/delete only this instance. The MCP addresses the *instance* resource (`<event_id>_<original_start_time>`) and creates an exception or cancellation override.
- `"following"` — modify/delete from this instance forward. The MCP splits the parent recurrence rule at the given date (sets `UNTIL` on the original RRULE and creates a new series starting at the given instance).
- `"series"` — modify/delete the parent event id (which Google propagates to all instances).

Exposing the three high-level choices as one named parameter — rather than asking callers to know "address the parent for series, address the instance for single, do a two-call rule-split for following" — is the abstraction this tool earns. Default is `"single"` — the least-blast-radius choice for an accidental call. Tool descriptions make the three choices explicit and link to this section.

### Cache fit

Calendar has `events.list?syncToken=...` for incremental sync, structurally analogous to Gmail's `history.list`. **Defer caching to a follow-up ADR.** Rationale matching Gmail's ADR-0009 timing: ship the live-fetch path first, observe real workload, then decide whether incremental sync earns its complexity. Calendar workloads are also lower-frequency than Gmail (events change at meeting cadence, not minute-to-minute), so caching's value-per-LOC is lower.

### Notification (`sendUpdates`) semantics

Tools that touch attendees take `send_updates: SendUpdates` with values `"all" \| "external_only" \| "none"`. **Default `"none"`** — the agent must explicitly opt into sending invitation emails to attendees. Rationale: silently emailing a list of attendees on the agent's say-so is a real prompt-injection blast-radius concern; explicit param + default-off is the safe posture. Tool descriptions name the default.

### Out of scope (intentionally)

- **Calendar creation/deletion/sharing.** `create_calendar`, `delete_calendar`, `acl` operations. Separate ADR if a real use case emerges.
- **Settings.** Timezone, default reminders, working hours — operator manages in the web UI; not normal agent territory.
- **Conferencing creation.** We forward existing `conferenceData` on read; we don't synthesize new Meet links on create. Defer to a follow-up ADR; mandatory `conferenceDataVersion` request param + side-effect of generating a meeting link is its own design.
- **Reminders push.** Notification *delivery* (popup vs email) is per-calendar settings; we accept the calendar's default and surface a per-event `reminders` field on read but don't override on write in v1.
- **Snooze.** Snoozing a calendar event isn't a standard Calendar API operation.

## Options Considered

### Single `modify_event` vs `update_event` + `respond_to_event`

| Option | Pros | Cons |
| --- | --- | --- |
| (a) One `modify_event` tool, attendee-response folded in | Smaller surface | Forces every RSVP through full etag round-trip; conflates two semantically distinct ops; harder description |
| **(b) Two tools** (chosen) | Common RSVP path is one quota call; clear semantics; etag only where it matters | Two tools instead of one |

### Recurrence-scope default

| Option | Pros | Cons |
| --- | --- | --- |
| (c) Default `"series"` (whole series) | Matches Calendar UI's "yes apply to all" default | Highest blast radius on accidental call |
| **(d) Default `"single"`** (chosen) | Smallest blast radius; reversible by hand if wrong; explicit `"series"` matches the deliberate ask | Operator who wants series must say so |
| (e) Require explicit param, no default | Most explicit | Friction; most calls in practice want single-instance |

### `send_updates` default

| Option | Pros | Cons |
| --- | --- | --- |
| (f) Default `"all"` (Google's API default) | Matches Calendar UI behavior | Silently emails attendees on every agent-driven create/update |
| **(g) Default `"none"`** (chosen) | Agent must explicitly choose to email humans; respects ADR-0018 blast-radius posture | Diverges from Google's default — documented |

### Caching now vs later

| Option | Pros | Cons |
| --- | --- | --- |
| (h) Cache from day one (events + syncToken) | Lower steady-state quota cost | Premature — ADR-0009 cache complexity for unknown workload shape |
| **(i) Live-fetch v1; cache in a follow-up ADR** (chosen) | Same staged approach Gmail used; data on real workload before caching | Higher quota cost initially; acceptable |

## Consequences

**Positive:**

- The full event-management surface ships with one ADR; downstream tickets fall directly out of the tool inventory table above.
- Aspect classification means operators can ship Calendar read-only on day one and opt into writes/destructives per account deliberately.
- Recurrence semantics are exposed verbatim rather than abstracted — agents that don't understand recurrences fail loudly (`recurrence_scope` required for recurring events) rather than silently doing the wrong thing.
- `send_updates: none` default keeps agent-driven invitations from becoming a prompt-injection vector.

**Negative:**

- 10 tools is a meaningful surface-area expansion; each tool description, snapshot, and trust audit grows.
- Time-range required on `list_events` adds friction for "show me everything ever" agents — but unbounded list against a real calendar is a real footgun.
- Calendar's API has parts we deliberately don't surface (calendar management, ACL, conferencing creation) — gaps a future contributor may reopen.

**Risks:**

- *Risk:* Agent calls `update_event` on a recurring event with the default `recurrence_scope: "single"`, expects the whole series to change, gets confused by the surprising result.
  *Mitigation:* Tool description leads with the default; `update_event` response includes the `recurrence_scope_applied` field so the caller can see what happened.
- *Risk:* `query_freebusy` over many calendars/long windows balloons quota.
  *Mitigation:* Google enforces a `calendarExpansionMax: 50` cap on `freebusy.query` (hard refuse above 50 calendars). We surface that limit in the tool description and validate before issuing the call. For the time window, the MCP applies its own guard (default 31 days, configurable) — there is no documented Google cap on the window, but the API materializes busy intervals in memory, so an unbounded request against many calendars is a real cost; the MCP-side guard documents that intent.
- *Risk:* The `_untrusted` wrappers on event fields create a forest of structurally-similar wrappers; host LLMs may pattern-match to a "safe" shape and trust them inappropriately.
  *Mitigation:* This is the same risk Gmail's wrapping has (per [ADR-0018](0018-email-content-trust.md)) and the same answer: structural markers, not classifiers; downstream host is responsible for its own posture.
- *Risk:* Default `send_updates: "none"` causes confusion when operators create events with attendees and expect Google's normal "everyone gets emailed" default.
  *Mitigation:* Documented divergence; tool description names the default and the override.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — Calendar is the first follow-on service after Gmail
- [ADR-0002](0002-multi-account-architecture.md) — per-account isolation extends to Calendar
- [ADR-0005](0005-error-model.md) — typed errors used by all tools
- [ADR-0012](0012-idempotency-and-dry-run.md) — `dry_run` on `delete_event` / `batch_delete_events`
- [ADR-0015](0015-tool-versioning-policy.md) — Layer-4 snapshot captures every tool above
- [ADR-0016](0016-tool-surface-and-conventions.md) — naming + parameter conventions inherited
- [ADR-0018](0018-email-content-trust.md) — untrusted-content wrapping conventions
- [ADR-0022](0022-capability-gating.md) — aspect classification + per-account toggle
- Issue [#189](https://github.com/torsday/google-personal-mcp/issues/189) — this spike
- [Google Calendar API v3 reference](https://developers.google.com/calendar/api/v3/reference)
