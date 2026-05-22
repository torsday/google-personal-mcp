//! Shared batch-orchestration primitive for the per-thread destructive tools
//! (`batch_archive`, `batch_trash`, `batch_modify_thread_labels`). Each batch
//! tool was a near-clone of the others; this module centralizes the
//! `validate → dry-run shortcut → concurrent dispatch → ordered collect`
//! pattern so future fixes apply once rather than three times.
//!
//! Per [ADR-0016 §Batch response convention](../../docs/adr/0016-tool-surface-and-conventions.md):
//! batch tools return per-item `{ thread_id, ok, error? }` and never
//! short-circuit on per-item failure.

use std::collections::HashMap;
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
/// `JoinError` (panic in a spawned task) is logged at `tracing::error!`; the
/// unaccounted `thread_id` appears in the output as
/// `ok: false, error: Some("task did not complete")`. The `thread_id` is
/// preserved because we capture it before dispatch — `JoinError` itself does
/// not carry it.
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
    // Index tasks by position, not by thread_id, so duplicate IDs in the input
    // each get their own result slot. Using a HashMap keyed by thread_id would
    // cause the second duplicate's result to overwrite the first, and the
    // output loop's second removal would synthesise a spurious "task did not
    // complete" error. See #104.
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, thread_id) in thread_ids.iter().enumerate() {
        let fut = spawn_one(thread_id.clone());
        join_set.spawn(async move {
            let result = fut.await;
            (idx, result)
        });
    }

    let mut by_idx: HashMap<usize, Result<(), Error>> = HashMap::new();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok((idx, outcome)) => {
                by_idx.insert(idx, outcome);
            }
            Err(join_err) => {
                // JoinError doesn't carry the index; the unaccounted entry
                // surfaces as "task did not complete" in the ordered merge below.
                tracing::error!("batch task panicked: {join_err}");
            }
        }
    }

    thread_ids
        .into_iter()
        .enumerate()
        .map(|(idx, thread_id)| match by_idx.remove(&idx) {
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

    #[tokio::test]
    async fn run_thread_batch_empty_input_returns_empty() {
        let results = run_thread_batch(vec![], |_| async { Ok(()) }).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn run_thread_batch_handles_duplicate_thread_ids() {
        // Regression test for #104: duplicate IDs in the input must each get
        // their own result slot. Previously the HashMap-by-id approach caused
        // the second removal to return None and synthesise a spurious
        // "task did not complete" error.
        let ids = vec!["t1".into(), "t1".into(), "t2".into()];
        let results = run_thread_batch(ids, |_tid| async { Ok(()) }).await;

        assert_eq!(results.len(), 3);
        // All three entries must be populated — no spurious "task did not complete".
        for result in &results {
            assert!(result.ok, "expected ok=true, got: {result:?}");
            assert!(result.error.is_none());
        }
        assert_eq!(results[0].thread_id, "t1");
        assert_eq!(results[1].thread_id, "t1");
        assert_eq!(results[2].thread_id, "t2");
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
}
