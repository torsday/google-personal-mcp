# ADR-0011: Append-only local audit log of every tool invocation

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

This MCP exposes destructive operations (`send_email`, `trash_thread`, `archive_thread`, `batch_archive`, `modify_thread_labels`) to an LLM agent that may run unsupervised. The user-facing question "what did the agent do this morning" needs a definitive answer that doesn't depend on the agent's own honesty.

MCP client-side logs (Claude Desktop, etc.) are not adequate because:

- They show the model's **claims** about what it did, not what actually executed.
- The agent may summarize or omit operations when reporting back.
- Logs are scattered across the client's UI, the daemon's stderr, and (potentially) journald — no single source of truth.
- Stderr / journald logs are intentionally noisy for debugging; the operations of record are buried.

For an operator's trust model — "I let the agent run; I want a daily review of what it touched" — there should be an authoritative, append-only, tamper-resistant record on the operator's local disk, written by the daemon itself.

This is also a compliance/recovery primitive: if `send_email` was called incorrectly, the audit log tells the operator exactly which message ID was sent at what time, which they can then look up in Gmail.

If no decision were made, post-incident reconstruction depends on Gmail's own activity log (which doesn't capture intent or parameters in the form the daemon called them) and Claude Desktop's transcripts (lossy, agent-mediated). Neither is sufficient.

## Decision

The daemon writes an **append-only JSON Lines audit log** of every tool invocation, at `~/.config/google-personal-mcp/audit/<YYYY-MM>.log`. One line per call. Sensitive fields are redacted by default; the operator can opt into verbose mode for personal-use single-user installs.

### Record shape

```json
{
  "ts": "2026-04-25T14:32:11.123Z",
  "session_id": "8f4c1a",
  "transport": "http",
  "account": "work",
  "tool": "send_email",
  "params_hash": "sha256:5a3c...",
  "outcome": "success",
  "duration_ms": 234,
  "error_kind": null,
  "extra": {
    "to_domain": "@example.com",
    "subject_len": 47,
    "body_len": 1240,
    "in_reply_to_present": true,
    "thread_id": "1928cba93f0a4a"
  }
}
```

Every call gets a record regardless of outcome. Failures populate `error_kind` (matches the `Error` enum variant name from [ADR-0005](0005-error-model.md)).

### Redaction rules per tool

The `extra` object is hand-curated per tool to capture useful context without leaking content. The default redaction rules:

| Tool | Redacted (default) | Operator-opt-in (verbose) |
| --- | --- | --- |
| `send_email` | `to` → domain only; `subject` → length only; `body` → length only; `cc` → count only | full `to`, `subject`, first 200 chars of body, full `cc` |
| `search_threads` | `query` → length + token count only | full `query` |
| `get_thread` | `thread_id` (it's an ID, not content) | unchanged |
| `archive_thread`, `trash_thread` | `thread_id`, `dry_run` | unchanged |
| `batch_archive` | `thread_ids` → count, first/last only | full list |
| `modify_thread_labels` | `thread_id`, `added`, `removed` (label names are not content) | unchanged |
| `list_labels`, `list_accounts`, `mcp_status` | (none — these are read-only meta) | unchanged |
| `download_attachment` | `attachment_id`, `mime_type`, `size_bytes`, `save_to` | unchanged (path could be sensitive but operator chose it) |

The verbose mode exists because for a single-operator personal install, the operator IS the user — there's no privacy benefit to redacting their own email content from their own audit log. Verbose mode is opt-in to avoid the default footgun.

### Configuration (extends `[audit]` in `config.toml`, see [ADR-0006](0006-config.md))

```toml
[audit]
enabled = true                                         # default on; can disable for testing
dir = "~/.config/google-personal-mcp/audit"
rotate = "monthly"                                     # "monthly" | "weekly" | "daily" | "size:<bytes>"
verbose = false                                        # default redacted; true for full content
fsync = "per_record"                                   # "per_record" | "batched" | "off"
include_session_id = true                              # for HTTP-mode session correlation
include_status_calls = false                           # mcp_status / cache_status are noisy; default suppress
```

`fsync = "per_record"` means every audit line is durably on disk before the tool returns. Slow but correct. `"batched"` flushes every 1s. `"off"` relies on the OS page cache (lost on power-off).

For destructive tools, the audit record is **always** `fsync`'d before the tool's API call to Gmail starts. This is critical: it means even if the daemon crashes during the API call, the audit log shows the intent, and the operator can reconcile with Gmail manually.

The list of destructive tools is **not** maintained inline in this ADR (or [ADR-0012](0012-idempotency-and-dry-run.md) — the dry-run ADR — or [ADR-0013](0013-cross-account-fan-out.md) — the fan-out-rejection ADR). Instead, **destructiveness is a property of each tool, exposed via the tool registry**:

```rust
pub trait ToolMetadata {
    fn name(&self) -> &'static str;
    fn is_destructive(&self) -> bool;     // single source of truth
    fn requires_explicit_account(&self) -> bool;  // implies is_destructive() for our cases
    // ...
}
```

The audit module reads `is_destructive()` to decide pre-fsync. The dry-run requirement reads it to validate that destructive tools accept the `dry_run` parameter. The fan-out validator reads it to reject `account = "*"`. **One source of truth, no drift.** Adding a new destructive tool requires implementing the trait — the audit and safety policies attach automatically.

For read-only tools, the configured `fsync` policy applies — `batched` is fine because losing a few read-audit records on power-off doesn't matter.

### File layout and rotation

- Mode 600 enforced at create.
- Append-only convention: the daemon only opens the current rotation period's file with `O_APPEND`. There's no API for truncation or rewriting.
- No automatic deletion ever. Operator manages retention with `find -mtime +N -delete` or `logrotate`.

**Rotation strategy** is configurable via `[audit] rotate` in `config.toml` (per [ADR-0006](0006-config.md)):

| Value | Filename pattern | Use case |
| --- | --- | --- |
| `"monthly"` (default) | `audit-2026-04.log` | Personal use; a few hundred records/month is small |
| `"weekly"` | `audit-2026-W17.log` | Heavy automation; ~thousands of records/week |
| `"daily"` | `audit-2026-04-25.log` | High-volume agent installations |
| `"size:<bytes>"` | `audit-<seq>.log` (sequential) | Bound disk pressure regardless of volume — e.g. `"size:10485760"` for 10 MiB files |

Rotation happens at first write of the new period (lazy). The daemon caches the open file descriptor for the active period; period transitions reopen.

For high-volume installations, `size:` rotation prevents single-file bloat without requiring time-based prediction of volume.

### Backup and durability

The audit log is the only data on the daemon's disk that **cannot be reconstructed**:

- `tokens/*.json` can be regenerated by re-running `auth add` (manual step but possible).
- `accounts.toml` can be reconstructed from the operator's memory + listed Google account permissions.
- `cache/*.db` is fully derivative — drop and rebuild from Gmail any time.
- **`audit/*.log` is the only "if you lose it, it's gone forever" data.**

Recommended operator practice (documented in `deploy/INSTALL.md` and `mcp_status` audit field):

1. Back up `~/.config/google-personal-mcp/audit/` to a separate disk / cloud / git-crypt repo with cadence matching the operator's compliance needs (most personal-use cases: weekly).
2. Encrypt at rest if the host disk isn't already encrypted (`age` or `gpg` over the rotated log files works fine; current month's open file is harder — do not encrypt the live log).
3. The audit log is sensitive in two distinct ways: **operational pattern** (frequency / timing of agent activity is itself information) and, in `verbose = true` mode, **content** (subjects, recipients, query text). Verbose-mode logs warrant the same protection as the cache DB.

The daemon never automatically backs up or encrypts the audit log — out of scope. Operator owns the backup strategy.

### What is NOT in this audit log

- Tool *responses* (the data we returned to the model). That's the whole inbox; we'd duplicate Gmail.
- Anything from non-tool paths (background sync per [ADR-0009](0009-caching-with-sqlite-and-history-api.md), refresh per [ADR-0004](0004-oauth-token-refresh.md), startup/shutdown). Those go to the regular tracing log.
- Internal-status calls (`mcp_status`, `cache_status`) at default level — too noisy. Configurable to include.

The audit log is **what the agent did to your data on your behalf**, nothing more.

### Tool-side surface

A query tool that lets the operator (or the model) ask audit questions:

- `audit_summary(since: timestamp?, account: string?, tool: string?)` returns:
  - Per-tool call counts
  - Per-account call counts
  - Failure rate
  - Time of first / last call in window
  - Recent destructive ops (with `dry_run=false`) — last 5 with timestamp + tool + account, but *not* the full `extra` content

The tool deliberately does **not** return raw audit lines. The reason: if the model can selectively quote audit records back to the operator, the model controls the framing of "what did I do." The full audit content is for the human's eyes, read directly from disk (`tail`, `jq`, `less`).

Verbose-mode contents are similarly not returned by `audit_summary` — those are operator-only.

### Tamper resistance

We are not pretending this is cryptographically signed (no operator key management story). What we provide:

- File mode 600, owner-only.
- Append-only opens (`O_APPEND`); no daemon code path opens the file for write+seek or truncate.
- Each record includes `params_hash` (SHA-256 of the input parameters). A operator suspecting tampering can compare records' time-ordering against their `params_hash` continuity, though we don't currently chain hashes (no proof of insertion-order consistency).

A future ADR can add hash-chained records for cryptographic ordering proof, if the threat model warrants.

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) No audit log (status quo) | Simplest | No record of agent activity; trust model relies on agent honesty |
| (b) Use `tracing` logs with `audit` target | Reuses existing infrastructure | Mixed with debug logs; subject to `RUST_LOG` filter; not a stable contract; tracing logs are designed to be sometimes-dropped under load |
| **(c) Dedicated append-only JSONL file with redaction policy** (chosen) | Stable contract; auditable shape; survives logging-config changes; structured for `jq`/programmatic analysis | New file to manage; operator has to know about it |
| (d) SQLite audit DB | Queryable via SQL | Append-only is harder to enforce; same data, more complexity; JSONL with `jq` covers our query needs |
| (e) Send to a remote sink (S3, syslog server) | Off-machine, harder to tamper | Requires infra; complicates personal-VPS deploy; introduces network dependency on the audit path |
| (f) Hash-chained signed records | True tamper proof | Key management story for one-operator system isn't worth it; the threat is "agent lies about what it did," not "operator's local files are compromised" |

We choose (c). The simplicity and operator-readability dominate; (f) can be layered later if needed.

### Redaction default

| Option | Pros | Cons |
| --- | --- | --- |
| (g) Always full content | Maximum useful detail | Audit log itself becomes a sensitive data store as large as a small inbox; tooling questions about who can read it |
| **(h) Redacted by default, verbose opt-in** (chosen) | Safe default; operator who genuinely wants full content can flip the flag; verbose mode has the same security boundary as the SQLite cache | Default is less useful for some debugging; toggle requires restart |
| (i) Per-tool flags | Maximum flexibility | Combinatorial config surface; nobody will use it |

We choose (h). The verbose toggle is a one-line config change for the personal-use case where the operator IS the user.

## Consequences

**Positive:**

- Authoritative record of every operation, written by the daemon, not the agent.
- Append-only file mode prevents accidental erasure by the daemon's own code.
- `fsync = per_record` for destructive ops means the intent-to-act is durable before the action executes; survives daemon crashes.
- JSONL format trivially queryable with `jq` (`jq 'select(.tool == "send_email") | .ts + " " + (.extra.to_domain // "")' 2026-04.log`).
- Per-tool redaction policy means default-safe behavior; verbose opt-in means single-operator installs aren't crippled.
- `audit_summary` tool gives the model a controlled way to ask "what have I done recently" without the model having raw-line access (which would let it edit its own narrative).
- Trust differentiator: most MCP servers don't have this. Operators willing to let an agent run unsupervised will value it explicitly.

**Negative:**

- One more file to manage; one more config section to learn.
- `fsync = per_record` on destructive ops adds ~1ms per call (acceptable).
- Verbose mode duplicates significant content already in Gmail itself (subject, recipients) — wasteful for storage but the operator chose it.
- File-size growth is technically unbounded; documented operator responsibility.
- The audit log is itself sensitive data — if the disk is compromised, the audit log discloses operational patterns. Same disk holds tokens and the cache, so this isn't a new exposure.

**Risks:**

- *Risk:* Audit write failures (disk full, permission error) silently break the destructive-op pre-write durability.
  *Mitigation:* Audit-write errors are surfaced as `Error::Internal { context: "audit", ... }` (per [ADR-0005]) and the destructive op is **refused** with that error. We fail closed — better to refuse the operation than to perform it without an audit record. The operator gets a clear message in the response; daemon log records it at ERROR.
- *Risk:* The redaction rules leak more than intended (e.g., subject length combined with timestamp pattern is identifying).
  *Mitigation:* Redaction is heuristic, not a privacy guarantee. Documented as such. Operator's threat model determines whether default is enough.
- *Risk:* Operator forgets to rotate/prune; disk fills.
  *Mitigation:* `mcp_status` ([ADR-0014](0014-status-introspection-tool.md)) reports current month's audit log size. README documents `find -mtime +N -delete` pattern.
- *Risk:* The model uses `audit_summary` to construct misleading narratives ("I sent 3 emails" when it sent 5 by including different filters).
  *Mitigation:* Counts and per-tool breakdowns are factual. The summary tool does not return per-record content. Operators relying on audit should read the file directly.
- *Risk:* Concurrent writes from multiple MCP sessions (HTTP transport) interleave records.
  *Mitigation:* `O_APPEND` writes are atomic up to PIPE_BUF (~4 KB on Linux); audit records are well under that. Records remain whole; ordering across sessions is by completion time — documented.
- *Risk:* Audit log accidentally readable by other users of the host.
  *Mitigation:* Mode 600 enforced at create; verified at startup (refuse to open if other-readable; instruct operator to fix).

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — single-binary, single-operator scope where this audit model fits
- [ADR-0005](0005-error-model.md) — `Error::Internal` raised on audit write failures
- [ADR-0006](0006-config.md) — `[audit]` config section
- [ADR-0008](0008-observability-and-deployment.md) — distinguishes audit (this) from tracing (debug logs); audit log is NOT subject to `RUST_LOG`
- [ADR-0012](0012-idempotency-and-dry-run.md) — `dry_run` flag on destructive ops; audit records `dry_run` outcome distinctly
- [ADR-0014](0014-status-introspection-tool.md) — `mcp_status` reports audit log size
- POSIX `O_APPEND` semantics — atomic small writes that this model relies on
