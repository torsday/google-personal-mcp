# ADR-0020: HTTP-transport authentication — bearer tokens at the daemon, mTLS optional at nginx

**Date:** 2026-05-22
**Status:** Accepted (shipped in v1.0)

---

## Context

[ADR-0003](0003-transport-stdio-and-streamable-http.md) adopts a Streamable HTTP transport for the VPS-daemon use case, with stdio kept for local development. The HTTP listener is `127.0.0.1`-bound by default; remote access is "operator's nginx problem" per the same ADR. A startup WARN fires when the daemon binds non-loopback without TLS.

[ADR-0008](0008-observability-and-deployment.md) ships a working `deploy/nginx.conf.example` template that terminates TLS via Let's Encrypt and reverse-proxies to the daemon on `127.0.0.1:8765`. The template includes a commented-out line:

```nginx
# Optional: client-cert auth or basic auth at this layer
# auth_basic "google-personal-mcp"; auth_basic_user_file /etc/nginx/google-personal-mcp.htpasswd;
```

This is a hand-wave. The reverse-proxy template *enables* an auth layer at nginx, but does not commit to one, document trade-offs, or say what the daemon does if reached without auth. As of v0.x, the daemon trusts every request that reaches its listener — once a packet hits `127.0.0.1:8765`, the operator is authenticated by virtue of being able to reach the loopback interface. For VPS deployments where nginx terminates TLS and forwards, that means **anyone who can reach the nginx HTTPS endpoint can call any MCP tool**, including destructive ones (`send_email`, `purge_account` from [ADR-0019](0019-data-retention-and-purge.md), etc.) on the operator's actual Gmail accounts. TLS without auth defends against passive interception, not against active access.

The threat model:

- **In-scope adversaries:** internet scanners hitting the nginx endpoint, anyone with shell on the operator's other devices, neighbors on a network the operator joins, anyone who acquires the operator's domain via DNS hijack (defended in part by TLS cert checks but they assume cert validation is intact).
- **Out-of-scope adversaries:** the operator themselves (per [ADR-0017](0017-secrets-at-rest.md) — single-operator daemon), an attacker with root on the VPS (separate ADR territory).
- **What's at stake:** the daemon holds OAuth refresh tokens for one or more of the operator's Google accounts. Tool surface includes `send_email` (forge messages from any registered account), `purge_account` (irrevocably remove a registered account from the daemon), `trash_thread` (move mail to trash), and read tools that disclose private content.

[#72](https://github.com/torsday/google-personal-mcp/issues/72) (Streamable HTTP transport implementation) is filed for v1.0. The auth-layer decision is on its critical path — implementing HTTP transport without it ships a footgun.

If no decision were made, the v1.0 HTTP transport would launch with one of two failure modes:

- Operators believe TLS-at-nginx is "secure" and expose `/mcp` to the internet unprotected; the first internet scanner with an MCP client owns their Gmail.
- Operators improvise — htpasswd here, mTLS there, no consistent posture — and the documentation tells them nothing about which is sufficient.

## Decision

We adopt a **two-layer model** for HTTP-transport authentication:

1. **Daemon-side bearer token, required.** When the daemon binds non-loopback, a bearer token configured in `~/.config/google-personal-mcp/http_auth.toml` (mode 600) **must** be present, and every request must carry `Authorization: Bearer <token>`. Loopback binds (`127.0.0.1`, `::1`) do not require the bearer; the loopback boundary is itself the auth layer for local-only deployments.
2. **mTLS at nginx, optional.** For deployments that want stronger client identification, the nginx template ships an opt-in mTLS block. mTLS at nginx layers on top of (does not replace) the daemon-side bearer.

stdio transport is unaffected — auth is the process boundary.

**v1 scope.** Nothing in this ADR ships in v0.x — HTTP transport itself is v1.0 per [ADR-0003](0003-transport-stdio-and-streamable-http.md), and this ADR's enforcement primitives gate on that work. The bearer-token validator, config-file shape, `auth bearer generate` subcommand, and nginx mTLS template are all v1.0 deliverables, sequenced with [#72](https://github.com/torsday/google-personal-mcp/issues/72).

### Daemon-side bearer token

The daemon checks `Authorization: Bearer <token>` on every request before any MCP processing. The check happens **before** session establishment ([ADR-0003](0003-transport-stdio-and-streamable-http.md) `Mcp-Session-Id`): unauthenticated requests are rejected with HTTP 401 and never get a session ID.

**Config file:** `~/.config/google-personal-mcp/http_auth.toml`, mode 600 (enforced at startup like every other secret file per [ADR-0017](0017-secrets-at-rest.md)):

```toml
# Active bearer tokens. Any token in this list authorizes a request.
# During rotation, list both old and new; remove old after clients have updated.
tokens = [
  "long-random-opaque-string-1",
  "long-random-opaque-string-2",
]
```

**Format:** opaque random strings, ≥ 256 bits entropy. Generated via a new CLI subcommand:

```text
google-personal-mcp auth bearer generate
```

Prints a fresh token to stdout (suitable for piping into a `tokens = [...]` line). Does **not** modify the config file — operator pastes deliberately. Generation uses the OS CSPRNG (`getrandom(2)` / equivalent); no time-based or guessable patterns.

**Validation:** constant-time string comparison against each `tokens[]` entry. Wrong-length tokens are constant-time-padded before comparison to avoid leaking active token lengths via timing.

**Rotation:** by config edit + reload. The operator:

1. Generates a new token: `google-personal-mcp auth bearer generate > /tmp/new`.
2. Adds it to `tokens[]` (now two entries active).
3. Sends `SIGHUP` to the daemon (or `systemctl reload google-personal-mcp` — supported per [ADR-0008](0008-observability-and-deployment.md) systemd unit). The daemon re-reads `http_auth.toml` without dropping sessions.
4. Distributes the new token to clients; clients update their `Authorization` header at their own pace.
5. Once all clients have migrated, the operator removes the old token from `tokens[]` and `SIGHUP`s again.

Multiple active tokens during rotation are by design: a hard cutover would require synchronized client updates, which is operationally impossible for a daemon serving multiple devices.

**Fail-closed startup invariant:** if the daemon is invoked with `--http <addr>` and `<addr>` is non-loopback, the daemon **refuses to start** unless `http_auth.toml` exists with at least one non-empty token. The error names the file, names the subcommand to generate a token, and exits with a clear non-zero status. This is symmetric with the existing [ADR-0003](0003-transport-stdio-and-streamable-http.md) startup WARN on non-loopback-without-TLS — except this one is a hard refuse rather than a warning, because the cost of getting it wrong is unauthorized tool access.

**Loopback bind:** `--http 127.0.0.1:PORT` or `--http [::1]:PORT` does not require the bearer file. The daemon trusts the OS user boundary for loopback. This preserves the existing local-development workflow.

**Localhost-only operator tunneling:** an operator who wants to access a remote VPS without exposing any public endpoint can SSH-tunnel: `ssh -L 8765:127.0.0.1:8765 vps`. The daemon stays bound to `127.0.0.1`; nginx is unused; no bearer is required. This is a fully supported deployment shape, documented in [ADR-0008](0008-observability-and-deployment.md)'s INSTALL.md.

### nginx layer: mTLS optional, htpasswd discouraged

The shipped `deploy/nginx.conf.example` ([ADR-0008](0008-observability-and-deployment.md)) gains an opt-in mTLS block, commented but ready:

```nginx
# Optional: require client certificate (mTLS) on top of TLS.
# Generate with: openssl req -x509 -newkey rsa:4096 -keyout client.key -out client.crt -days 365 -nodes
# Distribute client.crt + client.key to each client device; configure the MCP client to send.
# ssl_client_certificate /etc/nginx/clients-ca.crt;
# ssl_verify_client      on;
```

mTLS is **additive** to the daemon's bearer check, not a substitute. The bearer-token check at the daemon stays in force regardless of nginx-layer auth. Reason: defense in depth (an nginx misconfiguration that silently disables mTLS doesn't drop the daemon to anonymous-access).

HTTP Basic auth at nginx (`auth_basic`) is **not recommended** and the existing commented line is removed in favor of the bearer-token approach. Reasons:

- Two secrets to manage (bearer + htpasswd) without meaningful additive security — both leak via the same channel (intercepted TLS).
- htpasswd lacks rotation discipline; operators tend to never change them.
- Browsers cache Basic credentials aggressively, which surprises operators when they think they've revoked access.

mTLS is the right "stronger than bearer" layer for serious deployments because it identifies the *client device*, which is a different property than "knows the secret" — useful when the operator has multiple devices with different trust levels.

### Failed-auth treatment

**Response:**

- HTTP `401 Unauthorized` with header `WWW-Authenticate: Bearer realm="google-personal-mcp"`.
- Response body: a generic message (`{"error": "unauthorized"}`). Specifically does **not** distinguish "token missing" from "token wrong" — both reveal information.

**Rate limiting:** the daemon's existing per-account rate limiter does not apply (no account context for an unauthenticated request). Add a separate **per-source-IP throttle** for `Authorization`-bearing requests that fail validation: 1 attempt/sec, burst 10, sliding 60s window. After the burst exhausts, return 429 with `Retry-After: 60`. The throttle is in-memory only; daemon restart resets it. Counter exposed as a Prometheus metric `gmcp_http_auth_failures_total{source_ip="..."}` per [ADR-0008](0008-observability-and-deployment.md).

**Logging:** failed auth attempts go to the **tracing log** at WARN level: `auth_failure source_ip=<ip> path=<request_path> reason=<missing|invalid>`. They do **not** go to the audit log. Rationale: [ADR-0011](0011-audit-log.md) §"What is NOT in this audit log" explicitly limits the audit log to "what the agent did to your data on your behalf" — pre-tool-invocation auth rejections are infrastructure events, not operator-data events. The audit log records *what a successfully-authenticated session did*; failed-auth attempts belong with rate-limiter rejections and similar transport-layer events in the tracing log.

**Successful auth:** does not write an audit record by itself. The first tool call inside the authenticated session writes the normal [ADR-0011](0011-audit-log.md) record, which already includes `session_id`. Successful auth is implicit in the existence of a session_id; logging it separately would double the log volume for no gain.

### Interaction with the multi-client session model

[ADR-0003](0003-transport-stdio-and-streamable-http.md) tracks server-side sessions keyed by `Mcp-Session-Id`. The bearer-auth layer sits **before** session lookup:

1. Client connects (no session yet) and sends a request with `Authorization: Bearer <token>`.
2. Daemon validates token. On success, daemon issues `Mcp-Session-Id` in the response.
3. Client retains both `Authorization` and `Mcp-Session-Id` for subsequent requests.
4. Every subsequent request validates `Authorization` first, then looks up the session.

A single bearer token may authorize many concurrent sessions. Tokens are not 1:1 with sessions; a single token compromise grants the same access as a session ID compromise plus a token, which is to say: full tool surface until rotation.

**Session expiry vs. token expiry:** sessions expire on idle (default 1 hour per [ADR-0003](0003-transport-stdio-and-streamable-http.md)). Tokens have no built-in expiry; they live until the operator removes them from `tokens[]`. The two timeouts are independent: a long-running session does not "refresh" the token, and removing a token from config does not retroactively kill in-flight sessions (it only blocks new sessions and the next request on existing sessions). This is intentional — kill-switch operators use `systemctl stop` to drop all sessions immediately, not config edits.

**Token rotation does not require session restart.** Existing sessions continue using their (still-listed) token; once a client refreshes its token, subsequent requests validate against the new token.

### What this ADR does NOT do

- It does not introduce per-account or per-tool authorization (every authorized request can call every tool). That's a separate feature; the threat model here is "is this the operator?", not "what is the operator allowed to do?".
- It does not address OAuth flows. `google-personal-mcp auth add` for Google accounts is unchanged — that's still local-only, browser-mediated PKCE per [ADR-0003](0003-transport-stdio-and-streamable-http.md).
- It does not commit to OIDC / SSO / external IdP integration. The single-operator threat model doesn't justify the operational complexity.
- It does not chain bearer tokens cryptographically (JWT-style). Opaque tokens are simpler and sufficient; signing infrastructure is overkill for a single-operator daemon.
- It does not address `/healthz` and `/metrics`. Those bind separately to `127.0.0.1:9100` per [ADR-0008](0008-observability-and-deployment.md) and are not exposed by nginx. No auth needed; loopback is the boundary.

## Options Considered

### Primary auth mechanism

| Option | Pros | Cons |
| --- | --- | --- |
| (a) No auth — trust the network boundary | Simplest; nothing to misconfigure | Any internet scanner that reaches `/mcp` owns Gmail; nginx-TLS-without-auth is a footgun trap |
| **(b) Bearer token at the daemon, required for non-loopback** (chosen) | Standards-conformant (`Authorization: Bearer`); MCP-spec aligned; lower operational burden than per-client certs; rotation is a config edit + reload; works with any client | Token compromise = full access until rotated; single secret model |
| (c) mTLS at nginx as the primary mechanism | Strongest client identification; per-device cert distribution makes "revoke this device" meaningful | High operational burden — every client device needs a cert; lost-device story is "regenerate CA, re-issue all certs"; many MCP clients don't easily support mTLS |
| (d) HTTP Basic at nginx | Familiar; no daemon code | Two secrets to manage with same threat model; htpasswd never rotates in practice; browsers cache aggressively |
| (e) Pre-shared key in `Mcp-Session-Id` header | Reuses an existing header | Abuses session-ID namespace (per MCP spec, sessions are server-issued opaque tokens, not client-supplied auth material); confuses two concerns |
| (f) Localhost-only, mandatory SSH tunnel for remote | Smallest attack surface — no public listener exists | Forces SSH-tunnel UX on every operator regardless of preference; doesn't compose with multi-device access patterns |

We choose (b). The bearer header is the lowest-friction auth that meaningfully defends the threat model. mTLS is preserved as the additive layer for paranoid deployments (`b + c`). Option (f) remains available — bearer is only required for non-loopback binds, so localhost-only with SSH tunneling is unaffected.

### Failed-auth log destination

| Option | Pros | Cons |
| --- | --- | --- |
| **(g) Tracing log only** (chosen) | Matches [ADR-0011](0011-audit-log.md)'s explicit scope ("agent actions on operator data"); auth failures are infrastructure events; tracing target supports source-IP and rate-limit context | Operators wanting an audit-grade record of attack attempts must scrape the tracing log |
| (h) Audit log | Single place to look for security-relevant events | Pollutes the audit log with non-tool-invocation events; breaks [ADR-0011](0011-audit-log.md)'s contract; tracing log already correctly handles per-source rate-limited events |
| (i) Both | Maximum visibility | Doubles the volume for no clear benefit; the audit log gains content the operator-facing tools (`audit_summary` per [ADR-0011](0011-audit-log.md)) aren't designed to surface |

We choose (g). Tracing-log destination matches the [ADR-0011](0011-audit-log.md) §"What is NOT in this audit log" contract. The Prometheus counter (`gmcp_http_auth_failures_total`) is the right surface for alerting on attack patterns.

### Startup invariant for non-loopback bind

| Option | Pros | Cons |
| --- | --- | --- |
| **(j) Hard refuse to start without `http_auth.toml`** (chosen) | Fail-closed; impossible to "accidentally" expose anonymous tool access; symmetric with [ADR-0017](0017-secrets-at-rest.md)'s perm-check refuse | Slightly more startup-time logic; operator who really wants anonymous bind must hand-edit code |
| (k) Warn-and-continue | Less friction during first-time setup | Identical to current behavior; the warn won't change operator behavior in practice; perpetuates the v0.x footgun |
| (l) Refuse only if no nginx-layer auth either (probe nginx config) | Most permissive | Can't reliably introspect nginx config from the daemon; brittle |

We choose (j). The "first-time setup" friction is one `auth bearer generate` invocation and a paste into a config file; the steady-state cost is zero.

## Consequences

**Positive:**

- VPS deployments are auth-required by default. No surface in v1.0 ships an unauthenticated path to destructive tools.
- Operators get one clear mechanism (bearer header) with explicit documentation, replacing the [ADR-0008](0008-observability-and-deployment.md) hand-wave.
- Token rotation is operator-driven and disruption-free: multiple active tokens during the rotation window, no client-side coordination required.
- Defense in depth is available without being mandatory: paranoid operators stack mTLS on top of bearer; pragmatic operators run bearer-only behind nginx TLS.
- Loopback-bound deployments and SSH-tunnel workflows are unaffected — the boundary they already rely on (OS user / SSH session) remains the auth layer.
- Failed-auth events surface as Prometheus metrics, enabling alerting on attack patterns ([ADR-0008](0008-observability-and-deployment.md) alertmanager rules can add a brute-force-detection rule).

**Negative:**

- One more config file (`http_auth.toml`) and one more CLI subcommand (`auth bearer generate`). Documented in INSTALL.md.
- Bearer tokens are long-lived secrets; an operator who never rotates is a single-leak-away from full compromise. Mitigated by documentation, not enforcement (no forced expiry — would surprise operators on long-running sessions).
- mTLS as an optional layer doesn't ship with cert-generation tooling; operators wanting it run `openssl` themselves. The nginx template provides the wire-up; the cert ceremony is on the operator.

**Risks:**

- *Risk:* Operators run `--http 0.0.0.0:8765` without `http_auth.toml`, the daemon refuses to start, operators get frustrated and `--insecure-no-auth` flag culture emerges.
  *Mitigation:* We do **not** ship `--insecure-no-auth`. The startup error message is specific and actionable (`run \`google-personal-mcp auth bearer generate\` and paste into ~/.config/google-personal-mcp/http_auth.toml`). Documented in `deploy/INSTALL.md` as a required step. Loopback bind for local dev remains escape hatch.

- *Risk:* Bearer token leaks via client-side logging, screenshot, or `Authorization` header captured in HTTP debug tooling.
  *Mitigation:* Client-side leakage is out of daemon's control. Documented as part of the rotation discipline. Operators with high-leak-risk deployments use mTLS as the additive layer (compromised bearer + missing client cert = no access).

- *Risk:* Per-source-IP throttle on failed auth is bypassable from a botnet (different IPs).
  *Mitigation:* The throttle's purpose is brute-force protection against a single attacker, not DoS. A botnet would still need to guess a ≥ 256-bit opaque string — guessing rate is the bottleneck, not request rate. If DoS becomes the threat, nginx-level rate limiting (operator-configured) is the right layer.

- *Risk:* Token validation timing leaks the active token count or position of the first-matching token.
  *Mitigation:* Validation iterates all entries even after a match, performing the constant-time compare against each (constant-time match-tracking). The bytes processed per validation is constant for a given `tokens[]` length. Tests cover the constant-time property.

- *Risk:* `SIGHUP` reload races a token rotation: a request authenticated against the old token in flight when the new config drops the old token causes mid-request "session unauthorized" failures.
  *Mitigation:* Auth check happens **once per request**, at the start. In-flight requests complete with their start-time validation result. Session-level tokens are not re-validated on every request beyond the initial bearer check. Operators rotating tokens accept brief mid-request failures as acceptable; the alternative (snapshot tokens for the session lifetime) creates a window where revoked tokens still work for up to the idle timeout, which is worse.

- *Risk:* The "no audit-log entry for failed auth" decision means an attacker probing `/mcp` doesn't appear in `audit_summary`.
  *Mitigation:* Documented as design intent. Tracing log + Prometheus counter is the surface for security investigations. `audit_summary` is for the operator asking "what did my agent do," not "is someone attacking my daemon."

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — single-operator daemon trust model
- [ADR-0003](0003-transport-stdio-and-streamable-http.md) — Streamable HTTP transport this ADR authenticates; defines the `Mcp-Session-Id` model
- [ADR-0005](0005-error-model.md) — `Error::AuthRequired` for unauthorized responses
- [ADR-0006](0006-config.md) — `http_auth.toml` location convention
- [ADR-0008](0008-observability-and-deployment.md) — amended by this ADR (nginx template gains mTLS block; htpasswd line removed; `gmcp_http_auth_failures_total` counter added)
- [ADR-0011](0011-audit-log.md) — failed-auth events go to tracing log, **not** audit log (consistent with §"What is NOT in this audit log")
- [ADR-0017](0017-secrets-at-rest.md) — `http_auth.toml` enforced at mode 600 like every other secret file at startup
- [ADR-0019](0019-data-retention-and-purge.md) — `purge_account` is one of the destructive tools this ADR protects against unauthenticated access
- Issue [#87](https://github.com/torsday/google-personal-mcp/issues/87) — origin
- Issue [#72](https://github.com/torsday/google-personal-mcp/issues/72) — gates this ADR's implementation (Streamable HTTP server)
- [MCP Spec — `Authorization` header](https://spec.modelcontextprotocol.io/specification/2025-03-26/basic/transports/) — the bearer-header pattern this ADR adopts
