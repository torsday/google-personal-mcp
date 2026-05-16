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
