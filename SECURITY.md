# Security policy

`google-personal-mcp` holds long-lived OAuth refresh tokens for personal Google accounts and exposes Gmail send/modify capabilities to an AI agent. Treat the deployment accordingly.

## Threat model summary

Three concrete threats shape the design:

1. **Local-filesystem read by another process or user account.** Defense: enforced `0600` / `0700` permissions on the config and token directories at startup, redacted `Debug` for token types, dedicated unprivileged user under systemd. Detail in [ADR-0017](docs/adr/0017-secrets-at-rest.md).
2. **Disk-image exfiltration** (stolen laptop, leaked VPS snapshot). Defense is **the operator's** responsibility — enable full-disk encryption (LUKS, FileVault, provider-level encryption). The daemon does not perform in-app encryption; documented rationale in [ADR-0017](docs/adr/0017-secrets-at-rest.md).
3. **Prompt injection via attacker-controlled email content.** A malicious sender writes instructions into a message body; the host LLM reads them via `get_thread` and may act on them. Defense: untrusted-content wrapping (`_untrusted` JSON suffix + `<<<UNTRUSTED:...>>>` string delimiter) on every attacker-controllable field, standard disclaimer in tool descriptions, no compound read+write tools. Detail in [ADR-0018](docs/adr/0018-email-content-trust.md). **This is not a complete defense** — final responsibility lies with the host LLM and the operator's review discipline.

## What the MCP defends

- Token files are loud-fail at startup if permissions are too wide.
- Tokens, refresh tokens, and client secrets never appear in logs (redacted `Debug`).
- Atomic writes (tmpfile + rename) on token persistence: crashes leave the old token or the new token, never a partial write.
- Destructive tool calls (`send_email`, `batch_archive`, `trash_thread`, `modify_thread_labels`) record full inputs to an audit log (when implemented per [ADR-0011](docs/adr/0011-audit-log.md)) and support `dry_run: true` per [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md).
- Attacker-controllable text returned to the host LLM is marked unambiguously.

## What you (the operator) defend

- **Disk encryption.** Required for any environment where image exfiltration is a concern. Not negotiable for a VPS.
- **Dedicated user.** Run the daemon as a non-interactive user (`User=google-personal-mcp` or `DynamicUser=yes` under systemd, never your interactive login user).
- **Network exposure of HTTP transport.** If you enable HTTP transport ([ADR-0003](docs/adr/0003-transport-stdio-and-streamable-http.md)), front it with TLS + an additional auth layer (mTLS, basic auth, IP allowlist). The MCP daemon itself does not implement transport-layer authentication beyond what the underlying server provides.
- **GCP scope minimization.** Each operator owns their own GCP OAuth client. Enable only the services you use; do not grant `gmail.send` if you only need read access.
- **Host LLM configuration.** If your host LLM supports content trust labels or system-prompt boundaries, configure them to recognize the `<<<UNTRUSTED:...>>>` delimiters as data, not as instructions.
- **Audit-log review.** When the audit log is implemented, the operator is the one who reviews it.

## Running on a company-managed Mac

Running `google-personal-mcp` on a corporate device introduces threats that don't exist on a personal VPS. Read this section before connecting any work-issued Mac to personal Google accounts.

### Threat model additions for company hardware

| Threat | Origin | Mitigation |
|--------|--------|------------|
| EDR scanning `~/.config` | Corporate endpoint-detection agent reading token files | Enable Keychain backend (`[secrets].backend = "keychain"`, build with `--features macos-keychain`) to move tokens out of the filesystem; FileVault reduces exposure window |
| IT-managed Keychain ACLs | MDM policy may reset or audit Keychain ACLs, exposing items to managed apps | Keep the Keychain item access-control locked to a single binary path; review MDM profile entitlements |
| DLP outbound monitoring | Corporate data-loss-prevention proxy reading HTTPS responses | Tokens are OAuth refresh tokens, not email content; they won't trigger content-based DLP rules, but the proxy sees unencrypted HTTP/2 frames post-TLS-intercept if corporate root CA is installed |
| Corporate root CA TLS interception | Corporate CA MITMs TLS; Google API traffic is inspected | Not directly exploitable by `google-personal-mcp`, but means your corporate proxy sees OAuth token-refresh payloads |
| Shared user session | Running daemon as your interactive login user means any app in your user session can read `~/.config/google-personal-mcp/` | See dedicated-user guidance below |

### Pre-flight checklist

Before running on a company Mac, verify each item:

- [ ] **FileVault enabled** — `diskutil apfs listVolumeGroups` should show encryption. Without FileVault, a stolen Mac exposes all of `~/.config` including token files.
- [ ] **Daemon runs as your own user, not root** — stdio transport via Claude Desktop runs as your login user, which is acceptable. Never `sudo google-personal-mcp serve`.
- [ ] **Scope minimized** — if you only need Gmail read access, do not grant `gmail.send` to your OAuth client. Fewer granted scopes = smaller blast radius if a token is exfiltrated.
- [ ] **AUP compliance** — personal Gmail on a company device may violate your employer's Acceptable Use Policy. This is your call, not the daemon's. When in doubt, ask IT.
- [ ] **No sensitive scopes on shared clients** — do not reuse a GCP OAuth client that has `gmail.send` scope across both personal and work accounts. Create a separate GCP project per account class.
- [ ] **Audit log review** — when the audit log is implemented ([#21](https://github.com/torsday/google-personal-mcp/issues/21)), review it periodically for unexpected `send_email` or `batch_archive` calls.

### Recommended configuration for company Mac

1. **Use the Keychain backend** — moves token files out of `~/.config` and into the macOS Keychain, which is encrypted independently of the filesystem and protected by ACLs. Build with `cargo install --features macos-keychain` (or `--features macos-keychain` in the workspace) and set `[secrets].backend = "keychain"` in `config.toml`. On macOS, this is the **default**. Selection: macOS → `keychain`, all other platforms → `file`. If the binary is built without the `macos-keychain` feature but config requests `"keychain"`, the daemon falls back to the file backend with a WARN log.
2. **Enable the read-only profile** (once implemented, [#22](https://github.com/torsday/google-personal-mcp/issues/22)) if you only need to read and search Gmail — removes `gmail.send` and all modify tools from the MCP surface entirely.
3. **Enable the audit log** (once implemented, [#21](https://github.com/torsday/google-personal-mcp/issues/21)) to record destructive tool calls with timestamps and arguments.
4. **Do not grant `gmail.send` on corporate accounts** unless your IT policy explicitly permits outbound email automation from personal tools.

### What this codebase does NOT do on corporate Macs

- It does not bypass or disable corporate EDR agents.
- It does not suppress TLS certificate validation, even if a corporate CA is installed. Token-refresh calls use the system trust store.
- It does not store credentials in plaintext outside of `~/.config/google-personal-mcp/` (filesystem mode) or the Keychain (Keychain mode). No `.env` files, no environment variables.

## Reporting a vulnerability

Report security issues via **[GitHub Security Advisories](https://github.com/torsday/google-personal-mcp/security/advisories/new)**. The repo's "Report a vulnerability" button is the canonical channel.

**Please do not file vulnerabilities as public issues** until they have been triaged and a fix is available.

### What counts

In scope:
- Code paths that could expose `access_token`, `refresh_token`, or `client_secret` to logs, error messages, audit records, or returned tool output.
- Path-traversal or symlink-following in attachment download / token persistence.
- Auth bypass on the HTTP transport (when implemented).
- Failure of the startup permission check to detect a leaky token file.
- A prompt-injection bypass where wrapped-untrusted content reaches the host LLM without the documented markers.

Out of scope:
- Lack of in-app encryption of token files. By design; see [ADR-0017](docs/adr/0017-secrets-at-rest.md).
- "An attacker who already has root on the machine can read tokens." By design; the trust boundary is the OS user.
- Host LLM behavior — including whether a specific LLM honors the untrusted-content markers. Out of scope of this codebase.

## Pre-1.0 disclosure

Before v1.0 the project has no formal disclosure SLA. Reports will be acknowledged and triaged on a best-effort basis. From v1.0 onward, a coordinated-disclosure policy with concrete timelines will be published here.

## Related documents

- [ADR-0017](docs/adr/0017-secrets-at-rest.md) — secrets-at-rest design and rationale
- [ADR-0018](docs/adr/0018-email-content-trust.md) — email content trust model
- [ADR-0011](docs/adr/0011-audit-log.md) — audit-log design (deferred to v1.0)
- [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md) — destructive-tool safety
- [CONTRIBUTING.md](CONTRIBUTING.md) — local-development setup, including dedicated test GCP project
