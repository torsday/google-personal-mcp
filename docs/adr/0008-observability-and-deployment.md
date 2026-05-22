# ADR-0008: Observability and deployment — `tracing`, Prometheus, `/healthz`, systemd + nginx

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

Two adjacent concerns are combined here because they are tightly coupled in practice: observability defines what we can see about the running daemon, and deployment defines where it runs and how it integrates with the host system.

The prototype's observability is limited to `tracing_subscriber` initialized in `main.rs` writing to stderr at INFO level. There are no spans on tool calls, no structured fields per error variant ([ADR-0005](0005-error-model.md) recommends them), no metrics, and no health endpoint. For a daemon that must run continuously serving 10+ accounts ([ADR-0002](0002-multi-account-architecture.md)) over both transports ([ADR-0003](0003-transport-stdio-and-streamable-http.md)), this is insufficient.

The prototype's deployment is implied by the README's "stdio integration with Claude Desktop" — i.e., Claude Desktop spawns the binary as a subprocess. There is no documented path for the stated "long-running on a personal VPS" goal. With [ADR-0003] settling on dual transports, we need a real deployment story for HTTP mode.

If no decision were made: we'd have a daemon that "works" but provides no signal during incidents (refresh storm, account-misroute, rate-limit hit) and no documented way to deploy it on a VPS.

## Decision

**v1 scope.** The full design below is the target for **v1.0** (first public release). For **v0.x** — the single-user phase before public release — implement only structured `tracing` logging to stderr, the systemd unit (with the [ADR-0017](0017-secrets-at-rest.md) dedicated-user / `StateDirectory=` update), and a liveness `/healthz` that returns 200 if the daemon is running. **Defer to v1.0:** the Prometheus exporter and `/metrics` endpoint, the 12-metric inventory, the alertmanager rules, and the SLO table. These earn their keep when an external operator runs the daemon with their own monitoring stack; for the maintainer-as-sole-user, `journalctl -u google-personal-mcp` is the dashboard. The metric crate seams can be added when the exporter ships — premature instrumentation is wasted code.

### Observability — three pillars (logging, metrics, health), implemented per the constraints below

#### Logging

- **Library:** `tracing` 0.1 (already in deps), `tracing-subscriber` 0.3 with `env-filter` and `json` features.
- **Destinations:**
  - **stdio mode:** stderr only (stdout is reserved for MCP wire). Format from `[server].log_format` config: `"compact"` (default, human-readable) or `"json"` (for structured ingest).
  - **HTTP mode:** stderr (typically captured by systemd → journald). Same format choice.
- **Spans:**
  - One span per MCP tool call: `tool.name`, `tool.account` (if applicable), `tool.duration_ms`, `tool.outcome` (`success` | `<error.kind>`)
  - One span per outbound Google API call: `google.service`, `google.endpoint`, `google.status`, `google.account`, `google.duration_ms`, `google.attempt` (for retries)
  - One span per OAuth refresh: `oauth.account`, `oauth.outcome`, `oauth.duration_ms`
- **Structured fields per error variant** (per [ADR-0005]): every `tracing::error!` / `tracing::warn!` includes `error.kind` (the variant name) and per-variant context fields (`account`, `service`, `status`, etc.). This enables queries like `error.kind == "RateLimited" AND account == "work"` against any structured-log sink.
- **What we don't log:**
  - Email bodies, message content, calendar event details (PII).
  - Access tokens or refresh tokens (ever, anywhere).
  - Full request/response bodies (only status, length, key error excerpts).
  - We log thread IDs, account aliases (not addresses), and tool names — enough for debugging without leaking content.
- **Filter convention:** `RUST_LOG=google_mcp=info,reqwest=warn,hyper=warn` is the documented default. The reqwest/hyper noise is silenced unless explicitly debugging the HTTP layer.

#### Metrics

- **Library:** `metrics` 0.23 facade + `metrics-exporter-prometheus` 0.15 (HTTP mode only — Prometheus scrape endpoint).
- **Exposure:** `/metrics` HTTP endpoint, served on a **separate** internal listener (default `127.0.0.1:9100`) — never on the public-facing MCP port. Configurable via `[metrics] bind = "..."` in [ADR-0006](0006-config.md). Disabled by default unless the section is present in config.
- **Counter / gauge / histogram inventory:**

  | Metric | Type | Labels | Notes |
  | --- | --- | --- | --- |
  | `gmcp_tool_calls_total` | Counter | `tool`, `outcome` | Outcome: `success` or error variant name |
  | `gmcp_tool_call_duration_seconds` | Histogram | `tool` | Buckets: 0.01, 0.05, 0.1, 0.5, 1, 5, 10, 30 |
  | `gmcp_google_api_calls_total` | Counter | `service`, `endpoint`, `status_class` | `status_class` is `2xx`/`3xx`/`4xx`/`5xx`/`network` |
  | `gmcp_google_api_call_duration_seconds` | Histogram | `service`, `endpoint` | |
  | `gmcp_token_refreshes_total` | Counter | `account`, `outcome` | `outcome` = `success`/`invalid_grant`/`network`/`upstream` |
  | `gmcp_active_accounts` | Gauge | (none) | Reflects current `accounts.toml` size |
  | `gmcp_http_sessions_active` | Gauge | (none) | HTTP transport only |
  | `gmcp_http_session_duration_seconds` | Histogram | (none) | Lifespan of MCP sessions in HTTP mode |
  | `gmcp_hot_reload_total` | Counter | `outcome` | `success`/`parse_error`/`validation_error` |
  | `gmcp_cache_write_discarded_total` | Counter | `account` | History-id race on cache write per [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — should be near zero in steady state |
  | `gmcp_rate_limit_blocks_total` | Counter | `account`, `service` | Increments when the per-account rate limiter actually delays a call |
  | `gmcp_build_info` | Gauge (always 1) | `version`, `git_sha`, `rust_version` | Standard "info metric" pattern |

- Metrics labels do **not** include high-cardinality fields (no thread IDs, no message IDs, no email addresses).

#### Service-level objectives (SLOs) and indicators (SLIs)

The daemon's "healthy" state is defined operationally — not "the process is running" but "the daemon is meeting its commitments to consumers." The following SLOs are the recommended starting point; tune per deployment based on real measured baselines.

| SLI (what we measure) | SLO (target) | Source metric |
| --- | --- | --- |
| Tool call latency p95 (cache-warm) | < 500 ms | `histogram_quantile(0.95, rate(gmcp_tool_call_duration_seconds_bucket{tool!~".*_status\|.*_summary"}[5m]))` |
| Tool call latency p95 (cache-miss / cold) | < 2 s | Same histogram, segmented by `gmcp_cache_misses_total` correlation |
| Token refresh failure rate (24h rolling) | < 0.1% | `rate(gmcp_token_refreshes_total{outcome!="success"}[24h]) / rate(gmcp_token_refreshes_total[24h])` |
| Cache hit rate (steady-state, after 1h warmup) | > 60% | `rate(gmcp_cache_hits_total[1h]) / (rate(gmcp_cache_hits_total[1h]) + rate(gmcp_cache_misses_total[1h]))` |
| Accounts with `auth_state != "ok"` | < 10% of total | `(gmcp_active_accounts - sum(account_auth_ok)) / gmcp_active_accounts` (custom gauge) |
| Hot-reload failure rate | < 1% | `rate(gmcp_hot_reload_total{outcome!="success"}[24h]) / rate(gmcp_hot_reload_total[24h])` |
| HTTP-mode `/healthz` availability (HTTP transport) | > 99.9% | external probe |
| Memory growth (24h trailing) | < 10% drift | `(process_resident_memory_bytes - process_resident_memory_bytes offset 24h) / process_resident_memory_bytes offset 24h` — Rust's GC-free promise |

#### Suggested Prometheus alerting rules

A starter alertmanager ruleset ships at `deploy/alerts.yml`:

```yaml
groups:
- name: google-personal-mcp
  rules:
  - alert: GoogleMcpHighErrorRate
    expr: rate(gmcp_tool_calls_total{outcome!="success"}[5m]) / rate(gmcp_tool_calls_total[5m]) > 0.05
    for: 10m
    labels: { severity: warning }
    annotations:
      summary: "google-personal-mcp tool error rate > 5% on {{ $labels.instance }}"

  - alert: GoogleMcpRefreshFailures
    expr: rate(gmcp_token_refreshes_total{outcome!="success"}[1h]) > 0.01
    for: 30m
    labels: { severity: warning }
    annotations:
      summary: "google-personal-mcp token refresh failing — likely revoked token; run google-personal-mcp auth refresh"

  - alert: GoogleMcpAccountStuck
    expr: gmcp_active_accounts - sum(account_auth_ok) > 0
    for: 1h
    labels: { severity: warning }
    annotations:
      summary: "{{ $value }} account(s) in non-ok auth state for >1h"

  - alert: GoogleMcpCacheRaceSpiking
    expr: rate(gmcp_cache_write_discarded_total[5m]) > 0.5
    for: 15m
    labels: { severity: warning }
    annotations:
      summary: "cache writes being discarded — concurrent history-sync race; investigate"

  - alert: GoogleMcpMemoryDrift
    expr: process_resident_memory_bytes / process_resident_memory_bytes offset 24h > 1.10
    for: 6h
    labels: { severity: warning }
    annotations:
      summary: "RSS grew >10% in 24h — Rust's GC-free promise broken; investigate leak"

  - alert: GoogleMcpHotReloadFailing
    expr: rate(gmcp_hot_reload_total{outcome!="success"}[1h]) > 0
    for: 2h
    labels: { severity: warning }
    annotations:
      summary: "hot-reload failing for {{ $value }}/hr — accounts.toml may be malformed"

  - alert: GoogleMcpDown
    expr: up{job="google-personal-mcp"} == 0
    for: 2m
    labels: { severity: critical }
    annotations:
      summary: "google-personal-mcp daemon is down"
```

These rules assume a Prometheus job is scraping `http://127.0.0.1:9100/metrics` from the daemon. Operator drops the file into their alertmanager and tunes thresholds per traffic.

#### Health endpoint

- `/healthz` HTTP endpoint, served on the same internal listener as `/metrics`.
- Returns `200 OK` with body `ok` if the daemon's basic invariants hold:
  - At least one account in registry (else `503 Service Unavailable` with body `no accounts configured`)
  - Last reload (if any) succeeded (else `503` with body `last reload failed: <error>`)
  - No accounts in `AuthRequired` state for >24h (else `200` but body lists the stuck accounts as a soft warning — health is still OK because other accounts work)
- Designed for integration with systemd's `Watchdog=` directive (which can SIGKILL the process if `/healthz` stops responding) **only if** we wire `sd_notify` separately. Default systemd unit does not enable Watchdog.

### Deployment

#### Local — Claude Desktop (stdio)

`~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "google": {
      "command": "/usr/local/bin/google-personal-mcp",
      "args": ["serve", "--stdio"]
    }
  }
}
```

That's the entirety of the local-mode deployment story. Auth happens via `google-personal-mcp auth add`; tokens live in `~/.config/google-personal-mcp/tokens/`.

#### VPS — systemd + nginx + Streamable HTTP

A unit file template ships at `deploy/google-personal-mcp.service`:

```ini
[Unit]
Description=google-personal-mcp — Google services MCP daemon
Documentation=https://github.com/torsday/google-personal-mcp
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=google-personal-mcp
Group=google-personal-mcp
ExecStart=/usr/local/bin/google-personal-mcp serve --http 127.0.0.1:8765
Restart=always
RestartSec=2s
Environment=RUST_LOG=google_mcp=info,reqwest=warn,hyper=warn

# Hardening
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=read-only           # google-personal-mcp reads its own ~/.config; nothing else
ProtectSystem=strict
ReadWritePaths=/home/google-personal-mcp/.config/google-personal-mcp
ReadOnlyPaths=/usr/local/bin/google-personal-mcp
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictRealtime=true
SystemCallArchitectures=native

# Resource limits
LimitNOFILE=4096
TasksMax=64

[Install]
WantedBy=multi-user.target
```

A reverse-proxy template ships at `deploy/nginx.conf.example` with TLS termination via Let's Encrypt:

```nginx
server {
    listen 443 ssl http2;
    server_name google-personal-mcp.your-domain.tld;

    ssl_certificate     /etc/letsencrypt/live/your-domain.tld/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/your-domain.tld/privkey.pem;

    # MCP traffic — Streamable HTTP with SSE support
    location / {
        proxy_pass http://127.0.0.1:8765;
        proxy_http_version 1.1;
        proxy_set_header Connection '';
        proxy_set_header Mcp-Session-Id $http_mcp_session_id;
        proxy_buffering off;                # SSE requires unbuffered upstream
        proxy_read_timeout 1h;              # match session_idle_timeout in config
        proxy_request_buffering off;
    }

    # Optional: client-cert auth or basic auth at this layer
    # auth_basic "google-personal-mcp"; auth_basic_user_file /etc/nginx/google-personal-mcp.htpasswd;
}
```

Metrics and `/healthz` are served on `127.0.0.1:9100` and intentionally NOT exposed by nginx — they're for a local Prometheus scrape and the systemd watchdog (if enabled).

#### Install steps (documented in `deploy/INSTALL.md`)

1. Create `google-personal-mcp` system user: `useradd -r -s /usr/sbin/nologin -d /home/google-personal-mcp -m google-personal-mcp`
2. Build release binary on a build host; `scp target/release/google-personal-mcp root@vps:/usr/local/bin/`; `chmod 755 /usr/local/bin/google-personal-mcp`
3. On the VPS, create `~/.config/google-personal-mcp/credentials/google.json` (the GCP OAuth client) with mode 600
4. On a **local** machine with a browser, run `google-personal-mcp auth add --alias <name>` for each account (per [ADR-0003] auth-always-local rule)
5. `scp ~/.config/google-personal-mcp/tokens/*.json root@vps:/home/google-personal-mcp/.config/google-personal-mcp/tokens/`
6. `chown -R google-personal-mcp:google-personal-mcp /home/google-personal-mcp/.config && chmod 600 /home/google-personal-mcp/.config/google-personal-mcp/tokens/*.json`
7. Install systemd unit: `cp deploy/google-personal-mcp.service /etc/systemd/system/`
8. Configure nginx: copy template, edit, `systemctl reload nginx`
9. `systemctl enable --now google-personal-mcp`
10. Verify: `curl https://google-personal-mcp.your-domain.tld/...` (expect MCP-protocol response)

### Process model

- Single Tokio multi-threaded runtime (default worker threads = CPU count).
- One task per HTTP session in HTTP mode.
- One task per file watcher (notify, [ADR-0002]).
- Graceful shutdown: SIGTERM triggers (a) stop accepting new MCP requests, (b) wait for in-flight tool calls to complete (with 30s timeout), (c) flush logs, (d) exit. Implemented via `tokio::signal::ctrl_c()` + `tokio::signal::unix::signal(SignalKind::terminate())`.

## Options Considered

### Logging library

| Option | Pros | Cons |
| --- | --- | --- |
| **(a) `tracing` + `tracing-subscriber`** (chosen) | Already in deps; structured fields native; spans for distributed-trace-style debugging; ecosystem standard | Slightly steeper learning curve than `log` |
| (b) `log` + `env_logger` | Simpler API | No structured fields, no spans — back to log-string parsing |
| (c) `slog` | Structured | Less popular; ecosystem fragmented; rmcp itself uses `tracing` |

### Metrics

| Option | Pros | Cons |
| --- | --- | --- |
| **(d) `metrics` facade + Prometheus exporter** (chosen) | Low overhead; Prometheus is the operational standard; pull model fits intermittent VPS scrapes | Adds 2 deps; only useful in HTTP mode (or with separate listener in stdio mode) |
| (e) StatsD / push-based | Decouples scrape; works behind NATs | Requires a StatsD daemon on the VPS; more infra |
| (f) OpenTelemetry | Future-proof; combines metrics + traces | Heavier dep; over-engineering for personal scale |
| (g) No metrics | Less code | Operating blind on a long-running daemon |

### Deployment topology

| Option | Pros | Cons |
| --- | --- | --- |
| **(h) systemd unit + nginx reverse-proxy + dedicated user** (chosen) | Standard Linux deployment pattern; systemd handles restart/logging; nginx handles TLS | Requires sysadmin knowledge to set up |
| (i) Run as root | Simpler | Trivially worse security; no upside |
| (j) Containerize (Docker) | Consistent across hosts | Personal VPS often doesn't run Docker; adds runtime; container escape risk for token files |
| (k) Just run via `nohup` / tmux | Trivial setup | No restart on crash; no log management; terrible for a "runs forever" daemon |

### TLS

| Option | Pros | Cons |
| --- | --- | --- |
| **(l) Terminate TLS at nginx; daemon listens on 127.0.0.1** (chosen) | nginx is the cert / TLS expert; daemon stays simple | Requires nginx |
| (m) Terminate TLS in the daemon (`rustls`) | One process | Cert renewal logic in our code; reload-on-renewal hassles |
| (n) Plain HTTP (no TLS) | Simplest | Tokens leak on every call; unacceptable |

## Consequences

**Positive:**

- Logs carry structured fields per error kind ([ADR-0005] integration); ops queries don't depend on log-string parsing.
- Metrics give continuous visibility into refresh rates, error rates, rate-limit blocks, hot-reload events. Catches slow regressions (e.g., refresh failure rate creeping up) that logs alone miss.
- `/healthz` enables systemd watchdog and external uptime monitoring.
- Deployment story is concrete: one systemd unit, one nginx config, one INSTALL.md. The maintainer (or future them in 18 months) can reproduce a deploy from documentation.
- systemd hardening (`ProtectSystem=strict`, `NoNewPrivileges`, etc.) is meaningful security with zero runtime cost. Excellence.
- Auth-always-local + token-file-portability ([ADR-0003]) means the VPS user (`google-personal-mcp` system user) never sees a browser flow. Tokens arrive via scp.
- Graceful shutdown (drain + flush) means restarts during deploy / config-change don't drop in-flight tool calls.

**Negative:**

- Metrics-exporter-prometheus is HTTP-server-spawning behavior; in stdio mode it requires a separate listener (or being disabled). Adds `[metrics]` config section.
- The systemd hardening directives are conservative — if we ever need to write to a path outside `~/.config/google-personal-mcp`, we'll hit `ProtectSystem=strict` and have to widen `ReadWritePaths`. Acceptable; the constraint is the point.
- nginx reverse proxy adds an operator skill requirement. Mitigated by shipping a working template.
- Log volume can grow; rotation is journald's job, not ours. Document `journalctl --vacuum-time=30d` for log retention if disk pressure becomes real.
- We commit to `metrics` + `tracing` library APIs; future migrations to OpenTelemetry would be a real refactor.

**Risks:**

- *Risk:* Token files end up world-readable on the VPS due to operator error during install.
  *Mitigation:* INSTALL.md explicitly documents `chmod 600`. Daemon refuses to start if any file in `tokens/` has world-readable bits set (one-line check at startup).
- *Risk:* nginx `proxy_buffering off` is required for SSE but is easy to miss when copying the config template.
  *Mitigation:* Comment in the template; daemon logs a "session appears to be buffered" warning if it sees client streams stalling at unusual checkpoints.
- *Risk:* `/healthz` returning OK while individual accounts are unhealthy could mask problems.
  *Mitigation:* Per-account auth state surfaces as a metric (`gmcp_token_refreshes_total{outcome="invalid_grant"}`) and as a soft `/healthz` body annotation; operator can alert on the metric.
- *Risk:* systemd `Restart=always` masks crash-loop bugs (silent rapid restarts).
  *Mitigation:* `RestartSec=2s` slows the loop; `StartLimitIntervalSec=300` + `StartLimitBurst=5` (added) makes systemd give up after 5 crashes in 5 minutes, surfacing the failure to the operator's monitoring.
- *Risk:* Metrics labels could have unbounded cardinality if an enum gains many variants.
  *Mitigation:* We document the inventory in this ADR; new labels require updating the inventory and considering cardinality. `tool` label cardinality = number of MCP tools (small); `service` label = enabled Google services (small); no per-account labels on histograms (use counter only for per-account).

## References

- [ADR-0002](0002-multi-account-architecture.md) — hot-reload events and metrics
- [ADR-0003](0003-transport-stdio-and-streamable-http.md) — both transports' observability shape; auth-always-local affects deploy steps
- [ADR-0004](0004-oauth-token-refresh.md) — refresh metrics and spans
- [ADR-0005](0005-error-model.md) — structured tracing fields per error variant
- [ADR-0006](0006-config.md) — `[server.log_format]`, `[metrics]`, `[http]` sections
- [ADR-0020](0020-http-transport-authentication.md) — amends the nginx template (mTLS block in place of the htpasswd hand-wave); adds `gmcp_http_auth_failures_total` to the metric inventory
- [`tracing`](https://docs.rs/tracing), [`tracing-subscriber`](https://docs.rs/tracing-subscriber) — logging
- [`metrics`](https://docs.rs/metrics), [`metrics-exporter-prometheus`](https://docs.rs/metrics-exporter-prometheus) — metrics
- systemd [hardening directives](https://www.freedesktop.org/software/systemd/man/systemd.exec.html) — security baseline for the unit file
- nginx [proxy_buffering off + SSE](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_buffering) — required for Streamable HTTP's SSE
