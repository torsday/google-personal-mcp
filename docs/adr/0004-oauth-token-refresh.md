# ADR-0004: OAuth token refresh — proactive expiry check + lazy 401 fallback, per-account

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

Google OAuth access tokens expire approximately one hour after issue. The `google-mcp` daemon is designed to run forever (per [ADR-0001](0001-monolithic-google-mcp-architecture.md)), serving 10+ accounts (per [ADR-0002](0002-multi-account-architecture.md)). Tokens for every account must be refreshed continuously without operator intervention. The prototype's current behavior — load token at startup, use forever, fail when expired — is unusable beyond the first hour.

Google's OAuth returns a `refresh_token` (alongside the `access_token`) on initial authorization with `access_type=offline`. The refresh token is long-lived and can be exchanged for a new access token via a `grant_type=refresh_token` call to Google's token endpoint. Google occasionally rotates the refresh token (returns a new one in a refresh response); we must handle this.

Constraints from prior ADRs:

- Per-account token storage at `~/.config/google-mcp/tokens/<alias>.json` ([ADR-0002])
- Hot-reload safety: in-flight tool calls take an `Arc` snapshot of the TokenManager; new accounts can appear, removed accounts are dropped, but live calls must continue ([ADR-0002])
- Both stdio and Streamable HTTP transports use the same TokenManager — concurrent calls from multiple HTTP clients must be safe ([ADR-0003])
- Typed Error variants for refresh failures ([ADR-0005]) — `AuthRequired { account, reason }` for the unrecoverable cases

If no decision were made, the maintainer would have to restart the daemon every hour per account. Unacceptable.

## Decision

We will use **proactive expiry-based refresh with a lazy 401 fallback**, with all token state keyed by account alias and persisted atomically to disk.

### TokenState (one per account)

```rust
pub struct TokenState {
    pub access_token: String,
    pub refresh_token: String,           // required — auth flow rejects no-refresh-token responses
    pub expires_at: chrono::DateTime<Utc>,
    pub scopes: Vec<String>,
    pub client_id: String,               // shared across accounts in v1, but stored per-account
    pub client_secret: String,           // for forward-compat with per-account OAuth clients
}
```

### TokenManager

```rust
pub struct TokenManager {
    /// Per-account state. Each entry has its own RwLock so refresh on one account
    /// does not block reads on others.
    states: HashMap<String, Arc<RwLock<TokenState>>>,
    http: reqwest::Client,
    token_uri: String,                   // shared (Google's endpoint)
    tokens_dir: PathBuf,                 // ~/.config/google-mcp/tokens/
}
```

The whole `TokenManager` is wrapped in `Arc<ArcSwap<TokenManager>>` at the `GoogleServer` level (per [ADR-0002] hot-reload model). When `accounts.toml` changes, a new `TokenManager` is constructed with the new account set and atomically swapped in. In-flight calls hold an `Arc<TokenManager>` snapshot from before the swap and continue using it for the call's duration.

### Access flow

```rust
impl TokenManager {
    pub async fn access_token(&self, account: &str) -> Result<String, Error> {
        let state = self.states.get(account)
            .ok_or_else(|| Error::AccountNotFound { account: account.into() })?;

        // Fast path: read lock, check if access token is still valid (with 60s buffer).
        {
            let s = state.read().await;
            if Utc::now() + Duration::seconds(60) < s.expires_at {
                return Ok(s.access_token.clone());
            }
        }

        // Slow path: take write lock, double-check (another writer may have refreshed),
        // then refresh via the refresh_token.
        let mut s = state.write().await;
        if Utc::now() + Duration::seconds(60) < s.expires_at {
            return Ok(s.access_token.clone());                 // raced; another writer refreshed
        }

        let new_state = self.refresh_with_lock_held(&s, account).await?;
        self.persist_atomic(account, &new_state).await?;       // tmpfile + rename
        *s = new_state;
        Ok(s.access_token.clone())
    }

    pub async fn force_refresh(&self, account: &str) -> Result<String, Error> {
        // Used by the 401 fallback path: skip the expiry check, refresh unconditionally.
        // Same write-lock + double-check pattern internally.
        ...
    }
}
```

### 401 fallback (lazy)

The Google API client (`gmail::GmailClient` and future `calendar::CalendarClient` etc.) wraps every authenticated request in a single retry-on-401:

```rust
async fn authed_get<T: DeserializeOwned>(&self, url: Url, account: &str) -> Result<T, Error> {
    let token = self.tokens.access_token(account).await?;
    let resp = self.http.get(url.clone()).bearer_auth(&token).send().await?;

    if resp.status() == StatusCode::UNAUTHORIZED {
        // Access token might be stale despite expiry check (clock skew, server-side revocation
        // of just the access token). Force-refresh once and retry.
        let token = self.tokens.force_refresh(account).await?;
        let resp = self.http.get(url).bearer_auth(&token).send().await?;
        return parse_response(resp).await;
    }

    parse_response(resp).await
}
```

If the second attempt also returns 401, the error is propagated as `Error::AuthRequired { account, reason: "401 after forced refresh — token may be revoked" }`. No further retries.

### Refresh request

```rust
async fn refresh_with_lock_held(&self, state: &TokenState, account: &str) -> Result<TokenState, Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", &state.refresh_token)
        .append_pair("client_id", &state.client_id)
        .append_pair("client_secret", &state.client_secret)
        .finish();

    let resp = self.http
        .post(&self.token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send().await?;

    let status = resp.status();
    let body = resp.text().await?;                              // capture body BEFORE status check for diagnostics

    if !status.is_success() {
        // Google returns invalid_grant when refresh_token is revoked / user changed password
        if body.contains("invalid_grant") {
            return Err(Error::AuthRequired {
                account: account.into(),
                reason: "refresh_token rejected (invalid_grant) — re-authenticate".into(),
            });
        }
        return Err(Error::Upstream {
            service: "google-oauth".into(),
            status: status.as_u16(),
            message: body,
        });
    }

    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| Error::Parse { context: "OAuth refresh response".into(), source: e })?;

    Ok(TokenState {
        access_token: parsed.access_token,
        // Google sometimes rotates refresh_token; keep old if not present in response
        refresh_token: parsed.refresh_token.unwrap_or_else(|| state.refresh_token.clone()),
        expires_at: Utc::now() + Duration::seconds(parsed.expires_in),
        scopes: state.scopes.clone(),                           // scopes don't change on refresh
        client_id: state.client_id.clone(),
        client_secret: state.client_secret.clone(),
    })
}
```

### Atomic persistence

Every successful refresh writes to disk before in-memory state is updated. The write is atomic (tmpfile + rename) so a crash mid-write cannot corrupt the token file:

```rust
async fn persist_atomic(&self, account: &str, state: &TokenState) -> Result<(), Error> {
    let final_path = self.tokens_dir.join(format!("{account}.json"));
    let tmp_path = self.tokens_dir.join(format!(".{account}.json.tmp"));
    let body = serde_json::to_string_pretty(state)?;
    tokio::fs::write(&tmp_path, body).await?;
    tokio::fs::rename(tmp_path, final_path).await?;             // atomic on POSIX
    Ok(())
}
```

The tokens directory is **not** watched by the hot-reload subsystem (per [ADR-0002]), so this write does not trigger spurious reloads.

### Refresh failure cooldown

If a refresh fails with anything other than `invalid_grant`, repeated calls to `access_token(alias)` should not hammer Google. Each failed refresh marks the account's TokenState with a cooldown timestamp (`failed_until: Option<Instant>`); calls during cooldown return the previous error immediately without attempting refresh. Cooldown grows exponentially from 1s up to 60s ceiling, resets on successful refresh.

`invalid_grant` does not cool down — it goes straight to `AuthRequired` and stays there until the user runs `google-mcp auth refresh <alias>` (which writes a new token file; daemon restart picks it up per [ADR-0002] v1 limitation).

### Incremental scope grant — `google-mcp auth grant <alias>`

When the operator enables a new service in `[services.calendar].enabled = true` ([ADR-0006](0006-config.md)), the existing token for an account doesn't have the new service's scopes. Without an incremental-grant path, the only recovery is `auth remove <alias>` + `auth add --alias <alias>` — which revokes and re-consents from scratch, including all the existing scopes. Annoying.

Google's OAuth supports **incremental authorization** via the `include_granted_scopes=true` URL parameter on the auth flow. The new grant is **additive** on top of existing grants; the user sees a consent screen showing only the *new* scopes being requested, and the resulting token covers all previously granted scopes plus the new ones.

`google-mcp auth grant <alias> [--scope <scope-url>]...` runs the OAuth flow with:

- `include_granted_scopes=true`
- Scopes = (currently granted scopes for this account, from `tokens/<alias>.json`) ∪ (currently configured scopes from `[services.<enabled-services>].scopes`) ∪ (any explicit `--scope` arguments)

The existing token file is replaced atomically (tmpfile + rename). The daemon picks up the new scopes via the same lazy-load path as `auth refresh`.

**Startup scope-mismatch warning:** at daemon startup, for each account, the granted scopes are compared against the union of enabled services' configured scopes. If any service's scopes are missing from any account, the daemon logs WARN naming the affected `(account, service)` pairs and pointing at `auth grant`. The daemon does **not** refuse to serve — disabled-per-account services just return `Error::InvalidArgument { detail: "service <X> not granted for account <Y>; run google-mcp auth grant <Y>" }` when called.

This pattern means turning on a new service is a 30-second flow: enable in config, restart daemon, see the WARN, run `auth grant` once per account that needs the new scope. No re-consenting from scratch.

`mcp_status` (per [ADR-0014](0014-status-introspection-tool.md)) surfaces missing scopes per-account so the operator can spot the problem from inside an MCP session without checking logs.

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Lazy refresh on 401 only | Minimal logic; only refreshes when actually needed | Every cold call has a potential extra round-trip; retry logic at every authed-call site (or one retry wrapper); no proactive maintenance |
| (b) Proactive refresh based on `expires_at` only | No retry overhead at call sites; predictable refresh schedule | Clock skew between daemon and Google can leave the daemon thinking a token is valid when Google has expired it; server-side revocation invisible until next call fails |
| **(c) Proactive expiry check + 401 fallback** (chosen) | Cheap proactive check (`Instant::now()` comparison + 60s buffer); 401 fallback handles clock skew, server revocation, and the rare "refresh succeeded but Google didn't propagate yet" case | Slightly more code than either alone; double the worst-case latency on a 401 (refresh + retry) |
| (d) Background refresh task per account | Refresh happens before any user-facing call observes near-expiry | Sharp edge: if the background task fails while the app is idle, the next user call is broken with no automatic recovery; complicates account add/remove (must spawn/cancel tasks on hot-reload) |
| (e) Refresh on every call | Trivially correct; no expiry tracking needed | Doubles the API calls to Google; massive latency penalty; quota-burning |

We choose (c). The proactive check is essentially free; the 401 fallback handles the edge cases where the proactive check is wrong. Background tasks (d) sound elegant but the failure mode (silent broken state when the refresh task fails during idle) is unacceptable for a multi-account daemon.

## Consequences

**Positive:**

- Daemon remains usable indefinitely without operator intervention (the original goal).
- Per-account `RwLock` means refresh activity on one account never blocks reads on another. With 10+ accounts this matters.
- Atomic persistence (tmpfile + rename) means a crash mid-refresh leaves either the old good token or the new good token on disk — never a partial write.
- The 401 fallback is the same pattern most production OAuth clients use; well-understood.
- Token rotation handling (`refresh_token.unwrap_or_else(|| state.refresh_token.clone())`) is correct for Google's behavior — they don't always rotate, so we keep the old one if the response doesn't include a new one.
- The `failed_until` cooldown protects against refresh-storm scenarios where a misconfigured client repeatedly hits `access_token` after a failed refresh.

**Negative:**

- The `(account, RwLock<TokenState>)` map means refresh contention is per-account, but cross-account contention is on the `HashMap` itself (for inserts/removes). The map is read-mostly; we use the snapshot pattern from [ADR-0002] so reads see a stable map for a tool call's duration.
- Persisting on every refresh is one disk write per ~hour per account. With 10 accounts, ~10 writes/hour. Negligible.
- The 60s expiry buffer means tokens are refreshed slightly early (1 minute before actual expiry). Loses ~1.7% of token lifetime per refresh. Acceptable.
- The cooldown logic is additional state on `TokenState` (`failed_until`, `consecutive_failures`) that needs its own tests.

**Risks:**

- *Risk:* Race between `force_refresh` (from 401 fallback) and proactive `access_token` (from a different concurrent call). Two writers could try to refresh at once.
  *Mitigation:* The write-lock + double-check pattern means at most one refresh actually runs per account-per-window. The losing writer sees the new token after the winning writer releases.
- *Risk:* `invalid_grant` from Google can have multiple causes (refresh token revoked, scopes changed, OAuth client deleted). All map to `AuthRequired`, but the user needs to know which.
  *Mitigation:* Include Google's error response body in the `reason` field so the user (or operator) can see the actual cause. The `auth refresh <alias>` command is the recovery path regardless of cause.
- *Risk:* If the daemon's clock is significantly ahead of Google's (>60s), proactive refresh fires too often (early). If significantly behind, tokens expire before proactive refresh and the 401 fallback fires every call.
  *Mitigation:* The 60s buffer absorbs typical clock skew (NTP-managed systems should be within milliseconds). The 401 fallback handles the worst case. Document NTP sync as a deployment requirement in [ADR-0008].
- *Risk:* `refresh_token` field is `String` (not `String` wrapped in a zeroize / secret type). Memory dumps could leak it.
  *Mitigation:* Out of scope for v1 — the daemon runs as the user's own process; access to its memory means access to the user's account anyway. If we ever ship a hosted multi-tenant version, revisit with `secrecy` crate.
- *Risk:* If Google deprecates the `refresh_token` grant type or changes the response format, refresh breaks silently across all accounts simultaneously.
  *Mitigation:* Surface refresh failures prominently in logs (per [ADR-0008] observability). Rate-limit refresh attempts via cooldown so we don't burn quota during the outage. Manual `auth refresh` works as recovery if the new flow is just a re-authentication.
- *Risk:* The `client_secret` is stored per-account in token files. For Google's "Desktop" OAuth client type the secret is not really secret (Google explicitly states this), but the file should still be `chmod 600`.
  *Mitigation:* The auth flow creates token files with restrictive permissions. Document this in the README under the security section.

## References

- [ADR-0001](0001-monolithic-google-mcp-architecture.md) — defines the always-on daemon model that requires refresh
- [ADR-0002](0002-multi-account-architecture.md) — per-account state structure that this ADR builds on; hot-reload snapshot pattern
- [ADR-0003](0003-transport-stdio-and-streamable-http.md) — shared TokenManager across both transports; multi-client HTTP mode demands the per-account RwLock granularity
- [ADR-0005](0005-error-model.md) — `Error::AuthRequired { account, reason }`, `Error::Upstream`, `Error::Parse` variants used here
- Google OAuth 2.0 [refresh token docs](https://developers.google.com/identity/protocols/oauth2/web-server#offline) — `access_type=offline`, refresh-token rotation, `invalid_grant` semantics
