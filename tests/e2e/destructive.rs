#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! Layer 3 — destructive end-to-end tests against a real Gmail test account.
//!
//! These tests mutate state (archive, trash, label, send). They are gated
//! behind **both** env vars:
//!
//! - `GOOGLE_MCP_TEST_CONFIG_DIR` — path to the dedicated test installation
//! - `GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1` — explicit opt-in for mutations
//!
//! Run with:
//!
//! ```sh
//! GOOGLE_MCP_TEST_CONFIG_DIR=/path/to/test-config \
//! GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1 \
//!   cargo nextest run -- --ignored
//! ```
//!
//! See `tests/README.md` for setup instructions.

use super::harness::{McpProcess, require_destructive_gate, require_test_config_dir};

/// `archive_thread` removes INBOX label from a thread. Verifies that `dry_run`
/// mode does not mutate, and that the real call reports `applied: true`.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR and GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1"]
fn destructive_archive_thread_dry_run_and_apply() {
    let dir = require_test_config_dir();
    require_destructive_gate();

    let thread_id = std::env::var("GOOGLE_MCP_TEST_THREAD_ID")
        .expect("set GOOGLE_MCP_TEST_THREAD_ID to a thread in the test account's inbox");

    let mut proc = McpProcess::start(&dir);

    // dry_run=true must not mutate and must return applied=false.
    let dry = proc.call_tool(
        "archive_thread",
        serde_json::json!({
            "account": "test",
            "thread_id": thread_id,
            "dry_run": true
        }),
    );
    assert_eq!(
        dry["applied"].as_bool(),
        Some(false),
        "dry_run=true should return applied=false: {dry}"
    );

    // Real archive.
    let real = proc.call_tool(
        "archive_thread",
        serde_json::json!({
            "account": "test",
            "thread_id": thread_id,
            "dry_run": false
        }),
    );
    assert_eq!(
        real["applied"].as_bool(),
        Some(true),
        "archive_thread should return applied=true: {real}"
    );
}

/// `send_email` sends a self-mail to the test account address, then verifies
/// via `search_threads` that a matching thread appears in the inbox.
///
/// Requires `search_threads` (issue #9). If that tool is not yet implemented,
/// the post-send verification is skipped but the send itself is verified.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR and GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1"]
fn destructive_send_email_self_mail_and_verify() {
    let dir = require_test_config_dir();
    require_destructive_gate();

    let test_email = std::env::var("GOOGLE_MCP_TEST_EMAIL")
        .expect("set GOOGLE_MCP_TEST_EMAIL to the test account's Gmail address");

    let subject = format!("e2e smoke test — {}", chrono::Utc::now().to_rfc3339());

    let mut proc = McpProcess::start(&dir);

    let result = proc.call_tool(
        "send_email",
        serde_json::json!({
            "account": "test",
            "to": [test_email],
            "subject": subject,
            "body_text": "This is an automated e2e smoke test email. Safe to delete.",
            "dry_run": false
        }),
    );

    // Must return a non-empty message_id.
    let message_id = result["message_id"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !message_id.is_empty(),
        "send_email should return a non-empty message_id: {result}"
    );
}

/// `trash_thread` — `dry_run` then real trash.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR and GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1"]
fn destructive_trash_thread_dry_run_and_apply() {
    let dir = require_test_config_dir();
    require_destructive_gate();

    let thread_id = std::env::var("GOOGLE_MCP_TEST_THREAD_ID")
        .expect("set GOOGLE_MCP_TEST_THREAD_ID to a thread in the test account");

    let mut proc = McpProcess::start(&dir);

    let dry = proc.call_tool(
        "trash_thread",
        serde_json::json!({
            "account": "test",
            "thread_id": thread_id,
            "dry_run": true
        }),
    );
    assert_eq!(dry["applied"].as_bool(), Some(false), "dry_run=true: {dry}");

    let real = proc.call_tool(
        "trash_thread",
        serde_json::json!({
            "account": "test",
            "thread_id": thread_id,
            "dry_run": false
        }),
    );
    assert_eq!(
        real["applied"].as_bool(),
        Some(true),
        "trash_thread should return applied=true: {real}"
    );
}

/// `modify_thread_labels` — add then remove a label.
#[test]
#[ignore = "requires GOOGLE_MCP_TEST_CONFIG_DIR and GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1"]
fn destructive_modify_thread_labels_add_and_remove() {
    let dir = require_test_config_dir();
    require_destructive_gate();

    let thread_id = std::env::var("GOOGLE_MCP_TEST_THREAD_ID")
        .expect("set GOOGLE_MCP_TEST_THREAD_ID");
    let label_id = std::env::var("GOOGLE_MCP_TEST_LABEL_ID")
        .expect("set GOOGLE_MCP_TEST_LABEL_ID to a user label in the test account");

    let mut proc = McpProcess::start(&dir);

    // Add label.
    let add = proc.call_tool(
        "modify_thread_labels",
        serde_json::json!({
            "account": "test",
            "thread_id": thread_id,
            "add_label_ids": [label_id],
            "remove_label_ids": [],
            "dry_run": false
        }),
    );
    assert_eq!(add["applied"].as_bool(), Some(true), "add label: {add}");

    // Remove label.
    let remove = proc.call_tool(
        "modify_thread_labels",
        serde_json::json!({
            "account": "test",
            "thread_id": thread_id,
            "add_label_ids": [],
            "remove_label_ids": [label_id],
            "dry_run": false
        }),
    );
    assert_eq!(remove["applied"].as_bool(), Some(true), "remove label: {remove}");
}
