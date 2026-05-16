# Contributing

This repo is in the **design phase**. There is no code yet — the prior Gmail-only prototype was discarded so its shape wouldn't constrain the rewrite. The current contributable surface is the design itself: 19 ADRs under [`docs/adr/`](docs/adr/).

When v0.2 implementation begins, this document will gain sections on local development, testing, and code style. For now, read on for how the design conversation works.

## Design-first culture

Non-trivial changes need an ADR before code. "Non-trivial" means anything that touches:

- The tool surface (new tool, renamed parameter, changed response shape)
- Auth, scopes, or token handling
- The error model
- The config schema
- Persistence (cache, audit log, tokens)
- The threat model
- The deployment model

Bug fixes, behavior-preserving refactors, doc updates, additive tests, and lint-config tweaks do not need an ADR. When in doubt: write the ADR. The cost is one file; the cost of a load-bearing decision made implicitly is much higher six months later.

The ADR process — numbering, statuses, the "v1 scope" convention, how to propose one — is documented in [ADR-0000](docs/adr/0000-adr-process.md).

## How to contribute (design phase)

1. **Read the SPEC and the ADRs.** Start with [SPEC.md](SPEC.md) for what the project is *for* (use cases, search-excellence criteria, non-goals). Then [ADR-0000](docs/adr/0000-adr-process.md) for the ADR corpus and open-questions queue. Then [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md), [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md), [ADR-0017](docs/adr/0017-secrets-at-rest.md), [ADR-0018](docs/adr/0018-email-content-trust.md) — the load-bearing decisions.
2. **Open an issue or a PR.** Disagreement with a specific decision is welcome; cite the ADR and the paragraph.
3. **Propose a new ADR.** Open a PR adding `docs/adr/NNNN-<slug>.md` with `Status: Proposed`. Discussion happens in PR review.

The open-questions queue at the bottom of [ADR-0000](docs/adr/0000-adr-process.md) lists known gaps where new ADRs would be welcome — quota model, attachment composition, data retention, HTTP-transport auth, keyring backend.

## What will change when v0.2 starts

When the first code commit lands, this file will grow sections on:

- Toolchain installation and the `cargo` workflow
- The three test layers from [ADR-0007](docs/adr/0007-testing-strategy.md)
- How each contributor sets up their own GCP OAuth client and dedicated test Google account
- Code style enforcement (`cargo fmt`, `cargo clippy`, `cargo deny`)
- Commit conventions

None of that infrastructure exists yet, on purpose.

## Commit and PR style

- **Conventional Commits** (`type(scope): subject` — imperative, lowercase, no trailing period). Common types: `feat`, `fix`, `chore`, `docs`, `refactor`, `test`, `perf`, `ci`.
- One logical change per commit.
- PRs that touch ADRs explain whether they propose a new decision, accept an existing proposal, or supersede an accepted ADR.

## Reporting issues

Non-security issues: open a GitHub issue. Security issues: see [SECURITY.md](SECURITY.md).
