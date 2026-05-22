//! `audit_summary` introspection tool per
//! [ADR-0011 §Tool-side surface](../../docs/adr/0011-audit-log.md).
//!
//! Aggregates over the JSONL audit log written by [`crate::audit::AuditWriter`].
//! Returns counts only — never per-record content — so the model can't
//! selectively quote audit lines back to the operator to control the framing
//! of "what did I do" (ADR-0011 rationale).
//!
//! ## Inputs
//!
//! - `since`: optional inclusive lower bound on `timestamp` (RFC 3339).
//!   Omit for "all recorded history".
//! - `account`: optional filter to a single account alias.
//! - `tool`: optional filter to a single tool name.
//!
//! ## Output shape
//!
//! ```json
//! {
//!   "window_first_at": "2026-05-01T12:00:00Z",
//!   "window_last_at":  "2026-05-22T16:00:00Z",
//!   "total":           42,
//!   "failures":        2,
//!   "failure_rate":    0.048,
//!   "counts_by_tool":  { "archive_thread": 30, "send_email": 12 },
//!   "counts_by_account": { "work": 25, "personal": 17 },
//!   "recent_destructive": [
//!     { "timestamp": "...", "tool": "send_email", "account": "work" },
//!     ...
//!   ]
//! }
//! ```
//!
//! `recent_destructive` lists at most 5 most-recent entries whose tool is
//! in the [`DESTRUCTIVE_TOOLS`] set. Verbose-mode `params_summary` content
//! is not surfaced.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::audit::AuditEntry;
use crate::error::Error;

/// Tools considered destructive for the purposes of the `recent_destructive`
/// shortlist. Mirrors the audit-log set in `src/server.rs` and the table in
/// [ADR-0011 §Redaction rules per tool].
pub(crate) const DESTRUCTIVE_TOOLS: &[&str] = &[
    "archive_thread",
    "batch_archive",
    "trash_thread",
    "batch_trash",
    "modify_thread_labels",
    "batch_modify_thread_labels",
    "send_email",
];

const RECENT_DESTRUCTIVE_LIMIT: usize = 5;

#[derive(Debug, Default)]
pub(crate) struct AuditSummaryInput {
    pub since: Option<DateTime<Utc>>,
    pub account: Option<String>,
    pub tool: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct DestructiveSummary {
    pub timestamp: DateTime<Utc>,
    pub tool: String,
    pub account: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct AuditSummaryOutput {
    pub window_first_at: Option<DateTime<Utc>>,
    pub window_last_at: Option<DateTime<Utc>>,
    pub total: u64,
    pub failures: u64,
    /// failures / total. `0.0` when total is zero.
    pub failure_rate: f64,
    pub counts_by_tool: BTreeMap<String, u64>,
    pub counts_by_account: BTreeMap<String, u64>,
    pub recent_destructive: Vec<DestructiveSummary>,
}

/// Run `audit_summary` against `audit_dir`. Reads every `*.jsonl` file under
/// `audit_dir`, filters per the input, and returns the aggregate. Errors
/// only on I/O faults talking to the audit dir; malformed lines are skipped
/// with a tracing warning so a corrupt entry doesn't poison the whole tool.
pub(crate) fn audit_summary(
    audit_dir: &Path,
    input: &AuditSummaryInput,
) -> Result<AuditSummaryOutput, Error> {
    let entries = read_all_entries(audit_dir)?;
    Ok(aggregate(entries.into_iter(), input))
}

/// Read every `*.jsonl` file in `audit_dir`. Missing directory returns an
/// empty vec (operator has never run a destructive tool). Malformed lines
/// are logged and skipped rather than failing the whole summary.
fn read_all_entries(audit_dir: &Path) -> Result<Vec<AuditEntry>, Error> {
    if !audit_dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    let read_dir = fs::read_dir(audit_dir)?;
    for dir_entry in read_dir {
        let dir_entry = dir_entry?;
        let path = dir_entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            let content = fs::read_to_string(&path)?;
            for (lineno, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<AuditEntry>(line) {
                    Ok(entry) => entries.push(entry),
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        line = lineno + 1,
                        error = %e,
                        "audit_summary: skipping malformed JSONL line"
                    ),
                }
            }
        }
    }
    Ok(entries)
}

/// Pure aggregation over an iterator of [`AuditEntry`]. Filters by `since`,
/// `account`, `tool` first, then folds into the response shape. Tests pass
/// synthetic iterators without touching disk.
pub(crate) fn aggregate(
    entries: impl Iterator<Item = AuditEntry>,
    input: &AuditSummaryInput,
) -> AuditSummaryOutput {
    let mut total = 0u64;
    let mut failures = 0u64;
    let mut counts_by_tool: BTreeMap<String, u64> = BTreeMap::new();
    let mut counts_by_account: BTreeMap<String, u64> = BTreeMap::new();
    let mut first_at: Option<DateTime<Utc>> = None;
    let mut last_at: Option<DateTime<Utc>> = None;
    // Collect destructive entries first; truncate to the last 5 after the
    // full pass so we don't have to know the total entry count up front.
    let mut destructive: Vec<DestructiveSummary> = Vec::new();

    for entry in entries {
        if !matches_filter(&entry, input) {
            continue;
        }
        total += 1;
        if entry.result.starts_with("error:") {
            failures += 1;
        }
        *counts_by_tool.entry(entry.tool.clone()).or_insert(0) += 1;
        *counts_by_account.entry(entry.account.clone()).or_insert(0) += 1;
        first_at = Some(first_at.map_or(entry.timestamp, |x| x.min(entry.timestamp)));
        last_at = Some(last_at.map_or(entry.timestamp, |x| x.max(entry.timestamp)));
        if DESTRUCTIVE_TOOLS.contains(&entry.tool.as_str()) {
            destructive.push(DestructiveSummary {
                timestamp: entry.timestamp,
                tool: entry.tool,
                account: entry.account,
            });
        }
    }

    // Most-recent 5: sort descending by timestamp, then truncate.
    // `Reverse` gives a descending sort via the natural `sort_by_key` path.
    destructive.sort_by_key(|d| std::cmp::Reverse(d.timestamp));
    destructive.truncate(RECENT_DESTRUCTIVE_LIMIT);

    #[allow(clippy::cast_precision_loss)]
    let failure_rate = if total == 0 {
        0.0
    } else {
        failures as f64 / total as f64
    };

    AuditSummaryOutput {
        window_first_at: first_at,
        window_last_at: last_at,
        total,
        failures,
        failure_rate,
        counts_by_tool,
        counts_by_account,
        recent_destructive: destructive,
    }
}

fn matches_filter(entry: &AuditEntry, input: &AuditSummaryInput) -> bool {
    if let Some(since) = input.since {
        if entry.timestamp < since {
            return false;
        }
    }
    if let Some(account) = input.account.as_deref() {
        if entry.account != account {
            return false;
        }
    }
    if let Some(tool) = input.tool.as_deref() {
        if entry.tool != tool {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use chrono::{Datelike as _, TimeZone as _};
    use serde_json::json;

    fn at(d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, d, h, 0, 0).unwrap()
    }

    fn entry(t: DateTime<Utc>, account: &str, tool: &str, result: &str) -> AuditEntry {
        AuditEntry {
            timestamp: t,
            account: account.into(),
            tool: tool.into(),
            params_summary: json!({}),
            action: "applied".into(),
            result: result.into(),
        }
    }

    // ── Empty / no-filter aggregation ────────────────────────────────────────

    #[test]
    fn empty_iterator_returns_zero_aggregate() {
        let out = aggregate(std::iter::empty(), &AuditSummaryInput::default());
        assert_eq!(out.total, 0);
        assert_eq!(out.failures, 0);
        assert!(out.failure_rate.abs() < f64::EPSILON);
        assert!(out.window_first_at.is_none());
        assert!(out.window_last_at.is_none());
        assert!(out.counts_by_tool.is_empty());
        assert!(out.counts_by_account.is_empty());
        assert!(out.recent_destructive.is_empty());
    }

    #[test]
    fn no_filter_aggregates_all_entries() {
        let entries = vec![
            entry(at(1, 9), "work", "archive_thread", "ok"),
            entry(at(2, 10), "work", "send_email", "ok"),
            entry(
                at(3, 11),
                "personal",
                "archive_thread",
                "error: rate limited",
            ),
        ];
        let out = aggregate(entries.into_iter(), &AuditSummaryInput::default());
        assert_eq!(out.total, 3);
        assert_eq!(out.failures, 1);
        assert!((out.failure_rate - (1.0 / 3.0)).abs() < 1e-9);
        assert_eq!(out.counts_by_tool["archive_thread"], 2);
        assert_eq!(out.counts_by_tool["send_email"], 1);
        assert_eq!(out.counts_by_account["work"], 2);
        assert_eq!(out.counts_by_account["personal"], 1);
        assert_eq!(out.window_first_at, Some(at(1, 9)));
        assert_eq!(out.window_last_at, Some(at(3, 11)));
    }

    // ── Filters ──────────────────────────────────────────────────────────────

    #[test]
    fn since_filter_excludes_older_entries() {
        let entries = vec![
            entry(at(1, 9), "work", "archive_thread", "ok"),
            entry(at(5, 10), "work", "send_email", "ok"),
        ];
        let input = AuditSummaryInput {
            since: Some(at(3, 0)),
            ..AuditSummaryInput::default()
        };
        let out = aggregate(entries.into_iter(), &input);
        assert_eq!(out.total, 1);
        assert_eq!(out.counts_by_tool.get("send_email"), Some(&1));
    }

    #[test]
    fn account_filter_isolates_one_account() {
        let entries = vec![
            entry(at(1, 9), "work", "archive_thread", "ok"),
            entry(at(2, 10), "personal", "archive_thread", "ok"),
        ];
        let input = AuditSummaryInput {
            account: Some("work".into()),
            ..AuditSummaryInput::default()
        };
        let out = aggregate(entries.into_iter(), &input);
        assert_eq!(out.total, 1);
        assert_eq!(out.counts_by_account["work"], 1);
        assert!(!out.counts_by_account.contains_key("personal"));
    }

    #[test]
    fn tool_filter_isolates_one_tool() {
        let entries = vec![
            entry(at(1, 9), "work", "archive_thread", "ok"),
            entry(at(2, 10), "work", "send_email", "ok"),
        ];
        let input = AuditSummaryInput {
            tool: Some("send_email".into()),
            ..AuditSummaryInput::default()
        };
        let out = aggregate(entries.into_iter(), &input);
        assert_eq!(out.total, 1);
        assert_eq!(out.counts_by_tool["send_email"], 1);
    }

    // ── recent_destructive ───────────────────────────────────────────────────

    #[test]
    fn recent_destructive_includes_only_destructive_tools() {
        // Mix destructive (send_email) and non-destructive (get_thread,
        // list_labels, search_threads). Only destructive should land in
        // recent_destructive.
        let entries = vec![
            entry(at(1, 9), "work", "send_email", "ok"),
            entry(at(2, 10), "work", "get_thread", "ok"),
            entry(at(3, 11), "work", "search_threads", "ok"),
            entry(at(4, 12), "work", "archive_thread", "ok"),
        ];
        let out = aggregate(entries.into_iter(), &AuditSummaryInput::default());
        let tools: Vec<&str> = out
            .recent_destructive
            .iter()
            .map(|d| d.tool.as_str())
            .collect();
        assert_eq!(tools, vec!["archive_thread", "send_email"]);
    }

    #[test]
    fn recent_destructive_caps_at_five() {
        let mut entries = Vec::new();
        for d in 1..=8 {
            entries.push(entry(at(d, 12), "work", "archive_thread", "ok"));
        }
        let out = aggregate(entries.into_iter(), &AuditSummaryInput::default());
        assert_eq!(out.recent_destructive.len(), 5);
        // Most recent first → day 8, 7, 6, 5, 4.
        let days: Vec<u32> = out
            .recent_destructive
            .iter()
            .map(|d| d.timestamp.day())
            .collect();
        assert_eq!(days, vec![8, 7, 6, 5, 4]);
    }

    #[test]
    fn recent_destructive_omits_params_only_timestamp_tool_account() {
        let entries = vec![entry(at(1, 9), "work", "send_email", "ok")];
        let out = aggregate(entries.into_iter(), &AuditSummaryInput::default());
        let d = &out.recent_destructive[0];
        // Round-trip through JSON to verify the surface is strictly
        // {timestamp, tool, account}.
        let v = serde_json::to_value(d).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["account", "timestamp", "tool"]);
    }

    // ── Disk path ────────────────────────────────────────────────────────────

    #[test]
    fn read_all_entries_returns_empty_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");
        let entries = read_all_entries(&nonexistent).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn read_all_entries_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("work.jsonl");
        fs::write(
            &path,
            // First line valid, second line malformed, third valid.
            "{\"timestamp\":\"2026-05-01T12:00:00Z\",\"account\":\"work\",\"tool\":\"archive_thread\",\"params_summary\":{},\"action\":\"applied\",\"result\":\"ok\"}\n\
             not-json\n\
             {\"timestamp\":\"2026-05-02T12:00:00Z\",\"account\":\"work\",\"tool\":\"send_email\",\"params_summary\":{},\"action\":\"applied\",\"result\":\"ok\"}\n",
        ).unwrap();
        let entries = read_all_entries(tmp.path()).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "malformed line must be skipped, not fatal"
        );
    }

    #[test]
    fn audit_summary_reads_from_disk_and_aggregates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("work.jsonl");
        fs::write(
            &path,
            "{\"timestamp\":\"2026-05-01T12:00:00Z\",\"account\":\"work\",\"tool\":\"send_email\",\"params_summary\":{},\"action\":\"applied\",\"result\":\"ok\"}\n",
        ).unwrap();
        let out = audit_summary(tmp.path(), &AuditSummaryInput::default()).unwrap();
        assert_eq!(out.total, 1);
        assert_eq!(out.counts_by_tool["send_email"], 1);
        assert_eq!(out.recent_destructive.len(), 1);
    }
}
