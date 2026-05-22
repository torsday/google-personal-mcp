# google-personal-mcp — Claude operating notes

## Status

**v0.2 shipped** (28 issues closed, milestone closed). **v0.3 in active planning** — see the [v0.3 milestone](https://github.com/torsday/google-personal-mcp/milestone/2) for the current backlog. Source lives under `src/` (~37 `.rs` files across `auth/`, `gmail/`, `tools/` plus root modules `audit.rs`, `config.rs`, `error.rs`, `http.rs`, `observability.rs`, `perm_check.rs`, `project_quota.rs`, `rate_limit.rs`, `server.rs`). Architecture is captured in 22 ADRs under [`docs/adr/`](docs/adr/).

## Where to read in what order

1. [SPEC.md](SPEC.md) — what the project is *for*: 36 concrete user stories, search-excellence criteria, non-goals
2. [ADR-0000](docs/adr/0000-adr-process.md) — ADR corpus + open-questions queue
3. [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md) — monolithic single-binary, Google-only, low-level-primitives-only
4. [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) — locked v0.2 tool inventory + per-tool schemas + cost model
5. [ADR-0017](docs/adr/0017-secrets-at-rest.md) — token-file permissions, redacted Debug, deferred Keychain
6. [ADR-0018](docs/adr/0018-email-content-trust.md) — untrusted-content wrapping; prompt-injection mitigation

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

GitHub Issues + Project board for this repo. v0.3 milestone holds the active backlog; v1.0 milestone holds deferred work (HTTP transport, caching, fan-out, observability v1.0). Every issue carries exactly one `model:*` tier label per the global convention.
