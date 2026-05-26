# Contributing

## v0.2 dev workflow

### Toolchain

```sh
# Rust (stable, via rustup)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable

# just (task runner)
cargo install just

# cargo-nextest (faster test runner)
cargo install cargo-nextest

# cargo-deny (license / advisory auditor used by `just check`)
cargo install cargo-deny
```

Minimum Rust version: **1.86** (see `rust-version` in `Cargo.toml`).

### Clone and build

```sh
git clone https://github.com/torsday/google-personal-mcp
cd google-personal-mcp
cargo build
```

### Run the full check suite

```sh
just check
```

This runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo nextest run`, and `cargo deny check` — same checks as CI.

### Binary size (Streamable HTTP transport — #72)

Per [ADR-0003](docs/adr/0003-transport-stdio-and-streamable-http.md), the
default build links **both** `rmcp` `transport-io` and
`transport-streamable-http-server`. Linking the HTTP server pulls in
`axum`, `tower`, and a server-side `hyper` stack — roughly **+5 MiB** to
the release binary on `aarch64-apple-darwin`. Reference figures
(`cargo build --release`):

| Build                                | Approx. size |
|--------------------------------------|--------------|
| Default (`transport-io` + HTTP)      | ~18 MiB      |
| Pre-HTTP baseline (`transport-io`)   | ~13 MiB      |

Operators who only need stdio (the local Claude-Desktop pattern) can
shrink the binary by disabling the HTTP feature:

```sh
cargo build --release --no-default-features \
  --features "rmcp/server,rmcp/macros,rmcp/transport-io,rmcp/schemars"
```

The CLI still accepts `--http <addr>` in such a build, but the call
will fail at link time since the `streamable_http_server` module isn't
compiled in. Most VPS deployments want the full default; this knob
exists for size-sensitive distributions and CI smoke binaries.

---

## Per-contributor GCP setup

Each contributor needs their own OAuth client credentials. The daemon never
ships with secrets baked in.

### 1. Create a GCP project

1. Go to [console.cloud.google.com](https://console.cloud.google.com).
2. Create a new project (e.g. `google-personal-mcp-dev`).
3. Enable the **Gmail API** (APIs & Services → Library → search "Gmail API").
4. Create an **OAuth 2.0 Client ID** (APIs & Services → Credentials → Create
   Credentials → OAuth client ID → Desktop app).
5. Download the JSON file.

### 2. Create the config directory

```sh
mkdir -p ~/.config/google-personal-mcp/credentials
chmod 700 ~/.config/google-personal-mcp
cp ~/Downloads/client_secret_*.json \
  ~/.config/google-personal-mcp/credentials/google.json
chmod 600 ~/.config/google-personal-mcp/credentials/google.json
```

Create `~/.config/google-personal-mcp/config.toml`:

```toml
[google]
credentials_path = "credentials/google.json"

[google.oauth]
redirect_port = 9876

[services.gmail]
enabled = true
profile = "modify+send"
scopes = [
  "https://www.googleapis.com/auth/gmail.modify",
  "https://www.googleapis.com/auth/gmail.send",
]
```

### 3. Authorize your Google account

```sh
cargo run -- auth add --alias personal
```

A browser window opens. Sign in and accept the consent screen.

### 4. Verify

```sh
cargo run -- auth list
```

Should print your alias and email.

---

## Test layers (ADR-0007)

### Layer 1 — Unit tests (always run)

Co-located in `mod tests` blocks throughout `src/`. Run with:

```sh
cargo nextest run
```

### Layer 2 — `wiremock` HTTP tests (planned for v0.3)

Not yet implemented. Will cover `GmailClient`, `TokenManager` refresh,
retry policy, and status-to-error mapping against a local mock server.

### Layer 3 — Ignored e2e smoke tests

Require a dedicated test installation (see [tests/README.md](tests/README.md)):

```sh
export GOOGLE_MCP_TEST_CONFIG_DIR=~/.config/google-personal-mcp-test
just test-e2e
```

Destructive tests additionally require:

```sh
export GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1
```

Never run by `just check`; run manually before tagging a release.

---

## Design-first culture

Non-trivial changes need an ADR before code. "Non-trivial" means anything
that touches:

- The tool surface (new tool, renamed parameter, changed response shape)
- Auth, scopes, or token handling
- The error model
- The config schema
- Persistence (cache, audit log, tokens)
- The threat model
- The deployment model

Bug fixes, behavior-preserving refactors, doc updates, additive tests, and
lint-config tweaks do not need an ADR. When in doubt: write the ADR. The
ADR process is documented in [ADR-0000](docs/adr/0000-adr-process.md).

The open-questions queue at the bottom of [ADR-0000](docs/adr/0000-adr-process.md) lists known gaps where new ADRs would be welcome — quota model, attachment composition, data retention, HTTP-transport auth, keyring backend.

## Deprecating a tool

The full procedure lives in [ADR-0015 §Deprecation procedure](docs/adr/0015-tool-versioning-policy.md). Quick reference:

1. **Sunset window** — non-destructive tools get 6 months; destructive tools (anything that mutates Gmail state or writes to disk) get 12 months. The clock starts the day the deprecation lands on `main`.
2. **Add a `Deprecation` entry** to the `production()` map in [`src/server/deprecation.rs`](src/server/deprecation.rs), keyed by the exact registered tool name. Set `sunset_date` to the ISO date the tool will be removed and `replacement` to the successor tool's name.
3. **Prefix the descriptor** — call `descriptors::apply_deprecation_prefix(&mut tool, &dep)` inside the tool's `*_descriptor()` fn before returning. The `[DEPRECATED — use {new} — sunset {YYYY-MM-DD}]` banner renders automatically.
4. **No further wiring needed.** The dispatcher's `call_tool` wrapper checks the registry on every call and emits a structured `tracing::warn!` plus increments the global counter surfaced by `mcp_status.deprecated_tool_invocations_total`.
5. **Bump `mcp_status::SCHEMA_VERSION`** only when adding or removing a field on the response envelope, not when adding deprecation entries.
6. **Removal day** — when the sunset date arrives, delete the descriptor fn, the dispatch arm, and the registry entry in one PR. The Layer 4 snapshot test catches drift.

## Commit and PR style

- **Conventional Commits** (`type(scope): subject` — imperative, lowercase,
  no trailing period). Common types: `feat`, `fix`, `chore`, `docs`,
  `refactor`, `test`, `perf`, `ci`.
- One logical change per commit.
- PRs that touch ADRs explain whether they propose a new decision, accept an
  existing proposal, or supersede an accepted ADR.

## Reporting issues

Non-security issues: open a GitHub issue. Security issues: see
[SECURITY.md](SECURITY.md).
