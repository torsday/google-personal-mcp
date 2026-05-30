# ADR-0000: ADR process and corpus

**Date:** 2026-05-16
**Status:** Accepted

---

## What ADRs are

Architecture Decision Records capture load-bearing design choices and the reasoning behind them. They are written once (when the decision is made) and never silently rewritten; they are superseded by later ADRs, not edited away. The corpus is the project's design memory.

In this repo, ADRs are the source of truth for design — the README summarizes, CONTRIBUTING points at, but design decisions live here.

## When to write one

Required for changes to:

- The tool surface (new tool, renamed parameter, changed response shape)
- Auth model, scopes, token handling
- The error model
- The config schema
- Persistence (cache, audit log, token files)
- The threat model
- The deployment model (transport, packaging, systemd unit)
- Cross-cutting policies (testing strategy, observability, versioning)

Not required for: bug fixes, behavior-preserving refactors, doc updates, test additions, lint config tweaks.

When in doubt: write the ADR. The cost is one file; the cost of a load-bearing decision made implicitly in a PR review thread is much higher six months later when nobody remembers why.

## Numbering

Sequential, never reused, never reordered. The next ADR is whatever number is next. Gaps are not allowed (no skipping); if an ADR is withdrawn before acceptance, its number remains and is marked `Withdrawn`.

Filename format: `NNNN-<short-kebab-case-slug>.md`. The slug should be a noun phrase that identifies the topic, not the decision (`oauth-token-refresh`, not `use-proactive-refresh`).

## Statuses

| Status | Meaning |
| --- | --- |
| **Proposed** | Draft, not yet decided. Open to objections. |
| **Accepted** | Decided. Implementation should follow. |
| **Accepted, deferred to v1.0** | Decision is correct; implementation is intentionally postponed to the v1.0 milestone. The ADR's "v1 scope" note states what is in/out of v0.x. |
| **Superseded by ADR-NNNN** | A later ADR replaces this one. Keep the original; do not delete. The replacement ADR links back. |
| **Withdrawn** | Proposed but never accepted. Reserved number; body explains why. |

A status change is a commit. Don't edit an Accepted ADR's substance — write a new one.

## Structure

Every ADR has these sections in this order:

1. **Title line** (`# ADR-NNNN: <topic>`)
2. **Date**, **Status** (and **Supersedes** / **Superseded by** if applicable)
3. **Context** — what the situation is, what constraints apply, what happens if we don't decide
4. **Decision** — what we're doing. If status is "Accepted, deferred to v1.0," start with a `**v1 scope.**` paragraph
5. **Options Considered** — table or list of alternatives, with pros/cons
6. **Consequences** — Positive / Negative / Risks
7. **References** — links to prior ADRs, external docs, source files

Tone: write for a contributor who has not been in the conversation and may read this in a year. Avoid jargon shorthand. Cite specific files and line numbers when relevant.

## The v1 scope convention

For ADRs whose full design targets v1.0 but whose v0.x implementation is a subset, add a `**v1 scope.**` paragraph at the top of the Decision section stating what is implemented now and what is deferred. Examples: [ADR-0008](0008-observability-and-deployment.md), [ADR-0013](0013-cross-account-fan-out.md), [ADR-0014](0014-status-introspection-tool.md), [ADR-0015](0015-tool-versioning-policy.md).

This avoids two failure modes:

- Implementing the full design in v0.x because "the ADR says so" — wastes effort on infrastructure with no consumer.
- Quietly skipping pieces because "the ADR is aspirational" — produces a code/doc drift that future contributors cannot resolve.

The "v1 scope" note records the intentional cut.

## How to propose

1. Open a PR adding a new ADR with `Status: Proposed`.
2. PR description summarizes the decision in 3-5 sentences.
3. Discussion happens in PR review threads.
4. When converged, status flips to `Accepted` and the PR merges.
5. The PR that implements the decision references the ADR.

A proposed ADR can be withdrawn (status → `Withdrawn`) if the discussion concludes "no, we should not do this" — the file stays, the number is reserved.

## Current corpus

| # | Title | Status |
| --- | --- | --- |
| [0000](0000-adr-process.md) | ADR process and corpus | Accepted |
| [0001](0001-monolithic-google-personal-mcp-architecture.md) | Monolithic Google-services MCP daemon | Accepted |
| [0002](0002-multi-account-architecture.md) | Multi-account registry, hot-reload | Accepted |
| [0003](0003-transport-stdio-and-streamable-http.md) | Dual transport (stdio + Streamable HTTP) | Accepted |
| [0004](0004-oauth-token-refresh.md) | OAuth token refresh — proactive + lazy 401 fallback | Accepted |
| [0005](0005-error-model.md) | Typed error model | Accepted |
| [0006](0006-config.md) | Config schema (TOML) | Accepted |
| [0007](0007-testing-strategy.md) | Testing strategy — units, wiremock, ignored e2e | Accepted |
| [0008](0008-observability-and-deployment.md) | Observability and deployment | Accepted (shipped in v1.0) |
| [0009](0009-caching-with-sqlite-and-history-api.md) | Caching with SQLite + Gmail History API | Accepted |
| [0010](0010-mime-and-encoding.md) | MIME and encoding | Accepted |
| [0011](0011-audit-log.md) | Append-only audit log | Accepted |
| [0012](0012-idempotency-and-dry-run.md) | Idempotency and dry-run | Accepted |
| [0013](0013-cross-account-fan-out.md) | Cross-account fan-out | Accepted (shipped in v1.0) |
| [0014](0014-status-introspection-tool.md) | `mcp_status` introspection tool | Accepted (shipped in v1.0) |
| [0015](0015-tool-versioning-policy.md) | Tool versioning policy | Accepted (shipped in v1.0) |
| [0016](0016-tool-surface-and-conventions.md) | Tool surface and parameter conventions | Accepted |
| [0017](0017-secrets-at-rest.md) | Secrets at rest | Accepted |
| [0018](0018-email-content-trust.md) | Email content trust / prompt-injection mitigation | Accepted |
| [0019](0019-data-retention-and-purge.md) | Data retention and purge — cache age cap, audit deletion opt-in, "right to forget" | Accepted (shipped in v1.0) |
| [0020](0020-http-transport-authentication.md) | HTTP-transport authentication — bearer tokens at the daemon, mTLS optional at nginx | Accepted (shipped in v1.0) |
| [0021](0021-attachment-download-policy.md) | Attachment download policy — path constraints, extension blocklist, size limits, MIME trust | Accepted (shipped in v0.3) |
| [0022](0022-capability-gating.md) | Capability gating — service × aspect toggles layered on the OAuth scope ceiling | Accepted, target v1.1 |
| [0023](0023-calendar-service-surface.md) | Calendar service surface — full read/write/destructive under the capability gate | Accepted, target v1.1 |
| [0024](0024-contacts-service-surface.md) | Contacts (People API) service surface — full read/write/destructive under the capability gate | Accepted, target v1.1 |
| [0025](0025-drive-service-surface.md) | Drive service surface — full read/write/destructive under the capability gate | Accepted, target v1.1 |
| [0026](0026-gmail-tool-surface-phase-2.md) | Gmail tool surface — Phase 2 expansion (drafts, labels CRUD, filters, send-as, vacation, forward, permanent delete + multipart/full-body/forwarded-attachment/rate-cap consolidation) | Accepted, target v1.1 |

## Open questions (decisions queued for later)

These are real gaps; no ADR has been written yet. Each is tagged with the milestone that forces a decision — write the ADR when implementation reaches that milestone, not before.

| Question | Triggers a decision before | Notes |
| --- | --- | --- |
| **Linux Secret Service / Windows Credential Manager backends** — keyring backends beyond the shipped macOS Keychain. | **post-v1.1** | [ADR-0017](0017-secrets-at-rest.md) defers; file-perm baseline + macOS Keychain cover the current operator base. |

**Resolved:**

- v0.2 search-result metadata shape and batch coverage (added `batch_trash`, `batch_modify_thread_labels`) — folded into [ADR-0016](0016-tool-surface-and-conventions.md) per-tool schemas (2026-05-16).
- Data retention / purge story — decided in [ADR-0019](0019-data-retention-and-purge.md) (2026-05-22).
- HTTP-transport authentication — decided in [ADR-0020](0020-http-transport-authentication.md) (2026-05-22).
- Attachment download policy — decided in [ADR-0021](0021-attachment-download-policy.md) (2026-05-22).
- Per-GCP-project daily quota model — implemented in [#30](https://github.com/torsday/google-personal-mcp/issues/30) (2026-05-17).
- **macOS Keychain backend for tokens** — shipped in v0.2 via [#20](https://github.com/torsday/google-personal-mcp/issues/20) behind the `macos-keychain` Cargo feature flag; macOS CI coverage in [#33](https://github.com/torsday/google-personal-mcp/issues/33). [ADR-0017](0017-secrets-at-rest.md) corrected to reflect (2026-05-29).
- **Outbound multipart `send_email` with attachments** — design folded into [ADR-0026](0026-gmail-tool-surface-phase-2.md) as an additive `attachments` parameter on `send_email` (2026-05-28; pending acceptance via [#192](https://github.com/torsday/google-personal-mcp/issues/192)).

## References

- [Michael Nygard, "Documenting Architecture Decisions"](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions) — the original convention this repo follows
- [CONTRIBUTING.md](../../CONTRIBUTING.md) — when an ADR is required vs. optional
