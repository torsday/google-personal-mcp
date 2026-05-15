# ADR-0014: `mcp_status` introspection tool — operational visibility from inside the MCP

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

Operational visibility for `google-personal-mcp` is split across three places after [ADR-0008](0008-observability-and-deployment.md):

- **Logs** — stderr / journald — debugging detail, not summary.
- **Metrics** — Prometheus on `127.0.0.1:9100/metrics` — numerical, requires Prometheus or `curl | grep`.
- **Audit log** — JSONL files — historical, requires shell tools.

None of these are accessible **from inside an MCP session** without leaving Claude Desktop / your CLI / the agent context. The natural questions an operator (or a watching agent) asks while interacting with the MCP — "is everything OK?", "are all my accounts authenticated?", "what's the cache hit rate?", "did the last reload succeed?" — currently require switching context to a shell.

For a single tool that surfaces this without context-switching, the operator gets a fast trust check; the model gets the data it needs to make smart routing decisions ("don't fan out to `acme` — it's in `AuthRequired` state").

Most MCP servers have nothing like this. It's a small ADR for an outsized differentiator: turning the MCP into something self-aware enough to report its own health to its consumer.

If no decision were made, every "is this thing healthy" question requires shell access to the host running the daemon — which defeats the point of an MCP that the operator interacts with via their LLM client.

## Decision

We will add a single tool `mcp_status` that returns a structured snapshot of the daemon's state. Read-only, fast (in-memory state mostly), no PII beyond what the operator already sees in `list_accounts`.

### Tool signature

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct McpStatusParams {
    #[schemars(description = "Include per-account detail (default: true)")]
    #[serde(default = "default_true")]
    pub include_accounts: bool,

    #[schemars(description = "Include cache statistics (default: true)")]
    #[serde(default = "default_true")]
    pub include_cache: bool,

    #[schemars(description = "Include error counters from the last hour (default: true)")]
    #[serde(default = "default_true")]
    pub include_recent_errors: bool,
}
```

All fields default to true; the parameters exist so a noisy "give me everything always" can be trimmed by a caller that only wants a specific slice.

### Response shape

```json
{
  "schema_version": 1,
  "version": "0.1.0",
  "git_sha": "8f4c1a2",
  "rust_version": "1.95.0",
  "build_profile": "release",
  "uptime_seconds": 86_400,
  "transport": "http",
  "transport_detail": {
    "bind_addr": "127.0.0.1:8765",
    "active_sessions": 1,
    "session_idle_timeout_seconds": 3600
  },
  "accounts": [
    {
      "alias": "work",
      "email": "you@workplace.com",
      "default": true,
      "auth_state": "ok",
      "last_refresh_at": "2026-04-25T13:32:11.123Z",
      "expires_at": "2026-04-25T15:02:11.123Z",
      "expires_in_seconds": 3540,
      "scopes_granted": ["https://www.googleapis.com/auth/gmail.modify",
                         "https://www.googleapis.com/auth/gmail.send"]
    },
    {
      "alias": "acme",
      "email": "you@acme.example",
      "default": false,
      "auth_state": "auth_required",
      "auth_state_detail": "refresh_token rejected (invalid_grant) — re-authenticate",
      "last_refresh_at": "2026-04-25T11:08:44.321Z",
      "expires_at": "2026-04-25T12:38:44.321Z",
      "expires_in_seconds": -7000,
      "scopes_granted": [...]
    }
  ],
  "services": [
    {"name": "gmail", "enabled": true, "tools_count": 8}
  ],
  "cache": {
    "enabled": true,
    "total_size_bytes": 47_182_336,
    "max_size_bytes_per_account": 524_288_000,
    "hit_rate_last_hour": 0.83,
    "hits_last_hour": 412,
    "misses_last_hour": 84,
    "by_account": [
      {"account": "work", "size_bytes": 32_104_211, "messages_cached": 1842,
       "last_history_id": 9_487_211, "last_sync_at": "2026-04-25T14:31:48.123Z"},
      {"account": "personal", "size_bytes": 11_948_103, "messages_cached": 671,
       "last_history_id": 4_122_889, "last_sync_at": "2026-04-25T14:31:48.123Z"},
      {"account": "acme", "size_bytes": 3_130_022, "messages_cached": 188,
       "last_history_id": 887_412,
       "last_sync_at": "2026-04-25T11:08:44.321Z",
       "stale": true}
    ]
  },
  "errors_last_hour": {
    "total": 4,
    "by_kind": {
      "RateLimited": 2,
      "Network": 1,
      "AuthRequired": 1
    },
    "by_account": {
      "work": 0,
      "personal": 1,
      "acme": 3
    },
    "by_tool": {
      "search_threads": 1,
      "send_email": 0,
      "get_thread": 3
    }
  },
  "rate_limits": {
    "requests_last_minute": {"work": 12, "personal": 4, "acme": 0},
    "blocks_last_hour": {"work": 0, "personal": 0, "acme": 0}
  },
  "audit": {
    "enabled": true,
    "current_log_file": "~/.config/google-personal-mcp/audit/2026-04.log",
    "current_log_size_bytes": 89_412
  },
  "last_hot_reload": {
    "at": "2026-04-25T09:14:22.000Z",
    "outcome": "success",
    "added_accounts": ["acme"],
    "removed_accounts": []
  },
  "tool_invocations_last_hour": {
    "total": 312,
    "by_tool": {
      "search_threads": 187,
      "get_thread": 91,
      "modify_thread_labels": 18,
      "send_email": 4,
      "mcp_status": 8,
      "list_labels": 4
    }
  }
}
```

### Field semantics

- **`schema_version`** — integer, currently `1`. Bumped on any breaking change to this response shape (field rename, removal, type change). Additive changes (new fields) do not bump the version. Consumers should accept any unknown additional fields and check `schema_version` only when they hit a parse error.
- **`auth_state`** — `"ok"` | `"refreshing"` | `"auth_required"` | `"unknown"`. The model can branch on this when fan-out is requested (skip `acme` if it's in `auth_required`, or warn the user).
- **`expires_in_seconds`** — negative if expired (e.g., `acme` above is 7000s past expiry; `auth_state = auth_required` because refresh fails).
- **`stale: true`** on a cache account indicates `last_sync_at` is older than `2 × background_sync_interval`; suggests the account is stuck on history sync.
- **`hit_rate_last_hour`** — `hits / (hits + misses)`; `null` if neither (no cache activity).
- **`tool_invocations_last_hour.by_tool`** — only includes tools actually invoked in the window.

### What's NOT in the response

- Email content of any kind.
- Token values, refresh tokens, client secrets — never.
- Audit log entries (those are operator-direct-read).
- Full Gmail label inventory (use `list_labels` for that).
- Per-account quota cost projection (Gmail's per-method quota costs are approximate; we surface raw request count, not calculated quota units).
- Performance histograms (those go to Prometheus; this is a snapshot, not a time series).

### Implementation notes

- All fields are computed from in-memory state plus a single SQLite query per account for cache stats. Fast (~10ms even with many accounts).
- Counters (`tool_invocations_last_hour`, `errors_last_hour`, `rate_limit.blocks_last_hour`) are sliding-window maintained alongside the existing `metrics` crate counters. The `mcp_status` reads from internal counters (separate from Prometheus exposition; deliberate — Prometheus needs cumulative, we want windowed).
- `last_hot_reload` is captured by the file-watcher subsystem (per [ADR-0002](0002-multi-account-architecture.md)) into a single in-memory `Option<HotReloadEvent>`.

### Audit

Calls to `mcp_status` are recorded in the audit log per [ADR-0011](0011-audit-log.md). At default audit verbosity these are noisy; the audit config supports `[audit] include_status_calls = false` (default `false`) to suppress them — they're the most-frequent tool call in any agent that uses status, and they're entirely safe.

## Options Considered

### Existence and shape

| Option | Pros | Cons |
| --- | --- | --- |
| (a) No status tool | Simplest | Operational state requires shell / Prometheus access — defeats interactive use |
| **(b) Single `mcp_status` tool returning a complete snapshot** (chosen) | One call gets the whole picture; the model can branch on it; structured for jq/programmatic post-processing | Response is verbose; some callers don't need everything (mitigated by the `include_*` parameters) |
| (c) Multiple specialized tools (`status_accounts`, `status_cache`, `status_errors`) | Smaller per-call payload | Tool-surface explosion; the typical use case is "show me everything" so multiple round trips are pure overhead |
| (d) Status only via metrics endpoint (no MCP tool) | No new tool | Requires `curl 127.0.0.1:9100/metrics`; not accessible to MCP-only consumers |
| (e) Pretty-printed text response (single `Content::text(...)`) | Reads nicely in Claude Desktop | Loses structure; consumers can't programmatically branch; harder to test |

### What gets included

| Option | Pros | Cons |
| --- | --- | --- |
| (f) Always full payload | Simplest | Verbose; some callers want a slice |
| **(g) `include_*` boolean parameters per section** (chosen) | Structured slicing; defaults are everything | Three params; small additional schema |
| (h) Selector strings (`include = ["accounts", "cache"]`) | Single param | Less self-documenting in tool description |

### Audit treatment

| Option | Pros | Cons |
| --- | --- | --- |
| (i) Full audit on every `mcp_status` call | Complete trail | Noisy if model uses status as a routine check |
| **(j) Configurable suppression with `include_status_calls = false` default** (chosen) | Quiet by default; can enable for debugging "what is the agent checking" | Slight default-behavior asymmetry vs other tools |

### Computation of windowed counters

| Option | Pros | Cons |
| --- | --- | --- |
| (k) Compute from Prometheus histograms at query time | Reuses existing instrumentation | Requires Prometheus exposition format; awkward for mid-process query; Prometheus is cumulative-since-start |
| **(l) Maintain separate sliding-window counters in-process** (chosen) | Native windowing; cheap; matches what `mcp_status` actually wants | Slight duplication of counter state with Prometheus exposition |

## Consequences

**Positive:**

- One tool call, one response, full operational picture. Operator can ask the model "how's the daemon doing" without leaving the chat.
- The model can use the response programmatically — skip stale accounts in fan-out, warn the user when `auth_required` is present, decide caching strategy based on hit rate.
- Cheap to implement; cheap to call (~10ms typical).
- Audit suppression means model status-checks don't bloat the audit log.
- `last_hot_reload` confirms the file-watcher subsystem (per [ADR-0002]) is alive and processed the last config change.
- The structured shape is consumable by other tools — could be the input to a future Grafana panel or an external monitoring agent.
- Differentiator. No competing MCP server has this kind of self-introspection.

**Negative:**

- Response size for many accounts grows linearly. Mitigated by `include_accounts = false`.
- Counter state duplicates some metrics-crate counters. Acceptable — different consumers, different semantics.
- `auth_state_detail` text varies slightly across Google API behavior changes; documented as informational.
- Operator could call `mcp_status` excessively from outside the MCP (curl-loop or whatever) — but it's an MCP tool, not a public endpoint; not realistic.
- Adds one more tool to the registry — the tool list grows by one.

**Risks:**

- *Risk:* `expires_in_seconds` slightly drifts during the call's execution.
  *Mitigation:* Computed at response build; documented as "snapshot at response time."
- *Risk:* Cache stats query temporarily holds a SQLite read lock; long-running fan-out could be slowed.
  *Mitigation:* Each cache stats query is one fast `SELECT COUNT(*)` + size-of-file; sub-millisecond. WAL mode means no blocking of writes.
- *Risk:* Response leaks information useful for an attacker who has gained an MCP session (e.g., listing all accounts shows attack surface).
  *Mitigation:* MCP session = the operator's own agent; if compromised, attacker already has full data access. `mcp_status` doesn't add disclosure surface beyond what `list_accounts` already provides.
- *Risk:* Counter drift between `mcp_status` (sliding window) and Prometheus metrics (cumulative).
  *Mitigation:* Documented as different views; both are correct for their purpose.
- *Risk:* Tool description bloat — `mcp_status` is a complex shape and its response schema needs explanation.
  *Mitigation:* Tool description points at this ADR (URL or doc anchor) for the full schema; tool description itself stays at "returns daemon state — accounts, cache, errors, rate limits."

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — single-process daemon model; status is global to that process
- [ADR-0002](0002-multi-account-architecture.md) — per-account state surfaced in `accounts` field; `last_hot_reload` reflects the file-watcher subsystem
- [ADR-0004](0004-oauth-token-refresh.md) — `auth_state` and `expires_at` come from `TokenManager`
- [ADR-0005](0005-error-model.md) — `errors_last_hour.by_kind` keys are the `Error` variant names
- [ADR-0008](0008-observability-and-deployment.md) — Prometheus metrics are the cumulative source of truth; `mcp_status` is the windowed snapshot view
- [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — `cache` field surfaces SQLite cache health
- [ADR-0011](0011-audit-log.md) — `audit` field reports current log file size; `mcp_status` calls audited subject to `include_status_calls`
- [ADR-0013](0013-cross-account-fan-out.md) — model uses `mcp_status` to filter unhealthy accounts before fan-out
