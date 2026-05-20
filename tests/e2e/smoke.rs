#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! Layer 3 — read-only end-to-end smoke tests against real Gmail.
//!
//! All tests are `#[ignore]` — run them manually with:
//!
//! ```sh
//! GOOGLE_MCP_TEST_CONFIG_DIR=/path/to/test-config \
//!   cargo nextest run -- --ignored
//! ```
//!
//! See `tests/README.md` for setup instructions.

use super::harness::{require_test_config_dir, McpProcess};

/// `list_accounts` returns at least the "test" alias with a non-empty email.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
fn smoke_list_accounts_returns_test_alias() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    let result = proc.call_tool("list_accounts", serde_json::json!({}));
    let accounts = result["accounts"]
        .as_array()
        .expect("list_accounts.accounts should be an array");

    assert!(
        !accounts.is_empty(),
        "expected at least one account in test installation"
    );
    let test_account = accounts
        .iter()
        .find(|a| a["alias"].as_str() == Some("test"))
        .expect("expected an account with alias=\"test\" in the test installation");
    assert!(
        !test_account["email"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "test account email should not be empty"
    );
}

/// `list_labels` returns a non-empty label set for the test account.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
fn smoke_list_labels_returns_labels() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    let result = proc.call_tool("list_labels", serde_json::json!({ "account": "test" }));
    let labels = result["labels"]
        .as_array()
        .expect("list_labels.labels should be an array");
    assert!(
        !labels.is_empty(),
        "expected at least one label (e.g. INBOX) in the test account"
    );
    // Every label must have a non-empty id and name.
    for label in labels {
        assert!(
            !label["id"].as_str().unwrap_or_default().is_empty(),
            "label.id should not be empty: {label}"
        );
        assert!(
            !label["name"].as_str().unwrap_or_default().is_empty(),
            "label.name should not be empty: {label}"
        );
    }
}

/// `get_thread` returns a non-error result for a thread that exists.
///
/// The test discovers a thread via the raw Gmail API then retrieves it via
/// the MCP tool. If the inbox is empty, the test is skipped.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR pointing at a test installation"]
fn smoke_get_thread_returns_thread_data() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    // First get a thread_id by calling search_threads (pending #9 — works if
    // that issue is done by the time this test is run). For now fall back to
    // reading a thread ID from GOOGLE_MCP_TEST_THREAD_ID if set.
    let Ok(thread_id) = std::env::var("GOOGLE_MCP_TEST_THREAD_ID") else {
        eprintln!("smoke_get_thread: set GOOGLE_MCP_TEST_THREAD_ID to a known thread ID, or implement search_threads first (#9)");
        return; // skip gracefully without failing
    };

    let result = proc.call_tool(
        "get_thread",
        serde_json::json!({ "account": "test", "thread_id": thread_id }),
    );

    assert_eq!(
        result["thread_id"].as_str().unwrap_or_default(),
        thread_id,
        "get_thread should return the requested thread_id"
    );
    // Messages array must be present (may be empty for a new draft thread).
    assert!(
        result.get("messages").is_some(),
        "get_thread result should have a messages field"
    );
}

/// `search_threads` (blocked on issue #9 — will be skipped until that tool
/// is implemented).
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR; also blocked on search_threads implementation (#9)"]
fn smoke_search_threads_returns_results() {
    let dir = require_test_config_dir();
    let mut proc = McpProcess::start(&dir);

    let result = proc.call_tool(
        "search_threads",
        serde_json::json!({
            "account": "test",
            "query": "in:inbox",
            "max_results": 5
        }),
    );

    let threads = result["threads"]
        .as_array()
        .expect("search_threads.threads should be an array");
    assert!(
        threads.len() <= 5,
        "search_threads should respect max_results=5"
    );
    for thread in threads {
        assert!(
            !thread["thread_id"].as_str().unwrap_or_default().is_empty(),
            "each thread should have a non-empty thread_id"
        );
    }
}
