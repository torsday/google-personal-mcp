# google-personal-mcp — VPS install

Per [ADR-0008 §VPS — systemd + nginx](../docs/adr/0008-observability-and-deployment.md)
and [ADR-0017 §Dedicated user](../docs/adr/0017-secrets-at-rest.md). For
Claude Desktop / local stdio installs, see the project [README](../README.md);
this document covers the always-on remote daemon path.

## What ships in this directory

| File | Purpose |
| --- | --- |
| `google-personal-mcp.service` | systemd unit template. Hardening flags come from ADR-0008 lines 188-202; matches the ADR template line-for-line. |
| `INSTALL.md` | This file — the 10-step deploy procedure (ADR-0008 lines 240-251). |

`nginx.conf.example` (HTTP transport TLS termination) is a v1.0 sibling — see
issue [#77](https://github.com/torsday/google-personal-mcp/issues/77); the
HTTP transport itself is gated by [#72](https://github.com/torsday/google-personal-mcp/issues/72) /
[#73](https://github.com/torsday/google-personal-mcp/issues/73). Until those
land, drop the `--http 127.0.0.1:8765` arg from the unit's `ExecStart` and run
the daemon in stdio mode — useful for systemd-managed local installations
even without the HTTP transport.

## Prerequisites

- Linux VPS with systemd (Debian 12, Ubuntu 22.04+, Fedora 39+, etc.).
- Root or `sudo` for the dedicated-user + systemd + (eventually) nginx steps.
- A **local** machine with a browser — OAuth grant is interactive, daemon
  refresh is not. This is the auth-always-local rule from
  [ADR-0003](../docs/adr/0003-transport-stdio-and-streamable-http.md).

## 10-step procedure

> Per [ADR-0008 §Install steps](../docs/adr/0008-observability-and-deployment.md).
> Commands assume Debian-family; adjust `useradd` flags for your distro.

### 1. Create the dedicated system user

```sh
sudo useradd -r -s /usr/sbin/nologin \
  -d /home/google-personal-mcp -m google-personal-mcp
```

The user owns the daemon's config + tokens. `nologin` shell prevents
interactive use; `-r` marks it as a system user (UID < 1000).

### 2. Build + ship the release binary

On the **build host** (your laptop or CI artifact pull):

```sh
just build                                                # or: cargo build --release --locked
scp target/release/google-personal-mcp root@vps:/usr/local/bin/
ssh root@vps 'chmod 755 /usr/local/bin/google-personal-mcp'
```

Don't build on the VPS unless you have to — the build pulls ~200 crates and
the daemon doesn't need a compiler at runtime.

### 3. Stage the GCP OAuth client credential

On the VPS:

```sh
sudo -u google-personal-mcp mkdir -p ~google-personal-mcp/.config/google-personal-mcp/credentials
sudo -u google-personal-mcp install -m 600 /dev/stdin \
  ~google-personal-mcp/.config/google-personal-mcp/credentials/google.json <<'EOF'
{ ...paste the OAuth client JSON downloaded from console.cloud.google.com... }
EOF
```

This is the **OAuth client** secret (project-level), not a user token. Mode
`600` matches the
[ADR-0017](../docs/adr/0017-secrets-at-rest.md) startup permission gate.

### 4. Run `auth add` on your laptop, once per Google account

> The daemon does **not** open a browser. OAuth grant happens locally; the
> daemon receives the resulting refresh tokens via the next step.

```sh
google-personal-mcp auth add --alias personal
google-personal-mcp auth add --alias work
```

Each `auth add` opens the system browser, walks the OAuth consent flow,
and writes `~/.config/google-personal-mcp/tokens/<alias>.json` with mode
0600.

### 5. Copy tokens to the VPS

```sh
scp ~/.config/google-personal-mcp/tokens/*.json \
    root@vps:/home/google-personal-mcp/.config/google-personal-mcp/tokens/
```

Tokens are per-account refresh credentials. The daemon refreshes
unattended; you only re-run `auth add` after a revocation (`invalid_grant`).

### 6. Restore ownership + perms on the VPS

```sh
sudo chown -R google-personal-mcp:google-personal-mcp \
  /home/google-personal-mcp/.config
sudo chmod 600 /home/google-personal-mcp/.config/google-personal-mcp/tokens/*.json
```

The daemon refuses to start with looser perms — ADR-0017 startup gate.
If you skipped step 1's `-m` flag, also `chmod 700` the parent dirs.

### 7. Install the systemd unit

```sh
sudo cp deploy/google-personal-mcp.service /etc/systemd/system/
sudo systemctl daemon-reload
```

Verify the unit parses cleanly:

```sh
systemd-analyze verify /etc/systemd/system/google-personal-mcp.service
```

### 8. (HTTP-transport deploys only) Configure bearer auth + nginx

**Bearer token (v1.0 / HTTP transport).** When you expose the daemon beyond
the loopback interface, every request must carry an `Authorization: Bearer
<token>` header. The daemon refuses to start on a non-loopback bind without
a bearer-token config file.

**First-time setup:**

```sh
# 1. Generate a token (prints a 43-char URL-safe base64 string to stdout).
google-personal-mcp auth bearer generate

# 2. Paste the output into http_auth.toml.
sudo -u google-personal-mcp \
  install -m 600 /dev/null \
  /home/google-personal-mcp/.config/google-personal-mcp/http_auth.toml

# Edit the file (run as google-personal-mcp or root):
cat > /home/google-personal-mcp/.config/google-personal-mcp/http_auth.toml <<'EOF'
# Active bearer tokens. During rotation, list both old and new; remove
# the old one after all clients have updated their Authorization header.
tokens = [
  "paste-token-here",
]
EOF
chmod 600 /home/google-personal-mcp/.config/google-personal-mcp/http_auth.toml
chown google-personal-mcp: \
  /home/google-personal-mcp/.config/google-personal-mcp/http_auth.toml
```

**Token rotation** — to rotate without dropping existing client connections:

```sh
NEW=$(google-personal-mcp auth bearer generate)
# Add $NEW to tokens[] alongside the old entry, then reload:
systemctl reload google-personal-mcp   # SIGHUP — re-reads http_auth.toml
# Distribute $NEW to clients; once all clients have migrated, remove the
# old token from tokens[] and reload again.
```

**nginx** — skip the nginx step until
[#77](https://github.com/torsday/google-personal-mcp/issues/77) ships the
`nginx.conf.example`. For stdio-only or dev installs, drop the `--http ...`
flag from `ExecStart` first — no bearer config file is needed for loopback
binds.

### 9. Enable + start the service

```sh
sudo systemctl enable --now google-personal-mcp
sudo systemctl status google-personal-mcp        # expect: active (running)
journalctl -u google-personal-mcp -f             # follow logs (stderr → journal)
```

### 10. Verify the daemon is healthy

If `[metrics]` is enabled in `config.toml` (see issue
[#70](https://github.com/torsday/google-personal-mcp/issues/70)):

```sh
curl -sf http://127.0.0.1:9100/healthz       # expect: 200 ok
```

For HTTP transport deploys (post-#72):

```sh
curl https://google-personal-mcp.your-domain.tld/...   # expect: MCP-protocol response
```

For stdio-only deploys, `journalctl` lines mentioning `"listening on stdio"`
plus successful tool calls from your Claude Desktop / Claude Code client are
the verification.

## Upgrading

```sh
scp target/release/google-personal-mcp root@vps:/usr/local/bin/google-personal-mcp.new
ssh root@vps '
  install -m 755 /usr/local/bin/google-personal-mcp.new /usr/local/bin/google-personal-mcp &&
  rm /usr/local/bin/google-personal-mcp.new &&
  systemctl restart google-personal-mcp
'
```

The `install` atomic-replace + `systemctl restart` gives you ~1s of
downtime; existing MCP clients reconnect on their own.

## Uninstalling

```sh
sudo systemctl disable --now google-personal-mcp
sudo rm /etc/systemd/system/google-personal-mcp.service
sudo rm /usr/local/bin/google-personal-mcp
sudo userdel -r google-personal-mcp        # also removes /home/google-personal-mcp
```

Audit logs under `~/.config/google-personal-mcp/audit/*.jsonl` are the only
data that **cannot** be reconstructed — copy them off the VPS first if you
want a record. See [ADR-0011 §Backup and durability](../docs/adr/0011-audit-log.md).

## Service-level objectives (SLOs)

These are the recommended starting points per ADR-0008 §SLOs. Tune thresholds to match your traffic baseline.

| SLI | SLO target | Source metric |
|-----|------------|---------------|
| Tool call latency p95 (cache-warm) | < 500 ms | `histogram_quantile(0.95, rate(gmcp_tool_call_duration_seconds_bucket[5m]))` |
| Token refresh failure rate (24h rolling) | < 0.1% | `rate(gmcp_token_refreshes_total{outcome!="success"}[24h]) / rate(gmcp_token_refreshes_total[24h])` |
| Cache hit rate (steady-state, after 1h warmup) | > 60% | `rate(gmcp_cache_hits_total[1h]) / (rate(gmcp_cache_hits_total[1h]) + rate(gmcp_cache_misses_total[1h]))` |
| Accounts with auth_state != "ok" | < 10% of total | `(gmcp_active_accounts - sum(account_auth_ok)) / gmcp_active_accounts` |
| Hot-reload failure rate (24h) | < 1% | `rate(gmcp_hot_reload_total{outcome!="success"}[24h]) / rate(gmcp_hot_reload_total[24h])` |
| `/healthz` availability | > 99.9% | external probe |
| Memory growth (24h trailing) | < 10% | `process_resident_memory_bytes / process_resident_memory_bytes offset 24h - 1` |

To enable alerting, copy `deploy/alerts.yml` into your Prometheus/Alertmanager rules directory and reload:

```bash
cp deploy/alerts.yml /etc/prometheus/rules/google-personal-mcp.yml
promtool check rules /etc/prometheus/rules/google-personal-mcp.yml
systemctl reload prometheus
```

## Hardening notes

The unit's hardening flags come from ADR-0008 §VPS lines 188-202 and ADR-0017
§Dedicated user. They're zero-cost; if `systemd-analyze security
google-personal-mcp.service` reports a high exposure score, double-check
you haven't dropped `ProtectSystem=strict` or `MemoryDenyWriteExecute=true`.

`Watchdog=` integration is intentionally **not** wired here — that requires
the daemon to call `sd_notify(WATCHDOG=1)` periodically (separate ticket;
not in v0.x scope). The unit's `Restart=always` + `RestartSec=2s` covers the
"daemon crashed, bring it back" case without it.
