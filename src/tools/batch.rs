//! Shared batch-orchestration primitive for the per-thread destructive tools
//! (`batch_archive`, `batch_trash`, `batch_modify_thread_labels`). Each batch
//! tool was a near-clone of the others; this module centralizes the
//! `validate → dry-run shortcut → concurrent dispatch → ordered collect`
//! pattern so future fixes apply once rather than three times.
//!
//! Per [ADR-0016 §Batch response convention](../../docs/adr/0016-tool-surface-and-conventions.md):
//! batch tools never short-circuit on per-item failure. The response shape is
//! selected by the `mode` parameter ([`BatchMode`], default
//! [`BatchMode::FailuresOnly`]) per
//! [ADR-0027 §5](../../docs/adr/0027-v1-1-surface-refinements.md); `run_thread_batch`
//! produces the per-item `{ thread_id, ok, error? }` outcomes that
//! [`BatchResponse::from_items`] then folds into the chosen envelope.

use std::future::Future;

use serde::Serialize;

use crate::error::Error;

/// One per-thread outcome in a batch response. Wire JSON shape per
/// [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md):
/// `{ "thread_id": "...", "ok": true|false, "error": "..." | null }`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct BatchItem {
    pub thread_id: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// One failed-thread entry in `"failures_only"` / `"summary"` responses. Wire
/// shape `{ "thread_id": "...", "error": "..." }` per
/// [ADR-0027 §5](../../docs/adr/0027-v1-1-surface-refinements.md). Only failures
/// are represented, so `error` is always present (unlike [`BatchItem`]).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct BatchFailure {
    pub thread_id: String,
    pub error: String,
}

/// Response verbosity for a batch tool, selected by the `mode` parameter per
/// [ADR-0027 §5](../../docs/adr/0027-v1-1-surface-refinements.md). Defaults to
/// [`BatchMode::FailuresOnly`] — the common all-success batch returns a terse
/// empty `failures` array rather than N redundant per-item rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BatchMode {
    /// Response lists only failed items (`failures: [{thread_id, error}]`);
    /// `failures: []` on full success. The default.
    #[default]
    FailuresOnly,
    /// Response lists every per-item outcome (`results: [{thread_id, ok, error}]`)
    /// — the v1.0 shape, for audit-trail callers.
    All,
    /// Response carries counts plus the first 5 failure details only — for
    /// "ran a 1000-item batch, just tell me how it went" callers.
    Summary,
}

impl BatchMode {
    /// Maximum failure details surfaced in [`BatchMode::Summary`] responses.
    const SUMMARY_FAILURE_CAP: usize = 5;

    /// Parse the `mode` tool argument. Absent → [`BatchMode::FailuresOnly`].
    /// Returns `Err(InvalidArgument)` on any unrecognized value so a typo
    /// fails loudly rather than silently defaulting.
    pub(crate) fn from_arg(raw: Option<&str>) -> Result<Self, Error> {
        match raw {
            None | Some("failures_only") => Ok(Self::FailuresOnly),
            Some("all") => Ok(Self::All),
            Some("summary") => Ok(Self::Summary),
            Some(other) => Err(Error::InvalidArgument {
                field: "mode".into(),
                detail: format!("mode must be one of failures_only|all|summary, got `{other}`"),
            }),
        }
    }
}

/// A batch tool's response envelope, shaped by the requested [`BatchMode`].
/// `succeeded_count` is present in every variant per
/// [ADR-0027 §5](../../docs/adr/0027-v1-1-surface-refinements.md) — it
/// distinguishes "0 succeeded, 0 failed" from "200 succeeded, 0 failed", which
/// an empty `failures` array alone cannot.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum BatchResponse {
    /// `mode: "failures_only"` — `{ succeeded_count, failures: [...] }`.
    FailuresOnly {
        succeeded_count: u32,
        failures: Vec<BatchFailure>,
    },
    /// `mode: "all"` — `{ succeeded_count, results: [...] }`.
    All {
        succeeded_count: u32,
        results: Vec<BatchItem>,
    },
    /// `mode: "summary"` — `{ succeeded_count, failed_count, failures: [≤5] }`.
    Summary {
        succeeded_count: u32,
        failed_count: u32,
        failures: Vec<BatchFailure>,
    },
}

impl BatchResponse {
    /// Fold per-item [`BatchItem`] outcomes into the envelope for `mode`.
    /// Consumes `items` since each variant either reshapes or moves them.
    pub(crate) fn from_items(items: Vec<BatchItem>, mode: BatchMode) -> Self {
        let succeeded_count = count_u32(items.iter().filter(|i| i.ok).count());
        match mode {
            BatchMode::FailuresOnly => Self::FailuresOnly {
                succeeded_count,
                failures: collect_failures(items, None),
            },
            BatchMode::All => Self::All {
                succeeded_count,
                results: items,
            },
            BatchMode::Summary => {
                let failed_count = count_u32(items.iter().filter(|i| !i.ok).count());
                Self::Summary {
                    succeeded_count,
                    failed_count,
                    failures: collect_failures(items, Some(BatchMode::SUMMARY_FAILURE_CAP)),
                }
            }
        }
    }
}

/// Saturating `usize → u32`. Batch sizes are bounded by [`MAX_BATCH_SIZE`], so
/// this never actually saturates; the conversion stays clippy-clean regardless.
fn count_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Pull failed items into [`BatchFailure`] entries, preserving input order
/// (the order guarantee from [#105](https://github.com/torsday/google-personal-mcp/issues/105)).
/// `cap` optionally limits how many are kept (summary mode).
fn collect_failures(items: Vec<BatchItem>, cap: Option<usize>) -> Vec<BatchFailure> {
    let failed = items.into_iter().filter(|i| !i.ok).map(|i| BatchFailure {
        thread_id: i.thread_id,
        // A non-ok item always carries an error; default defensively.
        error: i.error.unwrap_or_else(|| "unknown error".into()),
    });
    match cap {
        Some(n) => failed.take(n).collect(),
        None => failed.collect(),
    }
}

/// Maximum thread IDs per batch call, per
/// [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md). Saner
/// default than Gmail's 1000-id cap on `messages.batchModify`; configurable
/// per [ADR-0006](../../docs/adr/0006-config.md) (future ticket).
pub(crate) const MAX_BATCH_SIZE: usize = 100;

/// Validate the standard batch envelope: non-empty account, non-empty
/// `thread_ids`, ≤ [`MAX_BATCH_SIZE`] entries. Returns `Err(InvalidArgument)`
/// on any failure for the caller to propagate.
pub(crate) fn validate_batch_input(account: &str, thread_ids: &[String]) -> Result<(), Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    if thread_ids.is_empty() {
        return Err(Error::InvalidArgument {
            field: "thread_ids".into(),
            detail: "thread_ids must not be empty".into(),
        });
    }
    if thread_ids.len() > MAX_BATCH_SIZE {
        return Err(Error::InvalidArgument {
            field: "thread_ids".into(),
            detail: format!(
                "thread_ids must contain at most {MAX_BATCH_SIZE} items, got {}",
                thread_ids.len()
            ),
        });
    }
    Ok(())
}

/// Build the dry-run response: one `ok: true` entry per input thread, no API
/// call made.
pub(crate) fn dry_run_results(thread_ids: Vec<String>) -> Vec<BatchItem> {
    thread_ids
        .into_iter()
        .map(|thread_id| BatchItem {
            thread_id,
            ok: true,
            error: None,
        })
        .collect()
}

/// Concurrently apply `spawn_one(thread_id)` to each ID, collecting per-item
/// results in **input order**. Never short-circuits on per-item failure.
///
/// Results are tracked by **input index**, not by `thread_id`, so duplicate
/// IDs in `thread_ids` (e.g. `["t1", "t1"]`) produce two independently
/// populated result rows instead of overwriting each other's slot — fix
/// for [#104](https://github.com/torsday/google-personal-mcp/issues/104).
/// The caller's intent (asking the API twice for the same ID) is preserved
/// faithfully; if they didn't mean to dup, the duplicate is a caller-side
/// problem worth surfacing in the output.
///
/// `JoinError` (panic in a spawned task) is logged at `tracing::error!`; the
/// corresponding slot stays `None` and surfaces as
/// `ok: false, error: Some("task did not complete")`. We can't attribute a
/// panic to a specific index — `JoinError` doesn't carry one — so when N
/// tasks panic, the first N empty slots get the "did not complete" message.
/// In practice, batch tasks don't panic; the case is logged loudly enough
/// that the operator notices.
///
/// The caller is responsible for validation ([`validate_batch_input`]) and
/// the dry-run shortcut ([`dry_run_results`]) before reaching this helper.
pub(crate) async fn run_thread_batch<F, Fut>(
    thread_ids: Vec<String>,
    spawn_one: F,
) -> Vec<BatchItem>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<(), Error>> + Send + 'static,
{
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, thread_id) in thread_ids.iter().enumerate() {
        let fut = spawn_one(thread_id.clone());
        join_set.spawn(async move {
            let result = fut.await;
            (idx, result)
        });
    }

    // Pre-allocate one slot per input index. Per-index keying (rather
    // than per-thread_id) is what fixes #104: two tasks for the same
    // thread_id land in distinct slots instead of overwriting.
    let mut slots: Vec<Option<Result<(), Error>>> = std::iter::repeat_with(|| None)
        .take(thread_ids.len())
        .collect();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok((idx, outcome)) => {
                if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(outcome);
                }
            }
            Err(join_err) => {
                // JoinError doesn't carry the index; an empty slot
                // surfaces as "task did not complete" in the ordered
                // merge below.
                tracing::error!("batch task panicked: {join_err}");
            }
        }
    }

    thread_ids
        .into_iter()
        .zip(slots)
        .map(|(thread_id, slot)| match slot {
            Some(Ok(())) => BatchItem {
                thread_id,
                ok: true,
                error: None,
            },
            Some(Err(e)) => BatchItem {
                thread_id,
                ok: false,
                error: Some(e.to_string()),
            },
            None => BatchItem {
                thread_id,
                ok: false,
                error: Some("task did not complete".into()),
            },
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // ── validate_batch_input ──────────────────────────────────────────────

    #[test]
    fn validate_accepts_valid_input() {
        assert!(validate_batch_input("personal", &["t1".into()]).is_ok());
        let many: Vec<String> = (0..MAX_BATCH_SIZE).map(|i| format!("t{i}")).collect();
        assert!(validate_batch_input("work", &many).is_ok());
    }

    #[test]
    fn validate_rejects_empty_account() {
        let err = validate_batch_input("", &["t1".into()]).unwrap_err();
        match err {
            Error::InvalidArgument { field, .. } => assert_eq!(field, "account"),
            _ => panic!("expected InvalidArgument"),
        }
    }

    #[test]
    fn validate_rejects_empty_thread_ids() {
        let err = validate_batch_input("personal", &[]).unwrap_err();
        match err {
            Error::InvalidArgument { field, .. } => assert_eq!(field, "thread_ids"),
            _ => panic!("expected InvalidArgument"),
        }
    }

    #[test]
    fn validate_rejects_oversized_batch() {
        let big: Vec<String> = (0..=MAX_BATCH_SIZE).map(|i| format!("t{i}")).collect();
        let err = validate_batch_input("personal", &big).unwrap_err();
        match err {
            Error::InvalidArgument { field, detail } => {
                assert_eq!(field, "thread_ids");
                assert!(detail.contains("at most"));
            }
            _ => panic!("expected InvalidArgument"),
        }
    }

    // ── dry_run_results ────────────────────────────────────────────────────

    #[test]
    fn dry_run_results_one_ok_per_input() {
        let out = dry_run_results(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].thread_id, "a");
        assert_eq!(out[1].thread_id, "b");
        assert_eq!(out[2].thread_id, "c");
        assert!(out.iter().all(|i| i.ok && i.error.is_none()));
    }

    #[test]
    fn dry_run_results_empty_input_returns_empty() {
        assert!(dry_run_results(vec![]).is_empty());
    }

    // ── run_thread_batch ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_thread_batch_preserves_input_order_on_all_success() {
        // Intentionally not in sort-order — verifies we're not sorting.
        let ids = vec!["z".into(), "a".into(), "m".into()];
        let results = run_thread_batch(ids, |_tid| async { Ok(()) }).await;

        let order: Vec<&str> = results.iter().map(|r| r.thread_id.as_str()).collect();
        assert_eq!(order, vec!["z", "a", "m"]);
        assert!(results.iter().all(|r| r.ok && r.error.is_none()));
    }

    #[tokio::test]
    async fn run_thread_batch_preserves_order_with_mixed_outcomes() {
        let ids = vec!["good-1".into(), "bad".into(), "good-2".into()];
        let results = run_thread_batch(ids, |tid| async move {
            if tid == "bad" {
                Err(Error::NotFound { what: tid })
            } else {
                Ok(())
            }
        })
        .await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].thread_id, "good-1");
        assert!(results[0].ok);
        assert_eq!(results[1].thread_id, "bad");
        assert!(!results[1].ok);
        assert!(results[1].error.as_ref().unwrap().contains("not found"));
        assert_eq!(results[2].thread_id, "good-2");
        assert!(results[2].ok);
    }

    #[tokio::test]
    #[allow(clippy::manual_assert)]
    async fn run_thread_batch_handles_panic_in_one_task() {
        // A panic in one spawned task must not lose siblings' results. We
        // deliberately use `if ... { panic!(...) }` rather than `assert!`
        // because the intent is "this branch must panic", not "this condition
        // must hold" — they behave the same but the if-form documents intent.
        let ids = vec!["ok-1".into(), "panicky".into(), "ok-2".into()];
        let results = run_thread_batch(ids, |tid| async move {
            if tid == "panicky" {
                panic!("simulated task panic");
            }
            Ok(())
        })
        .await;

        assert_eq!(results.len(), 3);
        assert!(results[0].ok);
        assert_eq!(results[1].thread_id, "panicky");
        assert!(!results[1].ok);
        assert_eq!(results[1].error.as_deref(), Some("task did not complete"));
        assert!(results[2].ok);
    }

    // ── #104: duplicate thread_ids get distinct, populated result slots ──

    #[tokio::test]
    async fn run_thread_batch_handles_duplicate_thread_ids() {
        // Before #104: HashMap<thread_id, Result> meant the second
        // task's outcome overwrote the first, and the merge loop's
        // second remove() returned None → spurious "task did not
        // complete". After: index-keyed slots produce two populated
        // entries.
        let ids = vec!["t1".into(), "t1".into(), "t2".into(), "t1".into()];
        let results = run_thread_batch(ids, |_tid| async { Ok(()) }).await;

        assert_eq!(results.len(), 4, "one row per input, even duplicates");
        // Every row populated — no "task did not complete" leaks.
        assert!(
            results.iter().all(|r| r.ok && r.error.is_none()),
            "every row must be ok; got: {results:?}"
        );
        // Input order preserved verbatim.
        let order: Vec<&str> = results.iter().map(|r| r.thread_id.as_str()).collect();
        assert_eq!(order, vec!["t1", "t1", "t2", "t1"]);
    }

    #[tokio::test]
    async fn run_thread_batch_duplicates_with_mixed_outcomes() {
        // Two `bad` rows must independently report the error; the
        // intervening `good` row stays populated.
        let ids = vec!["bad".into(), "good".into(), "bad".into()];
        let results = run_thread_batch(ids, |tid| async move {
            if tid == "bad" {
                Err(Error::NotFound { what: tid })
            } else {
                Ok(())
            }
        })
        .await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].thread_id, "bad");
        assert!(!results[0].ok);
        assert!(results[0].error.as_ref().unwrap().contains("not found"));
        assert_eq!(results[1].thread_id, "good");
        assert!(results[1].ok);
        assert_eq!(results[2].thread_id, "bad");
        assert!(!results[2].ok);
        assert!(results[2].error.as_ref().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn run_thread_batch_empty_input_returns_empty() {
        let results = run_thread_batch(vec![], |_| async { Ok(()) }).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn batch_item_serializes_with_expected_wire_keys() {
        // Wire JSON contract per ADR-0016 — must remain stable.
        let ok_item = BatchItem {
            thread_id: "t1".into(),
            ok: true,
            error: None,
        };
        let json = serde_json::to_value(&ok_item).unwrap();
        assert_eq!(json["thread_id"], "t1");
        assert_eq!(json["ok"], true);
        assert!(json["error"].is_null());

        let err_item = BatchItem {
            thread_id: "t2".into(),
            ok: false,
            error: Some("nope".into()),
        };
        let json = serde_json::to_value(&err_item).unwrap();
        assert_eq!(json["error"], "nope");
    }

    // ── BatchMode::from_arg ────────────────────────────────────────────────

    #[test]
    fn batch_mode_from_arg_defaults_to_failures_only() {
        assert_eq!(BatchMode::from_arg(None).unwrap(), BatchMode::FailuresOnly);
        assert_eq!(
            BatchMode::from_arg(Some("failures_only")).unwrap(),
            BatchMode::FailuresOnly
        );
    }

    #[test]
    fn batch_mode_from_arg_parses_each_variant() {
        assert_eq!(BatchMode::from_arg(Some("all")).unwrap(), BatchMode::All);
        assert_eq!(
            BatchMode::from_arg(Some("summary")).unwrap(),
            BatchMode::Summary
        );
    }

    #[test]
    fn batch_mode_from_arg_rejects_unknown() {
        let err = BatchMode::from_arg(Some("verbose")).unwrap_err();
        match err {
            Error::InvalidArgument { field, detail } => {
                assert_eq!(field, "mode");
                assert!(detail.contains("verbose"));
            }
            _ => panic!("expected InvalidArgument"),
        }
    }

    // ── BatchResponse::from_items ──────────────────────────────────────────

    fn items() -> Vec<BatchItem> {
        // 2 ok, 1 failed — input order: ok, bad, ok.
        vec![
            BatchItem {
                thread_id: "a".into(),
                ok: true,
                error: None,
            },
            BatchItem {
                thread_id: "b".into(),
                ok: false,
                error: Some("boom".into()),
            },
            BatchItem {
                thread_id: "c".into(),
                ok: true,
                error: None,
            },
        ]
    }

    #[test]
    fn failures_only_lists_just_failures_with_count() {
        let resp = BatchResponse::from_items(items(), BatchMode::FailuresOnly);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["succeeded_count"], 2);
        assert_eq!(json["failures"].as_array().unwrap().len(), 1);
        assert_eq!(json["failures"][0]["thread_id"], "b");
        assert_eq!(json["failures"][0]["error"], "boom");
        // `all` and `summary` fields must be absent in this variant.
        assert!(json.get("results").is_none());
        assert!(json.get("failed_count").is_none());
    }

    #[test]
    fn failures_only_full_success_is_empty_array_not_omitted() {
        let all_ok = vec![BatchItem {
            thread_id: "a".into(),
            ok: true,
            error: None,
        }];
        let resp = BatchResponse::from_items(all_ok, BatchMode::FailuresOnly);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["succeeded_count"], 1);
        // Present and empty — distinguishes "all succeeded" from "no data".
        assert_eq!(json["failures"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn all_mode_preserves_every_item_in_order() {
        let resp = BatchResponse::from_items(items(), BatchMode::All);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["succeeded_count"], 2);
        let results = json["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["thread_id"], "a");
        assert_eq!(results[1]["thread_id"], "b");
        assert_eq!(results[1]["ok"], false);
        assert!(json.get("failures").is_none());
    }

    #[test]
    fn summary_mode_carries_both_counts_plus_failures() {
        let resp = BatchResponse::from_items(items(), BatchMode::Summary);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["succeeded_count"], 2);
        assert_eq!(json["failed_count"], 1);
        assert_eq!(json["failures"].as_array().unwrap().len(), 1);
        assert!(json.get("results").is_none());
    }

    #[test]
    fn summary_mode_caps_failures_at_five() {
        // 7 failures → summary keeps the first 5 (input order), but
        // failed_count reports the true total.
        let many: Vec<BatchItem> = (0..7)
            .map(|i| BatchItem {
                thread_id: format!("t{i}"),
                ok: false,
                error: Some(format!("err{i}")),
            })
            .collect();
        let resp = BatchResponse::from_items(many, BatchMode::Summary);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["succeeded_count"], 0);
        assert_eq!(json["failed_count"], 7);
        let failures = json["failures"].as_array().unwrap();
        assert_eq!(failures.len(), 5, "capped at first 5");
        assert_eq!(failures[0]["thread_id"], "t0");
        assert_eq!(failures[4]["thread_id"], "t4");
    }

    #[test]
    fn empty_batch_succeeded_count_is_zero() {
        let resp = BatchResponse::from_items(vec![], BatchMode::FailuresOnly);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["succeeded_count"], 0);
        assert_eq!(json["failures"].as_array().unwrap().len(), 0);
    }
}
