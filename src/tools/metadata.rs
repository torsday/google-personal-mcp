//! Per-tool metadata used by request-layer guards.
//!
//! [`is_destructive`] is the single source of truth for "this tool mutates
//! Gmail state" — consumed by the fan-out validator in
//! [`crate::server`] to reject `account = "*"` on destructive tools per
//! [ADR-0013](../../docs/adr/0013-cross-account-fan-out.md). One list, no
//! drift: if a future destructive tool is added to the dispatch table,
//! add its name here and the fan-out rejection picks it up automatically.

/// Every destructive tool name as it appears in the MCP `call_tool`
/// request's `name` field. Maintained in lockstep with
/// `crate::server::descriptors::registered_tools` and the dispatch arms in
/// `crate::server::GoogleServer::call_tool`.
///
/// Order is intentional: single-thread variant first, then its `batch_`
/// sibling — the same grouping used in `registered_tools`.
const DESTRUCTIVE_TOOLS: &[&str] = &[
    "archive_thread",
    "batch_archive",
    "trash_thread",
    "batch_trash",
    "modify_thread_labels",
    "batch_modify_thread_labels",
    // `send_email` is not yet wired into call_tool dispatch (separate ticket)
    // but is unambiguously destructive — list it here so the guard is ready
    // the moment the dispatch arm lands.
    "send_email",
];

/// `true` when the named tool mutates Gmail state and therefore cannot
/// safely fan out via `account = "*"`.
///
/// Unknown tool names return `false` — the dispatch layer rejects them
/// separately with `"unknown tool"`; the fan-out guard is not the right
/// place to enforce tool existence.
pub(crate) fn is_destructive(tool_name: &str) -> bool {
    DESTRUCTIVE_TOOLS.contains(&tool_name)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // ── Exhaustive: every known destructive tool must register as destructive ──

    #[test]
    fn archive_thread_is_destructive() {
        assert!(is_destructive("archive_thread"));
    }

    #[test]
    fn batch_archive_is_destructive() {
        assert!(is_destructive("batch_archive"));
    }

    #[test]
    fn trash_thread_is_destructive() {
        assert!(is_destructive("trash_thread"));
    }

    #[test]
    fn batch_trash_is_destructive() {
        assert!(is_destructive("batch_trash"));
    }

    #[test]
    fn modify_thread_labels_is_destructive() {
        assert!(is_destructive("modify_thread_labels"));
    }

    #[test]
    fn batch_modify_thread_labels_is_destructive() {
        assert!(is_destructive("batch_modify_thread_labels"));
    }

    #[test]
    fn send_email_is_destructive_even_though_not_yet_dispatched() {
        assert!(is_destructive("send_email"));
    }

    // ── Exhaustive: every known read-only tool must NOT register as destructive ──

    #[test]
    fn list_accounts_is_not_destructive() {
        assert!(!is_destructive("list_accounts"));
    }

    #[test]
    fn list_labels_is_not_destructive() {
        assert!(!is_destructive("list_labels"));
    }

    #[test]
    fn search_threads_is_not_destructive() {
        assert!(!is_destructive("search_threads"));
    }

    #[test]
    fn get_thread_is_not_destructive() {
        assert!(!is_destructive("get_thread"));
    }

    // ── Unknown tool names return false, not panic ───────────────────────────

    #[test]
    fn unknown_tool_returns_false() {
        assert!(!is_destructive("not_a_real_tool"));
        assert!(!is_destructive(""));
        assert!(!is_destructive("ARCHIVE_THREAD")); // case-sensitive
    }
}
