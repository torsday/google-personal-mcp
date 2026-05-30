# ADR-0024: Contacts (People API) service surface — full read/write/destructive under the capability gate

**Date:** 2026-05-28
**Status:** Accepted, target v1.1
**Depends on:** [ADR-0022](0022-capability-gating.md)

---

## Context

[ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) lists Contacts as a near-future service. The maintainer wants full read/write/destructive Contacts coverage, gated by the [ADR-0022](0022-capability-gating.md) capability model. The natural payoff is direct: name→address resolution makes the existing Gmail search and send paths materially better (an agent asked "email my landlord" doesn't have to be told an address).

The right backing API is **Google's People API** — Google Contacts API v3 is deprecated and being retired. People API differs from the contacts surface other ADRs in this corpus model:

- **`resourceName` + `etag` is the identity + concurrency pattern.** Every Person and ContactGroup has an opaque `resourceName` (e.g. `people/c12345`) and an `etag` returned with every read. Updates require the matching `etag`; mismatch returns 400.
- **`personFields` mask is required on reads.** Unlike Gmail's "get the whole thread" pattern, People API requires the caller to name which fields to return. Field selection is a real parameter, not a quality-of-implementation detail.
- **Two contact populations live side by side:** primary contacts (the operator's address book) and **"Other contacts"** (Gmail auto-saved senders, read-only). They have different scopes and lifecycle.
- **Directory contacts** (Workspace org-wide directory) are a third population, also read-only, with its own scope.
- **ContactGroups** are the label-analog for organizing contacts; system groups (`myContacts`, `starred`) coexist with user groups.

If no decision were made, the surface would either copy Gmail label semantics inappropriately to ContactGroups, or hide the populations distinction and surprise agents who try to update an "Other contact" they read (and get a 403).

## Decision

We adopt a 12-tool Contacts surface following [ADR-0016](0016-tool-surface-and-conventions.md) conventions, classified per [ADR-0022](0022-capability-gating.md) aspects, mapped onto People API scopes.

### Tool inventory

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `list_contacts` | read | `contacts.readonly` | Primary contacts; paginated via `page_token` |
| `search_contacts` | read | `contacts.readonly` | People API `searchContacts`: prefix match (not substring/full-text) on names / nicknames / email addresses / phone numbers / organizations; capped at 30 results; `CONTACT` source only (no Other / Directory); read-after-write via `readMask` |
| `get_contact` | read | `contacts.readonly` | By `resource_name`; required `person_fields` mask |
| `list_other_contacts` | read | `contacts.other.readonly` | Auto-saved-from-Gmail contacts; read-only by design |
| `list_directory_people` | read | `directory.readonly` | Workspace org directory; absent / 403 on consumer accounts |
| `list_contact_groups` | read | `contacts.readonly` | System + user groups; flagged via `group_type` |
| `get_contact_group` | read | `contacts.readonly` | By `resource_name`; includes member resource names |
| `create_contact` | write | `contacts` | Returns the created Person; client must supply at least one name or email |
| `update_contact` | write | `contacts` | Requires `etag`; partial update via `update_person_fields` mask |
| `delete_contact` | destructive | `contacts` | By `resource_name`; `dry_run: bool` per [ADR-0012](0012-idempotency-and-dry-run.md) |
| `modify_contact_group_membership` | write | `contacts` | Add/remove `resource_names` from a `contact_group_resource_name`; single tool covers both directions per Google's API |
| `batch_delete_contacts` | destructive | `contacts` | Parallel `delete_contact`; `dry_run` aggregates |

`merge_contacts` (Google's UI feature for deduping) is intentionally absent — the API surface is awkward and the use case is rare; defer to a follow-up ADR.

`create_contact_group` / `delete_contact_group` are intentionally absent in v1 — a follow-up ADR can add them. The membership tool covers the high-frequency need ("tag this contact with my custom 'family' group") without the lower-frequency vocabulary management.

### Scope-to-capability mapping

| Config aspect | Tools enabled | Scopes implied |
| --- | --- | --- |
| read | `list_contacts`, `search_contacts`, `get_contact`, `list_other_contacts`, `list_directory_people`, `list_contact_groups`, `get_contact_group` | `contacts.readonly` (+ `contacts.other.readonly`, `directory.readonly` as needed) |
| write | `create_contact`, `update_contact`, `modify_contact_group_membership` | `contacts` |
| destructive | `delete_contact`, `batch_delete_contacts` | `contacts` |

The scope set is granular by population:

- **Primary contacts read or write** → `contacts.readonly` / `contacts`
- **Other contacts** → `contacts.other.readonly` (no write scope exists at Google — Other contacts auto-populate and are not directly editable; promoting one to a primary contact happens via `create_contact` with the same details)
- **Directory** → `directory.readonly`

The capability config can disable read on `list_other_contacts` / `list_directory_people` independently from primary read by **disabling them at the per-tool aspect level** — addressed below.

### Per-tool aspect override

A tension surfaces: an operator may want `read = true` for primary contacts but `read = false` for Directory (work-account org members shouldn't be readable by the agent). [ADR-0022](0022-capability-gating.md)'s per-aspect default doesn't address that finer distinction.

**Decision:** add an optional `[services.contacts.tools.<name>]` block in config for narrower overrides:

```toml
[services.contacts.capabilities]
read  = true
write = true

[services.contacts.tools.list_directory_people]
enabled = false        # override: do not advertise/run this read tool
```

Contacts is the **first** service to use per-tool granularity; the mechanism is **sanctioned by ADR-0022** as a named exception for cases where aspect-level is coarser than the real distinction. [ADR-0026](0026-gmail-tool-surface-phase-2.md) extends the same mechanism to Gmail's `send_draft`. Both exceptions are listed explicitly in [ADR-0022 §Per-tool override](0022-capability-gating.md); a new per-tool exception requires its own ADR justification.

### Untrusted-content posture

Auto-saved-from-Gmail and Directory contacts are **fully attacker-influenceable** — anyone who sends the operator an email contributes to the Other contacts list. Primary contacts the operator created are trustworthy in identity but their *fields* (display name, email, address) are still strings whose only validation is "well-formed."

| Field | Trust | Why |
| --- | --- | --- |
| `display_name_untrusted` | untrusted | Attacker-set on inbound mail; operator-set on primary contacts but no validation |
| `email_address_untrusted` | untrusted | Attacker-controllable on Other / Directory; user-typed on primary |
| `phone_untrusted`, `address_untrusted` | untrusted | Same |
| `note_untrusted` | untrusted | Free-form; possible injection surface even on primary contacts |
| `birthday_untrusted`, `organization_untrusted`, `relation_untrusted` | untrusted | Operator-typed on primary; inherited from Other/Directory on auto-saved records. Free-form strings either way; the consistent wrapping disposition avoids per-field trust toggles a host LLM might miss |
| `resource_name`, `etag` | trusted | Google-side identifiers |
| `metadata.sources[].type` (e.g. `CONTACT` vs `OTHER_CONTACT` vs `DIRECTORY`) | trusted | Surfaced so the caller knows the population |

The `metadata.sources[].type` field is **surfaced** so callers can distinguish populations — important because trust posture differs across them.

### `personFields` mask

`get_contact`, `list_contacts`, `search_contacts` all take a required `person_fields: Vec<String>` parameter. The MCP joins it on `,` and forwards as the People API `personFields` (or `readMask` for `searchContacts`) parameter, which the API accepts as a single comma-delimited string. The tool description names the common sets (`["names", "emailAddresses", "phoneNumbers"]`) so callers don't have to hunt for the right values; we do not provide a "give me everything" shortcut — fields cost quota and the per-call shape should be deliberate.

### Cache fit

People API supports `syncToken` on `people.connections.list` for incremental sync. **Defer caching to a follow-up ADR** — same staged approach as Calendar (ADR-0023). Contacts change at human-edit cadence (months between writes on a typical account), so the read-heavy / write-cold pattern that motivates a cache is real, but live-fetch v1 buys the data needed to size the cache correctly.

### `etag` and optimistic concurrency

`update_contact` requires the `etag` returned by the most recent `get_contact`. The MCP does **not** auto-fetch the etag on update — the caller must round-trip to get a fresh copy first. Rationale: silent etag refetch papers over real concurrency conflicts (operator edited the contact in the Gmail UI between the agent's read and write); explicit etag is honest about the read-modify-write contract.

On etag mismatch, the typed error variant ([ADR-0005](0005-error-model.md)) is `ConcurrencyConflict { resource, hint }` with `hint` set to "re-fetch the resource and re-apply your changes."

### Out of scope (intentionally)

- **Contact group CRUD** beyond membership modification. Defer to follow-up.
- **`merge_contacts`** for deduping. Defer to follow-up.
- **Contact photos** (binary data). Defer — base64-blobbing photos through MCP tool responses is a separate design concern.
- **Domain Shared Contacts** (Workspace admin feature). Out of scope per [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md)'s "personal-data, not workspace-admin" framing.
- **Gmail send_email auto-resolves names via Contacts.** Tempting but per [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) no-smart-tools — the host composes by calling `search_contacts` then `send_email` with the resolved address.

## Options Considered

### People API vs Google Contacts API v3

| Option | Pros | Cons |
| --- | --- | --- |
| **(a) People API** (chosen) | Current; supported; richer Person model; future-proof | Slightly different mental model from typical contacts APIs (resourceName, personFields mask) |
| (b) Contacts API v3 (gdata) | Closer to a flat "name + email + phone" shape | Deprecated; being retired; we'd be writing tools against a dead API |

### Per-tool aspect override

| Option | Pros | Cons |
| --- | --- | --- |
| (c) Aspect-only (no per-tool override) | Simplest; matches Calendar/Drive | Cannot express "primary read on, Directory read off" without disabling the whole read aspect |
| **(d) Per-tool override for the three read populations** (chosen) | Expresses the threat-model distinction operators actually have | Adds one config layer; bounded to three tools so the surface stays small |
| (e) Treat each population as a separate "sub-service" with its own aspect set | Most explicit | Triples the config surface; over-engineered for three read tools |

### `personFields` default

| Option | Pros | Cons |
| --- | --- | --- |
| (f) Default to `["names", "emailAddresses"]` if omitted | Friction-free for common cases | Hidden behavior; agent doesn't know what it didn't get |
| **(g) Required, no default** (chosen) | Caller is explicit about what it wants; cost is visible | Tool descriptions need a recipe for common cases |

### `etag` auto-refresh on update

| Option | Pros | Cons |
| --- | --- | --- |
| (h) Auto re-fetch on `etag` mismatch and retry once | Hides intermittent conflicts | Silently masks real concurrency conflicts; "last write wins" is not what optimistic concurrency is for |
| **(i) Explicit etag required; mismatch → typed error** (chosen) | Honest about read-modify-write; operator sees the conflict | One more parameter to plumb |

## Consequences

**Positive:**

- The MCP covers People API completely enough that an agent can run the full address-book workflow without falling back to the Gmail UI.
- Aspect + per-tool granularity lets operators with a work account hide the Directory population entirely while keeping personal contacts available.
- The three-population distinction is surfaced explicitly via `metadata.sources[].type`, so callers know what they're looking at.
- `etag`-required updates make concurrency conflicts visible rather than silent-overwriting.

**Negative:**

- Per-tool config override is a Contacts-only carve-out from the otherwise-uniform aspect model — readers of [ADR-0022](0022-capability-gating.md) need to know this exception exists.
- `personFields` required adds friction for the simple use case; documentation must lead with the common recipe.
- Twelve tools is large for an "auxiliary" service.

**Risks:**

- *Risk:* Agent calls `update_contact` without first calling `get_contact` to obtain the current etag; gets `ConcurrencyConflict` on every update.
  *Mitigation:* Tool description for `update_contact` leads with "call `get_contact` first to obtain `etag`"; the typed error's `hint` repeats the recipe.
- *Risk:* Operator enables `write` aspect but the granted scope is `contacts.readonly`; agent calls `create_contact`, gets a runtime error that says "scope missing."
  *Mitigation:* The honest-failure path from [ADR-0022](0022-capability-gating.md) handles this — startup WARN names the gap and `auth grant` is the prescription.
- *Risk:* Other-contacts and Directory populations get treated as primary contacts by a naive agent; the agent tries to update an Other contact and fails with 403.
  *Mitigation:* `metadata.sources[].type` is in the response shape; tool descriptions note "Other / Directory contacts are read-only at Google's API layer — to promote, `create_contact` with the relevant fields."
- *Risk:* `personFields` masks hide a field the caller assumed was free; quota grows unexpectedly when a follow-up `get_contact` re-fetches with a larger mask.
  *Mitigation:* Cost model in tool description names typical per-field cost; `mcp_status` surfaces per-account People-API quota usage so over-fetching is observable.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — Contacts is the second follow-on service
- [ADR-0002](0002-multi-account-architecture.md) — per-account isolation
- [ADR-0005](0005-error-model.md) — typed `ConcurrencyConflict` variant for etag mismatch
- [ADR-0012](0012-idempotency-and-dry-run.md) — `dry_run` on `delete_contact` / `batch_delete_contacts`
- [ADR-0015](0015-tool-versioning-policy.md) — snapshot covers every tool
- [ADR-0016](0016-tool-surface-and-conventions.md) — naming + parameter conventions
- [ADR-0018](0018-email-content-trust.md) — untrusted-content wrapping
- [ADR-0022](0022-capability-gating.md) — aspect classification; per-tool override carve-out documented here
- Issue [#190](https://github.com/torsday/google-personal-mcp/issues/190) — this spike
- [People API reference](https://developers.google.com/people/api/rest)
