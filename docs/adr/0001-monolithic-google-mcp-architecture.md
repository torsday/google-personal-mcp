# ADR-0001: Monolithic Google-services MCP daemon (`google-mcp`)

**Date:** 2026-04-25
**Status:** Accepted

> **Note:** This ADR replaces an earlier in-flight draft titled "Build as `personal-mcp` with per-provider modules from day one." The earlier draft assumed scope spanning non-Google providers (Notion, Obsidian, generic notes/contacts) and never explicitly defended the topology choice. This revision narrows scope to Google services and explicitly defends monolithic over modular and hybrid alternatives.

---

## Context

The existing prototype is a single-purpose Gmail MCP server (`gmail-mcp`) — working code at `src/auth.rs`, `src/gmail/`, `src/tools/mod.rs`, `src/main.rs`. The maintainer has confirmed willingness to discard it and rewrite cleanly. Two architectural questions need to be settled before any rewrite begins, because they shape every other decision (auth, error model, config, testing):

1. **Topology** — single monolithic binary spanning multiple services, modular per-service binaries, or hybrid workspace-with-shared-library?
2. **Scope** — what data sources does this MCP cover?

The maintainer has clarified scope explicitly: **Google services only**. The MCP is a **data source** that exposes Google data (Gmail today, then Calendar, Contacts, possibly Tasks/Drive/Keep). It is not a knowledge-aggregation or "second brain" layer — those features belong in other tools that consume this MCP.

Constraints:

- Rust single binary, designed to run forever on a personal VPS (Rust chosen specifically for GC-free flat memory)
- `rmcp` 1.5 SDK for MCP protocol; verified that `#[tool_router]` impls on a shared handler struct compose via `Add` (`+`)
- One maintainer, personal use, zero installed users today
- Personal-use ops budget: prefer one daemon and one config over four
- Multi-account is a real requirement (10+ Gmail accounts) — orthogonal to topology, addressed in [ADR-0002](0002-multi-account-architecture.md)

If no decision were made, the default path is continuing to evolve `gmail-mcp` as single-purpose, paying the rename + restructure cost at the Phase 2 (Calendar) boundary, after install paths and OAuth redirect URIs are bound to the `gmail-mcp` name.

## Decision

We will build a **monolithic Rust daemon called `google-mcp`** that exposes Google services through MCP tools.

Specifically:

- **Crate name:** `google-mcp`
- **Binary name:** `google-mcp`
- **CLI:** `google-mcp auth ...` (account management — see [ADR-0002](0002-multi-account-architecture.md)) and `google-mcp serve` (run the MCP server)
- **Config directory:** `~/.config/google-mcp/`
- **Scope:** *Personal-data* Google services only — see the explicit in/out list below. Phase 1 = Gmail (rewrite). Future phases added as service modules without changing the foundation.

  **In scope** (services where the OAuth grant is "access this user's account data"):

  | Service | API | Phase | Notes |
  | --- | --- | --- | --- |
  | Gmail | `gmail.modify`, `gmail.send` | 1 | The rewrite target |
  | Calendar | `calendar` | 2 | Events, availability, scheduling |
  | Contacts | `contacts` (People API) | 3 | Cross-service entity resolution baseline |
  | Tasks | `tasks` | 4 | Google Tasks |
  | Drive | `drive`, `drive.file`, `drive.readonly` (caller's choice) | future | File metadata + content |
  | Keep | `keep` (limited API but present) | future | Notes |
  | Photos | `photoslibrary.readonly` | future | Personal photo library metadata |
  | Docs / Sheets / Slides | `documents`, `spreadsheets`, `presentations` | future | The user's own files |
  | Chat | `chat.messages.readonly`, `chat.spaces` | future | Personal Google Chat conversations |
  | YouTube — *personal slice only* | `youtube.readonly`, `youtube.upload` (as needed) | future | Subscriptions, playlists, watch history, the user's uploads, the user's channel |

  **Out of scope** (services where the API is utility/compute or public-data, not "your account"):

  - **Maps / Geocoding / Places** — compute APIs, no per-user data; if anyone wants this, it belongs in a separate `google-maps-mcp`
  - **Translate** — compute API
  - **YouTube as a public corpus** — searching public videos, fetching arbitrary video metadata, public comments, trending — different product, different access pattern; belongs in a `youtube-public-mcp` if anyone needs it
  - **Cloud Storage / GCP services** (BigQuery, Cloud Run, Pub/Sub, etc.) — workload APIs, not personal data; belongs in `gcp-mcp` or per-service MCPs
  - **Search Console / Analytics for sites you don't own** — out by definition (it's per-property, not per-user)
  - **Books, News, Trends, public-data services** — public-corpus, not personal
  - **Workspace Admin SDK** — manages other users' accounts; this MCP is for *your* data only

  The line that matters: **"is this an OAuth grant of the form `gmail.modify`-style 'access my account's data,' or is it `cloud-translation`-style 'use Google's compute against arbitrary input'?"** Personal-data services have user accounts and consent screens; compute services have API keys and billing dashboards. This MCP is for the former.

  YouTube is the interesting boundary case — same product, both shapes. We include the personal slice (your subscriptions, playlists, history, uploads); we exclude the public-corpus slice (search arbitrary videos). If a real "search public YouTube" use case appears, it gets its own MCP.
- **Tool philosophy:** low-level primitives only (search, get, list, create, modify, delete on each service). Higher-level reasoning ("summarize my week," "find emails about my next meeting") is the consumer's job. This MCP feeds knowledge layers; it is not one.
- **Source layout:**

  ```
  src/
  ├── main.rs              # CLI dispatch (auth | serve)
  ├── server.rs            # GoogleServer struct, composes service routers
  ├── config.rs            # TOML config (see ADR-0005)
  ├── error.rs             # Typed Error enum (see ADR-0004)
  ├── http.rs              # Shared reqwest::Client + retry/backoff + per-account rate limiting
  ├── auth/
  │   ├── mod.rs           # OAuth PKCE flow
  │   ├── account.rs       # Account registry (see ADR-0002)
  │   └── tokens.rs        # TokenManager, refresh logic (see ADR-0003)
  ├── gmail/
  │   ├── mod.rs           # GmailClient (HTTP wrapper)
  │   ├── types.rs
  │   └── tools.rs         # impl GoogleServer { gmail tools }, exports gmail_router()
  └── (future) calendar/, contacts/, tasks/ — same shape as gmail/
  ```

  Note: with Google-only scope, the auth abstraction simplifies — there is no `auth/google.rs` because there is no non-Google auth to contrast against.

- **Tool routing:** Each service's `tools.rs` declares `#[tool_router(router = <service>_router, vis = "pub")] impl GoogleServer { ... }`. `GoogleServer::new()` composes them via `+`.
- **OAuth client:** One Google OAuth client (one `credentials.json` from one GCP project), shared across all enabled services. Scopes are additive within the single consent screen per account.
- **Repo name:** GitHub repo stays `gmail-mcp` for now. Renaming the public repo is decoupled from this ADR and can happen anytime (or never).

  **Rename plan (when triggered):**

  1. **GitHub side:** Settings → Rename to `google-mcp`. GitHub auto-creates a redirect from `torsday/gmail-mcp` → `torsday/google-mcp` for clones, web, and API access. Old URL works indefinitely (until the maintainer creates a *new* repo with the old name, which we won't).
  2. **Local clones:** `git remote set-url origin git@github.com:torsday/google-mcp.git` — one command per clone. The old URL keeps working via the redirect, so this isn't urgent for any clone.
  3. **README + Cargo.toml:** update `[package].repository` field of `Cargo.toml` and any inline link in README. The redirect makes broken links impossible, but the canonical URL should reflect reality.
  4. **Releases / install instructions:** any cached install scripts (e.g., `curl https://raw.githubusercontent.com/torsday/gmail-mcp/...`) keep working via the GitHub redirect. Future install scripts use the new URL.
  5. **MCP client configs:** Claude Desktop's `mcpServers.google.command` references the binary path on disk, not the repo. Unaffected.
  6. **OAuth client:** the GCP-side OAuth client name doesn't reference the repo. Redirect URIs (`http://localhost:8080`) don't reference the repo. Unaffected.

  Total operator effort: ~30 seconds + the `git remote set-url` per machine. The rename is genuinely cheap because all the load-bearing references (binary path, config dir, OAuth client) are already `google-mcp`-named per the rewrite. Only the GitHub URL is left over as an artifact.

  **Trigger condition:** rename when a meaningful audience exists outside the maintainer (the moment the project is shared / linked from anywhere external). Until then, the URL doesn't matter and the rename creates only redirect churn.

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Status quo: single-purpose `gmail-mcp`, ship separate binaries per future Google service | Simplest scope per binary; matches MCP ecosystem norm; failure isolation between services | Pays design + integration cost N times; users run N daemons; multiple OAuth flows / consent screens per account; cross-service composition lives in the client only |
| **(b) Monolithic `google-mcp`** (chosen) | One Google OAuth flow per account (single consent screen, all enabled scopes); one daemon to deploy and monitor; shared infra (auth, http, error, retry) without workspace ceremony; cross-service tool composition trivially possible; one rmcp dependency to maintain; one config file | Single point of failure (a panic in calendar code kills gmail too); harder to share / publish individual services as standalone MCPs; one rmcp version pin spans all services |
| (c) Hybrid: Cargo workspace with `google-mcp-core` library + per-service binaries | Failure isolation per binary; shared core code without duplication; independent ship cadence per service | Workspace ceremony for a personal daemon; cross-binary token storage requires filesystem coordination; still N daemons to operate; per-service auth setup; defeats the single-OAuth-flow benefit unless you build a separate OAuth-helper process |
| (d) Broader scope: `personal-mcp` covering Notion, Obsidian, arbitrary providers in addition to Google | Maximum flexibility; "one MCP for all personal data" | Out of stated scope — maintainer explicitly limited this MCP to Google interactions; abstraction tax with no current beneficiary; non-Google providers belong in separate MCPs that other knowledge tools compose |

We reject (a) and (c) because Google's OAuth model rewards a single client (one consent, one refresh token per account, additive scopes) and personal-VPS solo ops favors one daemon over four. We reject (d) because the maintainer explicitly scoped this MCP to Google data; broader-scope second-brain functionality lives in consumers, not here.

We choose (b). Per the maintainer's framing: this MCP is a *data source* used by *other knowledge tools*, not the knowledge layer itself.

## Consequences

**Positive:**

- One binary, one daemon, one `~/.config/google-mcp/`, one systemd unit. Minimum ops surface for personal-VPS deployment.
- One Google Cloud OAuth client. One consent screen per account, all current and future enabled scopes presented at once.
- Adding a new Google service is: a new directory under `src/`, a `tools.rs` exporting a router function, one line added to `GoogleServer::new()`. No foundation changes.
- The "low-level primitives only" rule keeps the tool surface predictable and composable. Consumers (Claude, an Obsidian plugin, a CLI script, another MCP that aggregates) get raw Google data and decide what to do with it.
- README and code shape agree from commit one of the rewrite — no "the README says unified, but the code is gmail-only" mismatch.

**Negative:**

- All Google services share a process. A panic in calendar code crashes gmail too. (Mitigated by typed error handling per [ADR-0004] and `catch_unwind` at the tool boundary if needed.)
- Single `rmcp` version pin across all services. Upgrading rmcp requires regression-testing every service.
- The `GoogleServer` struct accumulates a client field per enabled service. Fine at 4–6 services; would feel cluttered at 20 (not a real risk for Google's surface).
- Per-service `tools.rs` files import `crate::server::GoogleServer` to define impls on it. They couple to the server struct's shape, not to each other.
- Reader onboarding cost: a newcomer must understand "one server struct, multiple service modules with `impl GoogleServer` blocks composed via `+`." Mitigated by a short architecture note in the README.

**Risks:**

- *Risk:* Future need to integrate non-Google services (e.g., Apple Calendar, Outlook, Fastmail) emerges, contradicting the Google-only scope.
  *Mitigation:* Accept that decision when it arises. Either build a separate MCP for the non-Google source, or supersede this ADR. The "Google-only" choice is current scope, not a permanent architectural constraint.
- *Risk:* The "low-level primitives only" rule erodes over time as it becomes tempting to add convenience tools ("summarize this thread," "find emails about my next meeting").
  *Mitigation:* Code review discipline. Every new tool must justify why it cannot live in the consumer. ADR-0001 is the place to point at when the temptation appears.
- *Risk:* `rmcp` 1.5 router composition via `Add` may break in future versions of the SDK.
  *Mitigation:* Pin `rmcp` to `"1.5"` in `Cargo.toml`. If upstream removes router composition in 2.x, fall back to a manual `ServerHandler` impl with explicit dispatch (more boilerplate, same external behavior). Track this dependency explicitly in the README.
- *Risk:* A bug or panic in one service module brings down access to all services for the duration of restart (typically <1s).
  *Mitigation:* Acceptable for personal use. Fast restart via systemd `Restart=always`. If restart latency ever becomes painful, revisit topology.

## References

- Existing prototype (to be discarded but referenced for `rmcp` macro shape and `oauth2` v5 + `reqwest` 0.13 token-exchange workaround): [src/auth.rs](../../src/auth.rs), [src/gmail/mod.rs](../../src/gmail/mod.rs), [src/gmail/types.rs](../../src/gmail/types.rs), [src/tools/mod.rs](../../src/tools/mod.rs)
- [README.md](../../README.md) — needs updating to reflect Google-only scope (action item, not part of this ADR)
- rmcp 1.5 documentation:
  - [`tool_router` attribute macro](https://docs.rs/rmcp-macros/1.5.0/rmcp_macros/attr.tool_router.html) — `router` and `vis` parameters; multiple impl block composition
  - [`ToolRouter<S>` struct](https://docs.rs/rmcp/1.5.0/rmcp/handler/server/router/tool/struct.ToolRouter.html) — `Add` impl requiring identical `S`
- [ADR-0002](0002-multi-account-architecture.md) — Multi-account architecture (orthogonal decision, depends on this ADR)
- ADRs that inherit this frame:
  - [ADR-0002](0002-multi-account-architecture.md) — Multi-account architecture
  - [ADR-0003](0003-transport-stdio-and-streamable-http.md) — Dual transport
  - [ADR-0004](0004-oauth-token-refresh.md) — Token refresh
  - [ADR-0005](0005-error-model.md) — Error model
  - [ADR-0006](0006-config.md) — Config schema
  - [ADR-0007](0007-testing-strategy.md) — Testing strategy
  - [ADR-0008](0008-observability-and-deployment.md) — Observability + deployment
  - [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — Caching layer (SQLite + Gmail History API)
  - [ADR-0010](0010-mime-and-encoding.md) — MIME / encoding handling
  - [ADR-0011](0011-audit-log.md) — Append-only audit log
  - [ADR-0012](0012-idempotency-and-dry-run.md) — Dry-run + send-deduplication for destructive ops
  - [ADR-0013](0013-cross-account-fan-out.md) — Cross-account fan-out for read tools
  - [ADR-0014](0014-status-introspection-tool.md) — `mcp_status` introspection tool
  - [ADR-0015](0015-tool-versioning-policy.md) — Tool versioning policy (additive-only, snapshot-enforced)
