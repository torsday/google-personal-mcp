# google-personal-mcp

> **Status: design phase.** No code lives in this tree yet — the prior Gmail-only prototype was discarded so its shape wouldn't constrain the design. The rewrite target is described in [`docs/adr/`](docs/adr/). Start there.

A [Model Context Protocol](https://modelcontextprotocol.io) server, to be written in Rust, that will expose personal Google services (Gmail first; Calendar, Contacts, Tasks, etc. to follow) to AI assistants. Designed as a single, always-on daemon: low memory footprint, multi-account from day one, suitable for a personal VPS or local machine.

This MCP is a **data source** consumed by other knowledge tools. It is not a knowledge layer itself — no summarization, no classification, no "smart" composition. Tools are low-level primitives that mirror Google's APIs.

## Why Rust

The daemon is designed to run forever. Rust's lack of a GC means memory stays flat over days and weeks — no GC pauses, no heap drift, no periodic restarts. The resulting binary is small, self-contained, and deploys without a runtime.

## Status and roadmap

| Phase | What it covers |
| --- | --- |
| **Design** (current) | 19 ADRs in [`docs/adr/`](docs/adr/). No implementation. |
| **v0.2** (not started) | First implementation: Gmail tools per [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md), single-account auth with refresh, stdio transport, enforced 0600 perms on token files. |
| **v0.3 – v0.x** | Multi-account, hot-reload, Calendar tools. |
| **v1.0** | First public release. Caching, HTTP transport, audit log, observability, fan-out, `mcp_status`, additive-only tool-versioning policy. |

The cut between v0.x and v1.0 is deliberate: features only earn their keep when there's a second user or an external operator. v1-scope notes in each affected ADR record what is deferred.

## Architecture

The design lives in 19 ADRs. The load-bearing ones:

- [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md) — monolithic single-binary architecture, Google personal-data scope only
- [ADR-0002](docs/adr/0002-multi-account-architecture.md) — multi-account registry, `account` parameter on every tool
- [ADR-0004](docs/adr/0004-oauth-token-refresh.md) — proactive expiry refresh + 401 fallback, per-account
- [ADR-0007](docs/adr/0007-testing-strategy.md) — three-layer testing (units, wiremock, ignored e2e)
- [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md) — `dry_run` and send-deduplication on destructive tools
- [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) — locked v1 tool inventory + parameter conventions
- [ADR-0017](docs/adr/0017-secrets-at-rest.md) — token-file permissions, redacted Debug, deployment guidance
- [ADR-0018](docs/adr/0018-email-content-trust.md) — untrusted-content wrapping; prompt-injection defense

See [ADR-0000](docs/adr/0000-adr-process.md) for the full corpus, the ADR process, and the open-questions queue.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Pre-v0.2 the work is design, not code: read the ADRs, file issues against specific decisions, propose new ADRs per [ADR-0000](docs/adr/0000-adr-process.md).

## Security

See [SECURITY.md](SECURITY.md). The daemon (once implemented) will hold long-lived OAuth refresh tokens for personal Google accounts; the threat model is in [ADR-0017](docs/adr/0017-secrets-at-rest.md) and [ADR-0018](docs/adr/0018-email-content-trust.md). Report vulnerabilities via [GitHub Security Advisories](https://github.com/torsday/google-personal-mcp/security/advisories/new).

## License

MIT. See [LICENSE](LICENSE).
