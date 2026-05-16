# Contributing

Welcome. This repo is in the **design phase** — the current `src/` is a prototype scheduled for replacement, and the architecture is captured in 18 ADRs under [`docs/adr/`](docs/adr/). Read those before opening a non-trivial PR.

## Design-first culture

Non-trivial changes need an ADR before code. "Non-trivial" means anything that touches:

- The tool surface (new tool, renamed parameter, changed response shape)
- Auth, scopes, or token handling
- The error model
- The config schema
- Persistence (cache, audit log, tokens)
- The threat model

Bug fixes, refactors that preserve behavior, doc updates, and additive tests do not need an ADR. When in doubt, open an issue or a draft PR and ask.

The ADR process — numbering, statuses, the "v1 scope" convention, how to propose one — is documented in [ADR-0000](docs/adr/0000-adr-process.md).

## Local development setup

### 1. Toolchain

`rust-toolchain.toml` pins the Rust version. `rustup` will install it on first build.

```bash
cargo build
cargo nextest run    # or `cargo test` if you don't have nextest
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

CI runs all of the above plus `cargo deny check` (see [.github/workflows/ci.yml](.github/workflows/ci.yml)). Match CI locally before opening a PR.

Install once:

```bash
cargo install cargo-nextest --locked
cargo install cargo-deny --locked
```

### 2. Your own GCP project

Each contributor needs their own Google Cloud project and OAuth client. The maintainer's credentials are not redistributed. One-time setup:

1. [Google Cloud Console](https://console.cloud.google.com/) → create project.
2. Enable **Gmail API** (and any other APIs the PR touches).
3. **APIs & Services → OAuth consent screen** → choose **External**, fill in the required fields, add yourself as a test user.
4. **Credentials → Create credentials → OAuth client ID → Desktop application**. Download the JSON.
5. Place at `~/.config/google-personal-mcp/credentials.json` (rewrite will move to `~/.config/google-personal-mcp/credentials/google.json` per [ADR-0006](docs/adr/0006-config.md)).
6. Run the auth flow: `./target/debug/google-personal-mcp auth` (prototype) — or whatever the rewrite renames it to.

### 3. A separate test account

[ADR-0007](docs/adr/0007-testing-strategy.md) Layer 3 — end-to-end smoke tests — runs against a real Gmail account. **Never use your primary account.** Create a dedicated test Google account, register it under a separate alias, and point the e2e harness at it via `GOOGLE_MCP_TEST_CONFIG_DIR`. See ADR-0007 for the exact layout.

Destructive e2e tests are gated on a second env var, `GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1`. Do not set it unless you know what you are doing.

## Testing expectations

[ADR-0007](docs/adr/0007-testing-strategy.md) defines four layers:

1. **Pure unit tests** (`mod tests` in each file): parsing, mapping, validation, math. Required for any non-trivial logic.
2. **`wiremock` integration tests**: HTTP-touching code. One test per status-code path per endpoint (200, 401, 404, 429, 5xx, malformed JSON). Required for new Google API calls.
3. **Ignored e2e smoke tests**: real Gmail behind `#[ignore]`. Run manually before release.
4. **Snapshot tests** (`insta`): tool descriptors and JSON schemas. Required for any tool surface change.

There is no enforced coverage percentage. Coverage as a number is a lying metric; coverage as a tool for finding gaps is welcome (`cargo llvm-cov`).

## Code style

- `cargo fmt` is canonical; CI enforces.
- `cargo clippy` with `-D warnings`; CI enforces.
- Naming follows [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) for tool surface (e.g. `account` not `acct`, `max_results` not `limit`).
- No comments that restate code. Comments explain *why* — a constraint, an invariant, a workaround.
- No `unwrap()` / `expect()` in non-test code outside the startup path. Use the typed `Error` enum per [ADR-0005](docs/adr/0005-error-model.md).
- Never log `access_token`, `refresh_token`, or `client_secret`. [ADR-0017](docs/adr/0017-secrets-at-rest.md) covers the redaction pattern; the format-output unit test enforces it.

## Commit and PR style

- **Conventional Commits** (`type(scope): subject` — imperative, lowercase, no trailing period). Common types: `feat`, `fix`, `chore`, `docs`, `refactor`, `test`, `perf`, `ci`.
- One logical change per commit. Atomic commits are easier to review and to revert.
- PRs reference the ADR(s) they implement or that govern the change. PRs that violate an ADR need to update the ADR (with rationale) in the same PR.

## What lives where

- `src/` — current prototype. Will be discarded.
- `docs/adr/` — architectural decisions. See [ADR-0000](docs/adr/0000-adr-process.md) for the corpus.
- `tests/` — integration and e2e tests (when added).
- `.github/workflows/ci.yml` — required checks. Run locally before pushing.
- `clippy.toml`, `deny.toml` — lint configuration.

## Reporting bugs

Non-security bugs: open an issue with a minimal reproduction. Security issues: see [SECURITY.md](SECURITY.md).

## Questions

Open an issue with the `question` label or start a discussion. Pre-v1.0 the maintainer is the sole reviewer; expect best-effort response.
