//! Tool aspect classification per
//! [ADR-0022 §The three aspects](../../docs/adr/0022-capability-gating.md).
//!
//! Every tool the daemon dispatches is classified into exactly one [`Aspect`].
//! The aspect is the unit the capability-gating layer toggles on: an operator
//! enables or disables a service's `read` / `write` / `destructive` aspect, and
//! the per-call gate refuses any tool whose aspect is closed for the calling
//! account.
//!
//! This generalizes the older `is_destructive()` boolean (per
//! [ADR-0011](../../docs/adr/0011-audit-log.md)) into a three-valued
//! classification; `is_destructive` is now a convenience over
//! [`Aspect::Destructive`]. The classifier itself — the single source of truth
//! mapping each tool name to its aspect — lives in
//! [`super::metadata::aspect`], next to the `is_destructive` it backs.

use serde::{Deserialize, Serialize};

/// The mutation class of a tool, in increasing order of blast radius.
///
/// Exactly one aspect applies to any given tool. The boundaries follow
/// [ADR-0022 §The three aspects](../../docs/adr/0022-capability-gating.md):
///
/// - [`Read`](Aspect::Read) — no mutation of Google-side state; idempotent and
///   side-effect-free (`search_threads`, `get_thread`, `list_labels`).
/// - [`Write`](Aspect::Write) — creates or modifies state, but the change is
///   recoverable / low-blast (`modify_thread_labels`, `archive_thread`).
/// - [`Destructive`](Aspect::Destructive) — irreversible, externally visible, or
///   high-blast-radius (`send_email`, `trash_thread`, `purge_account`).
///
/// `serde` uses `snake_case` so the variant names match the config keys in
/// `[services.<name>.capabilities]` (ADR-0022 §Config shape) — `read`,
/// `write`, `destructive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Aspect {
    /// No mutation of Google-side state. Idempotent, side-effect-free.
    Read,
    /// Creates or modifies state, but the change is recoverable / low-blast.
    Write,
    /// Irreversible, externally visible, or high-blast-radius.
    Destructive,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_snake_case() {
        // Config interop: the wire form must match the TOML capability keys.
        assert_eq!(
            serde_json::to_string(&Aspect::Read).expect("serialize"),
            "\"read\""
        );
        assert_eq!(
            serde_json::to_string(&Aspect::Write).expect("serialize"),
            "\"write\""
        );
        assert_eq!(
            serde_json::to_string(&Aspect::Destructive).expect("serialize"),
            "\"destructive\""
        );
    }

    #[test]
    fn deserializes_from_snake_case() {
        assert_eq!(
            serde_json::from_str::<Aspect>("\"read\"").expect("deserialize"),
            Aspect::Read
        );
        assert_eq!(
            serde_json::from_str::<Aspect>("\"destructive\"").expect("deserialize"),
            Aspect::Destructive
        );
    }

    #[test]
    fn rejects_unknown_aspect() {
        // A typo'd capability key must be loud, not silently coerced.
        assert!(serde_json::from_str::<Aspect>("\"delete\"").is_err());
    }
}
