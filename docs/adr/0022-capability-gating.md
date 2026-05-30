# ADR-0022: Capability gating — service × aspect toggles layered on the OAuth scope ceiling

**Date:** 2026-05-28
**Status:** Accepted, target v1.1

---

## Context

[ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) commits to adding Calendar, Contacts, and Drive as service modules. The maintainer wants all three at **full read + write**, with **an operator-facing way to enable or disable individual aspects** of a service — e.g. "Calendar read, but no write"; "Drive metadata, but never `update_permissions`"; "Gmail everything on my personal account, read-only on my work account."

[ADR-0006](0006-config.md) already has the coarse half of this: `[services.<name>]` with `enabled = true|false` and a `scopes` array. A disabled service contributes no tools and requests no scopes. Calendar and Contacts already ship pre-stubbed at `enabled = false`. But that switch is all-or-nothing per service. It cannot express "this service is on, but only its read aspect," which is exactly the control the three new full-read-write services need — each one introduces irreversible operations (`delete_event`, `delete_contact`, `delete_file`, `update_permissions`) whose blast radius the operator will want to gate independently of the safe read path.

There are two independent gates that already exist and must not be conflated:

- **OAuth scope** — set per account at `auth add` / `auth grant` ([ADR-0002](0002-multi-account-architecture.md), [ADR-0004](0004-oauth-token-refresh.md)). This is the *hard ceiling*: the daemon physically cannot perform an operation Google never granted a token for.
- **Service `enabled` flag** — the ADR-0006 master switch.

What's missing is a *soft control within the granted scope*: the operator has granted `https://www.googleapis.com/auth/calendar` (full read-write scope, because Google's Calendar scopes are coarse), but wants the daemon to refuse writes anyway. Scope alone can't express that — Google offers no "read-only slice" of many scopes — so the daemon must enforce it.

If no decision were made: each new service either ships all-or-nothing (the operator who wants Calendar-read-only must decline the whole service), or every service invents its own ad-hoc toggle, and the result is an inconsistent enablement surface across Gmail/Calendar/Contacts/Drive that the operator cannot reason about and `mcp_status` cannot report uniformly.

This ADR is the prerequisite for ADR-0023 (Calendar), ADR-0024 (Contacts), and ADR-0025 (Drive): each of those inherits the aspect vocabulary and config shape defined here rather than reinventing it.

## Decision

We add a third gate — **per-aspect capability toggles** — layered between the service `enabled` flag and the individual tools. The effective permission for any tool call is the intersection of all three gates:

```
allowed(account, tool) =
      service_enabled(service_of(tool))                    // ADR-0006 master switch
  AND scope_granted(account, scope_of(tool))               // OAuth hard ceiling (ADR-0004)
  AND aspect_enabled(account, service_of(tool), aspect_of(tool))   // NEW — this ADR
```

If any gate is closed, the call is refused. The three gates are checked in that order so the error names the *most actionable* cause (a disabled service is a bigger operator decision than a disabled aspect).

### The three aspects

Every tool is classified into exactly one aspect. The taxonomy reuses the existing tool metadata ([ADR-0011](0011-audit-log.md) already defines `is_destructive()`; this ADR generalizes it to a single `aspect()`):

| Aspect | Definition | Examples |
| --- | --- | --- |
| **read** | No mutation of Google-side state. Idempotent, side-effect-free. | `search_threads`, `get_thread`, `list_events`, `get_event`, `list_contacts`, `list_files`, `download_file` |
| **write** | Creates or modifies Google-side state, but the change is recoverable / low-blast. | `modify_thread_labels`, `archive_thread`, `create_event`, `update_event`, `create_contact`, `upload_file`, `create_folder` |
| **destructive** | Irreversible, externally-visible, or high-blast-radius. | `send_email`, `trash_thread`, `delete_event`, `delete_contact`, `trash_file`, `delete_file`, `update_permissions`, `purge_account` |

`send_email` is **destructive**, not write: it cannot be unsent and it is externally visible. This is consistent with [ADR-0012](0012-idempotency-and-dry-run.md), which already gates `send_email` behind `dry_run` + send-deduplication. Aspect classification is asserted in the Layer-4 snapshot ([ADR-0015](0015-tool-versioning-policy.md)) so it cannot drift silently.

### Config shape — extends ADR-0006 `[services.<name>]`

```toml
[services.calendar]
enabled = true
scopes  = ["https://www.googleapis.com/auth/calendar"]

# NEW (this ADR): per-aspect gating. Omitted block ⇒ defaults below.
[services.calendar.capabilities]
read        = true
write       = true
destructive = false        # operator hasn't opted into delete_event yet

# NEW (this ADR): optional per-account override. Merges over the service-level block.
[services.calendar.accounts.work.capabilities]
destructive = true         # work account may delete events
```

Resolution precedence (most specific wins): **per-tool override → per-account capability → service capability → built-in default**.

### Per-tool override — sanctioned exception, not the default

The aspect-level vocabulary (read / write / destructive) is the default surface for capability gating. A small number of tools have a finer threat-model distinction that aspect-level can't express; those are sanctioned exceptions:

- **Contacts' three read populations** (`list_contacts`, `list_other_contacts`, `list_directory_people` per [ADR-0024](0024-contacts-service-surface.md)) — primary, auto-saved-from-Gmail, and Workspace directory have genuinely different trust postures inside the same `read` aspect.
- **Gmail's `send_draft`** (per [ADR-0026](0026-gmail-tool-surface-phase-2.md)) — the grandfathered Gmail-all-on default leaves an operator wanting "drafts management but no automated sends" without a clean knob; per-tool override lets them disable just `send_draft` while keeping the rest of the destructive aspect on.

The override shape is uniform across exceptions:

```toml
[services.<service>.tools.<tool_name>]
enabled = false
```

Resolution: per-tool override is the highest precedence in the resolution ladder above. **New per-tool overrides require a follow-up ADR (or amendment to this one) naming the tool and the threat-model rationale.** The mechanism is not a general escape hatch — the bar to add one is the same bar applied to ADR-0024 and ADR-0026: aspect-level coarser than the real distinction.

### Default posture

| Service | read | write | destructive | Rationale |
| --- | --- | --- | --- | --- |
| **gmail** | true | true | true | **Grandfathered.** Gmail shipped all-on in v1.0 ([ADR-0006](0006-config.md)); flipping it to write-off on upgrade would silently break every existing operator. Backward compatibility wins. |
| **calendar / contacts / drive** (and any future service) | **true** | **false** | **false** | Conservative. A freshly-enabled service can read immediately but cannot mutate until the operator deliberately opts in — mirrors the read-only-profile precedent (#22) and the safe-default posture of [ADR-0019](0019-data-retention-and-purge.md) / [ADR-0020](0020-http-transport-authentication.md). |

The asymmetry is deliberate and documented: Gmail's default is compatibility-driven, new services' default is safety-driven.

### Tool visibility vs. per-account enforcement

The MCP tool list is **one list** for the whole daemon — it is not per-account (a single session may call any account). So visibility and enforcement operate at different layers:

- **Advertised tool list:** a tool is advertised if its aspect is enabled for *at least one* enabled account on an enabled service whose scope is granted. A tool that is disabled for *every* account is hidden entirely — no point advertising what nothing can call, and a smaller tool list is a smaller prompt-injection surface ([ADR-0018](0018-email-content-trust.md)).
- **Per-call enforcement:** because the list is global, a tool advertised on behalf of account A but disabled for account B must still be *present*. When the host calls it with `account: "B"`, the daemon returns a typed `CapabilityDisabled` error. Visibility is the coarse signal; the per-account check at call time is authoritative.

### Scope-coupling failure modes (honest failure)

Per the SPEC excellence criterion "surface the gap and point at the fix," the two mismatch cases resolve differently:

| Config says | Scope granted? | Behavior |
| --- | --- | --- |
| aspect **enabled** | **no** | Startup `WARN`; tool stays present; call returns `CapabilityDisabled { reason: "scope `<scope>` not granted for account `<a>`; run `auth grant <a> <scope>`" }`. The operator asked for it but didn't grant scope — tell them how to fix it, don't silently hide. |
| aspect **disabled** | yes | Tool hidden (if disabled for all accounts) or per-account-errors. The operator's deliberate choice; no warning needed. |

### New error variant

[ADR-0005](0005-error-model.md) gains one typed variant:

```rust
Error::CapabilityDisabled {
    service: &'static str,      // "calendar"
    aspect:  Aspect,            // Write
    account: String,            // "work"
    reason:  String,            // actionable: which gate closed, and how to open it
}
```

It maps to the MCP `invalid_params` error path (the request is well-formed but not permitted in this configuration), never to `internal_error`.

### Interaction with the snapshot test (ADR-0015)

A config-dependent tool list would make the Layer-4 snapshot non-deterministic. Resolution: the snapshot is generated against a **canonical fixture config** — all services enabled, all aspects enabled, one synthetic fully-scoped account — so the registry it captures is the *complete superset*. Runtime visibility filtering is a separate layer applied below the snapshot boundary; it never changes the snapshot. The snapshot therefore continues to assert the full contract, and capability filtering is tested separately with its own fixtures.

### `mcp_status` reporting (ADR-0014)

`mcp_status` ([ADR-0014](0014-status-introspection-tool.md)) gains a per-account capability matrix: for each (account, service), the effective `{read, write, destructive}` booleans and, where an aspect is forced off by a missing scope, that reason. This is the operator's single window into "what can the daemon actually do right now," which is the whole point of the tool.

## Options Considered

### Granularity

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Service on/off only (status quo, ADR-0006) | Simplest; already shipped | Cannot express "read but not write" — the exact control the new services need |
| **(b) Service × aspect {read, write, destructive}, per-account override, per-tool override as sanctioned exception** (chosen) | Matches the existing tool taxonomy (ADR-0011/0012); three buckets are few enough to reason about; per-account fits the 10+-account threat-varied reality; per-tool exceptions cover the cases aspect-level is too coarse for (Contacts populations, Gmail `send_draft`) | Two more config layers; tools must each declare an aspect; per-tool exception list grows by ADR amendment, not freely |
| (c) Per-tool toggles as the default vocabulary | Maximum control | Explodes config surface (30+ tools); operator can't reason about it; no natural default |
| (d) Per-scope toggles (mirror Google's scopes) | Aligns with OAuth | Google's scopes are coarse and inconsistent across services; can't express read-only where Google offers no read-only scope |

We choose (b). Three aspects map 1:1 onto distinctions the codebase already draws (`is_destructive()`, `dry_run`), and per-account override is required because the operator's accounts have genuinely different trust levels.

### Enforcement layer

| Option | Pros | Cons |
| --- | --- | --- |
| (e) Scope only — "don't grant the scope you don't want" | Zero new code; OS/Google enforce | Google's scopes can't express most read/write splits; an operator who wants Calendar-read must decline a scope that also gates the reads they *do* want |
| **(f) Config toggle enforced by the daemon, layered on the scope ceiling** (chosen) | Expresses splits Google's scopes can't; daemon is the natural enforcement point; composes with the existing `enabled` switch | Daemon must be trusted to enforce (acceptable — single-operator threat model, ADR-0001) |

### Tool visibility when disabled

| Option | Pros | Cons |
| --- | --- | --- |
| (g) Hide disabled tools from the list | Smaller prompt surface; host won't attempt disabled tools | Tool list becomes config-dependent (snapshot tension, resolved above); per-account complicates "hide" |
| (h) Always present, error on call | Stable tool list; simple snapshot | Advertises tools that always fail for some accounts; larger prompt surface |
| **(i) Hybrid: hide iff disabled for all accounts, else present + per-call error** (chosen) | Honors the global-list reality; smallest surface that still works per-account; snapshot uses the all-enabled superset | Two-layer mental model (visibility vs enforcement) |

## Consequences

**Positive:**

- The three new services (ADR-0023/0024/0025) inherit one consistent enablement vocabulary instead of each inventing its own.
- An operator can run Calendar/Contacts/Drive read-only on day one and opt into writes per service, per aspect, per account, deliberately.
- The safe default (new services read-only) means enabling a service is never a surprise grant of delete power.
- Gmail's grandfathered all-on default means zero breakage for existing v1.0 operators on upgrade.
- `mcp_status` becomes the single source of truth for "what can the daemon do," including scope-gap diagnosis.
- The hard scope ceiling is preserved as defense-in-depth: a config bug that enables an aspect still can't exceed the granted OAuth scope.

**Negative:**

- One more config layer and one more `ToolMetadata` method (`aspect()`) every tool must declare.
- The visibility/enforcement split is a two-layer model contributors must understand (documented in CONTRIBUTING).
- Per-account capability overrides multiply the config validation matrix (each override must be checked against the service's scope set at load time).

**Risks:**

- *Risk:* Operator disables an aspect in config but the corresponding scope is still granted, and assumes the scope grant is what matters — confusion about which gate is authoritative.
  *Mitigation:* `mcp_status` shows the *effective* (post-intersection) capability, not the raw config or raw scope. The error message names the closed gate explicitly.
- *Risk:* Aspect misclassification — a genuinely destructive tool tagged `write` slips past a `destructive=false` gate.
  *Mitigation:* Aspect is asserted in the Layer-4 snapshot; adding or reclassifying a tool shows in the snapshot diff and is reviewed as a contract change (ADR-0015).
- *Risk:* Grandfathered Gmail default drifts — a future contributor "tidies" Gmail to the conservative default and breaks existing operators.
  *Mitigation:* The asymmetry is documented here and in the config example; a startup self-check can assert Gmail's default is all-on unless explicitly overridden.
- *Risk:* The canonical-fixture snapshot hides a real capability-filtering bug because the snapshot never exercises a disabled state.
  *Mitigation:* Capability filtering gets its own dedicated test fixtures (disabled-for-all → hidden; disabled-for-one-account → present + per-call error), separate from the contract snapshot.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — service-module architecture; the three services this gating serves
- [ADR-0002](0002-multi-account-architecture.md) — per-account model; per-account capability override builds on it
- [ADR-0004](0004-oauth-token-refresh.md) — OAuth scope grant, the hard ceiling
- [ADR-0005](0005-error-model.md) — amended: new `CapabilityDisabled` variant
- [ADR-0006](0006-config.md) — amended: `[services.<name>.capabilities]` and per-account override extend the existing `[services.<name>]` block
- [ADR-0011](0011-audit-log.md) — `is_destructive()` generalized to `aspect()`
- [ADR-0012](0012-idempotency-and-dry-run.md) — why `send_email` is classified destructive
- [ADR-0014](0014-status-introspection-tool.md) — amended: `mcp_status` reports the per-account capability matrix
- [ADR-0015](0015-tool-versioning-policy.md) — aspect asserted in the Layer-4 snapshot; canonical-fixture resolution for config-dependent visibility
- [ADR-0018](0018-email-content-trust.md) — smaller advertised tool surface reduces prompt-injection surface
- ADR-0023 / ADR-0024 / ADR-0025 — Calendar / Contacts / Drive surfaces that inherit this model (issues #189 / #190 / #191)
- Issue [#188](https://github.com/torsday/google-personal-mcp/issues/188) — this spike
