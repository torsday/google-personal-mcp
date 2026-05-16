# ADR-0013: Cross-account fan-out for read tools (`account = "*"`); destructive tools refuse it

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

[ADR-0002](0002-multi-account-architecture.md) makes accounts first-class via the `account` parameter on every tool. With 10+ accounts and a routine question like "did anyone email me about X this week," the LLM consumer either:

1. Calls `search_threads(query, account="work")`, then `account="personal"`, then `account="acme"`, ... — N round trips, N copies of the query in context, N response blocks for the model to reconcile.
2. Asks the operator which account to search, defeating the multi-account convenience.
3. Just searches the default account and misses results elsewhere.

None of these are good. The MCP can do the fan-out itself: one tool call, parallel queries to N accounts, structured per-account response. This is a UX differentiator that only meaningfully exists *because* of the multi-account-from-day-one design.

But fan-out has sharp edges:

- **Destructive operations must never fan out.** `send_email(account="*")` would mean "send this to N people from N different addresses simultaneously," which is almost never the intended operation and is a catastrophic agent failure mode.
- **Concurrency needs bounds.** Unbounded parallel fan-out across 10+ accounts could exhaust connection pools, exceed quotas across the board, or make a slow account block the entire response.
- **Per-account error isolation matters.** One account in `AuthRequired` state shouldn't fail the whole call.
- **Response shape changes.** Fan-out returns `{account: result}` map; single-account returns a flat result. The shape difference must be obvious to the consumer.

If no decision were made, multi-account becomes a feature the model can't actually use efficiently — a footgun (pick the wrong account, miss the data) and a context burner (multiple separate calls).

## Decision

**v1 scope.** Fan-out is **deferred to v1.0**. For **v0.x**, every tool takes `account: String` (singular, required), per [ADR-0016](0016-tool-surface-and-conventions.md). The design below is the implementation target once fan-out is actually needed. The trigger for promotion to v1.0: the maintainer has manually issued the same read call against 3+ accounts in a single session enough times to identify the *specific* call shapes that warrant fan-out. Building generic fan-out before that signal produces an abstraction that may not fit the eventual specific need.

We will support fan-out on **read tools only**, via two equivalent forms:

1. `account = "*"` — fan out to all enabled accounts in the registry.
2. `accounts = ["alias1", "alias2", ...]` — fan out to a specified subset.

Both forms are mutually exclusive with each other and with the existing `account = "alias"` form. Destructive tools accept neither and reject explicitly with `Error::InvalidArgument`.

### Tool surface

| Tool | Fan-out support | Notes |
| --- | --- | --- |
| `search_threads` | yes | Fan-out is the obvious use case |
| `get_thread` | **no** — single account only | Thread IDs are per-account; cross-account is meaningless |
| `list_labels` | yes | "What labels exist across my accounts" |
| `list_accounts` | n/a — never per-account | |
| `cache_status` | yes | "Per-account cache health" |
| `mcp_status` | n/a — daemon-wide | |
| `archive_thread` / `trash_thread` / `modify_thread_labels` / `batch_archive` / `send_email` | **rejected** | Returns `InvalidArgument: cross-account fan-out not allowed for destructive tools` |
| `download_attachment` | no | Like `get_thread`, attachment IDs are per-account |

### Response shape (fan-out form)

```json
{
  "fanout": true,
  "accounts": [
    {
      "account": "work",
      "outcome": "success",
      "data": { ...same shape as single-account response... }
    },
    {
      "account": "personal",
      "outcome": "success",
      "data": { ... }
    },
    {
      "account": "acme",
      "outcome": "error",
      "error": {
        "kind": "AuthRequired",
        "message": "refresh_token rejected (invalid_grant) — re-authenticate"
      }
    }
  ],
  "summary": {
    "total_accounts": 3,
    "succeeded": 2,
    "failed": 1
  }
}
```

The presence of `fanout: true` is the disambiguating field for the consumer. Single-account responses omit this entirely (existing shape).

### Concurrency

Bounded parallel fan-out via `tokio::task::JoinSet`:

- **Account list snapshotted at call entry.** When the fan-out tool is invoked, it takes an `Arc` snapshot of the current `AccountRegistry` (per [ADR-0002](0002-multi-account-architecture.md) snapshot pattern). The set of accounts the call operates on is fixed for the duration of the call. Hot-reload changes (`auth add` / `auth remove`) during the call do not add or drop accounts mid-flight; they take effect on the next call. This guarantees a deterministic response shape (the consumer always sees the same N accounts in the response that the model intended).
- Default concurrency cap: `min(num_accounts_in_request, [fanout] max_concurrent_accounts)` from config (default 5).
- Per-account rate limiter (per [ADR-0002]) still applies independently — fan-out doesn't exempt anyone.
- Per-account timeout: `[fanout] per_account_timeout_seconds` (default 10s). An account that hangs returns `outcome: "error", error.kind: "Timeout"` — the rest of the fan-out completes.
- Total operation timeout: `[fanout] total_timeout_seconds` (default 30s). If hit, in-flight accounts return `Timeout`; complete accounts return their results.

### Per-account error isolation

A per-account failure produces an `outcome: "error"` entry in the fan-out response, never a top-level error. This keeps `account: "*"` resilient — a stale token on one account doesn't deny answers from healthy ones.

If **all** accounts fail with the same error kind, the response still uses the fan-out shape (the consumer can see "every account is `AuthRequired`," which is a useful diagnostic).

### `account` and `accounts` validation

```rust
fn resolve_target_accounts(
    account: Option<&str>,
    accounts: Option<&[String]>,
    registry: &AccountRegistry,
) -> Result<TargetAccounts, Error> {
    match (account, accounts) {
        (None, None) => Ok(TargetAccounts::Default(registry.default()?)),
        (Some("*"), None) => Ok(TargetAccounts::All(registry.all_aliases())),
        (Some(alias), None) => {
            registry.get(alias)
                .ok_or_else(|| Error::AccountNotFound { account: alias.into() })?;
            Ok(TargetAccounts::One(alias.into()))
        }
        (None, Some(list)) if list.is_empty() => {
            Err(Error::InvalidArgument {
                field: "accounts".into(),
                detail: "must contain at least one alias".into(),
            })
        }
        (None, Some(list)) => {
            // Validate every alias exists
            for alias in list {
                registry.get(alias).ok_or_else(|| Error::AccountNotFound {
                    account: alias.into(),
                })?;
            }
            Ok(TargetAccounts::Many(list.to_vec()))
        }
        (Some(_), Some(_)) => {
            Err(Error::InvalidArgument {
                field: "account/accounts".into(),
                detail: "specify exactly one of `account` or `accounts`".into(),
            })
        }
    }
}
```

### Destructive-tool rejection

For `send_email`, `archive_thread`, `trash_thread`, `modify_thread_labels`, `batch_archive`, the schema accepts `account: Option<String>` only (no `accounts`). At the top of each tool:

```rust
fn reject_fanout_marker(account: Option<&str>) -> Result<(), Error> {
    if account == Some("*") {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "fan-out (`*`) is not allowed for destructive operations; \
                     specify a single account explicitly".into(),
        });
    }
    Ok(())
}
```

Schema-only enforcement (no `accounts` parameter) catches the obvious case at MCP-client tool-validation time. Runtime check on `"*"` catches the edge case where the consumer puts the wildcard in the single-account field.

`send_email` additionally requires `account` to be **explicitly specified** (no default fallback) — rationale per [ADR-0002](0002-multi-account-architecture.md) Risks. The default-account convention applies to read tools.

### Configuration (`[fanout]` in `config.toml`, see [ADR-0006](0006-config.md))

```toml
[fanout]
max_concurrent_accounts = 5            # bounded parallelism
per_account_timeout_seconds = 10
total_timeout_seconds = 30
```

## Options Considered

### Fan-out invocation form

| Option | Pros | Cons |
| --- | --- | --- |
| (a) No fan-out | Simplest schema | N round-trips for N-account questions; defeats multi-account UX |
| **(b) `account="*"` and `accounts=[...]` both supported on read tools** (chosen) | Two natural forms; "all" is one keystroke; subset is explicit; same response shape | Two parameters where one might suffice — small schema cost |
| (c) Only `accounts=[...]` (no wildcard) | Explicit always | "All my accounts" requires the consumer to first call `list_accounts` and reconstruct the list — extra round trip; defeats half the win |
| (d) Only `account="*"` (no subset list) | Simplest | "Search work + personal but not acme" requires N calls — partial fan-out is real |
| (e) Implicit "all" when `account` omitted | One less parameter | Conflicts with the existing default-account semantic; consumers expecting default get fan-out unexpectedly |

We choose (b). The wildcard for "all" is convenient; the explicit list handles the partial-fan-out case.

### Destructive tools

| Option | Pros | Cons |
| --- | --- | --- |
| (f) Allow fan-out on destructive tools | Consistent schema across tools | Catastrophic failure mode (send to N people from N accounts simultaneously); no realistic legitimate use case |
| **(g) Reject fan-out on destructive tools** (chosen) | Safety; respects [ADR-0002] Risks discussion | Inconsistency with read tools — but the inconsistency is the point |
| (h) Allow fan-out, but require explicit `confirm_fanout = true` flag | Explicit consent | Adds a parameter to support an operation with no realistic use case |

We choose (g). The schema asymmetry is a feature, not a bug.

### Response shape

| Option | Pros | Cons |
| --- | --- | --- |
| (i) Same flat shape as single-account; just include the account inline | Schema-uniform | Caller has to detect "is this a list" vs "is this a single result"; harder to distinguish from a single-account result with multiple items |
| **(j) Wrapped fan-out shape with `fanout: true` flag** (chosen) | Disambiguating; allows per-account `outcome` field; structured for partial-failure inspection | Different shape from single-account responses |
| (k) Always use fan-out shape (even single-account) | Uniform response | Breaks existing single-account callers; over-wraps the common case |

We choose (j). The `fanout: true` flag makes the difference obvious; consumers handle the two shapes via a one-line check.

### Concurrency

| Option | Pros | Cons |
| --- | --- | --- |
| (l) Sequential | Simplest; deterministic ordering | Slowest account dictates total response time; defeats half the win |
| **(m) Bounded parallel via `JoinSet`** (chosen) | Fast for typical case; bounded resource use; per-account isolation | Bounded-cap is a tunable that needs reasonable default |
| (n) Unbounded parallel | Maximum speed | 10+ accounts × concurrent connection pools could exhaust per-account or network limits |

## Consequences

**Positive:**

- "Find emails about X across all accounts" is one tool call. Massive UX win for multi-account users.
- Per-account error isolation means partial failures don't deny partial answers — a stale token on one account doesn't break the rest.
- Bounded concurrency prevents pathological resource use without sacrificing typical-case speed.
- Destructive tools' rejection of fan-out is an explicit schema constraint plus runtime check — defense in depth against the agent-confusion failure mode.
- The `fanout: true` response flag makes consumer code trivially branched ("is this a fan-out response or single-account").
- Per-account timeout means a slow account doesn't dominate the response.
- Cache (per [ADR-0009](0009-caching-with-sqlite-and-history-api.md)) is per-account → fan-out hits are independent → typical fan-out cost is N × (small cache hit) instead of N × (full API round trip).

**Negative:**

- Schema gains both `account` and `accounts` fields on read tools. Tool descriptions document the relationship; it's learnable.
- Response payload size scales with `N`. For 10 accounts × `search_threads(max_results=20)`, that's potentially 200 thread summaries. Within reason.
- Per-account rate limiter still throttles even during fan-out — a heavy fan-out can run into rate-limit blocks; surfaces as `outcome: "error", error.kind: "RateLimited"` per account.
- Destructive tools that reject fan-out create an inconsistency with read tools. Documented as deliberate.
- Consumer must handle two response shapes (single-account vs fan-out).

**Risks:**

- *Risk:* Consumer doesn't check `fanout: true` and treats the fan-out shape as a single-account result, producing garbled output.
  *Mitigation:* `fanout: true` is the FIRST field in the response (object-key order in the JSON). Tool descriptions explicitly call out the two shapes. Invalid usage results in obvious downstream parse errors, not silent wrong-data.
- *Risk:* Fan-out total payload exceeds some MCP-protocol or transport limit.
  *Mitigation:* Per-account `max_results` limits scale linearly; total bounded. Streamable HTTP transport supports SSE chunks if responses get truly large (out of scope for v1; the typical fan-out fits in a normal response).
- *Risk:* Account in `AuthRequired` state stays in fan-out responses indefinitely, creating noise.
  *Mitigation:* Operator runs `google-personal-mcp auth refresh <alias>` to fix; meanwhile the per-account error in fan-out responses is the diagnostic signal that something needs attention. `mcp_status` (per [ADR-0014](0014-status-introspection-tool.md)) surfaces stuck accounts proactively.
- *Risk:* `[fanout] max_concurrent_accounts` is too low — slow fan-out for 10+ accounts.
  *Mitigation:* Tunable in config; documented; default is a reasonable starting point for a personal VPS.
- *Risk:* Fan-out hides per-account quota status — heavy fan-out usage burns quotas across the board, the operator doesn't notice until everything fails.
  *Mitigation:* Per-account quota usage is a metric (per [ADR-0008](0008-observability-and-deployment.md) `gmcp_rate_limit_blocks_total{account=...}`), surfaced in `mcp_status`.
- *Risk:* Total timeout cancels in-flight account fetches mid-write to cache (per [ADR-0009]).
  *Mitigation:* Cache writes are atomic; partial fetches are discarded. The slow account simply has no cached entry until next call.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — single-binary architecture that makes in-process fan-out trivial
- [ADR-0002](0002-multi-account-architecture.md) — `account` parameter that this ADR extends; `send_email` requires explicit account (no default)
- [ADR-0005](0005-error-model.md) — `Error::InvalidArgument` for destructive-tool fan-out rejection; per-account `Error::AuthRequired` etc. in fan-out responses
- [ADR-0006](0006-config.md) — `[fanout]` config section
- [ADR-0008](0008-observability-and-deployment.md) — per-account metrics that show fan-out distribution
- [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — per-account cache makes fan-out cheap
- [ADR-0011](0011-audit-log.md) — fan-out logged as N records (one per touched account)
- [ADR-0014](0014-status-introspection-tool.md) — surfaces per-account health for fan-out diagnostics
