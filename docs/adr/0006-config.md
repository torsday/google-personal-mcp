# ADR-0006: Configuration via optional TOML at `~/.config/google-personal-mcp/config.toml`

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

The prototype hardcodes everything: credentials path (`~/.config/google-personal-mcp/credentials.json`), token path, OAuth redirect port (8080), Gmail scopes, and there is no way to enable/disable services. As soon as Calendar arrives ([ADR-0001](0001-monolithic-google-personal-mcp-architecture.md)), this becomes painful: the maintainer has to fork the binary or add per-service feature flags to disable services they haven't authorized for an account.

A long-running daemon also has tunables that change deployment-to-deployment without code changes:

- HTTP bind address ([ADR-0003](0003-transport-stdio-and-streamable-http.md))
- HTTP session idle timeout
- Per-service rate limits ([ADR-0002](0002-multi-account-architecture.md))
- Logging level (already env-var-controlled, but should be config-overridable too)

There are two related but separate state files:

1. **`accounts.toml`** — managed by the auth CLI; one entry per Google account; hot-reloaded ([ADR-0002]).
2. **`config.toml`** — managed by the operator (the maintainer themselves); single file with operational settings; **not** hot-reloaded in v1 (config changes are rare; restart is fine).

If no decision were made, every tunable becomes either a hardcoded constant (forces rebuild on change) or a CLI flag (explodes the CLI surface). Neither scales.

## Decision

We will load configuration from an optional TOML file at `~/.config/google-personal-mcp/config.toml`. Every key is optional and has a documented default. Missing file = use all defaults. Malformed file = fail loudly at startup with a helpful error pointing at the offending key.

### File schema (full reference; all keys optional)

```toml
# ── Server-wide settings ─────────────────────────────────────────────
[server]
log_level = "info"                                  # tracing filter; overridden by RUST_LOG if set
log_format = "compact"                              # "compact" | "json" (json for production / structured ingest)

# ── Google OAuth client (shared across all accounts in v1) ───────────
[google]
credentials_path = "~/.config/google-personal-mcp/credentials/google.json"

[google.oauth]
redirect_port = 8080                                # local listener port for OAuth redirect during `auth add`

# ── Per-service enablement and scopes ────────────────────────────────
# A service is "enabled" if its tools are exposed by the MCP server. Disabled
# services contribute no tools and don't request their scopes during auth.
[services.gmail]
enabled = true
scopes = [
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/gmail.send",
]

[services.calendar]                                 # ships disabled by default until phase 2 lands
enabled = false
scopes = ["https://www.googleapis.com/auth/calendar"]

[services.contacts]                                 # ships disabled by default until phase 3 lands
enabled = false
scopes = ["https://www.googleapis.com/auth/contacts.readonly"]

# ── Per-account-per-service rate limits ──────────────────────────────
# Gmail's quota is per-user-per-second; these defaults are intentionally
# conservative. Tune up cautiously if you hit the limits in practice.
[rate_limit.gmail]
requests_per_second = 5
burst = 20

[rate_limit.calendar]
requests_per_second = 5
burst = 20

# ── HTTP transport tunables (only relevant when `serve --http` is used) ──
[http]
bind = "127.0.0.1:8765"                             # default address used if `serve --http` has no addr arg
session_idle_timeout_secs = 3600                    # 1 hour
max_concurrent_sessions = 50
require_loopback_or_tls = true                      # refuse to start on non-loopback bind without TLS termination

# ── Retry policy (per ADR-0005); zero overrides means "use the ADR defaults" ──
[retry]
max_attempts_5xx = 3                       # idempotent methods only — see ADR-0005
max_attempts_429 = 5
max_attempts_network = 3
backoff_base_ms = 100
backoff_cap_ms = 5000
max_total_duration_seconds = 30            # cap on a single tool call's retry window — never loop indefinitely
```

### Loading and validation

```rust
// src/config.rs
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]                       // typo in a key → loud error, not silent ignore
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub google: GoogleConfig,
    #[serde(default = "default_services")]
    pub services: ServicesConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let path = config_path();
        if !path.exists() {
            tracing::info!(path = %path.display(), "no config file found; using defaults");
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&path)
            .map_err(|e| ConfigError::Read { path: path.clone(), source: e })?;
        let cfg: Self = toml::from_str(&body)
            .map_err(|e| ConfigError::Parse { path, source: e })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        // Examples of validations enforced at load time:
        if self.http.bind.parse::<SocketAddr>().is_err() {
            return Err(ConfigError::Invalid {
                key: "http.bind".into(),
                detail: format!("`{}` is not a valid socket address", self.http.bind),
            });
        }
        if self.rate_limit.gmail.requests_per_second == 0 {
            return Err(ConfigError::Invalid {
                key: "rate_limit.gmail.requests_per_second".into(),
                detail: "must be > 0; set a positive integer or remove the key to use the default".into(),
            });
        }
        // ...etc
        Ok(())
    }
}
```

Notable choices:

- **`#[serde(deny_unknown_fields)]`**: typos in config keys produce a loud parse error rather than silently being ignored. Excellence is failing fast on operator mistakes.
- **Tilde expansion (`~`) is honored** for path values. Implement via `shellexpand` crate (~50 lines, no system shell invocation).
- **Validation runs at load time**, not lazily on first use. The daemon either starts with a known-good config or refuses to start.
- **Defaults match prototype behavior** where the prototype has reasonable defaults; new defaults are conservative.

### Env var precedence (limited and documented)

Most settings are config-file only. Two exceptions:

| Env var | Overrides config key | Why |
| --- | --- | --- |
| `RUST_LOG` | `server.log_level` | Standard tracing convention; useful for debugging without touching config |
| `GOOGLE_MCP_CONFIG` | (path to config file itself) | Allows alternate config files for testing / CI |

We intentionally do **not** support `GOOGLE_MCP_<KEY>=value` for arbitrary settings. Per-key env var overrides become an undocumented parallel config surface; force everything else through the file.

### Hot reload

**Not supported in v1.** Config changes require restart. Rationale:

- Config is operator-managed (changes rarely — once at deploy, occasional tunable adjustments).
- Hot-reloading `config.toml` would multiply the hot-reload surface from "accounts only" ([ADR-0002]) to "config + accounts," with several tunables (`http.bind`, `services.gmail.enabled`) that can't safely change at runtime anyway.
- The CLI's `auth` subcommands intentionally don't touch `config.toml`; the only writer is the operator with a text editor or a deploy script. Restart is acceptable.

Defer hot-reload to a future ADR if it becomes painful.

## Options Considered

### Format

| Option | Pros | Cons |
| --- | --- | --- |
| **(a) TOML** (chosen) | Human-readable; standard in Rust ecosystem; comments supported; section grouping natural for our shape | Slightly more verbose than YAML for deeply nested data; less ubiquitous than JSON |
| (b) YAML | Familiar to ops folks; comments supported | Dependency footprint (`serde_yaml` is heavier); whitespace-sensitive footguns; no ecosystem reason to prefer over TOML for this scale |
| (c) JSON | Universal; trivially serializable | No comments; no trailing commas; bad UX for human-edited config |
| (d) Rust source file (`config.rs` containing constants) | No parser needed; type-safe by definition | Forces rebuild on change; not deployable as separate operator artifact |
| (e) Env vars only | Twelve-factor; CI-friendly | Doesn't scale to tens of tunables; no comments; no grouping |

We choose TOML. Same format as `accounts.toml`, same parser dep already in scope (added by `accounts.toml` decision in [ADR-0002]).

### Layered config sources

| Option | Pros | Cons |
| --- | --- | --- |
| (f) Single config file, no overlays | Simplest; one source of truth | No way to override per-machine without editing the file |
| **(g) Single config file + two specific env var overrides (chosen)** | Trivial mental model; no surprises | Less flexible than full env-var overrides |
| (h) Layered: `/etc/google-personal-mcp/config.toml` then `~/.config/google-personal-mcp/config.toml` then env vars | System/user/runtime separation familiar from many tools | Multi-source debugging is "where did this value come from"; YAGNI for personal-use solo daemon |

We choose (g). For a personal daemon, layered config is over-engineering. The two env vars (`RUST_LOG`, `GOOGLE_MCP_CONFIG`) cover the actual use cases (debugging, alt config for testing).

### Validation strictness

| Option | Pros | Cons |
| --- | --- | --- |
| (i) Lenient: ignore unknown keys, log warnings | Forward-compat with old daemon reading new config | Operator typos go unnoticed; "I changed `redirect_port` and it's still 8080" debug sessions |
| **(j) Strict: `deny_unknown_fields`, fail at startup on any unknown or invalid key (chosen)** | Operator mistakes fail fast with location info | Adding new keys requires coordinated config + binary update |
| (k) Strict on unknown keys, permissive on values | Compromise | Same coordination cost as (j) for the unknown-key half; doesn't add real value |

We choose (j). For a personal daemon under one operator, the coordination cost of "update config when you update binary" is zero. The benefit (typos caught immediately) is large.

## Consequences

**Positive:**

- Everything tunable in one file, with comments. Operator can `cat ~/.config/google-personal-mcp/config.toml` to see the entire surface.
- Strict validation means "I changed X but it didn't take effect" cannot happen — the daemon either refuses to start or honors the change.
- Disabled services (`services.calendar.enabled = false`) means the binary ships with all services compiled in but only enabled ones contribute tools. Adding a new service is a config flip plus token re-auth (per [ADR-0002]'s `auth add` flow which requests current scopes).
- Defaults are documented in the ADR (this file) and in code (`Default` impls). The README points operators here for the schema.
- TOML matches `accounts.toml`'s format; one parser, one mental model.

**Negative:**

- The config has many sections (~7 top-level tables). Operators new to the project face a learning curve. Mitigated by inline comments in a shipped `config.toml.example`.
- `deny_unknown_fields` means renaming or deprecating a key requires either a major version bump or a transitional period with a custom deserializer. Acceptable cost; won't happen often.
- HTTP-mode tunables (`[http]`) are present even when running stdio. They're inert in stdio mode but still validated. Slight clutter; acceptable.
- Validation logic in `Config::validate` is hand-written, not derived. Each new field with rules adds a hand-written check. Mitigated by keeping per-field validation simple (range checks, address parsing).

**Risks:**

- *Risk:* `~` expansion via `shellexpand` differs subtly from shell `~` (no `~user` for other users by default). For a personal daemon this is fine.
  *Mitigation:* Document that `~` means `$HOME` only. Reject `~otheruser` patterns explicitly with a clear error.
- *Risk:* `config.toml` gets out of sync with `accounts.toml` (e.g., `services.gmail.enabled = false` but accounts have gmail.send scopes). Inconsistency is allowed but produces unexpected behavior.
  *Mitigation:* On startup, log a warning if any account's granted scopes don't match any enabled service's scope set. Not an error (the operator may have intentionally disabled a service without revoking tokens).
- *Risk:* Operator commits `config.toml` to a shared dotfiles repo and accidentally publishes the path to `credentials.json`.
  *Mitigation:* The credentials path is a path, not a secret. The OAuth client_secret inside `credentials.json` for Desktop apps is not really secret per Google. Document this nuance in the README.
- *Risk:* `http.bind = "0.0.0.0:8765"` without TLS termination is catastrophic (unauthenticated tokens over the wire). The `require_loopback_or_tls = true` default refuses to start in this case, but an operator could disable it.
  *Mitigation:* `require_loopback_or_tls = true` ships as default. Disabling it requires an explicit `false` in the config — visible during code review of the deploy artifact. CLI also emits a WARN at startup if `require_loopback_or_tls = false`.
- *Risk:* New config keys added in a later release silently default to values different from operator expectations.
  *Mitigation:* Document new keys and their defaults in the relevant ADR or in `CHANGELOG.md`. Strict parsing helps detect direction-of-change (operator can't be unaware that a new key exists if their old config now produces a parse error — but for that to happen, the new key has to be REQUIRED, which we avoid).

## References

- [ADR-0002](0002-multi-account-architecture.md) — `accounts.toml` is the sibling state file managed by the CLI; this ADR governs the operator-managed `config.toml`
- [ADR-0003](0003-transport-stdio-and-streamable-http.md) — `[http]` section provides defaults for `serve --http`
- [ADR-0005](0005-error-model.md) — `[retry]` section overrides retry-policy defaults
- [`toml`](https://docs.rs/toml) — TOML parser (already pulled in transitively by other deps)
- [`shellexpand`](https://docs.rs/shellexpand) — `~` expansion in path values
- [`serde`](https://docs.rs/serde) `deny_unknown_fields` — strict-key validation
