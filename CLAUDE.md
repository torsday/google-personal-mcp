# google-personal-mcp — Claude operating notes

## Status

**v0.2 and v1.0 shipped** (66 issues closed across both milestones). **v1.1 in active design** — see the [v1.1 milestone](https://github.com/torsday/google-personal-mcp/milestones) for the design program: capability gating ([ADR-0022](docs/adr/0022-capability-gating.md)), Calendar / Contacts / Drive service surfaces ([ADR-0023](docs/adr/0023-calendar-service-surface.md) / [0024](docs/adr/0024-contacts-service-surface.md) / [0025](docs/adr/0025-drive-service-surface.md)), and a Gmail Phase 2 surface expansion ([ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md)). Source lives under `src/` (~74 `.rs` files across `auth/`, `gmail/`, `cache/`, `tools/`, `http/` plus root modules; ~28K LOC). Architecture is captured in 26 ADRs under [`docs/adr/`](docs/adr/).

## Where to read in what order

1. [SPEC.md](SPEC.md) — what the project is *for*: user stories, search-excellence criteria, non-goals (covers Gmail through v1.0; Calendar/Contacts/Drive added by [ADR-0023](docs/adr/0023-calendar-service-surface.md)/[0024](docs/adr/0024-contacts-service-surface.md)/[0025](docs/adr/0025-drive-service-surface.md))
2. [ADR-0000](docs/adr/0000-adr-process.md) — ADR corpus + open-questions queue
3. [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md) — monolithic single-binary, Google-only, low-level-primitives-only
4. [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) — locked v1.0 Gmail tool inventory + per-tool schemas + cost model; [ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md) extends to v1.1
5. [ADR-0017](docs/adr/0017-secrets-at-rest.md) — token-file permissions, redacted Debug, macOS Keychain shipped behind `macos-keychain` feature
6. [ADR-0018](docs/adr/0018-email-content-trust.md) — untrusted-content wrapping; prompt-injection mitigation
7. [ADR-0022](docs/adr/0022-capability-gating.md) — capability gating model: service × aspect (read/write/destructive) layered on the OAuth scope ceiling; foundation for v1.1 services

## Verification

`just check` runs fmt + clippy + nextest + deny — match CI locally. See [justfile](justfile).

## Locked conventions

- **Tool naming and parameters** per [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md): `account: String` required on every tool that touches Google; `thread_id`/`message_id`/`label_id` (never `id`); `max_results` (never `limit`); booleans default `false`; batch tools prefixed `batch_` and take `thread_ids: Vec<String>`.
- **Untrusted content** per [ADR-0018](docs/adr/0018-email-content-trust.md): every attacker-controllable response field is suffixed `_untrusted` and wrapped in `<<<UNTRUSTED:KIND ... UNTRUSTED>>>` delimiters. The MCP itself does not sanitize.
- **Errors** per [ADR-0005](docs/adr/0005-error-model.md): typed `Error` enum with `thiserror`. Never log `access_token`, `refresh_token`, or `client_secret` — `Debug` impls redact. Format-output unit test asserts redaction.
- **Module layout** per [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md): one directory per service (`auth/`, `gmail/`, `tools/`, ...). Files stay under ~400 lines where possible.
- **Conventional Commits** (`type(scope): subject` — imperative, lowercase, no trailing period). No `Co-Authored-By` footers.

## Do not

- Don't add ADRs without explicit ask — design is locked; the open-questions queue in [ADR-0000](docs/adr/0000-adr-process.md) is for future-deferred items.
- Don't commit, stage, or push without an explicit ask from the maintainer (the standing rule overrides any default behavior).
- Don't bypass the startup permission check from [ADR-0017](docs/adr/0017-secrets-at-rest.md). `GOOGLE_PERSONAL_MCP_SKIP_PERM_CHECK` is for edge cases (WSL on mounted drives), not for "I don't want to chmod."
- Don't add fields to `ThreadSummary` or any tool schema without updating [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md). Layer 4 snapshot tests catch silent drift.

## Quick reference — Gmail API quota (validated 2026-05-16)

| Method | Units |
| --- | --- |
| `threads.list` | 10 |
| `threads.get` (any format) | 40 |
| `threads.modify` | 10 |
| `threads.trash` | 20 |
| `messages.get` | 20 |
| `messages.send` | 100 |
| `messages.batchModify` | 50 |
| `history.list` | 2 |
| `getProfile` | 1 |
| `labels.list` | 1 |

Per-user-per-minute cap: **6,000 units**. Per-project-per-minute: 1,200,000. History `historyId` is "typically valid at least a week, sometimes only hours" — reseed-on-404 must be cheap.

## Tracker

GitHub Issues + Project board for this repo. **v1.1 milestone** holds the active design program (4 service-surface ADRs + 8 future-ADR deferral spikes + capability gating). v1.0 (HTTP transport, caching, fan-out, observability) and v0.2 (Gmail core + audit) are both closed. Note: the operator archives completed items off the board, so the board's `Done` column shows only the recent shipping cohort — `gh issue list --state closed` is the complete done-record. Every issue carries exactly one `model:*` tier label per the global convention.
