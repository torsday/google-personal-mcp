# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). From v1.0.0 forward, the public contract is the tool surface captured by the Layer-4 snapshot test ([ADR-0015](docs/adr/0015-tool-versioning-policy.md)); changes are governed by the additive-only versioning policy there.

## [Unreleased]

The **v1.1 design program** is locked but not yet implemented. See [`docs/adr/INDEX.md`](docs/adr/INDEX.md) for the corpus map.

### Designed (Accepted, target v1.1)
- [ADR-0022](docs/adr/0022-capability-gating.md) — capability gating: per-service, per-aspect (read / write / destructive), per-account toggles layered on the OAuth scope ceiling. Foundational; gates everything below.
- [ADR-0023](docs/adr/0023-calendar-service-surface.md) — Calendar service surface (10 tools): list/get/freebusy/create/update/respond/delete events.
- [ADR-0024](docs/adr/0024-contacts-service-surface.md) — Contacts service surface via People API (12 tools): list/search/CRUD contacts + group membership.
- [ADR-0025](docs/adr/0025-drive-service-surface.md) — Drive service surface (15 tools) including `drive.file` safe-default scope and literal-string `confirm` guard on `update_permissions`.
- [ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md) — Gmail Phase 2 expansion (24 tools): drafts, labels CRUD, filters, untrash, account profile, send-as (read-only), vacation, forward, permanent delete, plus multipart `send_email`, `get_full_body`, `parse_forwarded_attachment`, send rate-cap.

## [1.0.0] — 2026-05-30

First tagged release. v0.2 (Gmail core + audit, closed 2026-05-21) and v0.3 (audit completion + Phase-2 tools, closed 2026-05-30) shipped as untagged development milestones; their content is summarized under v1.0.0 because this is the project's first public-contract release. From here on, every change to the tool surface is governed by [ADR-0015](docs/adr/0015-tool-versioning-policy.md).

92 issues closed across v0.2 / v0.3 / v1.0. Full per-issue record via `gh issue list --state closed`.

### Added — capability and tool surface
- Multi-account OAuth registry with proactive token refresh and lazy 401 fallback ([ADR-0002](docs/adr/0002-multi-account-architecture.md), [ADR-0004](docs/adr/0004-oauth-token-refresh.md); #4, #5, #27).
- Gmail v1.0 tool surface ([ADR-0016](docs/adr/0016-tool-surface-and-conventions.md)): `list_accounts`, `list_labels`, `search_threads`, `get_thread`, `archive_thread`, `trash_thread`, `modify_thread_labels` and their `batch_` variants, `send_email`, `list_attachments`, `download_attachment` ([ADR-0021](docs/adr/0021-attachment-download-policy.md) policy).
- Cross-account fan-out via `account: "*"` on read tools, rejected on destructive tools ([ADR-0013](docs/adr/0013-cross-account-fan-out.md); #84, #85).
- `mcp_status` introspection tool ([ADR-0014](docs/adr/0014-status-introspection-tool.md); #61).
- Audit-summary tool (#65) over the append-only JSONL audit log ([ADR-0011](docs/adr/0011-audit-log.md)).
- Cache-status and cache-invalidate tools (#83) over the SQLite cache layer.

### Added — daemon, transport, observability
- Streamable HTTP transport with session lifecycle, idle expiry, and non-loopback-without-TLS startup WARN ([ADR-0003](docs/adr/0003-transport-stdio-and-streamable-http.md); #72, #73, #74).
- HTTP-transport bearer-token authentication with fail-closed startup, constant-time compare, per-source-IP throttle on failed auth, and SIGHUP reload of `http_auth.toml` ([ADR-0020](docs/adr/0020-http-transport-authentication.md); #162, #163, #170).
- Prometheus exporter + 12-metric inventory + alertmanager rules + SLO table + criterion benchmark harness ([ADR-0008](docs/adr/0008-observability-and-deployment.md); #69, #70, #75, #76, #77, #90).
- Systemd unit + nginx TLS-termination + mTLS-optional example for VPS deployment ([ADR-0008](docs/adr/0008-observability-and-deployment.md); #71, #164).

### Added — persistence
- SQLite per-account cache with hand-rolled migrations, `history.list` sync loop, watermark race prevention, LRU eviction, time-based body purge, cache_status counters; enabled by default ([ADR-0009](docs/adr/0009-caching-with-sqlite-and-history-api.md), [ADR-0019](docs/adr/0019-data-retention-and-purge.md); #79, #80, #81, #82, #83, #149, #150, #168, #169).
- Audit log: structured JSONL with configurable rotation, fsync-before-call invariant for destructive ops, per-tool redaction rules, verbose-mode opt-in, opt-in `delete_after_days` retention ([ADR-0011](docs/adr/0011-audit-log.md); #21, #64, #65, #66, #67, #68, #165).
- `purge_account` "right to forget" tool ([ADR-0019](docs/adr/0019-data-retention-and-purge.md); #166).
- macOS Keychain backend for token storage behind `macos-keychain` Cargo feature flag ([ADR-0017](docs/adr/0017-secrets-at-rest.md); #20, #33).

### Added — safety primitives
- Typed `Error` enum with redacted `Debug` impls for OAuth credentials ([ADR-0005](docs/adr/0005-error-model.md); #2, #103).
- `dry_run` + send-deduplication infrastructure for destructive ops ([ADR-0012](docs/adr/0012-idempotency-and-dry-run.md); #11).
- `_untrusted` content wrapping with `<<<UNTRUSTED:KIND ... UNTRUSTED>>>` delimiters on every attacker-controllable field ([ADR-0018](docs/adr/0018-email-content-trust.md); #6).
- Startup permission check on token files (mode 0600) and config dirs (mode 0700) ([ADR-0017](docs/adr/0017-secrets-at-rest.md); #3).
- Per-account Gmail rate limiter (token-bucket against 6K/min) + per-GCP-project daily quota model (#25, #30).
- Tool-deprecation infrastructure: `[DEPRECATED — use <new> — sunset <date>]` banner + tracing WARN + `mcp_status` counter ([ADR-0015](docs/adr/0015-tool-versioning-policy.md); #167).

### Added — testing
- Layer 1 unit tests across modules ([ADR-0007](docs/adr/0007-testing-strategy.md); #16).
- Layer 2 wiremock integration tests (#17).
- Layer 3 ignored e2e smoke tests against real Gmail (#26).
- Layer 4 snapshot tests for tool descriptors (#18).
- macOS CI job for the Keychain code path (#33).

### Fixed (security and correctness)
- OAuth response body leaked verbatim in `Error::AuthRequired.reason` (#103).
- `revoke_token_at_google` leaked refresh_token via URL query (#107, #108).
- PKCE redirect listener: bound the unbounded request-line read (#109); `pkce_flow.accept` blocking forever (#102).
- `quota_check` leaked per-account budget on project-quota denial (#100).
- Path traversal in audit-log filename via attacker-controlled `account` (#101).
- `batch_archive` reported "task did not complete" on duplicate `thread_ids` (#104); `batch_trash` and `batch_modify_thread_labels` destroyed input order via `sort_by(thread_id)` (#105).
- `Error::upstream` panicked on UTF-8 multi-byte char straddling a 4 KiB boundary (#98).
- Server `main.rs` constructed `TokenManager` with empty `HashMap` — Gmail tools failed with `AccountNotFound` after restart (#96).
- Percent-encode `account` / `thread_id` in interpolated API URL paths (#106).

### Refactors
- Decomposed `src/server.rs` (1029→~400-line target), `src/config.rs` (983→~400), `src/auth/tokens.rs` (897→~400) into focused modules (#91, #92, #93).
- Extracted shared batch-orchestration into `tools::batch` (#110).
- Introduced `GmailService` seam above `GmailClient` for cache integration (#149).

### Docs
- 26 ADRs under [`docs/adr/`](docs/adr/) with [INDEX.md](docs/adr/INDEX.md) navigational layer + mermaid dependency graph.
- [SPEC.md](SPEC.md): 75 concrete user stories across Gmail, Calendar, Contacts, Drive, capability gating + the search-excellence checklist + non-goals.
- README + CONTRIBUTING + SECURITY + INSTALL.

## [0.1.0]

Pre-tag development. Initial crate scaffold (#1).
