//! Per-tool metadata used by request-layer guards.
//!
//! [`aspect`] is the single source of truth for a tool's mutation class
//! (`read` / `write` / `destructive`) per
//! [ADR-0022 §The three aspects](../../docs/adr/0022-capability-gating.md). It
//! generalizes the older destructive/not-destructive boolean: [`is_destructive`]
//! is now a convenience over [`Aspect::Destructive`].
//!
//! [`is_fanout_eligible`] (aspect == read) is the predicate consumed by the
//! fan-out validator in [`crate::server`] to reject `account = "*"` on any
//! non-read tool per
//! [ADR-0013](../../docs/adr/0013-cross-account-fan-out.md). One table, no
//! drift: if a future tool is added to the dispatch surface, add its name and
//! aspect to [`TOOL_ASPECTS`] and the fan-out guard, the audit surface, and the
//! capability-gating layer all pick it up automatically.

use super::aspect::Aspect;

/// Every dispatchable tool name paired with its [`Aspect`], per the
/// classification tables in
/// [ADR-0022 §The three aspects](../../docs/adr/0022-capability-gating.md).
/// Maintained in lockstep with
/// `crate::server::descriptors::registered_tools` and the dispatch arms in
/// `crate::server::GoogleServer::call_tool`.
///
/// Aspect rationale for the non-obvious entries (the rest follow directly from
/// the ADR-0022 examples):
///
/// - `cache_invalidate` is **write**, not read: it mutates daemon-local cache
///   state, but the change is fully recoverable — the next fetch repopulates
///   from Google, so it is low-blast, never destructive.
/// - `purge_account` is **destructive**: it irreversibly drops all local state
///   for an account ([ADR-0019](../../docs/adr/0019-data-retention-and-purge.md)).
/// - `send_email` is **destructive**, not write: it cannot be unsent and is
///   externally visible ([ADR-0012](../../docs/adr/0012-idempotency-and-dry-run.md)).
///   It is listed here even though its `call_tool` dispatch arm is not yet
///   wired, so the guard and gate are ready the moment it lands.
/// - `mcp_status` / `cache_status` are **read** — daemon introspection, no
///   Google mutation.
const TOOL_ASPECTS: &[(&str, Aspect)] = &[
    // ── read ────────────────────────────────────────────────────────────────
    ("list_accounts", Aspect::Read),
    ("list_labels", Aspect::Read),
    ("mcp_status", Aspect::Read),
    ("audit_summary", Aspect::Read),
    ("search_threads", Aspect::Read),
    ("get_thread", Aspect::Read),
    ("get_message", Aspect::Read),
    ("get_full_body", Aspect::Read),
    ("list_attachments", Aspect::Read),
    ("download_attachment", Aspect::Read),
    ("cache_status", Aspect::Read),
    ("list_calendars", Aspect::Read),
    ("list_events", Aspect::Read),
    ("get_event", Aspect::Read),
    ("query_freebusy", Aspect::Read),
    ("list_contacts", Aspect::Read),
    ("search_contacts", Aspect::Read),
    ("get_contact", Aspect::Read),
    ("list_contact_groups", Aspect::Read),
    ("get_contact_group", Aspect::Read),
    // ── write ───────────────────────────────────────────────────────────────
    ("cache_invalidate", Aspect::Write),
    ("archive_thread", Aspect::Write),
    ("batch_archive", Aspect::Write),
    ("modify_thread_labels", Aspect::Write),
    ("batch_modify_thread_labels", Aspect::Write),
    // ── destructive ─────────────────────────────────────────────────────────
    ("purge_account", Aspect::Destructive),
    ("trash_thread", Aspect::Destructive),
    ("batch_trash", Aspect::Destructive),
    ("send_email", Aspect::Destructive),
];

/// The [`Aspect`] of the named tool, or `None` for an unknown name.
///
/// Unknown names return `None` rather than a default aspect: a tool that is
/// not in [`TOOL_ASPECTS`] has *no declared aspect*, and silently defaulting it
/// (e.g. to `read`) is exactly the misclassification ADR-0022 guards against.
/// The dispatch layer rejects unknown tool names separately with
/// `"unknown tool"`; callers that need a classification for a dispatchable tool
/// can rely on the Layer-4 snapshot and the
/// `server::descriptors::tests::every_registered_tool_declares_an_aspect` test
/// to guarantee `Some`.
pub(crate) fn aspect(tool_name: &str) -> Option<Aspect> {
    TOOL_ASPECTS
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, aspect)| *aspect)
}

/// `true` when the named tool is irreversible / externally visible /
/// high-blast-radius — i.e. its aspect is [`Aspect::Destructive`].
///
/// Convenience over [`aspect`]. Unknown tool names return `false` (their aspect
/// is `None`). Consumed by the ADR-0011 audit surface (`audit_summary`'s
/// `recent_destructive` shortlist) and the descriptor sanity checks.
pub(crate) fn is_destructive(tool_name: &str) -> bool {
    aspect(tool_name) == Some(Aspect::Destructive)
}

/// `true` when the named tool may be invoked with the cross-account fan-out
/// wildcard `account = "*"` — that is, when it is a [`Aspect::Read`] tool.
///
/// Per [ADR-0013](../../docs/adr/0013-cross-account-fan-out.md), fan-out is a
/// **read-tool affordance only**: write and destructive tools must target a
/// single account so one mistaken `"*"` call cannot mutate every registered
/// account at once. This is the precise predicate the fan-out guard needs —
/// the older `is_destructive` check happened to reject the same dispatched
/// tools only because every mutating tool was historically lumped into the
/// "destructive" list; aspect classification (ADR-0022) separates `write` from
/// `destructive`, so the guard must key off "not read" rather than "is
/// destructive" to keep rejecting writes.
///
/// Unknown tool names return `false` (no declared aspect ⇒ not fan-out
/// eligible) — the safe default; dispatch rejects unknown names separately.
pub(crate) fn is_fanout_eligible(tool_name: &str) -> bool {
    aspect(tool_name) == Some(Aspect::Read)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // ── aspect classification (ADR-0022) ──────────────────────────────────────

    #[test]
    fn read_tools_classify_as_read() {
        for name in [
            "list_accounts",
            "list_labels",
            "mcp_status",
            "audit_summary",
            "search_threads",
            "get_thread",
            "get_message",
            "get_full_body",
            "list_attachments",
            "download_attachment",
            "cache_status",
            "list_calendars",
            "list_events",
            "get_event",
            "query_freebusy",
            "list_contacts",
            "search_contacts",
            "get_contact",
            "list_contact_groups",
            "get_contact_group",
        ] {
            assert_eq!(aspect(name), Some(Aspect::Read), "{name} should be read");
        }
    }

    #[test]
    fn write_tools_classify_as_write() {
        for name in [
            "cache_invalidate",
            "archive_thread",
            "batch_archive",
            "modify_thread_labels",
            "batch_modify_thread_labels",
        ] {
            assert_eq!(aspect(name), Some(Aspect::Write), "{name} should be write");
        }
    }

    #[test]
    fn destructive_tools_classify_as_destructive() {
        for name in ["purge_account", "trash_thread", "batch_trash", "send_email"] {
            assert_eq!(
                aspect(name),
                Some(Aspect::Destructive),
                "{name} should be destructive"
            );
        }
    }

    // The "every registered tool declares an aspect" completeness check lives
    // in `crate::server::descriptors` tests, where `registered_tools()` is in
    // scope — `tools` must not depend on `server`.

    #[test]
    fn unknown_tool_has_no_aspect() {
        assert_eq!(aspect("not_a_real_tool"), None);
        assert_eq!(aspect(""), None);
        assert_eq!(aspect("ARCHIVE_THREAD"), None); // case-sensitive
    }

    // ── is_destructive remains correct as a convenience over aspect() ─────────

    #[test]
    fn archive_thread_is_destructive() {
        // archive is `write`, not destructive — it is recoverable.
        assert!(!is_destructive("archive_thread"));
    }

    #[test]
    fn batch_archive_is_not_destructive() {
        assert!(!is_destructive("batch_archive"));
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
    fn modify_thread_labels_is_not_destructive() {
        // modify is `write` — recoverable label change.
        assert!(!is_destructive("modify_thread_labels"));
    }

    #[test]
    fn send_email_is_destructive_even_though_not_yet_dispatched() {
        assert!(is_destructive("send_email"));
    }

    #[test]
    fn purge_account_is_destructive() {
        assert!(is_destructive("purge_account"));
    }

    // ── read-only tools must NOT register as destructive ──────────────────────

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

    // ── fan-out eligibility (ADR-0013) tracks the read aspect ─────────────────

    #[test]
    fn read_tools_are_fanout_eligible() {
        assert!(is_fanout_eligible("search_threads"));
        assert!(is_fanout_eligible("list_labels"));
    }

    #[test]
    fn write_and_destructive_tools_are_not_fanout_eligible() {
        // The historically-rejected mutation set must stay rejected even though
        // archive/modify are now `write`, not `destructive`.
        for name in [
            "archive_thread",
            "batch_archive",
            "modify_thread_labels",
            "batch_modify_thread_labels",
            "trash_thread",
            "batch_trash",
            "send_email",
            "purge_account",
        ] {
            assert!(
                !is_fanout_eligible(name),
                "{name} must not be fan-out eligible"
            );
        }
    }

    #[test]
    fn unknown_tool_is_not_fanout_eligible() {
        assert!(!is_fanout_eligible("not_a_real_tool"));
        assert!(!is_fanout_eligible(""));
    }
}
