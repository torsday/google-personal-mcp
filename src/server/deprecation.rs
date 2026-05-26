//! Tool-deprecation infrastructure per
//! [ADR-0015 §Deprecation procedure](../../docs/adr/0015-tool-versioning-policy.md).
//!
//! Pre-1.0 there are no deprecated tools to test against — this module ships
//! the *runtime mechanics* so the day v1.0 retires its first tool the
//! plumbing is already in place. The production registry returned by
//! [`production`] is empty; deprecation entries are added here when a tool
//! actually starts its sunset clock.
//!
//! ## Source of truth
//!
//! `is this tool deprecated` is answered by the [`Registry`] map, **not** by
//! parsing the description text. The description prefix
//! (`[DEPRECATED — use … — sunset YYYY-MM-DD]`) is operator-visible
//! formatting derived *from* the registry — never the other way around.
//! This means a typo or formatting drift in the description can't make a
//! tool silently stop emitting deprecation telemetry.
//!
//! ## Sunset-date format
//!
//! ISO-8601 (`YYYY-MM-DD`) so the description-prefix renders without further
//! formatting and the structured tracing field is sortable lexically.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::NaiveDate;

/// Metadata for one deprecated tool. Lives in the [`Registry`] under the
/// tool's exact registered name.
#[derive(Debug, Clone)]
pub(crate) struct Deprecation {
    /// ISO date after which the tool will be removed entirely. Operators
    /// see this in the description prefix and in tracing logs; tooling
    /// can scrape it from `mcp_status` for sunset countdowns.
    pub sunset_date: NaiveDate,
    /// Successor tool name to recommend in the deprecation banner. May
    /// be the empty string if there is no direct replacement (rare; the
    /// ADR-0015 procedure prefers having one).
    pub replacement: String,
}

impl Deprecation {
    /// Banner inserted at the start of a deprecated tool's description.
    /// Trailing space ensures the rest of the description follows
    /// cleanly. Format matches the ADR-0015 convention:
    /// `[DEPRECATED — use {new_tool} — sunset {YYYY-MM-DD}]`.
    pub(crate) fn description_prefix(&self) -> String {
        format!(
            "[DEPRECATED — use {replacement} — sunset {sunset}] ",
            replacement = self.replacement,
            sunset = self.sunset_date,
        )
    }
}

/// Map of `tool_name -> Deprecation`. Snapshot at server-construction
/// time; never mutated at runtime (changing deprecations requires a
/// daemon restart so the description-prefix is consistent with what the
/// host LLM already cached).
pub(crate) type Registry = HashMap<String, Deprecation>;

/// Production registry. Empty pre-1.0; add entries here when a tool
/// starts its sunset clock per ADR-0015's procedure (6-month sunset,
/// 12-month for destructive ops).
pub(crate) fn production() -> Registry {
    HashMap::new()
}

/// Process-lifetime counter of deprecated-tool invocations across all
/// accounts and tools. Bumped by the dispatcher whenever `call_tool`
/// resolves to a name in the registry; read by `mcp_status` and the
/// Prometheus exporter (#75, future).
///
/// Lifetime, not rolling — a last-hour window is deferred until #75
/// lands time-bucketed infrastructure (same posture taken for cache
/// hit-rate in #83). Documented on the `mcp_status` field.
static DEPRECATED_TOOL_INVOCATIONS: AtomicU64 = AtomicU64::new(0);

/// Bump the global counter. Cheap: `Relaxed` add on an `AtomicU64`.
pub(crate) fn record_invocation() {
    DEPRECATED_TOOL_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
}

/// Read the global counter. Snapshot only; the value can change
/// between this load and the caller using it.
pub(crate) fn invocations_total() -> u64 {
    DEPRECATED_TOOL_INVOCATIONS.load(Ordering::Relaxed)
}

/// Test-only: reset the counter so unit tests can run in any order
/// without cross-contamination. Not exposed in production.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    DEPRECATED_TOOL_INVOCATIONS.store(0, Ordering::Relaxed);
}

/// Emit the structured tracing WARN and bump the counter. Single entry
/// point so the dispatcher and unit tests exercise identical behavior.
pub(crate) fn on_deprecated_invocation(tool_name: &str, deprecation: &Deprecation) {
    tracing::warn!(
        tool.name = tool_name,
        tool.deprecated = true,
        tool.sunset_date = %deprecation.sunset_date,
        tool.replacement = %deprecation.replacement,
        "deprecated tool invoked; see ADR-0015",
    );
    record_invocation();
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn fixture() -> Deprecation {
        Deprecation {
            sunset_date: NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(),
            replacement: "list_threads_v2".into(),
        }
    }

    #[test]
    fn description_prefix_renders_adr_0015_format() {
        let d = fixture();
        let prefix = d.description_prefix();
        assert_eq!(
            prefix,
            "[DEPRECATED — use list_threads_v2 — sunset 2026-12-31] ",
        );
    }

    #[test]
    fn production_registry_is_empty_pre_v1() {
        assert!(production().is_empty(), "no deprecations expected pre-1.0");
    }

    /// Counter is process-global; reset bracket isolates this test.
    #[test]
    fn on_deprecated_invocation_bumps_counter() {
        reset_for_tests();
        assert_eq!(invocations_total(), 0);
        on_deprecated_invocation("list_threads", &fixture());
        assert_eq!(invocations_total(), 1);
        on_deprecated_invocation("list_threads", &fixture());
        assert_eq!(invocations_total(), 2);
        reset_for_tests();
    }

    /// Two distinct deprecated tools share the same counter — that's
    /// the documented semantic (lifetime total across all deprecated
    /// invocations).
    #[test]
    fn counter_is_global_across_tools() {
        reset_for_tests();
        on_deprecated_invocation("a", &fixture());
        on_deprecated_invocation("b", &fixture());
        assert_eq!(invocations_total(), 2);
        reset_for_tests();
    }
}
