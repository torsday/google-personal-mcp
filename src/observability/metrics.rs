//! Prometheus exporter and metric inventory per
//! [ADR-0008 §Metrics](../../../docs/adr/0008-observability-and-deployment.md)
//! (issue [#75]).
//!
//! [`install`] builds a [`PrometheusBuilder`] recorder, sets it as the
//! global `metrics` recorder, registers all 12 metrics from the ADR
//! table with their counter/gauge/histogram types and label sets, and
//! returns a [`PrometheusHandle`] the `/metrics` route hands to
//! `handle.render()`. The recorder is installed exactly once per process
//! — repeated calls return the existing handle so unit tests don't
//! collide.
//!
//! The `metrics` facade is a no-op when no recorder is installed, so
//! every counter / gauge / histogram bump in the rest of the codebase
//! is safe to run unconditionally. Operators opt in by adding a
//! `[metrics]` section to `config.toml`; without it, [`install`] is
//! never called and the `/metrics` endpoint never binds.
//!
//! Label cardinality: high-cardinality fields (thread IDs, message
//! IDs, email addresses) are forbidden by ADR-0008 line 64; helpers in
//! this module accept already-low-cardinality labels (`tool`, `account`
//! alias, `service`, `endpoint`, status class). Callers that have only
//! the high-cardinality form must collapse it before recording.
//!
//! [#75]: https://github.com/torsday/google-personal-mcp/issues/75

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::error::Error;

/// Metric names, kept as `const` so call sites and tests share the
/// canonical strings. Each is referenced from at least one bump-site
/// elsewhere in the crate; rename here and `grep` will find every
/// instrumentation point.
pub(crate) mod names {
    pub(crate) const TOOL_CALLS_TOTAL: &str = "gmcp_tool_calls_total";
    pub(crate) const TOOL_CALL_DURATION_SECONDS: &str = "gmcp_tool_call_duration_seconds";
    pub(crate) const GOOGLE_API_CALLS_TOTAL: &str = "gmcp_google_api_calls_total";
    pub(crate) const GOOGLE_API_CALL_DURATION_SECONDS: &str =
        "gmcp_google_api_call_duration_seconds";
    pub(crate) const TOKEN_REFRESHES_TOTAL: &str = "gmcp_token_refreshes_total";
    pub(crate) const ACTIVE_ACCOUNTS: &str = "gmcp_active_accounts";
    pub(crate) const HTTP_SESSIONS_ACTIVE: &str = "gmcp_http_sessions_active";
    pub(crate) const HTTP_SESSION_DURATION_SECONDS: &str = "gmcp_http_session_duration_seconds";
    pub(crate) const HOT_RELOAD_TOTAL: &str = "gmcp_hot_reload_total";
    pub(crate) const CACHE_WRITE_DISCARDED_TOTAL: &str = "gmcp_cache_write_discarded_total";
    pub(crate) const CACHE_BODIES_PURGED_TOTAL: &str = "gmcp_cache_bodies_purged_total";
    pub(crate) const CACHE_BODIES_PURGED_DUE_TO_DELETE_TOTAL: &str =
        "gmcp_cache_bodies_purged_due_to_delete_total";
    pub(crate) const RATE_LIMIT_BLOCKS_TOTAL: &str = "gmcp_rate_limit_blocks_total";
    pub(crate) const BUILD_INFO: &str = "gmcp_build_info";
}

/// Bucket set used by `gmcp_tool_call_duration_seconds`. Matches the
/// ADR-0008 table: 0.01, 0.05, 0.1, 0.5, 1, 5, 10, 30 seconds.
const TOOL_DURATION_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0];

/// Bucket set used by `gmcp_google_api_call_duration_seconds`. Same
/// shape as the tool histogram — Gmail API latencies fall in the same
/// rough range.
const API_DURATION_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0];

/// HTTP-session lifespan histogram bucketing. Sessions skew long-lived
/// (minutes to hours); buckets reflect that and extend out to one day.
const HTTP_SESSION_BUCKETS: &[f64] = &[
    1.0, 10.0, 60.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0, 21_600.0, 86_400.0,
];

/// Process-global cell holding the installed handle. The
/// `PrometheusBuilder` recorder install is one-shot (panics on second
/// install); the `OnceLock` guards against that.
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder (idempotent), register every metric
/// from the ADR-0008 table, and seed startup-time gauges. Returns the
/// shared [`PrometheusHandle`] the `/metrics` HTTP route renders.
///
/// `accounts` is the number of configured accounts at startup; sets
/// `gmcp_active_accounts`. `build_info_labels` populates the
/// `gmcp_build_info` gauge — `(version, git_sha, rust_version)`.
///
/// Errors:
/// - [`Error::Internal`] if the global recorder was already installed
///   by something other than this function (e.g. a different test).
pub(crate) fn install(
    accounts: usize,
    build_info_labels: BuildInfoLabels,
) -> Result<&'static PrometheusHandle, Error> {
    if let Some(h) = HANDLE.get() {
        // Already installed — re-set the build-info gauge in case the
        // labels changed (rebuilt binary, same process) and return the
        // existing handle. Active accounts may also have changed.
        seed_runtime_state(accounts, build_info_labels);
        return Ok(h);
    }

    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(
                names::TOOL_CALL_DURATION_SECONDS.to_owned(),
            ),
            TOOL_DURATION_BUCKETS,
        )
        .and_then(|b| {
            b.set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full(
                    names::GOOGLE_API_CALL_DURATION_SECONDS.to_owned(),
                ),
                API_DURATION_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full(
                    names::HTTP_SESSION_DURATION_SECONDS.to_owned(),
                ),
                HTTP_SESSION_BUCKETS,
            )
        })
        .map_err(|e| Error::Internal {
            context: "metrics::install: configure buckets".into(),
            source: anyhow::Error::new(e),
        })?
        .build_recorder();

    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).map_err(|e| Error::Internal {
        context: "metrics::install: set global recorder".into(),
        source: anyhow::anyhow!("recorder already installed: {e}"),
    })?;

    describe_all();
    seed_runtime_state(accounts, build_info_labels);

    let _ = HANDLE.set(handle);
    // `set` may return Err if a race lost; either way the OnceLock now
    // holds an installed handle, so `get()` succeeds.
    HANDLE.get().ok_or_else(|| Error::Internal {
        context: "metrics::install: handle vanished".into(),
        source: anyhow::anyhow!("OnceLock empty after set"),
    })
}

/// Read the previously-installed handle. Returns `None` when
/// [`install`] has not been called this process (the `[metrics]`
/// config section is absent).
pub(crate) fn handle() -> Option<&'static PrometheusHandle> {
    HANDLE.get()
}

/// Build-info gauge labels. Each is low-cardinality (one value per
/// build).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BuildInfoLabels {
    pub version: &'static str,
    pub git_sha: &'static str,
    pub rust_version: &'static str,
}

impl BuildInfoLabels {
    /// Pull from cargo/env so callers don't have to remember every
    /// field. `git_sha` falls back to `"unknown"` when no build script
    /// surfaces one — acceptable for a metric whose value is always 1.
    pub(crate) const fn from_env() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            // Bare crate doesn't set GIT_SHA; a future build.rs can.
            // Keeping this static avoids a build-script dep for v1 of #75.
            git_sha: "unknown",
            rust_version: env!("CARGO_PKG_RUST_VERSION"),
        }
    }
}

/// Describe every metric with its type + help text. Called once during
/// [`install`]. Even un-bumped metrics appear in `/metrics` output once
/// described, so `gmcp_hot_reload_total` (no call sites until the
/// reload subsystem lands) shows as `# TYPE … counter` with no series.
fn describe_all() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        names::TOOL_CALLS_TOTAL,
        "MCP tool dispatch outcomes, by tool name and success/error variant",
    );
    describe_histogram!(
        names::TOOL_CALL_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "End-to-end tool dispatch duration (lock acquisition through response serialization)",
    );
    describe_counter!(
        names::GOOGLE_API_CALLS_TOTAL,
        "Outbound Google API requests, by service / endpoint / 2xx-3xx-4xx-5xx-network status class",
    );
    describe_histogram!(
        names::GOOGLE_API_CALL_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Outbound Google API request duration, by service / endpoint",
    );
    describe_counter!(
        names::TOKEN_REFRESHES_TOTAL,
        "OAuth token refresh outcomes (success / invalid_grant / network / upstream)",
    );
    describe_gauge!(
        names::ACTIVE_ACCOUNTS,
        "Number of accounts in the running TokenManager registry",
    );
    describe_gauge!(
        names::HTTP_SESSIONS_ACTIVE,
        "HTTP-transport MCP sessions currently held by the session store",
    );
    describe_histogram!(
        names::HTTP_SESSION_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Lifespan of MCP sessions in HTTP mode (creation to removal)",
    );
    describe_counter!(
        names::HOT_RELOAD_TOTAL,
        "config/accounts hot-reload outcomes (success / parse_error / validation_error)",
    );
    describe_counter!(
        names::CACHE_WRITE_DISCARDED_TOTAL,
        "query_cache writes discarded because the history watermark advanced during the fetch (ADR-0009 §Race-prevention)",
    );
    describe_counter!(
        names::CACHE_BODIES_PURGED_TOTAL,
        "Message body columns nulled because the row exceeded `body_max_age_days` (ADR-0019 §Cache body age cap)",
    );
    describe_counter!(
        names::CACHE_BODIES_PURGED_DUE_TO_DELETE_TOTAL,
        "Message body columns nulled because the row was soft-deleted more than the 7-day grace window ago (ADR-0019)",
    );
    describe_counter!(
        names::RATE_LIMIT_BLOCKS_TOTAL,
        "Tool calls delayed by the per-account rate limiter",
    );
    describe_gauge!(
        names::BUILD_INFO,
        "Static build identity — value is always 1; labels carry version / git_sha / rust_version",
    );
}

/// Set the two startup-time gauges. Called from [`install`] and
/// re-callable on hot reload (when that lands) without re-installing
/// the recorder.
fn seed_runtime_state(accounts: usize, build_info: BuildInfoLabels) {
    #[allow(clippy::cast_precision_loss)]
    let accounts_f64 = accounts as f64;
    metrics::gauge!(names::ACTIVE_ACCOUNTS).set(accounts_f64);
    metrics::gauge!(
        names::BUILD_INFO,
        "version" => build_info.version,
        "git_sha" => build_info.git_sha,
        "rust_version" => build_info.rust_version,
    )
    .set(1.0);
}

/// Map a Google API HTTP status into the `status_class` label used by
/// `gmcp_google_api_calls_total`. Network failures (no status) map to
/// `"network"`.
pub(crate) fn status_class(status: Option<u16>) -> &'static str {
    match status {
        None => "network",
        Some(s) if (200..300).contains(&s) => "2xx",
        Some(s) if (300..400).contains(&s) => "3xx",
        Some(s) if (400..500).contains(&s) => "4xx",
        _ => "5xx",
    }
}

/// Map an [`Error`] to the `outcome` label used by
/// `gmcp_tool_calls_total`. `"success"` is the only non-error value;
/// errors return their variant name so dashboards can segment by
/// failure mode.
pub(crate) const fn outcome_label(result: Result<&(), &Error>) -> &'static str {
    match result {
        Ok(()) => "success",
        Err(e) => e.kind(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn status_class_partitions_correctly() {
        assert_eq!(status_class(None), "network");
        assert_eq!(status_class(Some(200)), "2xx");
        assert_eq!(status_class(Some(204)), "2xx");
        assert_eq!(status_class(Some(301)), "3xx");
        assert_eq!(status_class(Some(404)), "4xx");
        assert_eq!(status_class(Some(429)), "4xx");
        assert_eq!(status_class(Some(500)), "5xx");
        assert_eq!(status_class(Some(503)), "5xx");
    }

    #[test]
    fn build_info_labels_from_env_carries_crate_version() {
        let labels = BuildInfoLabels::from_env();
        assert_eq!(labels.version, env!("CARGO_PKG_VERSION"));
        assert!(!labels.version.is_empty());
    }
}
