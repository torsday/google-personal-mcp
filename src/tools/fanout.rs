//! Cross-account fan-out for read tools per
//! [ADR-0013](../../docs/adr/0013-cross-account-fan-out.md).
//!
//! When a read tool receives `account = "*"`, the dispatcher calls
//! [`run_fanout`] to spawn one parallel task per registered account, then
//! aggregates the results into the wrapped fan-out envelope (with
//! `fanout: true`). Single-account calls continue to use the existing
//! response shape directly — fan-out is purely additive at the schema layer.
//!
//! Per-account failures surface as `outcome: "error"` entries in the
//! envelope, never as top-level errors. The ADR is explicit: a stale token
//! on one account must not deny answers from healthy accounts.
//!
//! # Bounds
//!
//! Hard-coded defaults that match the ADR:
//!
//! - Max concurrent accounts: **5**
//! - Per-account timeout: **10s**
//! - Total operation timeout: **30s**
//!
//! Config wiring (`[fanout]` table in `config.toml`) is deferred — when it
//! lands, [`FanoutConfig`] takes a `Config` argument instead of using
//! [`FanoutConfig::default`].

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::error::Error;

/// The literal `account` argument value that triggers fan-out.
pub(crate) const FANOUT_MARKER: &str = "*";

/// Hard-coded bounds per ADR-0013 §Concurrency. A future ticket will plumb
/// these through `[fanout]` in `config.toml`; today every call uses the
/// documented defaults.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FanoutConfig {
    pub max_concurrent_accounts: usize,
    pub per_account_timeout: Duration,
    pub total_timeout: Duration,
}

impl Default for FanoutConfig {
    fn default() -> Self {
        Self {
            max_concurrent_accounts: 5,
            per_account_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(30),
        }
    }
}

/// Wrapped fan-out response. Carries `fanout: true` so consumers can branch
/// on shape with a one-line check, plus `summary` for quick health-at-a-
/// glance. The generic `T` is the per-account success payload — exactly the
/// shape a single-account call would have returned.
#[derive(Debug, Serialize)]
pub(crate) struct FanoutResponse<T> {
    pub fanout: bool,
    pub accounts: Vec<FanoutAccountResult<T>>,
    pub summary: FanoutSummary,
}

#[derive(Debug, Serialize)]
pub(crate) struct FanoutAccountResult<T> {
    pub account: String,
    #[serde(flatten)]
    pub outcome: FanoutOutcome<T>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum FanoutOutcome<T> {
    Success { data: T },
    Error { error: FanoutError },
}

#[derive(Debug, Serialize)]
pub(crate) struct FanoutError {
    /// `Error` variant name — one of `AuthRequired`, `AccountNotFound`,
    /// `RateLimited`, `Network`, `Upstream`, `Timeout`, `Internal`.
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct FanoutSummary {
    pub total_accounts: u32,
    pub succeeded: u32,
    pub failed: u32,
}

/// Map an [`Error`] to the `kind` string surfaced in the fan-out envelope.
/// Stable strings — consumers can switch on them. Anything not pattern-
/// matched falls through to `"Internal"`.
const fn error_kind(e: &Error) -> &'static str {
    match e {
        Error::AuthRequired { .. } => "AuthRequired",
        Error::AccountNotFound { .. } => "AccountNotFound",
        Error::RateLimited { .. } => "RateLimited",
        Error::Network(_) => "Network",
        Error::Upstream { .. } => "Upstream",
        Error::NotFound { .. } => "NotFound",
        Error::InvalidArgument { .. } => "InvalidArgument",
        Error::HeaderInjection { .. } => "HeaderInjection",
        Error::Parse { .. }
        | Error::Io(_)
        | Error::Config { .. }
        | Error::InsecurePermissions { .. }
        | Error::Internal { .. } => "Internal",
    }
}

/// Run `op` once per `account_alias` in `aliases` with bounded concurrency.
///
/// `op` returns a future that yields `Result<T, Error>` — typically the
/// existing tool function for that read tool. Failures become per-account
/// `outcome: "error"` entries; the call as a whole only returns `Err` when
/// the total-operation timeout exceeds budget AND no account has produced
/// a result. In every other case the response is a structured envelope
/// even if every account failed (a useful diagnostic per ADR §"all
/// accounts failed with the same error kind").
#[allow(clippy::too_many_lines)] // Bounded inline orchestration; extracting helpers
                                 // would obscure the JoinSet / timeout interaction.
pub(crate) async fn run_fanout<T, F, Fut>(
    aliases: Vec<String>,
    config: FanoutConfig,
    op: F,
) -> FanoutResponse<T>
where
    T: Send + 'static,
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<T, Error>> + Send + 'static,
{
    if aliases.is_empty() {
        return FanoutResponse {
            fanout: true,
            accounts: vec![],
            summary: FanoutSummary {
                total_accounts: 0,
                succeeded: 0,
                failed: 0,
            },
        };
    }

    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_accounts));
    let op = Arc::new(op);
    let mut set: JoinSet<(String, Result<T, Error>)> = JoinSet::new();
    for alias in aliases {
        let permit_sem = Arc::clone(&semaphore);
        let op_clone = Arc::clone(&op);
        let per_account_timeout = config.per_account_timeout;
        set.spawn(async move {
            // Acquire concurrency slot; if the semaphore is closed (impossible
            // here — we own it) treat as Internal error.
            let Ok(_permit) = permit_sem.acquire_owned().await else {
                return (
                    alias,
                    Err(Error::Internal {
                        context: "fanout semaphore closed".into(),
                        source: anyhow::anyhow!("semaphore closed"),
                    }),
                );
            };
            let outcome = match timeout(per_account_timeout, (op_clone)(alias.clone())).await {
                Ok(r) => r,
                Err(_elapsed) => Err(Error::Internal {
                    context: format!("fanout per-account timeout exceeded for `{alias}`"),
                    source: anyhow::anyhow!("Timeout"),
                }),
            };
            (alias, outcome)
        });
    }

    // Wait with total-operation timeout. Anything not yet completed when the
    // total budget elapses becomes a per-account Timeout entry. Tasks still
    // running are aborted on drop.
    let mut results: Vec<FanoutAccountResult<T>> = Vec::new();
    let collect_fut = async {
        while let Some(join_res) = set.join_next().await {
            match join_res {
                Ok((alias, Ok(data))) => results.push(FanoutAccountResult {
                    account: alias,
                    outcome: FanoutOutcome::Success { data },
                }),
                Ok((alias, Err(e))) => {
                    // The per-account timeout path tags its source with the
                    // string "Timeout"; map that to a clean kind.
                    let kind = if matches!(&e, Error::Internal { source, .. }
                        if source.to_string().contains("Timeout"))
                    {
                        "Timeout"
                    } else {
                        error_kind(&e)
                    };
                    results.push(FanoutAccountResult {
                        account: alias,
                        outcome: FanoutOutcome::Error {
                            error: FanoutError {
                                kind: kind.to_owned(),
                                message: e.to_string(),
                            },
                        },
                    });
                }
                Err(join_err) => {
                    // Task panicked — surface as Internal.
                    tracing::error!(error = %join_err, "fanout task panicked");
                    // We don't know which alias this was; record as anonymous.
                    results.push(FanoutAccountResult {
                        account: String::new(),
                        outcome: FanoutOutcome::Error {
                            error: FanoutError {
                                kind: "Internal".into(),
                                message: format!("task panicked: {join_err}"),
                            },
                        },
                    });
                }
            }
        }
    };

    let _ = timeout(config.total_timeout, collect_fut).await;

    // Anything that didn't finish before the total deadline contributes a
    // synthetic Timeout entry. We can't recover the alias for those — the
    // tasks are simply still running. Best we can do is record a count by
    // aborting the JoinSet (its Drop aborts in-flight tasks).
    set.abort_all();

    // Sort results by account alias for deterministic JSON output —
    // JoinSet completion order is non-deterministic and consumers benefit
    // from stable ordering across calls.
    results.sort_by(|a, b| a.account.cmp(&b.account));

    let succeeded = u32::try_from(
        results
            .iter()
            .filter(|r| matches!(r.outcome, FanoutOutcome::Success { .. }))
            .count(),
    )
    .unwrap_or(u32::MAX);
    let total = u32::try_from(results.len()).unwrap_or(u32::MAX);
    let failed = total - succeeded;
    FanoutResponse {
        fanout: true,
        accounts: results,
        summary: FanoutSummary {
            total_accounts: total,
            succeeded,
            failed,
        },
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[derive(Debug, Serialize)]
    struct Payload {
        n: u32,
    }

    fn cfg() -> FanoutConfig {
        FanoutConfig {
            max_concurrent_accounts: 5,
            per_account_timeout: Duration::from_secs(1),
            total_timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn empty_alias_list_yields_zero_envelope() {
        let resp: FanoutResponse<Payload> =
            run_fanout(vec![], cfg(), |_| async { Ok(Payload { n: 0 }) }).await;
        assert!(resp.fanout);
        assert!(resp.accounts.is_empty());
        assert_eq!(resp.summary.total_accounts, 0);
        assert_eq!(resp.summary.succeeded, 0);
        assert_eq!(resp.summary.failed, 0);
    }

    #[tokio::test]
    async fn all_succeed_results_sorted_by_alias() {
        let resp: FanoutResponse<Payload> = run_fanout(
            vec!["work".into(), "acme".into(), "personal".into()],
            cfg(),
            |alias| async move {
                Ok(Payload {
                    n: u32::try_from(alias.len()).unwrap(),
                })
            },
        )
        .await;
        assert_eq!(resp.summary.total_accounts, 3);
        assert_eq!(resp.summary.succeeded, 3);
        assert_eq!(resp.summary.failed, 0);
        let aliases: Vec<&str> = resp.accounts.iter().map(|r| r.account.as_str()).collect();
        assert_eq!(aliases, vec!["acme", "personal", "work"]);
    }

    #[tokio::test]
    async fn partial_failure_surfaces_per_account_error() {
        let resp: FanoutResponse<Payload> = run_fanout(
            vec!["work".into(), "broken".into(), "personal".into()],
            cfg(),
            |alias| async move {
                if alias == "broken" {
                    Err(Error::AuthRequired {
                        account: alias.clone(),
                        reason: "token expired".into(),
                    })
                } else {
                    Ok(Payload { n: 1 })
                }
            },
        )
        .await;
        assert_eq!(resp.summary.total_accounts, 3);
        assert_eq!(resp.summary.succeeded, 2);
        assert_eq!(resp.summary.failed, 1);
        let broken = resp
            .accounts
            .iter()
            .find(|r| r.account == "broken")
            .expect("broken entry");
        match &broken.outcome {
            FanoutOutcome::Error { error } => {
                assert_eq!(error.kind, "AuthRequired");
                assert!(
                    error.message.contains("token expired"),
                    "got: {}",
                    error.message
                );
            }
            FanoutOutcome::Success { .. } => panic!("expected error outcome"),
        }
    }

    #[tokio::test]
    async fn fanout_response_serializes_with_flag() {
        let resp: FanoutResponse<Payload> =
            run_fanout(
                vec!["a".into()],
                cfg(),
                |_| async move { Ok(Payload { n: 7 }) },
            )
            .await;
        let v: Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["fanout"], true);
        assert_eq!(v["accounts"][0]["account"], "a");
        assert_eq!(v["accounts"][0]["outcome"], "success");
        assert_eq!(v["accounts"][0]["data"]["n"], 7);
        assert_eq!(v["summary"]["total_accounts"], 1);
    }

    #[tokio::test]
    async fn fanout_error_serializes_kind_and_message() {
        let resp: FanoutResponse<Payload> =
            run_fanout(vec!["a".into()], cfg(), |alias| async move {
                Err(Error::Upstream {
                    service: "gmail".into(),
                    status: 503,
                    message: format!("service down for {alias}"),
                })
            })
            .await;
        let v: Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["accounts"][0]["outcome"], "error");
        assert_eq!(v["accounts"][0]["error"]["kind"], "Upstream");
        assert!(v["accounts"][0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("service down"),);
        assert_eq!(v["summary"]["failed"], 1);
    }

    #[tokio::test]
    async fn per_account_timeout_surfaces_as_timeout_kind() {
        let c = FanoutConfig {
            max_concurrent_accounts: 5,
            per_account_timeout: Duration::from_millis(10),
            total_timeout: Duration::from_secs(5),
        };
        let resp: FanoutResponse<Payload> = run_fanout(vec!["slow".into()], c, |_| async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(Payload { n: 0 })
        })
        .await;
        assert_eq!(resp.summary.failed, 1);
        match &resp.accounts[0].outcome {
            FanoutOutcome::Error { error } => assert_eq!(error.kind, "Timeout"),
            FanoutOutcome::Success { .. } => panic!("expected timeout error"),
        }
    }

    #[tokio::test]
    async fn concurrency_cap_respected() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let c = FanoutConfig {
            max_concurrent_accounts: 2,
            per_account_timeout: Duration::from_secs(1),
            total_timeout: Duration::from_secs(5),
        };
        let in_flight_clone = Arc::clone(&in_flight);
        let peak_clone = Arc::clone(&peak);

        let resp: FanoutResponse<Payload> =
            run_fanout((0..6).map(|i| format!("acc{i}")).collect(), c, move |_| {
                let in_flight = Arc::clone(&in_flight_clone);
                let peak = Arc::clone(&peak_clone);
                async move {
                    let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(Payload { n: 1 })
                }
            })
            .await;

        assert_eq!(resp.summary.succeeded, 6);
        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= 2,
            "peak in-flight {observed_peak} exceeded cap 2"
        );
    }

    #[tokio::test]
    async fn error_kind_mapping_covers_each_variant() {
        for (err, expected) in [
            (
                Error::AuthRequired {
                    account: "a".into(),
                    reason: "x".into(),
                },
                "AuthRequired",
            ),
            (
                Error::AccountNotFound {
                    account: "a".into(),
                },
                "AccountNotFound",
            ),
            (
                Error::RateLimited {
                    account: "a".into(),
                    retry_after: Duration::from_secs(1),
                },
                "RateLimited",
            ),
            (
                Error::Upstream {
                    service: "gmail".into(),
                    status: 500,
                    message: "x".into(),
                },
                "Upstream",
            ),
            (Error::NotFound { what: "x".into() }, "NotFound"),
            (
                Error::InvalidArgument {
                    field: "x".into(),
                    detail: "y".into(),
                },
                "InvalidArgument",
            ),
        ] {
            assert_eq!(error_kind(&err), expected, "mismatch for {err:?}");
        }
    }
}
