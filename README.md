# google-personal-mcp

> **Status: v1.0.0 released ([2026-05-30](https://github.com/torsday/google-personal-mcp/releases/tag/v1.0.0)).** Gmail + multi-account OAuth + SQLite cache + Streamable HTTP transport with bearer auth + observability (Prometheus + alertmanager + nginx template) + cross-account fan-out + macOS Keychain + 92 issues closed across v0.2 / v0.3 / v1.0. **v1.1** is in active design: capability gating + Calendar + Contacts + Drive + Gmail Phase 2 ([ADR-0022..0026](docs/adr/INDEX.md)). See [CONTRIBUTING.md](CONTRIBUTING.md) for local dev setup.

A [Model Context Protocol](https://modelcontextprotocol.io) server written in Rust that exposes personal Google services (Gmail shipped; Calendar / Contacts / Drive designed for v1.1; Tasks / Keep / Photos / Docs / Chat / YouTube future) to AI assistants. Designed as a single, always-on daemon: low memory footprint, multi-account from day one, suitable for a personal VPS or local machine.

This MCP is a **data source** consumed by other knowledge tools. It is not a knowledge layer itself — no summarization, no classification, no "smart" composition. Tools are low-level primitives that mirror Google's APIs.

See [SPEC.md](SPEC.md) for concrete use cases and the search-excellence checklist.

## Architecture at a glance

```mermaid
flowchart LR
    subgraph Host["Host LLM"]
        Claude["Claude Desktop /<br/>Claude Code /<br/>any MCP client"]
    end

    subgraph Daemon["google-personal-mcp daemon"]
        Tools["Tool router<br/>(search, get, send, ...)"]
        Auth["TokenManager<br/>(per-account refresh)"]
        CapGate["Capability gate<br/>(v1.1)"]
        Audit["Audit log<br/>(shipped)"]
        Cache[("SQLite cache<br/>(shipped, on)")]
    end

    subgraph Google["Google APIs"]
        Gmail["Gmail<br/>(shipped)"]
        Cal["Calendar<br/>(v1.1)"]
        Contacts["Contacts<br/>(v1.1)"]
        Drive["Drive<br/>(v1.1)"]
        Other["Tasks, Keep,<br/>Photos, Docs, ...<br/>future"]
    end

    subgraph Disk["~/.config/google-personal-mcp/ (0700)"]
        TokensF[("tokens/&lt;alias&gt;.json<br/>0600")]
        AccountsF["accounts.toml"]
        ConfigF["config.toml"]
        CredsF["credentials/google.json"]
    end

    Claude <-->|MCP protocol<br/>stdio + Streamable HTTP<br/>(bearer auth on non-loopback)| Tools
    Tools -.-> CapGate
    Tools --> Auth
    Tools --> Audit
    Tools --> Cache
    Auth <-->|OAuth2 +<br/>REST| Gmail
    Auth <-.->|OAuth2 +<br/>REST| Cal
    Auth <-.->|OAuth2 +<br/>REST| Contacts
    Auth <-.->|OAuth2 +<br/>REST| Drive
    Auth <-.->|OAuth2 +<br/>REST| Other
    Auth <--> TokensF
    Daemon -.reads.-> AccountsF
    Daemon -.reads.-> ConfigF
    Daemon -.reads.-> CredsF
```

Solid lines are shipped in v1.0.0. Dotted lines are the v1.1 design program: capability gating + Calendar/Contacts/Drive + Gmail Phase 2 ([ADR-0022..0026](docs/adr/INDEX.md)). The daemon is one process per operator. Tokens live in `0600` files on disk; the daemon refuses to start if permissions are wider ([ADR-0017](docs/adr/0017-secrets-at-rest.md)).

## Why Rust

The daemon is designed to run forever. Rust's lack of a GC means memory stays flat over days and weeks — no GC pauses, no heap drift, no periodic restarts. The resulting binary is small, self-contained, and deploys without a runtime.

## Status and roadmap

| Phase | What it covers | State |
| --- | --- | --- |
| **Design** | 26 ADRs in [`docs/adr/`](docs/adr/) with [INDEX.md](docs/adr/INDEX.md) corpus map + mermaid dependency graph. | ✅ Complete |
| **v0.2** | Gmail tools (`list_accounts`, `list_labels`, `search_threads`, `get_thread`, `archive_thread`, `batch_archive`, `trash_thread`, `batch_trash`, `modify_thread_labels`, `batch_modify_thread_labels`, `send_email`), multi-account OAuth with hot-reload, `dry_run` + send-dedup, per-account rate limiter, minimal JSONL audit log, macOS Keychain backend, Layer 1–4 tests. | ✅ Shipped (28 issues, closed 2026-05-21) |
| **v0.3** | Full audit surface ([ADR-0011](docs/adr/0011-audit-log.md)) — per-tool redaction rules, `audit_summary`, fsync-before-destructive, configurable rotation. `mcp_status`, `list_attachments`, `download_attachment`. Observability v0.x subset: structured tracing spans, `/healthz`, systemd unit + INSTALL.md. | ✅ Shipped (26 issues, closed 2026-05-30) |
| **v1.0** | [First tagged release](https://github.com/torsday/google-personal-mcp/releases/tag/v1.0.0). Streamable HTTP transport with bearer auth + per-source-IP throttle ([ADR-0003](docs/adr/0003-transport-stdio-and-streamable-http.md), [ADR-0020](docs/adr/0020-http-transport-authentication.md)). Observability v1.0: Prometheus exporter + alertmanager rules + nginx template + criterion benchmarks ([ADR-0008](docs/adr/0008-observability-and-deployment.md)). SQLite cache with Gmail History API, watermark race prevention, LRU eviction, time-based body purge, default-on ([ADR-0009](docs/adr/0009-caching-with-sqlite-and-history-api.md), [ADR-0019](docs/adr/0019-data-retention-and-purge.md)). Cross-account fan-out ([ADR-0013](docs/adr/0013-cross-account-fan-out.md)). `purge_account` "right to forget" ([ADR-0019](docs/adr/0019-data-retention-and-purge.md)). Tool versioning policy + deprecation infrastructure ([ADR-0015](docs/adr/0015-tool-versioning-policy.md)). | ✅ Shipped (38 issues, closed 2026-05-30) |
| **v1.1** | Capability gating ([ADR-0022](docs/adr/0022-capability-gating.md)) + Calendar ([ADR-0023](docs/adr/0023-calendar-service-surface.md), 10 tools) + Contacts ([ADR-0024](docs/adr/0024-contacts-service-surface.md), 12 tools) + Drive ([ADR-0025](docs/adr/0025-drive-service-surface.md), 15 tools) + Gmail Phase 2 ([ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md), 24 tools — drafts, labels CRUD, filters, send-as, vacation, forward, permanent delete, …). | 🔜 Designed; implementation in progress |

From v1.0.0 forward, the public contract is the Layer-4 snapshot ([ADR-0015](docs/adr/0015-tool-versioning-policy.md)). Tool-surface changes are governed by the additive-only versioning policy.

## Architecture

The design lives in 26 ADRs. See [`docs/adr/INDEX.md`](docs/adr/INDEX.md) for the corpus map + mermaid dependency graph + three audience-specific reading orders. The load-bearing ones:

- [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md) — monolithic single-binary architecture, Google personal-data scope only
- [ADR-0002](docs/adr/0002-multi-account-architecture.md) — multi-account registry, `account` parameter on every tool
- [ADR-0004](docs/adr/0004-oauth-token-refresh.md) — proactive expiry refresh + 401 fallback, per-account
- [ADR-0007](docs/adr/0007-testing-strategy.md) — four-layer testing (units, wiremock, ignored e2e, snapshot)
- [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md) — `dry_run` and send-deduplication on destructive tools
- [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) — locked v1.0 tool inventory + parameter conventions
- [ADR-0017](docs/adr/0017-secrets-at-rest.md) — token-file permissions, redacted Debug, macOS Keychain
- [ADR-0018](docs/adr/0018-email-content-trust.md) — untrusted-content wrapping; prompt-injection defense
- [ADR-0022](docs/adr/0022-capability-gating.md) — v1.1 capability gating; service × aspect × per-account toggles layered on the OAuth scope ceiling

See [ADR-0000](docs/adr/0000-adr-process.md) for the full corpus, the ADR process, and the open-questions queue.

## Quick start

```sh
# 1. Build
cargo build --release

# 2. Create GCP credentials and config (see CONTRIBUTING.md)

# 3. Add a Google account
google-personal-mcp auth add --alias personal

# 4. Wire into your MCP client (Claude Desktop, Claude Code, etc.)
#    Command: /path/to/google-personal-mcp serve
#    Transport: stdio
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full setup walkthrough.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for local dev setup, the three test layers, and the GCP project / OAuth client configuration each contributor needs.

## Security

See [SECURITY.md](SECURITY.md). The daemon holds long-lived OAuth refresh tokens for personal Google accounts; the threat model is in [ADR-0017](docs/adr/0017-secrets-at-rest.md) and [ADR-0018](docs/adr/0018-email-content-trust.md). Report vulnerabilities via [GitHub Security Advisories](https://github.com/torsday/google-personal-mcp/security/advisories/new).

## License

MIT. See [LICENSE](LICENSE).
