# google-personal-mcp

> **Status: design phase.** The current `src/` is a Gmail-only prototype that is being rewritten from scratch. The rewrite target is described in [`docs/adr/`](docs/adr/) — start there.

A [Model Context Protocol](https://modelcontextprotocol.io) server written in Rust that exposes personal Google services (Gmail first; Calendar, Contacts, Tasks, etc. to follow) to AI assistants. Designed as a single, always-on daemon: low memory footprint, multi-account from day one, suitable for a personal VPS or local machine.

This MCP is a **data source** consumed by other knowledge tools. It is not a knowledge layer itself — no summarization, no classification, no "smart" composition. Tools are low-level primitives that mirror Google's APIs.

## Why Rust

The daemon is designed to run forever. Rust's lack of a GC means memory stays flat over days and weeks — no GC pauses, no heap drift, no periodic restarts. The resulting binary is small, self-contained, and deploys without a runtime.

## Status and roadmap

| Phase | What ships | Target |
| --- | --- | --- |
| **Prototype** (current `src/`) | Gmail-only, single-account, stdio | shipped (will be discarded) |
| **v0.2** (in progress) | Rewrite per [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md): Gmail tools, single-account auth with refresh, stdio, enforced 0600 perms on token files | next milestone |
| **v0.3–v0.x** | Multi-account, hot-reload, Calendar tools | |
| **v1.0** | First public release. Caching, HTTP transport, audit log, observability, fan-out, `mcp_status`, additive-only tool-versioning policy | when v0.x is stable |

The cut between v0.x and v1.0 is deliberate: features only earn their keep when there's a second user or an external operator. v1-scope notes in each affected ADR record what is deferred.

## Architecture

The design is captured in 18 ADRs. The load-bearing ones:

- [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md) — monolithic single-binary architecture, Google personal-data scope only
- [ADR-0002](docs/adr/0002-multi-account-architecture.md) — multi-account registry, `account` parameter on every tool
- [ADR-0004](docs/adr/0004-oauth-token-refresh.md) — proactive expiry refresh + 401 fallback, per-account
- [ADR-0007](docs/adr/0007-testing-strategy.md) — three-layer testing (units, wiremock, ignored e2e)
- [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md) — `dry_run` and send-deduplication on destructive tools
- [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) — locked v1 tool inventory + parameter conventions
- [ADR-0017](docs/adr/0017-secrets-at-rest.md) — token-file permissions, redacted Debug, deployment guidance
- [ADR-0018](docs/adr/0018-email-content-trust.md) — untrusted-content wrapping; prompt-injection defense

See [ADR-0000](docs/adr/0000-adr-process.md) for the full corpus and ADR process.

## Setup (prototype)

The prototype builds and runs as-is. The instructions below describe the prototype, not the rewrite — they will change.

1. **GCP project.** Each user creates their own. Go to [Google Cloud Console](https://console.cloud.google.com/), create a project, enable the Gmail API, create OAuth 2.0 credentials of type "Desktop application," and download the JSON.
2. **Place credentials.** `~/.config/google-personal-mcp/credentials.json` (will move to `credentials/google.json` in the rewrite per [ADR-0006](docs/adr/0006-config.md)).
3. **Build.** `cargo build --release`
4. **Authenticate.** `./target/release/google-personal-mcp auth` — browser-based PKCE flow, one-time.
5. **Wire into your MCP client.** Example for Claude Desktop:
   ```json
   {
     "mcpServers": {
       "gmail": {
         "command": "/absolute/path/to/google-personal-mcp",
         "args": []
       }
     }
   }
   ```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The repo is design-first: non-trivial changes need an ADR. The ADR process is documented in [ADR-0000](docs/adr/0000-adr-process.md).

## Security

See [SECURITY.md](SECURITY.md). The daemon holds long-lived OAuth refresh tokens for personal Google accounts; treat the config directory accordingly. To report a vulnerability, use [GitHub Security Advisories](https://github.com/torsday/google-personal-mcp/security/advisories/new).

## License

MIT. See [LICENSE](LICENSE).
