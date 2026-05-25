//! `cache_status` tool — operator introspection for the per-account cache.
//!
//! Returns one row per registered account plus a coarse process-lifetime
//! hit/miss snapshot. Per [ADR-0009 §"New tools"](../../docs/adr/0009-caching-with-sqlite-and-history-api.md)
//! lines 263-268.
//!
//! **Hit-rate scope:** the ADR mentions "hit rate (last hour)". This v0.3
//! cut surfaces the *process-lifetime* hit rate instead — the cumulative
//! `gmcp_cache_hits_total / (hits + misses)`. A rolling-window
//! breakdown ships with the Prometheus exporter ([#75]), which has the
//! supporting time-bucketed infrastructure. Documented as a known
//! simplification in the tool description so operators don't read the
//! field as a recent-traffic gauge.
//!
//! [#75]: https://github.com/torsday/google-personal-mcp/issues/75

use serde::Serialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::service::GmailService;

/// Per-account row.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CacheAccountStatus {
    pub alias: String,
    /// `page_count * page_size` of the per-account `.db` file.
    pub size_bytes: u64,
    /// `account_state.last_history_id` — the Gmail watermark the next
    /// `history.list` call will replay from. `None` until first sync.
    pub last_history_id: Option<i64>,
    /// `account_state.last_full_sync_at` — ms epoch of the most recent
    /// successful sync tick. `None` until first sync.
    pub last_sync_at_ms: Option<i64>,
}

/// Tool response envelope.
#[derive(Debug, Serialize)]
pub(crate) struct CacheStatusOutput {
    /// `true` when `[cache] enabled = true` in the daemon config. When
    /// `false` the rest of the fields are zero / empty and the operator
    /// knows the cache wasn't constructed at startup.
    pub enabled: bool,
    /// One entry per registered account. Empty when `enabled = false`
    /// or when `account_filter` matched no known alias.
    pub accounts: Vec<CacheAccountStatus>,
    /// Cumulative since process start, across all accounts.
    pub hits_total: u64,
    /// Cumulative since process start, across all accounts.
    pub misses_total: u64,
    /// Cumulative since process start. See ADR-0009 §"Race-prevention"
    /// (#81) for the semantic; should be near zero in steady state.
    pub write_discarded_total: u64,
    /// `hits_total / (hits_total + misses_total)`, lifetime — `None`
    /// when no lookups have happened yet. **Not** a last-hour window;
    /// see module docs.
    pub hit_rate_lifetime: Option<f64>,
}

/// Build the `cache_status` response.
///
/// `account_filter`: when `Some`, only that account is returned (or an
/// empty list if the alias is unknown); when `None`, all registered
/// accounts are returned in `Cache::account_aliases` order (sorted).
///
/// Cache disabled: returns `enabled = false` with zeroed counters. The
/// tool is still callable so the operator can inspect the config state
/// from the host LLM.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "cache_status", tool.account_filter = ?account_filter),
)]
pub(crate) async fn cache_status<T: RefreshTransport + 'static>(
    gmail: &GmailService<T>,
    account_filter: Option<&str>,
) -> Result<CacheStatusOutput, Error> {
    let Some(cache) = gmail.cache() else {
        return Ok(CacheStatusOutput {
            enabled: false,
            accounts: Vec::new(),
            hits_total: 0,
            misses_total: 0,
            write_discarded_total: 0,
            hit_rate_lifetime: None,
        });
    };

    let snapshot = cache.metrics_snapshot();
    let mut accounts = Vec::new();
    for alias in cache.account_aliases() {
        if let Some(filter) = account_filter {
            if alias != filter {
                continue;
            }
        }
        let size_bytes = cache.db_size_bytes(alias).await?.unwrap_or(0);
        let last_history_id = cache.last_history_id(alias).await?;
        let last_sync_at_ms = cache.last_full_sync_at(alias).await?;
        accounts.push(CacheAccountStatus {
            alias: alias.to_owned(),
            size_bytes,
            last_history_id,
            last_sync_at_ms,
        });
    }

    Ok(CacheStatusOutput {
        enabled: true,
        accounts,
        hits_total: snapshot.hits,
        misses_total: snapshot.misses,
        write_discarded_total: snapshot.write_discarded,
        hit_rate_lifetime: snapshot.hit_rate(),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};

    use super::*;
    use crate::auth::tokens::{TokenManager, TokenState};
    use crate::cache::Cache;
    use crate::gmail::client::GmailClient;

    struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            Ok((
                200,
                r#"{"access_token":"NEW","expires_in":3600}"#.to_owned(),
            ))
        }
    }

    fn make_gmail(cache: Option<Arc<Cache>>) -> GmailService<NoRefresh> {
        let state = TokenState {
            access_token: "T".into(),
            refresh_token: "R".into(),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            scopes: vec![],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        };
        let tdir = std::env::temp_dir().join(format!(
            "gpm-cs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&tdir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            tdir,
        ));
        let client = Arc::new(GmailClient::new(
            "http://localhost:1",
            tokens,
            reqwest::Client::new(),
        ));
        GmailService::new(client, cache)
    }

    async fn open_cache(accounts: &[&str]) -> Arc<Cache> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let aliases: Vec<String> = accounts.iter().map(|s| (*s).to_owned()).collect();
        let cache = Arc::new(
            Cache::new(
                dir.path().to_owned(),
                &aliases,
                std::time::Duration::from_mins(5),
            )
            .await
            .unwrap(),
        );
        // Leak the TempDir so the .db files outlive this helper. Test
        // process owns the disk; per-test tempdir cleanup not worth
        // the extra plumbing.
        let _ = dir.keep();
        cache
    }

    #[tokio::test]
    async fn returns_disabled_envelope_when_cache_is_none() {
        let gmail = make_gmail(None);
        let out = cache_status(&gmail, None).await.expect("ok");
        assert!(!out.enabled);
        assert!(out.accounts.is_empty());
        assert_eq!(out.hits_total, 0);
        assert_eq!(out.misses_total, 0);
        assert_eq!(out.hit_rate_lifetime, None);
    }

    #[tokio::test]
    async fn enumerates_accounts_when_cache_is_some() {
        let cache = open_cache(&["work", "personal"]).await;
        let gmail = make_gmail(Some(Arc::clone(&cache)));
        let out = cache_status(&gmail, None).await.expect("ok");
        assert!(out.enabled);
        // Cache::account_aliases sorts deterministically.
        let aliases: Vec<&str> = out.accounts.iter().map(|a| a.alias.as_str()).collect();
        assert_eq!(aliases, vec!["personal", "work"]);
        // Fresh DBs have non-zero size (schema + WAL setup) but no sync state.
        for row in &out.accounts {
            assert!(row.size_bytes > 0, "row.size_bytes = {}", row.size_bytes);
            assert!(row.last_history_id.is_none());
            assert!(row.last_sync_at_ms.is_none());
        }
    }

    #[tokio::test]
    async fn account_filter_returns_only_matching_alias() {
        let cache = open_cache(&["work", "personal"]).await;
        let gmail = make_gmail(Some(cache));
        let out = cache_status(&gmail, Some("work")).await.expect("ok");
        assert_eq!(out.accounts.len(), 1);
        assert_eq!(out.accounts[0].alias, "work");
    }

    #[tokio::test]
    async fn account_filter_unknown_returns_empty() {
        let cache = open_cache(&["work"]).await;
        let gmail = make_gmail(Some(cache));
        let out = cache_status(&gmail, Some("missing")).await.expect("ok");
        assert!(out.accounts.is_empty());
        assert!(out.enabled, "envelope still reports enabled = true");
    }

    #[tokio::test]
    async fn surfaces_last_history_id_and_sync_at_after_sync() {
        let cache = open_cache(&["work"]).await;
        cache.set_last_history_id("work", 42).await.expect("seed");
        let gmail = make_gmail(Some(Arc::clone(&cache)));
        let out = cache_status(&gmail, Some("work")).await.expect("ok");
        assert_eq!(out.accounts[0].last_history_id, Some(42));
        assert!(
            out.accounts[0].last_sync_at_ms.is_some(),
            "set_last_history_id bumps last_full_sync_at",
        );
    }

    #[tokio::test]
    async fn hit_rate_lifetime_reflects_metrics_snapshot() {
        let cache = open_cache(&["work"]).await;
        cache.metrics().record_hit("work", "thread");
        cache.metrics().record_hit("work", "thread");
        cache.metrics().record_miss("work", "thread");
        let gmail = make_gmail(Some(cache));
        let out = cache_status(&gmail, None).await.expect("ok");
        assert_eq!(out.hits_total, 2);
        assert_eq!(out.misses_total, 1);
        let rate = out.hit_rate_lifetime.expect("Some");
        assert!((rate - 2.0 / 3.0).abs() < 1e-9, "rate = {rate}");
    }
}
