#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! Layer 3 — search-excellence verification suite (issue #193).
//!
//! [SPEC.md §Search-excellence checklist](../../SPEC.md) lists 8 concrete,
//! testable claims about the v1.0 Gmail search surface. This module turns each
//! prose claim into an executable verification so the maintainer can prove —
//! not assert — that the search story holds, then tick the SPEC checkbox with
//! the run that demonstrated it.
//!
//! All tests here are `#[ignore]` and require `GOOGLE_MCP_TEST_CONFIG_DIR`
//! pointing at a dedicated test installation (see `tests/README.md`). They are
//! read-only — no `GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E` gate needed. Run with:
//!
//! ```sh
//! GOOGLE_MCP_TEST_CONFIG_DIR=/path/to/test-config \
//!   cargo nextest run -E 'test(claim)' -- --ignored
//! ```
//!
//! ## Claim → verification map
//!
//! | # | SPEC claim | Verified by |
//! |---|------------|-------------|
//! | 1 | Gmail web-UI query syntax works in `search_threads` | [`claim1_gmail_query_syntax_executes`] (executes ≥20 representative queries; parity vs web UI is a human spot-check — see test doc) |
//! | 2 | ≥1000-thread query paginates via `page_token`, no drops/dupes | [`claim2_large_result_pagination_no_dupes`] (env-gated) |
//! | 3 | One `search_threads` call carries enough metadata for "senders + subjects of 20 unread" | [`claim3_single_call_metadata_sufficiency`] |
//! | 4 | Zero-result query → clean empty list, `next_page_token: null`, never an error | [`claim4_zero_results_clean_empty_list`] |
//! | 5 | Quota-exhausting query → typed `RateLimited` with `retry_after_secs` | **Synthetic** — `src/rate_limit.rs` (`keyed_limiter_returns_rate_limited_on_exhaustion`) and `src/project_quota.rs` (`registry_returns_rate_limited_on_exhaustion`). Not exercised here: safely exhausting a real 6,000-unit/min budget over the stdio harness is impractical and the issue permits synthetic verification. |
//! | 6 | Same query against two accounts is scoped, no cross-talk | [`claim6_two_account_scoping`] (env-gated on a second alias) |
//! | 7 | Expired access token refreshes transparently | [`claim7_token_refresh_is_transparent`] (see test doc for how to force expiry) |
//! | 8 | Rich search at `max_results=25` costs ~1010 units; sustained rate fits budget | **Cost model** — `src/tools/search_threads.rs` documents `10 + 25×40 = 1010` units. End-to-end unit accounting is observable via the Prometheus counters (#75) over the HTTP transport, not the stdio harness used here. Note: `6000 / 1010 = 5.9`, so **5** sustained rich searches/min fit under the cap (the SPEC's "~6" is optimistic by one). |
//!
//! Claims 1–4 and 6–7 are mechanically verifiable through the stdio MCP
//! harness; claims 5 and 8 are documented above with their verification path.

use std::collections::HashSet;

use serde_json::json;

use super::harness::{require_test_config_dir, McpProcess};

/// Default account alias provisioned by `tests/README.md` setup.
const TEST_ALIAS: &str = "test";

/// Representative Gmail query-syntax features that work in the Gmail web UI.
/// Each must execute cleanly via `search_threads` (return a well-formed list,
/// never an RPC error). True result-parity against the web UI is a human
/// spot-check; this constant is the agreed ≥20-query corpus to compare.
const REPRESENTATIVE_QUERIES: &[&str] = &[
    "in:inbox",
    "in:sent",
    "in:anywhere",
    "is:unread",
    "is:read",
    "is:starred",
    "is:important",
    "has:attachment",
    "label:inbox",
    "from:me",
    "to:me",
    "subject:hello",
    "newer_than:1y",
    "older_than:10y",
    "category:primary",
    "category:social",
    "larger:1M",
    "smaller:10M",
    "in:inbox is:unread",
    "from:me has:attachment",
    "subject:(invoice OR receipt)",
    "-in:chats",
];

/// Pull the parsed `items` array out of a `search_threads` response, asserting
/// the listing envelope shape (`items` present and an array) along the way.
fn items_of(result: &serde_json::Value) -> Vec<serde_json::Value> {
    result["items"]
        .as_array()
        .unwrap_or_else(|| panic!("search_threads response missing `items` array: {result}"))
        .clone()
}

// ── Claim 1 — Gmail query-syntax parity ────────────────────────────────────────

/// Every representative Gmail-web query executes via `search_threads` without
/// an RPC error and returns a well-formed listing. `call_tool` panics on RPC
/// error, so reaching the assertions proves each query was accepted.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
fn claim1_gmail_query_syntax_executes() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    assert!(
        REPRESENTATIVE_QUERIES.len() >= 20,
        "SPEC requires spot-checking ≥20 representative queries; corpus has {}",
        REPRESENTATIVE_QUERIES.len()
    );

    for query in REPRESENTATIVE_QUERIES {
        let result = proc.call_tool(
            "search_threads",
            json!({ "account": TEST_ALIAS, "query": query, "max_results": 5 }),
        );
        let items = items_of(&result);
        assert!(
            items.len() <= 5,
            "query `{query}` must respect max_results=5, got {}",
            items.len()
        );
        for item in &items {
            assert!(
                !item["thread_id"].as_str().unwrap_or_default().is_empty(),
                "query `{query}` returned an item with empty thread_id: {item}"
            );
        }
    }
}

// ── Claim 2 — large-result pagination, no drops or duplicates ──────────────────

/// Paginate a query that returns ≥1000 threads in the test account and verify
/// `page_token` walks the full window without dropping or duplicating a
/// `thread_id`. Gated on `GOOGLE_MCP_TEST_LARGE_QUERY` — a query the maintainer
/// knows returns ≥1000 threads (e.g. `in:anywhere`). Skips cleanly if unset.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR and a ≥1000-result GOOGLE_MCP_TEST_LARGE_QUERY"]
fn claim2_large_result_pagination_no_dupes() {
    let dir = require_test_config_dir();
    let Ok(query) = std::env::var("GOOGLE_MCP_TEST_LARGE_QUERY") else {
        eprintln!(
            "claim2: set GOOGLE_MCP_TEST_LARGE_QUERY to a query returning ≥1000 threads \
             in the test account (e.g. in:anywhere) — skipping"
        );
        return;
    };

    let mut proc = McpProcess::start(&dir);

    let mut seen: HashSet<String> = HashSet::new();
    let mut page_token: Option<String> = None;
    // 1000 threads / 100 per page = 10 pages minimum. Cap at 20 to bound a
    // runaway loop while still covering the full ≥1000 window.
    let mut pages = 0;
    loop {
        let mut args = json!({ "account": TEST_ALIAS, "query": query, "max_results": 100 });
        if let Some(token) = &page_token {
            args["page_token"] = json!(token);
        }
        let result = proc.call_tool("search_threads", args);
        for item in items_of(&result) {
            let id = item["thread_id"].as_str().unwrap_or_default().to_owned();
            assert!(
                !id.is_empty(),
                "paginated result contained an empty thread_id"
            );
            assert!(
                seen.insert(id.clone()),
                "duplicate thread_id `{id}` across pages — pagination is dropping or repeating"
            );
        }
        pages += 1;
        match result["next_page_token"].as_str() {
            Some(token) if pages < 20 => page_token = Some(token.to_owned()),
            _ => break,
        }
    }

    assert!(
        seen.len() >= 1000,
        "expected ≥1000 unique threads across pages, collected {} — \
         is GOOGLE_MCP_TEST_LARGE_QUERY broad enough?",
        seen.len()
    );
}

// ── Claim 3 — single-call metadata sufficiency ─────────────────────────────────

/// A single `search_threads` call must carry enough per-thread metadata to
/// answer "show me the senders and subjects of the 20 most recent unread
/// emails" with no follow-up `get_thread` per result. We make exactly one tool
/// call and assert every row exposes a usable subject, from, and date.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
fn claim3_single_call_metadata_sufficiency() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    let result = proc.call_tool(
        "search_threads",
        json!({ "account": TEST_ALIAS, "query": "in:inbox", "max_results": 20 }),
    );
    let items = items_of(&result);
    if items.is_empty() {
        eprintln!("claim3: test inbox is empty — seed it to exercise this claim; skipping asserts");
        return;
    }

    for item in &items {
        // Untrusted fields serialize as wrapped strings (ADR-0018); their
        // presence + non-emptiness is what makes a follow-up fetch unnecessary.
        for field in ["subject_untrusted", "from_untrusted"] {
            let val = item[field].as_str().unwrap_or_default();
            assert!(
                !val.is_empty(),
                "row is missing `{field}` — host LLM would need a get_thread: {item}"
            );
            assert!(
                val.contains("UNTRUSTED"),
                "`{field}` must be wrapped per ADR-0018: {val}"
            );
        }
        assert!(
            !item["internal_date"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "row is missing internal_date (needed to order 'most recent'): {item}"
        );
        assert!(
            !item["thread_id"].as_str().unwrap_or_default().is_empty(),
            "row is missing thread_id: {item}"
        );
    }
}

// ── Claim 4 — zero results return a clean empty list ───────────────────────────

/// A query that matches nothing must return a clean empty list — never an RPC
/// error — with `next_page_token: null`. `call_tool` panics on RPC error, so a
/// passing run proves the no-error half; the assertions prove the shape.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
fn claim4_zero_results_clean_empty_list() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    // A subject token overwhelmingly unlikely to exist in any inbox.
    let result = proc.call_tool(
        "search_threads",
        json!({
            "account": TEST_ALIAS,
            "query": "subject:zzzq-no-such-thread-9f3a1c7e8b2d",
            "max_results": 25
        }),
    );

    assert!(
        items_of(&result).is_empty(),
        "expected an empty items list for a no-match query: {result}"
    );
    assert!(
        result["next_page_token"].is_null(),
        "next_page_token must be null for a zero-result page, got: {}",
        result["next_page_token"]
    );
}

// ── Claim 6 — two-account scoping, no cross-talk ───────────────────────────────

/// The same query against two different accounts returns results scoped to
/// each, with no cross-talk. Gated on `GOOGLE_MCP_TEST_SECOND_ALIAS` — a second
/// authorized alias in the test installation. We assert both accounts answer
/// and that their `in:inbox` thread-id sets do not bleed into each other.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR and a second GOOGLE_MCP_TEST_SECOND_ALIAS account"]
fn claim6_two_account_scoping() {
    let dir = require_test_config_dir();
    let Ok(second) = std::env::var("GOOGLE_MCP_TEST_SECOND_ALIAS") else {
        eprintln!(
            "claim6: set GOOGLE_MCP_TEST_SECOND_ALIAS to a second authorized alias — skipping"
        );
        return;
    };

    let mut proc = McpProcess::start(&dir);

    let ids_for = |proc: &mut McpProcess, alias: &str| -> HashSet<String> {
        let result = proc.call_tool(
            "search_threads",
            json!({ "account": alias, "query": "in:inbox", "max_results": 50 }),
        );
        items_of(&result)
            .iter()
            .map(|i| i["thread_id"].as_str().unwrap_or_default().to_owned())
            .collect()
    };

    let first_ids = ids_for(&mut proc, TEST_ALIAS);
    let second_ids = ids_for(&mut proc, &second);

    // Gmail thread IDs are account-local; an identical ID appearing in both
    // sets would indicate the daemon served one account's data for the other.
    let overlap: Vec<&String> = first_ids.intersection(&second_ids).collect();
    assert!(
        overlap.is_empty(),
        "thread_id cross-talk between `{TEST_ALIAS}` and `{second}`: {overlap:?}"
    );
}

// ── Claim 7 — transparent token refresh ────────────────────────────────────────

/// An expired access token refreshes transparently (ADR-0004) with no
/// operator-visible failure. To *force* a refresh, expire the stored token
/// before running:
///
/// ```sh
/// # Edit the token file's expires_at to a past timestamp, then:
/// GOOGLE_MCP_TEST_CONFIG_DIR=... cargo nextest run -E 'test(claim7)' -- --ignored
/// ```
///
/// Without that manipulation this test still asserts the steady-state
/// invariant: a search succeeds even when the daemon must transparently mint a
/// fresh access token, surfacing no error to the caller.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
fn claim7_token_refresh_is_transparent() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    // A bare call that must succeed regardless of whether a refresh was needed.
    // call_tool panics on RPC error, so a refresh failure would fail the test.
    let result = proc.call_tool(
        "search_threads",
        json!({ "account": TEST_ALIAS, "query": "in:inbox", "max_results": 1 }),
    );
    let _ = items_of(&result); // shape assertion; value irrelevant here
}
