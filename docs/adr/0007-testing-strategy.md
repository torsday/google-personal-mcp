# ADR-0007: Testing strategy — pure units, wiremock for HTTP, ignored e2e for smoke

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

The prototype has zero tests. The rewrite introduces enough surface (multi-account state, hot-reload races, OAuth refresh, two transports, retry policy) that "works on my machine" is no longer sufficient.

A daemon that runs forever serving real Google data has specific testing pressures:

- **Parsing correctness matters** — multipart MIME (Gmail body extraction) is famously tricky; a regression silently returns empty bodies.
- **Error mapping matters** — every Google response shape (200, 400, 401, 404, 429, 5xx, malformed JSON) needs verified handling per [ADR-0005](0005-error-model.md).
- **Concurrency matters** — multi-account TokenManager, hot-reload, and HTTP-mode multi-client all introduce race surfaces.
- **Security matters** — header injection in `send_email` ([ADR-0005] `HeaderInjection` variant) is a real attack class against agentic email tools and needs explicit test coverage.

Wrapping a live API also constrains what "good test coverage" means:

- We can't test against real Gmail in CI (auth, quota, irreversible side effects).
- We *should* test against real Gmail occasionally with `#[ignore]` smoke tests, run manually on demand.
- We cannot reasonably test rmcp's own protocol conformance — that's their library, their tests.

If no decision were made, the rewrite would ship with the same "no tests" position and every regression discovered in production via "Claude told me my email subject is empty."

## Decision

We will adopt a **three-layer testing strategy**, in priority order:

### Layer 1 — Pure unit tests (no I/O, no network, no clock)

Target: parsing, mapping, validation, math.

- `Message::header`, `Message::subject`, `Message::body_text` (multipart MIME walk)
- `Email`-construction logic (the rfc2822 string-builder for `send_email`) — extracted into a pure function `compose_rfc2822(...)` so it's directly testable
- Header-injection detection (test that `\r\n` in subject/to/cc is rejected per [ADR-0005] `HeaderInjection`)
- Token-expiry math (`expires_at - now < 60s` boundary cases; clock-skew defense)
- Error-mapping function (`Error → rmcp::ErrorData`) — table-driven test
- TOML config parsing (every `[section]` validates as expected; unknown keys produce loud errors)
- Account registry diff logic (computing added/removed/changed sets from old vs new `accounts.toml`)

Co-located in `mod tests` blocks within each module file. Standard Rust idiom.

### Layer 2 — `wiremock` for HTTP-touching code

Target: `GmailClient`, `TokenManager` refresh, retry policy, status-to-error mapping.

```rust
#[tokio::test]
async fn search_threads_handles_429_with_retry_after() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/threads"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string(r#"{"error":{"message":"User-rate limit exceeded"}}"#)
        )
        .up_to_n_times(2)
        .expect(2)
        .mount(&mock_server).await;

    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/threads"))
        .respond_with(ResponseTemplate::new(200)
            .set_body_string(include_str!("fixtures/gmail/search_threads_success.json")))
        .expect(1)
        .mount(&mock_server).await;

    let client = test_client_pointing_at(&mock_server.uri());
    let result = client.search_threads("from:test", 20, "personal").await.unwrap();
    assert_eq!(result.len(), 3);
}
```

Notable patterns:
- `GmailClient::new` parameterizes on a `base_url` (instead of the current hardcoded `const GMAIL_API`) — this is the only refactor needed for testability. No traits, no dyn dispatch.
- Fixtures (real-world Gmail response shapes) live at `tests/fixtures/gmail/*.json` and `tests/fixtures/oauth/*.json`. Captured with `curl` against a real account, then sanitized.
- One test per error-path per endpoint at minimum: 200 (happy), 401 (refresh), 404 (NotFound), 429 (RateLimited), 500 (Upstream after retries), malformed JSON (Parse).
- TokenManager refresh tests exercise: success, `invalid_grant`, transient 5xx with backoff, `refresh_token` rotation (Google sometimes returns a new one).

`wiremock = "0.6"` as `[dev-dependencies]`. Per-test `MockServer` instance — fast (in-process) and isolated.

### Layer 3 — Ignored end-to-end smoke tests against real Gmail

Target: catastrophic regression detection (e.g., we accidentally start sending wrong Authorization header).

**Test config isolation.** The e2e tests do **not** read from the operator's real `~/.config/google-mcp/` (which has live accounts; tests would make real API calls against personal data). They read from a dedicated test installation pointed at by the env var **`GOOGLE_MCP_TEST_CONFIG_DIR`**:

```
$GOOGLE_MCP_TEST_CONFIG_DIR/
├── credentials/
│   └── google.json           # OAuth client for a TEST GCP project
├── accounts.toml             # registers the test account aliases
├── tokens/
│   └── test.json             # OAuth tokens for the test account
└── config.toml               # test-specific config (cache dir, etc.)
```

The setup is one-time per test environment: create a separate Google account, separate GCP project, run `GOOGLE_MCP_CONFIG=$TEST_DIR google-mcp auth add --alias test`. Document in `tests/README.md`.

```rust
#[tokio::test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
async fn smoke_search_threads_returns_results() {
    let test_dir = std::env::var("GOOGLE_MCP_TEST_CONFIG_DIR")
        .expect("e2e tests require GOOGLE_MCP_TEST_CONFIG_DIR");
    let client = real_client_for_test_account(&test_dir).await.unwrap();
    let result = client.search_threads("in:inbox", 5, "test").await.unwrap();
    assert!(result.len() <= 5, "respect max_results");
    for thread in result {
        assert!(!thread.id.is_empty());
    }
}
```

Rules:
- Gated behind `#[ignore]` — `cargo test` does NOT run them; explicit `cargo test -- --ignored` does
- Read against a dedicated test Gmail account via `GOOGLE_MCP_TEST_CONFIG_DIR` — never the operator's primary
- Read-only operations only: `search_threads`, `list_labels`, `list_accounts`. Destructive ops are gated on a **separate** env var `GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1` (must be set explicitly per CI run; never default-true)
- Run manually before tagging a release; not on every PR

### Layer 4 — Snapshot tests for tool descriptions and JSON schemas

Target: catch accidental MCP-protocol surface changes that would break downstream consumers (per the tool-versioning policy in [ADR-0015](0015-tool-versioning-policy.md)).

The `rmcp` macros generate JSON schemas for every tool's parameters and the tool's description text. Both are part of the public MCP contract — changes affect how every connected LLM sees the tool. We snapshot them with `insta`:

```rust
#[test]
fn snapshot_tool_registry() {
    let server = GoogleServer::new(test_state());
    let descriptors: Vec<ToolDescriptor> = server.tool_router.list_descriptors();
    insta::assert_yaml_snapshot!("tool_registry", descriptors);
}
```

The snapshot file `tests/snapshots/snapshot_tool_registry.snap` is committed to the repo. Any change to a tool's name, description, or parameter schema produces a snapshot mismatch and fails CI. The intentional update workflow:

1. Make the tool change.
2. Run `cargo insta review`.
3. Inspect the diff. If additive (new optional param, expanded description) → accept, commit the new snapshot.
4. If breaking (renamed param, removed param, type change) → reject; either revert or follow the [ADR-0015](0015-tool-versioning-policy.md) versioning protocol (new tool name, deprecation cycle).

This snapshot doubles as the **versioning baseline** referenced in [ADR-0015](0015-tool-versioning-policy.md) — one mechanism, two purposes.

### Concurrency tests

Target: hot-reload races, refresh-write contention.

These are special — they require deliberate orchestration of concurrent tasks. Use `tokio::test(flavor = "multi_thread")` and `tokio::join!` / `tokio::spawn` to provoke races. Property-style testing with `loom` is overkill for v1; revisit if we hit a real race that unit tests miss.

Specific cases:
- Two concurrent `access_token(alias)` calls during expiry — verify exactly one refresh fires
- Reload of `accounts.toml` while a tool call is in-flight against an account — verify the in-flight call completes successfully on the snapshot
- Reload of `accounts.toml` removing an account; subsequent call to that account returns `AccountNotFound`

### Test runner

`cargo nextest` (not `cargo test`):
- Parallel execution by default
- Better failure output (per-test status vs `cargo test`'s buffered output)
- Built-in retry policy for known-flaky tests (we shouldn't need this, but option exists)

Configured via `.config/nextest.toml`. CI uses nextest; local devs can use either.

### Coverage

`cargo llvm-cov` available locally. **No CI enforcement of a coverage percentage** — coverage as a number is a lying metric (100% line coverage with assertion-free tests is worse than 60% with meaningful assertions). Use coverage to find untested code paths during development; don't make it a gate.

## Options Considered

### Test framework / structure

| Option | Pros | Cons |
| --- | --- | --- |
| **(a) Layer 1 (units) + Layer 2 (wiremock) + Layer 3 (ignored e2e)** (chosen) | Each layer catches a different bug class; cheap layers run fast; expensive layer runs on demand | Fixtures need maintenance as Gmail changes its responses |
| (b) Mock `GmailClient` behind a trait | Test the tool layer in isolation | Adds a trait that exists only for testing — known anti-pattern; wiremock gives full coverage anyway |
| (c) Skip tests, rely on type system | Fastest to write | Multipart MIME, header injection, retry policy, hot-reload races are not type-system-enforceable |
| (d) Snapshot tests via `insta` for tool responses | Catches accidental output changes | Brittle when output format intentionally changes; tool output is structured (JSON) so easier to assert directly |
| (e) Property-based tests via `proptest` | Find edge cases in parsers | Worth it for a few targeted things (token expiry math, multipart traversal) but not the default; YAGNI for this scale |

We choose (a). Snapshot tests are worth considering for the rfc2822 message construction (one snapshot per case), but not as the primary tool. Property tests are worth adding for the multipart MIME walker once we have it.

### Test runner

| Option | Pros | Cons |
| --- | --- | --- |
| (f) `cargo test` (default) | No new tooling | Serial output buffering hides which test is running; no per-test status until completion |
| **(g) `cargo nextest`** (chosen) | Parallel by default; better output; faster on multi-core | New tool to install (small); slightly different config surface |

### Coverage gating

| Option | Pros | Cons |
| --- | --- | --- |
| (h) No coverage gate | Honest signal that coverage is for finding gaps, not satisfying a number | Could lull us into not writing tests |
| (i) 80% line coverage gate in CI | Forces test-writing | Coverage-as-metric encourages bad tests (assertion-free, branch-coverage) just to hit the number |
| **(j) Coverage available locally, no CI gate (chosen)** | Use coverage as a tool, not a target; honest about its limitations | Requires reviewer discipline to check "is this PR adequately tested?" |

### E2E test gating

| Option | Pros | Cons |
| --- | --- | --- |
| (k) `#[ignore]` + manual run before release | Can't accidentally hit real Gmail in CI; explicit operator action | Requires remembering to run them |
| (l) Run on every PR with separate test account credentials | Continuous coverage of real API | Quota burn; requires CI secret management; risk of accidentally destructive op |
| (m) Skip e2e entirely | Zero risk | No safety net for "we got the auth header wrong" |

We choose (k). Manual smoke before release is the right balance for a personal-use daemon.

## Consequences

**Positive:**

- Each test layer catches a distinct bug class — pure tests catch parser bugs, wiremock catches HTTP-shape bugs, e2e catches "we got the wire format wrong" bugs.
- The wiremock layer is fast (in-process) and isolated (per-test mock server) — no shared state, no flakes.
- Co-located unit tests (`mod tests` in each file) keep the test next to the code; reading the code shows the contract.
- `nextest` parallelism scales the test suite as it grows.
- The header-injection test surface is explicit — there's a place where this attack class is tested, not "implicit in dispatch."
- Fixtures captured from real Gmail responses mean wiremock tests stay realistic without re-recording.

**Negative:**

- Fixtures (`tests/fixtures/gmail/*.json`) need refresh when Gmail meaningfully changes its response schemas. Mitigated by capturing fixtures via a documented script (`scripts/refresh-fixtures.sh`) that re-runs the captures.
- The `GmailClient::new` parameterization on `base_url` is a small refactor. Trivial.
- E2E tests require maintaining a dedicated test Google account. Acceptable cost.
- No CI-enforced coverage gate means "is this PR tested" requires reviewer judgment. Acceptable for one-maintainer scale; would tighten if more contributors join.
- The retry-policy tests against wiremock require careful timing assertions (e.g., "retried within 5 seconds total"). Use `tokio::time::pause()` / `tokio::time::advance()` to make them deterministic.

**Risks:**

- *Risk:* wiremock-based tests pass but the real Gmail API behaves differently (e.g., a header we don't set is now required).
  *Mitigation:* The Layer 3 e2e smoke tests catch this — that's their job. Run them before each release; document in release process.
- *Risk:* Fixtures contain sensitive data (test account email addresses, message IDs).
  *Mitigation:* Fixture-capture script sanitizes sensitive fields. Manual review of new fixtures before commit.
- *Risk:* Concurrency tests are inherently nondeterministic; a race that exists 1% of the time may pass the test.
  *Mitigation:* Run concurrency tests under `--test-threads=8` repeatedly in CI to surface low-probability races. If a race is suspected but not reproducible, reach for `loom`.
- *Risk:* `nextest` introduces config drift from `cargo test` defaults; behaviors differ between local-with-cargo-test and CI-with-nextest.
  *Mitigation:* Document nextest as the canonical runner in `CONTRIBUTING.md`. CI is the source of truth.
- *Risk:* The "no destructive e2e ops" guard (`GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1`) is a runtime check, not a compile check. A test could forget the guard.
  *Mitigation:* Code review; consider making the guard a `tokio::sync::OnceCell` initialized at e2e module load that all destructive ops `expect(...)`.

## References

- [ADR-0002](0002-multi-account-architecture.md) — concurrency tests for hot-reload + snapshot behavior
- [ADR-0004](0004-oauth-token-refresh.md) — refresh-flow tests (success, `invalid_grant`, rotation, transient)
- [ADR-0005](0005-error-model.md) — every error variant gets at least one wiremock test exercising the path that produces it
- [`wiremock`](https://docs.rs/wiremock) — HTTP mocking
- [`cargo-nextest`](https://nexte.st/) — test runner
- [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) — coverage tool (local use only)
- [`insta`](https://docs.rs/insta) — snapshot testing (optional, for rfc2822 composition)
