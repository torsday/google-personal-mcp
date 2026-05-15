# google-personal-mcp

> **Status: design phase — code in this repo is a working prototype that is being rewritten from scratch.**
>
> The architecture for the rewrite is captured in [`docs/adr/`](docs/adr/) — read those before sending PRs against the source tree. Highlights:
>
> - Crate, binary, and repo are all **`google-personal-mcp`** (renamed from `gmail-mcp` on 2026-05-15, see [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md)).
> - **Scope: personal-data Google services only** — Gmail (Phase 1), Calendar, Contacts, Tasks, Drive, Keep, Photos, Docs/Sheets/Slides, Chat, YouTube *personal slice* (subscriptions / playlists / history / your channel). Out of scope: Maps, Translate, Cloud APIs, public-corpus YouTube — those belong in separate MCPs. This MCP is a *data source* consumed by other knowledge tools, not a knowledge layer itself.
> - **Multi-account from day one** with hot-reload of the account registry ([ADR-0002](docs/adr/0002-multi-account-architecture.md)) and `account = "*"` fan-out for read tools ([ADR-0013](docs/adr/0013-cross-account-fan-out.md)).
> - **Dual transport**: stdio (Claude Desktop integration) + Streamable HTTP (VPS daemon), selectable at runtime ([ADR-0003](docs/adr/0003-transport-stdio-and-streamable-http.md)).
> - **Persistent SQLite cache + Gmail History API** for incremental sync ([ADR-0009](docs/adr/0009-caching-with-sqlite-and-history-api.md)) — 10-50× quota reduction in steady state.
> - **Real MIME / charset handling** ([ADR-0010](docs/adr/0010-mime-and-encoding.md)) — HTML email, multipart, non-UTF-8.
> - **Append-only audit log** of every operation ([ADR-0011](docs/adr/0011-audit-log.md)) — verifiable trail of agent activity.
> - **Dry-run + automatic send-deduplication** for destructive ops ([ADR-0012](docs/adr/0012-idempotency-and-dry-run.md)) — safety nets against agent failure modes.
> - **Self-introspection** via `mcp_status` tool ([ADR-0014](docs/adr/0014-status-introspection-tool.md)) — daemon health from inside the MCP session.
> - Typed error model, configurable retry policy, structured logging, per-account rate limiting, Prometheus metrics, systemd + nginx deployment template — see [ADR-0004](docs/adr/0004-oauth-token-refresh.md) through [ADR-0008](docs/adr/0008-observability-and-deployment.md).
> - **Tool surface and parameter conventions** locked in [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md). **Secrets at rest** (token-file permissions, redacted Debug, deferred keyring) in [ADR-0017](docs/adr/0017-secrets-at-rest.md). **Prompt-injection defense** (untrusted-content wrapping) in [ADR-0018](docs/adr/0018-email-content-trust.md).
>
> The text below this banner describes the current prototype, not the rewrite target.

---

A Gmail [Model Context Protocol](https://modelcontextprotocol.io) server written in Rust. Gives Claude (or any MCP client) first-class access to your Gmail — search, read, archive, label, send, and trash — via a long-running stdio daemon with a minimal memory footprint.

Built as the first module of a planned **unified personal data MCP**: a single, always-on Rust daemon that exposes your email, calendar, notes, and contacts to AI assistants.

---

## Why Rust

This server is designed to run **forever** on a small personal VPS or homelab node. Rust's lack of a garbage collector means memory stays flat over days and weeks — no GC pauses, no heap drift, no periodic restarts. The resulting binary is small, self-contained, and deploys without a runtime.

---

## Tools

| Tool | Description |
|---|---|
| `search_threads` | Search using Gmail query syntax (`from:`, `is:unread`, `has:attachment`, etc.) |
| `get_thread` | Full thread content — headers, body text, labels |
| `archive_thread` | Remove a thread from inbox (preserves it, fully searchable) |
| `batch_archive` | Archive multiple threads in one call |
| `modify_thread_labels` | Add or remove any label (`STARRED`, `UNREAD`, custom labels) |
| `list_labels` | List all system and user-created labels with their IDs |
| `send_email` | Send a new email or reply into an existing thread |
| `trash_thread` | Move a thread to trash (recoverable for 30 days) |

---

## Setup

### 1. Google Cloud credentials

1. Go to [Google Cloud Console](https://console.cloud.google.com/)
2. Create or select a project
3. Enable the **Gmail API**
4. Create **OAuth 2.0 credentials** → Desktop application
5. Download `credentials.json`
6. Place it at `~/.config/google-personal-mcp/credentials.json`

### 2. Build

```bash
cargo build --release
```

### 3. Authenticate

```bash
./target/release/google-personal-mcp auth
```

This opens your browser, runs the OAuth2 PKCE flow, and saves a token to `~/.config/google-personal-mcp/token.json`. You only need to do this once.

### 4. Wire into Claude

Add to your MCP client config (e.g. `~/.claude/mcp.json` or Claude Desktop's `claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "gmail": {
      "command": "/path/to/google-personal-mcp",
      "args": []
    }
  }
}
```

---

## Architecture

```
src/
├── main.rs          Entry point — `auth` subcommand or MCP stdio server
├── auth.rs          OAuth2 PKCE flow, token persistence (~/.config/google-personal-mcp/)
├── gmail/
│   ├── mod.rs       GmailClient — typed wrappers around Gmail REST API
│   └── types.rs     Thread, Message, Label, and related types
└── tools/mod.rs     MCP ServerHandler — tool definitions and dispatch
```

**Transport:** stdio (standard MCP pattern for local servers — stdout is the MCP wire, stderr is logs)

**Auth:** OAuth2 with PKCE. The `oauth2` crate handles auth URL generation and PKCE challenge/verifier; the token exchange POST is done directly with `reqwest` to avoid crate version conflicts. Tokens are persisted as JSON and loaded at startup.

**Gmail API:** All calls go through `GmailClient` using `reqwest` + `serde_json`. Query parameters are built with `url::Url::query_pairs_mut()`.

---

## Planned: Unified Personal Data MCP

This Gmail module is intended as the first piece of a broader personal data server. The roadmap:

- **Phase 1 — Gmail** ← you are here
- **Phase 2 — Google Calendar** — events, availability, scheduling
- **Phase 3 — Notes / Tasks** — personal knowledge base integration
- **Phase 4 — Contacts** — cross-module entity resolution (email ↔ calendar ↔ contacts)
- **Phase 5 — Token refresh** — automatic OAuth2 token refresh before expiry

The long-term goal is a single `personal-mcp` binary: one always-on daemon, one auth flow, one config file, exposing a unified personal information graph to any MCP client.

---

## Development

```bash
# Build
cargo build

# Run with debug logging
RUST_LOG=gmail_mcp=debug ./target/debug/google-personal-mcp

# Check for issues
cargo clippy
cargo fmt
```

### Key dependencies

| Crate | Purpose |
|---|---|
| `rmcp` | MCP server SDK (official, from modelcontextprotocol org) |
| `tokio` | Async runtime |
| `reqwest` | HTTP client for Gmail API calls |
| `oauth2` | OAuth2 PKCE flow (auth URL + code verifier) |
| `serde` / `serde_json` | Serialization |
| `url` | URL + query string construction |
| `tracing` | Structured logging to stderr |

---

## License

MIT
