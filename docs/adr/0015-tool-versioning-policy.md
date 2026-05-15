# ADR-0015: Tool versioning policy — additive-only, snapshot-enforced

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

The MCP tools exposed by `google-personal-mcp` are a public contract. Once consumers — Claude Desktop sessions, downstream MCP-of-MCP layers ([ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) explicitly anticipates these), CI scripts that drive the daemon — start using a tool, the tool's name, parameter schema, and response shape become things they depend on.

Without a versioning policy, every PR that touches a tool risks silently breaking downstream consumers. The failure mode is bad: an LLM consumer that worked yesterday returns wrong-typed errors today because the response shape changed. Worse, no compile-time signal — the consumer's prompts to the model are themselves brittle.

This problem is sharper for `google-personal-mcp` than for most MCP servers because:

1. The "data source for other knowledge tools" framing in [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) explicitly invites downstream MCPs to consume our output.
2. We expose a relatively large tool surface (12+ tools across multi-account ops, status, audit, cache, fan-out).
3. The combinatorics of `dry_run` × `account` × `accounts` × per-tool params means the parameter schema isn't trivially stable across casual edits.

If no decision were made: every refactor risks silent breakage; downstream consumers learn about the contract by reverse-engineering it from prompt failures.

## Decision

We will adopt an **additive-only versioning policy** for the tool surface, with **snapshot-test enforcement** in CI.

### What "additive-only" means

For an existing tool (name + parameter schema + response shape), the following changes are **allowed** without a version bump:

- Add a new **optional** parameter (with a sensible default — typically `null` / `None`).
- Add a new field to a structured response (consumers ignore unknown fields per the standard JSON parsing convention).
- Expand a parameter's accepted value set if existing callers' inputs remain valid (e.g., adding `"draft"` to an enum that previously only accepted `"sent"`).
- Improve / clarify a tool description text.
- Add new tools.
- Mark a parameter or tool as **deprecated** in its description (must remain functional for ≥6 months after deprecation announcement).

The following changes are **forbidden** on an existing tool name:

- Rename a parameter.
- Change a parameter's type (string → integer, optional → required, etc.).
- Make an optional parameter required.
- Remove a parameter.
- Change a response field's type or structure.
- Remove a response field.
- Change the meaning of an existing tool name (semantic rewrite without name change).

When a forbidden change is needed, the path is: **publish a new tool with a versioned name** (`search_threads_v2`), keep the old tool functional, deprecate-mark the old tool's description, and remove the old tool only after a 6-month sunset window (or longer for breaking changes that affect destructive ops).

### Naming conventions

- Tool names use `snake_case` and the `verb_noun` form (`search_threads`, `get_thread`, `archive_thread`, `mcp_status`, `audit_summary`).
- Service prefix only when needed for disambiguation across services. As of v1, every tool name is unique without a prefix; if Calendar's `list_events` clashes with a future need, add the prefix at that point (`gmail_list_events`, `calendar_list_events`).
- Versioning suffix only on breaking changes: `_v2`, `_v3`. Never `_new`, `_better`, `_fixed`.
- Lowercase ASCII alphanumeric + underscore; no hyphens (consistent with rmcp's macro generation).

### Response shape conventions

- Top-level `fanout: true` for fan-out responses (per [ADR-0013](0013-cross-account-fan-out.md)).
- Errors flow via MCP's error-result path, **not** as a `success: false` envelope inside the response data.
- Read tools that return objects with versioned schemas (currently only `mcp_status`, per [ADR-0014](0014-status-introspection-tool.md)) include `schema_version: <int>` at the response root. Bumping that integer signals a breaking change to the response shape — which itself is governed by this ADR (i.e., bump only when forced, prefer additive).
- Timestamps: ISO-8601 with `Z` (UTC) suffix. Never raw epoch seconds in the response (epoch fine in internal storage; not in tool responses).
- IDs: opaque strings; consumers should not parse them.

### CI enforcement: tool-registry snapshot test

The `insta`-based snapshot test from [ADR-0007](0007-testing-strategy.md) (Layer 4) is the enforcement mechanism. The committed snapshot at `tests/snapshots/snapshot_tool_registry.snap` is the **baseline**; any PR that changes the tool registry without updating the snapshot fails CI.

The intentional-change workflow:

1. Make the tool change in code.
2. Run `cargo insta review` locally.
3. Inspect the diff:
   - **Additive** (new optional field, new tool, new optional param) → accept; commit the new snapshot file.
   - **Forbidden** (rename, removal, type change) → either revert OR introduce the new tool with a versioned name and keep the old. Update snapshot accordingly.
4. PR description must explicitly call out the tool surface change category ("additive: new optional `dry_run` param") so the reviewer can verify.

The snapshot file IS the contract. Reviewing the diff is reviewing the contract.

### Deprecation procedure

When a tool needs to be retired (because we're shipping a `_v2`):

1. Add `[DEPRECATED — use <new_tool> — sunset 2026-10-25]` prefix to the old tool's description. Reflected in the snapshot; CI accepts this as additive (description change).
2. Tracing-WARN every invocation of the deprecated tool with `tool.deprecated = true` field, surfaced in `mcp_status` (`tool_invocations_last_hour.deprecated`).
3. Wait minimum 6 months, longer for tools that affect destructive ops (where downstream consumer behavior shifts could cause real harm if the consumer hasn't updated).
4. Remove the deprecated tool. Snapshot diff shows tool removal; PR explicitly notes the sunset cycle is complete.

This is heavyweight on purpose. Removing a tool from a public contract is a real cost; the procedure makes the cost visible.

### What "the tool surface" includes

Specifically, the snapshot test captures:

- All registered tool names.
- Every tool's full description text (the `#[tool(description = ...)]` content).
- Every tool's parameter JSON Schema (generated by `schemars` from the params struct).
- The structural-response shape for tools with declared response types (e.g., `mcp_status` has a typed response struct; the schema for it is captured).

Implementation detail not yet captured (and intentionally excluded):

- Internal data structures behind the tool dispatch (cache schema, error enum variants — those are governed by their respective ADRs and changing them doesn't break MCP consumers).
- Performance characteristics (latency targets are in [ADR-0008](0008-observability-and-deployment.md) SLOs, not the tool surface contract).

## Options Considered

### Versioning model

| Option | Pros | Cons |
| --- | --- | --- |
| (a) No policy | Minimal process | Silent breakage of downstream consumers; reviewer judgment varies; impossible to assess "is this PR safe?" |
| **(b) Additive-only with snapshot enforcement** (chosen) | Mechanical enforcement; reviewer just checks the diff is additive; clear escape hatch via versioned tool names | More PRs need snapshot updates (small overhead); operator must understand the "review the snapshot" step |
| (c) SemVer per tool | Maximum precision | Tool-level semver in MCP has no protocol support; consumer would have to parse versions from descriptions; useless without consumer cooperation |
| (d) Strict (no changes ever) | Ultimate stability | Cannot fix bugs in tool descriptions, cannot improve schemas; over-strict |
| (e) Versioned every release | Heavyweight | Most releases don't change tool surface; ceremony for nothing |

We choose (b). Snapshot tests are the de facto standard for catching unintended schema changes in API ecosystems (Stripe uses them, every well-maintained REST API uses them). The mechanism is automatic; the policy is "review the snapshot diff like you review the code diff."

### Sunset window length

| Option | Pros | Cons |
| --- | --- | --- |
| (f) 30 days | Fast iteration | Insufficient for slow-moving downstream consumers (think: a script that runs weekly) to notice and update |
| (g) 90 days | Reasonable speed | OK; a quarter is enough for most consumer cycles |
| **(h) 180 days, longer for destructive** (chosen) | Conservative; matches typical OSS ecosystem norms; respects the "trust" axis of an MCP that touches user data | Longer maintenance burden for the deprecated path |
| (i) Never sunset | Maximum compatibility | Tool surface accumulates dead code forever |

Six months is a reasonable middle ground for a personal-data tool. Destructive operations get longer because consumer mistakes there have larger blast radius.

### What's in the snapshot

| Option | Pros | Cons |
| --- | --- | --- |
| (j) Just tool names | Smallest surface | Misses the actual schema; doesn't catch parameter changes |
| **(k) Tool names + descriptions + parameter schemas + response schemas** (chosen) | Full contract captured | Snapshot file is larger; more PRs touch it |
| (l) Above + behavioral snapshots (sample inputs → outputs) | Catches behavior changes too | Hugely brittle; effectively-an-integration-test-suite; not the right shape |

## Consequences

**Positive:**

- Downstream consumer breakage caught automatically at PR time, not in production.
- The snapshot file is human-readable; reviewers can see the exact contract change without tracing through code.
- The deprecation procedure is explicit; nobody can quietly remove a tool that's in active use.
- The "additive default" sets the right culture: prefer extending over modifying.
- Consumers (including future-us writing other MCPs that consume this one) can rely on the contract surviving casual refactors.
- Encourages thinking about parameter design carefully *before* shipping a tool — once it's in the snapshot, changing it is real work.

**Negative:**

- More PRs touch the snapshot file (every legitimate tool-surface change). Small overhead per PR.
- Operator has to understand the snapshot review step; documented in `CONTRIBUTING.md`.
- Forbidden changes that are "obviously the right thing" require following the versioned-tool path even when no real consumer exists yet (early stage). Mitigation: until the project has external consumers, the maintainer can be lenient about the sunset window — the snapshot enforcement is what catches the *unintentional* breakage; intentional breakage with no users is fine to do quickly.
- The `_v2` etc. naming creates a tool-surface-bloat trajectory for actively-evolving tools. Mitigated by aggressive removal at sunset.

**Risks:**

- *Risk:* The snapshot test doesn't catch semantic changes (e.g., a tool that previously returned threads sorted by date now returns them sorted by relevance — same schema, different behavior).
  *Mitigation:* Acknowledge limitation. Behavioral guarantees aren't in the snapshot; they're in the tool description and the test suite. Reviewers should call out behavioral changes in PR descriptions even when the snapshot is unchanged.
- *Risk:* `insta` snapshot diffs in PR review get rubber-stamped because they look mechanical.
  *Mitigation:* Cultural — reviewers are responsible for verifying additivity. CI cannot tell additive-vs-forbidden apart structurally; that's a human judgment.
- *Risk:* Adding new optional parameters with defaults that subtly change existing behavior (e.g., new `truncate_at: usize = 1000` default that truncates responses that were previously full). Schema is "additive"; behavior changed.
  *Mitigation:* New optional params with non-trivial defaults must be documented as such in the PR description and in the tool's description text. "Additive" is about the *schema*; behavioral changes need explicit reviewer attention regardless.
- *Risk:* Versioned-tool sprawl (`search_threads`, `search_threads_v2`, `search_threads_v3`) becomes confusing for both consumers and maintainers.
  *Mitigation:* Aggressive sunset. If `_v3` ships, `_v1` should be at the end of its deprecation window simultaneously.
- *Risk:* The snapshot file becomes a merge-conflict magnet during heavy development.
  *Mitigation:* `insta` snapshots are deterministic and reviewable; conflicts are resolvable. Not different from any other generated artifact in version control.
- *Risk:* External consumers (other people's MCPs that depend on this one) don't know about the versioning policy and assume tools can change freely.
  *Mitigation:* Document policy in `README.md` under "For consumers" and in `CONTRIBUTING.md`. Tool descriptions for deprecated tools include the sunset date prominently.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — "data source for other knowledge tools" framing that makes this policy load-bearing
- [ADR-0007](0007-testing-strategy.md) — Layer 4 snapshot tests (the enforcement mechanism)
- [ADR-0013](0013-cross-account-fan-out.md) — `fanout: true` response convention
- [ADR-0014](0014-status-introspection-tool.md) — `schema_version` on `mcp_status` response (the only currently-versioned response shape)
- [`insta`](https://docs.rs/insta) — snapshot test crate
- [`schemars`](https://docs.rs/schemars) — JSON Schema generation (already in deps)
- Stripe API versioning approach — additive default, dated versions on breaking changes (analogous policy at REST scale)
