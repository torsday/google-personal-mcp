# ADR-0005: Typed `Error` enum at service boundary, mapped at MCP boundary

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

The prototype returns `anyhow::Result<T>` from every service call and converts to `rmcp::ErrorData::internal_error(e.to_string(), None)` at the tool layer. This loses error categorization: a 401 (auth), a 429 (rate limit), a 5xx (server-side outage), a 404 (not found), a parse failure, and a header-injection attempt all look identical to the model and to ops.

For a long-running daemon serving multiple accounts, error categorization is load-bearing because:

- **Auth failures** require user action (re-authenticate). The model should be told distinctly so it can surface the correct recovery instruction. Per [ADR-0004](0004-oauth-token-refresh.md), this is `Error::AuthRequired { account, reason }`.
- **Rate-limited responses** are recoverable with backoff. The HTTP layer should retry transparently with bounded attempts; only surface to the model after exhaustion. Per-account because Gmail's quota is per-user-per-second ([ADR-0002](0002-multi-account-architecture.md)).
- **Transient network failures and 5xx upstream** are recoverable with backoff. Same: retry, only surface after exhaustion.
- **User errors** (invalid query, unknown thread ID, unknown account alias) should be reported clearly to the model and *not* logged at ERROR — those are normal client-side mistakes, not operator-actionable bugs.
- **Bugs** (parse failures, panics caught at the tool boundary, internal invariant violations) are real ERRORs — they want loud logs with full context for ops.
- **Security violations** (header injection in `send_email` — flagged in earlier design discussion) need their own variant so they can be detected, blocked, and audited separately.

The prototype already pulls in `thiserror` 2 in `Cargo.toml` but doesn't use it. The cost of typed errors is approximately one Rust file (~150 lines).

## Decision

We will define a single typed `Error` enum at the service boundary. The HTTP layer maps upstream responses into variants. The tool layer maps the enum into `rmcp::ErrorData` with appropriate MCP error codes. Retry policy lives in the HTTP layer (one place), not at every call site.

### `src/error.rs`

```rust
use std::time::Duration;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// Re-authentication needed. Carries the account so the user knows which.
    #[error("authentication required for account `{account}`: {reason}")]
    AuthRequired { account: String, reason: String },

    /// User specified an account alias that doesn't exist.
    #[error("account `{account}` not found; run `google-mcp auth list` to see available accounts")]
    AccountNotFound { account: String },

    /// Resource not found (thread, message, label, event, etc.).
    #[error("not found: {what}")]
    NotFound { what: String },

    /// Tool received an argument it cannot use.
    #[error("invalid argument `{field}`: {detail}")]
    InvalidArgument { field: String, detail: String },

    /// Header injection attempt detected (e.g. CR/LF in email subject).
    /// This is a security event — log loud, refuse the operation.
    #[error("header injection attempt in field `{field}`")]
    HeaderInjection { field: String },

    /// Upstream rate-limited us. Surfaced only AFTER the HTTP layer's bounded retries are exhausted.
    #[error("rate limited on account `{account}`; retry after {retry_after:?}")]
    RateLimited { account: String, retry_after: Duration },

    /// Upstream returned a non-success status; carries body for diagnosis.
    /// 4xx (other than the specific variants above) and 5xx (after retries) end up here.
    #[error("upstream {service} returned {status}: {message}")]
    Upstream { service: String, status: u16, message: String },

    /// Network-level failure (connection reset, DNS, TLS, timeout).
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    /// Response parsing failed — usually means upstream changed schema, or a bug.
    #[error("parse error in {context}: {source}")]
    Parse { context: String, #[source] source: serde_json::Error },

    /// Local IO failure (token file unreadable, config dir missing, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Catch-all for anything not yet categorized. Treated as a bug to triage.
    #[error("internal error in {context}: {source}")]
    Internal { context: String, #[source] source: anyhow::Error },
}
```

### Mapping at the tool boundary

```rust
fn to_mcp_error(e: Error) -> rmcp::ErrorData {
    match e {
        // Model-actionable: bad input or recoverable user state.
        // MCP `invalid_params` tells the client "you can fix and retry."
        Error::AccountNotFound { .. }
        | Error::NotFound { .. }
        | Error::InvalidArgument { .. } => {
            rmcp::ErrorData::invalid_params(e.to_string(), None)
        }

        // Re-auth is a model-actionable instruction (surface the account in the message
        // so Claude can tell the user "run google-mcp auth refresh work").
        Error::AuthRequired { .. } => {
            rmcp::ErrorData::invalid_params(e.to_string(), None)
        }

        // Security event: refuse, log loud at WARN. Surface as invalid_params so the
        // model knows the operation was rejected (not retried).
        Error::HeaderInjection { ref field } => {
            tracing::warn!(field = %field, "header injection blocked");
            rmcp::ErrorData::invalid_params(e.to_string(), None)
        }

        // Recoverable transient conditions that exhausted retries — internal_error
        // because at this point we don't know how to proceed.
        Error::RateLimited { .. }
        | Error::Network(_)
        | Error::Upstream { .. } => {
            rmcp::ErrorData::internal_error(e.to_string(), None)
        }

        // Bugs and unexpected failures — log full chain at ERROR.
        Error::Parse { .. } | Error::Io(_) | Error::Internal { .. } => {
            tracing::error!(error = ?e, "internal error in tool dispatch");
            rmcp::ErrorData::internal_error(e.to_string(), None)
        }
    }
}
```

### Retry policy (in the HTTP layer)

`http.rs` exposes `authed_request_with_retries(...)` which wraps every Google API call. **Critically, retry behavior depends on whether the request is idempotent.** Retrying a non-idempotent POST after a 5xx is unsafe: the server may have processed the original but lost the response on the wire, so the retry executes a second time. For `messages.send`, that means a duplicate email — a real, silent, production-grade failure mode.

The HTTP layer classifies methods:

- **Idempotent**: `GET`, `HEAD`, `OPTIONS` — safe to retry on transient failures.
- **Non-idempotent**: `POST`, `PUT`, `PATCH`, `DELETE` — retry only on **pre-send** failures (network reset before any byte left the wire) where we know the server never saw the request. Never retry on responses received from the server (any HTTP status), even 5xx, because we cannot prove the server didn't process the request.

| HTTP outcome | Idempotent (GET/HEAD/OPTIONS) | Non-idempotent (POST/PUT/PATCH/DELETE) |
| --- | --- | --- |
| 200–299 | Return success | Return success |
| 401 | Force-refresh token, retry once (per [ADR-0004]). If second attempt is 401 → `Error::AuthRequired` | Same — refresh + single retry is safe because the server explicitly told us it didn't accept the request |
| 403 (`quotaExceeded` body) | Treat as 429-equivalent. Backoff + retry up to 3 times | Same — server rejected the request, no side effect |
| 404 | `Error::NotFound`. No retry (semantic) | `Error::NotFound`. No retry |
| 429 | Honor `Retry-After`, else exponential backoff + jitter. Up to 5 attempts within 30s cap. After exhaustion → `Error::RateLimited` | Same — server rejected before processing |
| Other 4xx | `Error::Upstream`. No retry | `Error::Upstream`. No retry |
| 5xx | Exponential backoff + jitter (100ms base, doubling, cap 5s). Up to 3 attempts. After exhaustion → `Error::Upstream` | **`Error::Upstream` immediately, no retry.** Server may have processed the request; retry could double-execute |
| Connection refused / DNS / TLS handshake failure (pre-byte-send) | Backoff + retry up to 3 attempts. After exhaustion → `Error::Network` | Same — guaranteed the server never saw the request |
| Connection reset / timeout AFTER request sent | Backoff + retry up to 3 attempts | **`Error::Network` immediately, no retry.** Cannot prove the server didn't process the request |

`reqwest::Error::is_connect()` distinguishes pre-send vs post-send failures for the network branch.

For `messages.send` specifically: the in-process send-dedup in [ADR-0012](0012-idempotency-and-dry-run.md) catches user-driven retries (model retries the tool call) within a 60s window. The retry policy here is the daemon-internal complement: don't *automatically* re-execute a non-idempotent call we can't prove failed. The two layers compose to give end-to-end "no double-send."

A future ADR may add caller-supplied idempotency-key support (Stripe-style) to allow opt-in retry of non-idempotent ops. v1 chooses safety over robustness.

**Total operation timeout:** every retry sequence is also bounded by `[retry] max_total_duration_seconds` (default 30s) so a single tool call cannot loop indefinitely under combinations of network failures + 429 backoff.

**Critical rule: capture the response body BEFORE the status check.** Google's error responses contain the actual reason ("Invalid label ID: FOO", "User-rate limit exceeded", "Insufficient Permission") in the body. The prototype throws this away via `error_for_status()` and we lose the diagnostic. Implementation:

```rust
let resp = self.http.get(url).bearer_auth(&token).send().await
    .map_err(Error::Network)?;
let status = resp.status();
let body = resp.text().await.map_err(Error::Network)?;
if !status.is_success() {
    return Err(map_status_to_error(status, body, service, account));
}
serde_json::from_str(&body).map_err(|e| Error::Parse { context: ..., source: e })
```

### Tracing fields

Every error variant logs with structured fields for ops searchability:

```rust
tracing::error!(
    error.kind = "AuthRequired",
    account = %account,
    reason = %reason,
    "auth required"
);
```

Per-variant `kind` field allows queries like "show me all RateLimited errors in the last hour grouped by account" without log-string parsing. Detail in [ADR-0008](0008-observability-and-deployment.md).

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Keep `anyhow::Error` everywhere | Trivial; no new code | Loses categorization; every error is `internal_error` to MCP; no actionable signal to the model; no structured logging |
| (b) Per-service error enums (`gmail::Error`, `calendar::Error`, ...) | Each service can have specific variants | Duplication of common variants (Network, Parse, RateLimited); requires `From` impls between enums; tools must handle N error types |
| **(c) Single typed `Error` at the service boundary, mapped at the tool boundary** (chosen) | Single source of truth for variant shapes; one mapping function at the tool boundary; retry policy in one place | Some service-specific failures get squashed into generic variants (e.g. `Upstream { service: "gmail", status: 400, message: "..." }`); tradeoff: less granular than per-service, more maintainable |
| (d) Single error type, but using `Box<dyn std::error::Error + Send + Sync>` for all sources | Maximum flexibility | No `match`-able variants — back to the same problem as (a); model still gets generic errors |
| (e) Use `eyre` / `color-eyre` instead of `thiserror` | Better human-readable error chains | Same problem as anyhow — runtime types not match-able; harder to map to MCP error codes |

We choose (c). The per-service granularity argument for (b) is real but doesn't outweigh the cost of duplication and inter-enum conversion. The single enum is the same pattern used in well-built Rust HTTP clients (`reqwest::Error`, `octocrab::Error`, etc.).

## Consequences

**Positive:**

- Every failure has a known shape. The model can react to `AuthRequired` distinctly from `RateLimited` distinctly from `NotFound`.
- Retry policy lives in one place (`http.rs`) and is consistent across all services. Adding Calendar later doesn't re-derive retry logic.
- `error_for_status()`-style discarded-body bugs become impossible — the explicit "capture body before status check" pattern is the only way to construct the error.
- Structured tracing fields (`error.kind = "..."`) enable real ops queries instead of log-string grepping.
- The `HeaderInjection` variant codifies the security boundary — there's a typed thing to look for, not just a string check buried in `send_email`.
- `thiserror`'s `#[from]` impl for `std::io::Error` and `#[source]` for chained errors give correct error-chain printing for free.

**Negative:**

- Adding a new error variant requires updating the tool-boundary mapping function. Forgotten updates become "all errors are `internal_error`" — caught by lints if we add `#[deny(unreachable_patterns)]`-style checks (no direct equivalent for `match` exhaustiveness on a non-exhaustive enum, but compile errors fire if a variant is added without a match arm).
- The retry-then-error flow means a single tool call can take 30+ seconds in worst case (5 retries on 429 with backoff). Acceptable for a daemon, but tool callers should be told the call may be slow.
- `Error::Internal { context, source: anyhow::Error }` is an escape hatch for paths we haven't fully categorized. Code review should challenge each new use ("why isn't this a typed variant?").
- The catch-rules per status code (401 retries once; 5xx retries with backoff; 429 honors Retry-After) are spelled out in this ADR but live in code. Drift between ADR and code is a real risk; mitigated by referencing this ADR in the retry-policy module's doc-comments.

**Risks:**

- *Risk:* The "capture body before status check" rule isn't enforced by the type system — a future PR could regress to `error_for_status()`.
  *Mitigation:* (a) The HTTP wrapper (`authed_request_with_retries`) is the only path Google API calls take, so it's the only place the rule needs to hold. (b) Lint check via `clippy::disallowed_methods` configured to warn on `error_for_status` direct use.
- *Risk:* `Error::Upstream { message: String }` carries the raw response body. If Google ever returns a body containing user data, that data ends up in logs. For Gmail this is not a typical concern (errors don't contain message bodies), but worth noting.
  *Mitigation:* Truncate Upstream `message` to first 4KB on construction; document the truncation.
- *Risk:* The tool-boundary mapping is non-exhaustive in spirit (the catch-all `Internal` exists). Future variants might end up in `internal_error` MCP responses when they should be `invalid_params`.
  *Mitigation:* Code review discipline; the mapping function is small enough to review wholesale every time it changes.
- *Risk:* The retry policy intersects with rate limiting (also per-account, [ADR-0002]). A misconfigured retry could exceed the rate limiter's budget.
  *Mitigation:* The rate limiter is upstream of the retry logic — retries acquire from the per-account semaphore each time. Retries thus respect the rate limit naturally.
- *Risk:* `Error::Internal { source: anyhow::Error }` accepts anything via `anyhow`; the type erasure means we can't `match` on the underlying cause.
  *Mitigation:* `Internal` is intentionally an escape hatch for "we know this is a bug, just record it." Anything we want to react to programmatically gets its own variant.

## References

- [ADR-0002](0002-multi-account-architecture.md) — `AuthRequired { account }` and `AccountNotFound` carry the alias context defined here
- [ADR-0003](0003-transport-stdio-and-streamable-http.md) — both transports map errors at the tool boundary identically (one mapping function, two transports)
- [ADR-0004](0004-oauth-token-refresh.md) — refresh failure paths (`invalid_grant` → `AuthRequired`; transient → retry)
- [ADR-0008](0008-observability-and-deployment.md) — structured tracing fields per error variant
- `thiserror` 2 — the error-derive crate already in `Cargo.toml`
- Google API [error response format](https://developers.google.com/workspace/gmail/api/guides/handle-errors) — body shape that the `Upstream` variant captures
