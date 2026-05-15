# ADR-0002: Multi-account architecture per Google account

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

The maintainer routinely uses multiple Google accounts (10+ Gmail accounts: personal, work, consulting clients, side projects, etc.). The MCP server must support all of them concurrently from a single running process.

Google's OAuth model permits multiple accounts under a single OAuth client (one Google Cloud project's `credentials.json`). Each account has its own browser consent and produces its own refresh + access token pair. All scopes for an account are shared across Google services for that account (one token covers Gmail + Calendar + Contacts for that user).

Per [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md), `google-personal-mcp` is a single monolithic daemon. The multi-account question is therefore: how does one process manage N tokens, address tool calls to specific accounts, and expose account discovery to MCP clients?

This decision is load-bearing because it shapes:

- Token storage layout on disk
- The auth CLI surface (`google-personal-mcp auth ...` subcommands)
- Every tool's parameter schema (whether `account` is a parameter)
- The `TokenManager` API (per [ADR-0003], yet to be written)
- Per-account rate limiting (Gmail's quota is per-user-per-second)
- Whether the daemon supports adding/removing accounts without restart (hot-reload)

If no decision were made, the prototype's single-token model would persist — usable for one account, broken for the maintainer's actual use case.

## Decision

We will treat each Google account as a first-class entity identified by a **user-chosen alias** (string). All tools accept an optional `account` parameter; omitted means "use the default account." Accounts are hot-reloadable: the running daemon picks up CLI-driven account changes without restart.

Concretely:

- **Account = user-chosen alias.** Examples: `personal`, `work`, `acme`, `oss`. Aliases are local to this installation; they have no meaning to Google.

  **Alias validation rules (enforced by both CLI and runtime):**
  - Must match regex `^[a-z0-9][a-z0-9_-]{0,31}$` (DNS-label style: lowercase alphanumeric + hyphen + underscore; must start with alphanumeric; 1–32 characters).
  - Must not be one of the reserved values: `*` (fan-out marker per [ADR-0013](0013-cross-account-fan-out.md)), `default`, `all`, `none`.
  - Aliases are **case-sensitive** in storage but case-insensitive on lookup (`work` and `Work` both reach the same account; `Work` is rejected at creation time as not-lowercase).
  - These rules exist primarily to prevent path-injection attacks (alias is interpolated into `tokens/<alias>.json`) and CLI / URL ambiguity.
  - Validation lives in `auth::account::validate_alias(s: &str) -> Result<(), Error>` so a single function is the source of truth for both `auth add` and runtime tool-parameter validation.
- **Account registry:** TOML file at `~/.config/google-personal-mcp/accounts.toml`:

  ```toml
  [accounts.personal]
  email = "you@gmail.com"
  added_at = "2026-04-25T10:30:00Z"
  default = true

  [accounts.work]
  email = "you@workplace.com"
  added_at = "2026-04-26T09:00:00Z"
  ```

  Exactly one account has `default = true`. The CLI enforces this invariant.

- **Tokens:** stored per-account at `~/.config/google-personal-mcp/tokens/<alias>.json`. File contains access token, refresh token, expires-at timestamp, and the granted scopes. One Google OAuth client (one `credentials.json`) is shared across all accounts.
- **Auth CLI:**
  - `google-personal-mcp auth add [--alias <name>]` — runs OAuth PKCE flow in browser, captures token, prompts for alias if not given (with the email as the suggested default), registers account. First account added is automatically the default. Daemon picks up the new account automatically (see Hot-reload below).

    **CLI write order is load-bearing for hot-reload correctness:**
    1. Validate alias (per the rules above).
    2. Write `tokens/<alias>.json` atomically (tmpfile + rename).
    3. Write `accounts.toml` atomically (tmpfile + rename) with the new entry appended.

    Write order matters because the hot-reload subsystem watches `accounts.toml`. If the registry were written first, the daemon could observe the new alias before its token file exists, briefly producing `AuthRequired` errors during the race window. Token-then-registry guarantees that any registry entry the daemon sees has a corresponding readable token. The daemon's lazy-token-load pattern then works correctly.

    `auth remove` reverses the order: registry first (so the daemon stops trying to use the account), then token file deletion. `auth set-default` modifies only the registry.
  - `google-personal-mcp auth list` — prints accounts with alias, email, default marker, expires-at.
  - `google-personal-mcp auth remove <alias>` — **revokes the OAuth grant at Google** (calls `https://oauth2.googleapis.com/revoke` with the refresh token), then deletes the local token file and the registry entry. Revoking server-side cleans up the entry on `myaccount.google.com/permissions` so old refresh tokens don't accumulate forever in the user's Google account. If revocation fails (network error, token already invalid at Google), the local removal still proceeds and a WARN is logged with the reason. Order: registry first (so the daemon stops trying to use the account during the brief window), then revoke, then delete token file. Daemon picks up the registry change automatically.

    `--keep-grant` flag: skip the Google-side revocation; only delete local state. Use this when the local token file is corrupted but the grant itself is healthy (you'll re-add the same account without a fresh consent screen). Rare; the safe default is full revoke.
  - `google-personal-mcp auth set-default <alias>` — updates the default flag. Daemon picks up the change automatically.
  - `google-personal-mcp auth refresh <alias>` — forces a refresh-token roundtrip (for testing or recovery from suspected bad state). **v1 limitation:** because this writes only a token file (not `accounts.toml`), the daemon does not pick up the new token until restart. The CLI prints a restart reminder. Lifting this limitation is deferred to a future ADR.
- **Tool parameter:** every Google-service tool (today: gmail; future: calendar, contacts, ...) gains an optional `account: Option<String>` parameter. If omitted, the default account is used. Tool descriptions explicitly mention the parameter and its default behavior.
- **Discovery tool:** `list_accounts` (returns the contents of the registry) is added to the tool surface so an MCP client can choose an account programmatically.
- **Tool response convention:** every tool response includes which account was used (e.g., a structured field or an explicit prefix in the text response). This makes misroutes diagnosable.
- **TokenManager:** keyed by account alias. Each `(alias)` has its own `expires_at`, refresh state, and persistence path. Detail in [ADR-0003]. Holders of `TokenManager` references must use the snapshot pattern (see Hot-reload below) to be safe across reloads.
- **Rate limiting:** per-account. Each account gets its own semaphore + token-bucket on `http.rs`'s shared `reqwest::Client`. Gmail's quota is per-user-per-second; one hammered account must not starve others. Detail likely lives in the http/error ADR.
- **Server-side state structure (the actual shape of `GoogleServer`):**

  ```rust
  pub struct GoogleServer {
      /// Hot-swapped on accounts.toml change. In-flight tool calls hold an Arc snapshot.
      accounts: Arc<ArcSwap<AccountState>>,
      config: Arc<Config>,
      /// Shared HTTP client — connection pool reused across all accounts and services.
      http: reqwest::Client,
      tool_router: ToolRouter<Self>,
  }

  pub struct AccountState {
      registry: AccountRegistry,                                 // accounts.toml content
      per_account: HashMap<String /* alias */, AccountServices>,
      default_alias: Option<String>,
  }

  /// Everything that hangs off ONE account: its token, its rate limiter, its cache,
  /// and its per-service API clients (only the ones the operator enabled in [services]).
  pub struct AccountServices {
      token: Arc<RwLock<TokenState>>,                            // per-account RwLock — see ADR-0004
      rate_limiter: Arc<TokenBucket>,                            // per-account; Gmail quota is per-user
      cache: Arc<CacheConn>,                                     // per-account SQLite — see ADR-0009
      gmail: Option<GmailClient>,                                // present if [services.gmail].enabled
      calendar: Option<CalendarClient>,                          // future
      contacts: Option<ContactsClient>,                          // future
      // ...one Option<…> per supported service
  }
  ```

  Key invariants:

  - **Per-(account, service) clients, not per-service.** Each account has its own token; each service client holds a reference to its account's token via the `AccountServices` struct. There is no global `gmail_client: GmailClient` at the `GoogleServer` level.
  - **Shared `reqwest::Client`** at the `GoogleServer` level. HTTP connection pool, DNS cache, TLS session resumption — all reused across accounts and services. This is critical for performance with 10+ accounts.
  - **Per-account rate limiter** is held in `AccountServices`, not at the service-client level. Gmail's quota is per-Google-user, not per-service-call-type, so all services for an account share one budget per [ADR-0008](0008-observability-and-deployment.md) plumbing.
  - **Hot-reload swaps `Arc<AccountState>` atomically** via `ArcSwap` (see Hot-reload below). In-flight tool calls hold their snapshot's `Arc` and continue using the pre-reload `AccountServices`; new calls see the new state.
  - **Disabled services** at `[services.<name>].enabled = false` produce `None` in the `AccountServices` field; tool dispatch for that service returns `Error::InvalidArgument { field: "service", detail: "<name> is not enabled in config" }`.

- **Account hot-reload:** **Supported in v1.** The daemon watches the **parent directory** `~/.config/google-personal-mcp/` via the `notify` crate, filtering events to those affecting `accounts.toml`. (Watching the parent dir rather than the file directly is the `notify`-recommended pattern for surviving atomic-rename writes — when the CLI does `tmpfile + rename`, watching the file's old inode breaks; watching the parent catches the `IN_MOVED_TO` / `Create` event reliably.) On registry change, the in-memory account registry is atomically swapped (via `arc-swap`); tokens for newly-added accounts are loaded from disk on next access; in-memory state for removed accounts is dropped. In-flight tool calls take an `Arc` snapshot of the registry + token state at tool entry and use that snapshot for the duration of the call, so a mid-call removal does not corrupt the call. Token files (`tokens/<alias>.json`, in a subdirectory) are explicitly **not** watched, even though the parent watch could be widened — this avoids spurious reloads when the daemon's own proactive token-refresh logic writes them. Reloads are debounced (~100ms) to coalesce burst writes. Reload-failure-mode: on parse or validation failure, keep the previous good registry, log a WARN, continue serving.

## Options Considered

### Account routing

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Single account only | Simplest implementation; matches the prototype | Rejected — explicit user requirement is 10+ accounts |
| (b) Account-per-tool naming (`gmail_work_search_threads`, `gmail_personal_search_threads`, ...) | No tool param needed; static dispatch by tool name | Combinatorial explosion: 8 gmail tools × 10 accounts = 80 tools; breaks DRY catastrophically; tool list becomes unreadable for the model; adding an account means re-registering all tools |
| **(c) Account as parameter on every tool** (chosen) | One tool per logical operation regardless of account count; model can dynamically select; supports default for single-account UX; tool list stays small and stable | Every tool description carries account-parameter documentation overhead; the "default account" concept is soft global state and needs care |
| (d) Separate MCP server processes per account | Strong process-level isolation; per-account systemd units; per-account failure isolation | Operational nightmare for 10+ accounts; cross-account composition (e.g., "forward this work email to my personal account") becomes inter-process; massive resource waste; defeats the monolithic-daemon decision in ADR-0001 |
| (e) Account passed via MCP request metadata, not tool parameters | Cleaner tool params; account becomes a "context" rather than an argument | rmcp / MCP do not idiomatically expose request metadata to tool handlers; would require non-standard hooks; consumers (especially Claude) have no UI for setting per-call metadata; tool parameters are the load-bearing surface |
| (f) Default account by environment variable (`GOOGLE_MCP_ACCOUNT=work`) | No CLI state; per-shell scoping | Doesn't compose well with Claude Desktop and similar clients (they don't propagate env vars per call); still need a default for omitted-parameter case; reinvents (c) with extra steps |

We choose (c). It is the only option that preserves a clean tool surface (one tool per operation), works with how MCP clients actually call tools (parameters, not metadata), and scales to the maintainer's account count without combinatorial blowup.

### Hot-reload mechanism

| Option | Pros | Cons |
| --- | --- | --- |
| (g) No hot reload — restart required | Simplest; no concurrency; no extra deps | Real ops friction with 10+ accounts during initial setup; retrofitting later requires invasive refactor (TokenManager holders → `Arc<ArcSwap<...>>`); manual edits to `accounts.toml` for debugging require restart |
| **(h) File watching on `accounts.toml` via `notify` crate** (chosen) | No CLI ↔ daemon coupling (no PID file, no signal infra); works for any source of change including manual edits; reactive to its own config | Adds `notify` dep with platform-specific quirks (FSEvents aggregation on macOS, inotify watch limits on Linux); requires snapshot pattern for in-flight calls; reload error paths must be handled |
| (i) SIGHUP from CLI to daemon | No new dep; well-known UNIX idiom; CLI knows exactly when to reload | Requires PID file management (`~/.config/google-personal-mcp/daemon.pid`) with stale-PID handling; doesn't help if user manually edits `accounts.toml`; CLI must handle "no daemon running" case |
| (j) Watch both `accounts.toml` and the `tokens/` directory | Picks up CLI's `auth refresh` automatically | Spurious reloads from the daemon's own proactive token-refresh writes; requires dedup logic (compare content hash, track in-flight writes) — all complexity for the `auth refresh` edge case |
| (k) MCP admin tool (`_reload_accounts`) | Reload triggered from within the MCP session | Conflates admin concerns with the user-facing tool surface; the model could trigger reload accidentally; doesn't help when the daemon is running but no MCP client is connected |

We choose (h). File watching gives us the cleanest operational model — the CLI just writes to disk and the daemon reacts. The `notify` dep is mature, the snapshot pattern is the standard way to handle reloads against in-flight work, and we avoid PID coordination. The `auth refresh` edge case (which only writes a token file, not the registry) accepts a v1 limitation rather than complicate the watch surface.

## Consequences

**Positive:**

- Realistic for the maintainer's actual usage (10+ accounts).
- Single MCP process handles all accounts — no per-account daemon proliferation.
- Cross-account composition is trivial: a tool sequence can read from one account and write to another in one MCP session.
- Adding a new account is one CLI invocation; the running daemon picks it up automatically via filesystem watch. No restart, no code changes, no tool re-registration.
- `list_accounts` gives MCP clients a programmatic way to discover available accounts, supporting workflows like "let me see all my work emails first."
- The default-account convention preserves single-account UX (don't have to specify `account: "personal"` on every call when there's only one).
- Manual edits to `accounts.toml` (during debugging or recovery) are picked up automatically — no daemon-restart dance.
- The snapshot pattern means in-flight tool calls have a stable view of state for the call's duration, even across reloads. This is also the foundation for graceful shutdown later.

**Negative:**

- Every tool description must mention the `account` parameter and its default behavior. Increases tool-description length; mitigated by including a single shared sentence in a helper.
- "Default account" is a soft global. If the maintainer changes the default mid-session (via CLI), tools that ran earlier in the session may have used a different default than tools that run after. Manageable but a known footgun.
- Account aliases are user-chosen strings. Typos in tool calls return an "account not found" error rather than silently doing the wrong thing — but typos are still possible and require an error-handling pass.
- The `account` parameter in tool schemas means slightly more bytes per tool description sent to the MCP client. Negligible.
- Hot-reload adds two crate dependencies (`notify` for filesystem events, `arc-swap` for atomic state pointer) and an estimated ~150–250 lines of reload + diff + snapshot logic. Reload error paths (parse failure, missing token file for a newly-listed alias) need explicit handling — the daemon must keep running with the previous good state on reload failure, never apply a partial reload.
- The `auth refresh` v1 limitation (token file changes don't trigger reload) is real and may surprise users in recovery scenarios. The CLI's restart reminder is the workaround.

**Risks:**

- *Risk:* Model misroutes a tool call to the wrong account (e.g., sends a personal email from the work account because it defaulted to `work`).
  *Mitigation:* (1) Every tool response includes the account that was used, so misroutes are visible. (2) Tool descriptions emphasize specifying `account` explicitly when the context is ambiguous. (3) The `send_email` tool in particular should require `account` (no default-fallback for destructive cross-account-confusable operations); this rule should be codified per-tool in their descriptions and validated in the tool layer.
- *Risk:* Tokens revoked for one account (user changed Google password, revoked OAuth grant) cause noisy errors that could be misread as a system-wide failure.
  *Mitigation:* Per-account error reporting in [ADR-0004]. The `Error::AuthRequired` variant carries the account alias. The CLI command `google-personal-mcp auth refresh <alias>` re-runs the OAuth flow for that account only (subject to the v1 restart limitation).
- *Risk:* Account registry corruption (manual edit, partial write) leaves the daemon in an inconsistent state.
  *Mitigation:* Atomic writes (tmpfile + rename) for `accounts.toml` on the CLI write path. Reload validates the parsed TOML before applying; on parse or validation failure, the daemon keeps the previous good registry, logs an error at WARN level, and continues serving. Document the file format in the README.
- *Risk:* Hot-reload race between an in-flight tool call and a registry mutation (e.g., the call references account `work` while the CLI removes it).
  *Mitigation:* Tool calls take an `Arc` snapshot of the registry + token state at tool entry. The snapshot is held for the duration of the call. Mid-call removals do not affect the running call; the next call sees the post-reload state. This is the standard `arc-swap` pattern.
- *Risk:* Spurious reloads triggered by the daemon's own proactive token-refresh writes to the tokens directory.
  *Mitigation:* Watch `accounts.toml` only, not the `tokens/` directory. Token files are loaded lazily on registry-driven account additions and on first use; the daemon's own token writes do not trigger reload.
- *Risk:* `notify` crate behaves differently across platforms (macOS FSEvents aggregation delays, Linux inotify watch limits, network-mounted home directories with inotify gaps).
  *Mitigation:* Debounce reload events by ~100ms to coalesce burst writes. For the maintainer's stated personal-VPS deployment (Linux, local disk), inotify is reliable. Document the dep behavior in the README under "Known limitations" if a non-Linux deployment is ever attempted.
- *Risk:* A malicious or confused MCP client iterates `list_accounts` and exfiltrates the alias-to-email mapping.
  *Mitigation:* The MCP client already runs with full access to the daemon's tool surface. Hiding the alias-to-email mapping does not meaningfully improve security against a hostile client. Trust boundary is at the MCP client, not within the daemon.

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — Monolithic Google-services MCP daemon (parent decision; defines the single-process scope this ADR builds within)
- Future ADRs that inherit from this:
  - ADR-0003 — OAuth token refresh (TokenManager keyed by account alias; uses snapshot pattern for hot-reload safety)
  - ADR-0004 — Error model (per-account error variants like `AuthRequired { account: String }`)
  - ADR-0005 — Config (TOML schema for `accounts.toml` and `config.toml`)
- Crate dependencies introduced by this ADR: [`notify`](https://docs.rs/notify) (filesystem events), [`arc-swap`](https://docs.rs/arc-swap) (atomic `Arc` swap for lock-free reads)
- Google OAuth 2.0 docs — multi-user / multi-account patterns under a single OAuth client
