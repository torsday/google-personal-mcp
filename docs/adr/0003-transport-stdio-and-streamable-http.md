# ADR-0003: Dual transport (stdio + Streamable HTTP), selectable at runtime

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

[ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) declared `google-personal-mcp` as a monolithic daemon designed to "run forever on a personal VPS." The prototype uses **stdio transport** — the standard MCP pattern for local servers, where the MCP client (Claude Desktop, a CLI tool, etc.) launches the daemon as a subprocess and communicates via the daemon's stdin/stdout.

These two facts are in tension. Stdio transport has properties incompatible with the stated VPS-daemon vision:

- **Daemon lifetime = client lifetime.** When the launching client exits, the daemon dies. There is no "always-on" model with stdio.
- **One client per daemon.** Each MCP client launches its own copy. No multiplexing.
- **No network.** stdio uses local pipes; the client must be on the same machine as the daemon.

For a real "VPS daemon serving multiple clients" the right transport is **Streamable HTTP**, the current MCP transport spec (introduced 2025-03-26, replacing the older HTTP+SSE). Streamable HTTP supports:

- Long-lived server, many clients connect/reconnect over time
- Standard HTTP semantics (sessions via `Mcp-Session-Id` header, normal request/response)
- Server-Sent Events (SSE) within HTTP for streaming tool-progress notifications
- Frontable by nginx with TLS, served on a well-known port

We verified that `rmcp` 1.5 supports both transports as separate Cargo features:

- `transport-io` — stdio (server)
- `transport-streamable-http-server-session` — Streamable HTTP (server)

The Rust GC-free flat-memory justification in the README only fully delivers when the daemon actually outlives client sessions — which stdio prevents. Either we commit to one transport and accept its limits, or we support both.

## Decision

We will support **both transports in a single binary**, selectable at runtime via CLI flag.

Concretely:

- **Cargo features:** Both `transport-io` and `transport-streamable-http-server-session` enabled by default. Users can disable either via `default-features = false` if they want a smaller binary, but the standard build includes both.
- **CLI surface:**
  - `google-personal-mcp serve --stdio` — stdio mode (default for local development and Claude Desktop integration). Reads MCP from stdin, writes to stdout, logs to stderr.
  - `google-personal-mcp serve --http <addr>` — Streamable HTTP mode. Listens on the given address (e.g., `127.0.0.1:8765` for local-only HTTP, `0.0.0.0:8765` for network-exposed; the latter is intended to be fronted by nginx + TLS for production).
  - Default if no flag: `--stdio` (least-surprise for users running `google-personal-mcp serve` interactively to test).
  - Exactly one of `--stdio` or `--http` may be specified.
- **Tools and state are transport-agnostic.** The `GoogleServer` struct, the per-account `TokenManager`, the rate limiter, all error handling — everything below the transport adapter is identical between modes. Only the bytes-in / bytes-out path differs.
- **Auth is always local.** The OAuth PKCE flow uses a localhost HTTP listener for the redirect. Therefore `google-personal-mcp auth add ...` always runs locally on a machine with a browser, regardless of where the daemon will eventually serve from. Tokens written by `auth add` (to `~/.config/google-personal-mcp/tokens/<alias>.json`) are portable: copy them to the VPS via `scp`/`rsync` (or run `auth add` directly on the VPS via SSH X-forwarding, or use a Cloudflare tunnel during the redirect window — operator's choice). The `serve` subcommand never runs OAuth flows.
- **HTTP mode uses session-based MCP (per spec 2025-03-26).** Each connecting client gets a session ID; sessions are tracked server-side. Session cleanup on disconnect; idle session expiry tunable via config (default: 1 hour).
- **Local-only HTTP is the default address shape.** Binding to `127.0.0.1` requires reverse-proxy + TLS for any non-loopback access. The CLI emits a WARN on startup if bound to a non-loopback address without TLS. Refusing to bind 0.0.0.0 by default would be more strict; we choose to allow it with a warning because legitimate VPS deployments need it.

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) stdio only | Matches MCP local-server convention; simplest deployment for Claude Desktop; smallest binary | Breaks the stated VPS-daemon vision; Rust GC-free benefit largely wasted (daemon lifetime tied to client); cannot multi-client |
| (b) Streamable HTTP only | Pure VPS-daemon model; matches stated long-running goal; multi-client capable | Breaks local Claude Desktop integration (Claude Desktop's stdio-spawn pattern is the standard local path); requires HTTP listener even for trivial local dev |
| **(c) Both transports, runtime-selectable** (chosen) | Covers all deployment scenarios with one binary; same code below the transport adapter; users pick their model per-deployment without rebuilding | Slightly larger binary; two test paths (one per transport); more deps (`hyper` / `axum` style HTTP stack pulled in by `transport-streamable-http-server-session`) |
| (d) stdio over SSH wrapper | No HTTP server needed on VPS; reuses stdio model | Hacky; not idiomatic MCP; SSH wrapper is per-user setup friction; doesn't multi-client |
| (e) Two separate binaries (`google-personal-mcp-stdio` + `google-personal-mcp-http`) | Clean separation; smaller per-binary footprint; clearer deployment model | More CI complexity; same code base; users need to know which binary to install; defeats the "single binary" simplicity that monolithic-daemon ADR-0001 settled on |

We choose (c). The cost (slightly bigger binary, more deps, two test paths) is small relative to the operational flexibility it preserves. Either constraint — "must be local-only" or "must be VPS-only" — would be a significant limitation for a daemon meant to serve a maintainer's full personal-data workflow.

## Consequences

**Positive:**

- One binary, two deployment models. Local dev (`serve --stdio`), Claude Desktop integration (stdio via `mcp.json`), and VPS daemon (`serve --http`) all use the same artifact.
- The "auth always local, serve potentially remote" model cleanly separates the one-time OAuth ceremony from the ongoing serve workload. Token files become a portable, syncable artifact.
- Stdio remains the default for `serve` with no flag — least surprise for someone running the binary interactively to verify it works.
- The transport-agnostic design (everything below the transport adapter is shared) means tool tests, error tests, and refresh tests don't multiply per-transport.
- Reverse-proxy via nginx + TLS is the standard path for HTTP mode; deployment doc ([ADR-0008]) covers this.

**Negative:**

- Binary size increases by the HTTP server stack (`hyper` or whatever rmcp's Streamable HTTP server uses internally). For a Rust binary this is on the order of a few hundred KB to ~1 MB; acceptable.
- Two test paths in the integration test suite (one per transport). Mitigated by the transport-agnostic design — most tests target the layer below transport.
- HTTP mode introduces concerns stdio doesn't have: session timeout, connection limits, slowloris-style request abuse. Mitigated by binding to localhost by default and pushing TLS + DoS handling to nginx.
- More auth model surface: explaining "auth always runs locally; serve can be remote; tokens are portable files" requires real documentation. It's the right model but adds README content.

**Risks:**

- *Risk:* HTTP mode without TLS is a security disaster — auth tokens flow over the wire on every tool call.
  *Mitigation:* CLI emits a startup WARN if bound to a non-loopback address; deployment doc ([ADR-0008]) is explicit that nginx + TLS is required for non-loopback HTTP. Consider adding a hard refuse-to-bind for non-loopback addresses unless an explicit `--insecure-no-tls` flag is passed (excellence-grade option; document in [ADR-0008]).
- *Risk:* `rmcp` 1.5's Streamable HTTP server API may have rough edges (it is the newer transport; less battle-tested than stdio).
  *Mitigation:* The HTTP transport adapter is isolated to one module (`transport/http.rs`). If we hit rmcp limitations, we can replace with a hand-rolled MCP-over-HTTP layer using `axum` directly. Compile-test the HTTP path before committing to it during implementation.
- *Risk:* Token portability (auth-then-scp model) creates accidental long-lived tokens on dev laptops that the maintainer forgets to delete.
  *Mitigation:* `google-personal-mcp auth list` shows where tokens live (path on disk). Documentation recommends restrictive file permissions (`chmod 600`) and explicitly lists the cleanup step ("delete `~/.config/google-personal-mcp/tokens/work.json` from your laptop after copying to the VPS, if you don't intend to authenticate from the laptop").
- *Risk:* Session lifecycle in Streamable HTTP mode is non-trivial — clients reconnecting expect session continuity; orphaned sessions accumulate memory.
  *Mitigation:* Idle-session timeout (1 hour default, configurable in [ADR-0006]); session count metric exposed via [ADR-0008]'s observability surface so we can spot leaks.
- *Risk:* The "two transports in one binary" pattern is not demonstrated in `rmcp` examples (we verified this), so we are pioneering the integration.
  *Mitigation:* The transport selection is a `match` on the CLI flag at the top of `serve` — wiring is straightforward even without an example. If composition is harder than expected, fall back to two server-construction paths sharing all sub-state.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — declared the VPS-daemon goal that requires HTTP transport
- [ADR-0002](0002-multi-account-architecture.md) — auth flow per account; this ADR clarifies that auth is always local regardless of serve transport
- Future ADRs:
  - [ADR-0006](0006-config.md) — session timeout, HTTP bind address, related transport tunables
  - [ADR-0008](0008-observability-and-deployment.md) — nginx config, systemd unit, TLS strategy, session metrics
- [MCP Spec — Transports](https://spec.modelcontextprotocol.io/specification/2025-03-26/basic/transports/) — Streamable HTTP transport definition
- rmcp 1.5 transport modules:
  - [`transport-io` (stdio server)](https://docs.rs/rmcp/1.5.0/rmcp/transport/io/index.html)
  - [`transport-streamable-http-server-session`](https://docs.rs/rmcp/1.5.0/rmcp/transport/streamable_http_server/index.html)
