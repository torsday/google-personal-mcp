//! `cache_invalidate` tool — operator-facing manual reset for the
//! per-account cache. Per [ADR-0009 §"New tools"](../../docs/adr/0009-caching-with-sqlite-and-history-api.md)
//! lines 266-268.
//!
//! Scopes:
//!
//! - `queries`: drop every row from `query_cache` (memoized `threads.list`
//!   results).
//! - `labels`: drop the `labels` catalog and every per-message
//!   `message_labels` row. Bodies and thread rows survive; the next
//!   `get_thread` re-populates label state.
//! - `all`: both of the above. **Does not** delete message bodies — those
//!   are immutable per ADR-0009. Operators wipe bodies with `rm` on the
//!   `.db` file directly.
//!
//! Destructive; the dispatcher records a fsync'd `intent` audit record
//! per [ADR-0011](../../docs/adr/0011-audit-log.md) lines 83-86 before
//! the call lands.

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::service::GmailService;

/// Scope argument. `serde` rename maps the wire shape to lowercase
/// snake-case per ADR-0016 conventions (`account`, `dry_run`, etc).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InvalidateScope {
    /// Both `query_cache` and label-catalog rows. Bodies preserved.
    All,
    /// `query_cache` only.
    Queries,
    /// `labels` + `message_labels` only.
    Labels,
}

impl InvalidateScope {
    /// Parse from the wire-shape string. Used by the dispatcher's
    /// argument extraction; the JSON-schema `enum` constraint already
    /// rejects unknown values upstream, so the error message is a
    /// defence-in-depth fallback.
    pub(crate) fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "all" => Ok(Self::All),
            "queries" => Ok(Self::Queries),
            "labels" => Ok(Self::Labels),
            other => Err(Error::InvalidArgument {
                field: "scope".into(),
                detail: format!(
                    "expected one of \"all\" | \"queries\" | \"labels\", got {other:?}"
                ),
            }),
        }
    }
}

/// Response envelope.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CacheInvalidateOutput {
    /// `true` when `[cache] enabled = true` and a real invalidation ran;
    /// `false` when the cache is disabled (the call becomes a no-op).
    pub applied: bool,
    /// Echoed back so the host LLM can confirm the operation.
    pub account: String,
    /// Echoed back as a lowercase string (matches the JSON-schema enum).
    pub scope: InvalidateScope,
}

/// Execute the requested invalidation.
///
/// Bodies are never deleted by this tool — that's intentional per
/// ADR-0009 ("bodies are immutable so never invalidated through tools").
///
/// Errors:
/// - [`Error::InvalidArgument`] for empty `account` (defence-in-depth;
///   the JSON-schema `required` array already rejects this upstream).
/// - Upstream errors from the `SQLite` layer.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "cache_invalidate", tool.account = %account, tool.scope = ?scope),
)]
pub(crate) async fn cache_invalidate<T: RefreshTransport + 'static>(
    gmail: &GmailService<T>,
    account: &str,
    scope: InvalidateScope,
) -> Result<CacheInvalidateOutput, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    let Some(cache) = gmail.cache() else {
        return Ok(CacheInvalidateOutput {
            applied: false,
            account: account.to_owned(),
            scope,
        });
    };

    match scope {
        InvalidateScope::Queries => {
            cache.invalidate_all_queries(account).await?;
        }
        InvalidateScope::Labels => {
            cache.invalidate_all_labels(account).await?;
        }
        InvalidateScope::All => {
            // Ordering doesn't matter — the two tables are independent —
            // but be deterministic so the audit log can be replayed.
            cache.invalidate_all_queries(account).await?;
            cache.invalidate_all_labels(account).await?;
        }
    }

    Ok(CacheInvalidateOutput {
        applied: true,
        account: account.to_owned(),
        scope,
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
    use crate::gmail::threads::RawThreadsList;

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
            "gpm-ci-{}-{}",
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
        // process owns the disk; per-test tempdir cleanup not worth the
        // extra plumbing.
        let _ = dir.keep();
        cache
    }

    /// Seed one row in `query_cache` and one in `message_labels` so the
    /// scope-specific tests can observe what survived.
    async fn seed_two_rows(cache: &Cache) {
        cache
            .insert_query(
                "work",
                "q",
                10,
                None,
                &RawThreadsList {
                    threads: vec![],
                    next_page_token: None,
                },
                0,
            )
            .await
            .expect("seed query");
        // Insert a labels row + a message_labels row directly. The
        // tests below assert both tables are touched on scope=labels.
        let conn = cache.connection("work").expect("conn").clone();
        conn.call(|c| -> rusqlite::Result<()> {
            c.execute(
                "INSERT INTO labels (id, name, kind, fetched_at) \
                 VALUES ('INBOX', 'INBOX', 'system', 0)",
                [],
            )?;
            // FK needs a parent messages row first; the eviction test
            // already proves we have a working pattern.
            c.execute(
                "INSERT INTO threads (id, snippet, history_id, fetched_at) \
                 VALUES ('t1', NULL, NULL, 0)",
                [],
            )?;
            c.execute(
                "INSERT INTO messages \
                 (id, thread_id, internal_date, headers_json, body_text, body_html, \
                  snippet, has_attachments, attachments_json, raw_size, fetched_at, deleted_at) \
                 VALUES ('m1', 't1', 0, '{}', 'BODY', NULL, NULL, 0, NULL, NULL, 0, NULL)",
                [],
            )?;
            c.execute(
                "INSERT INTO message_labels (message_id, label_id) VALUES ('m1', 'INBOX')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("seed labels");
    }

    async fn row_counts(cache: &Cache) -> (i64, i64, i64, i64) {
        let conn = cache.connection("work").expect("conn").clone();
        conn.call(|c| -> rusqlite::Result<(i64, i64, i64, i64)> {
            let q: i64 = c.query_row("SELECT COUNT(*) FROM query_cache", [], |r| r.get(0))?;
            let lbl: i64 = c.query_row("SELECT COUNT(*) FROM labels", [], |r| r.get(0))?;
            let mlbl: i64 = c.query_row("SELECT COUNT(*) FROM message_labels", [], |r| r.get(0))?;
            let msgs: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
            Ok((q, lbl, mlbl, msgs))
        })
        .await
        .expect("counts")
    }

    #[test]
    fn parse_scope_accepts_canonical_lowercase() {
        assert_eq!(InvalidateScope::parse("all").unwrap(), InvalidateScope::All);
        assert_eq!(
            InvalidateScope::parse("queries").unwrap(),
            InvalidateScope::Queries
        );
        assert_eq!(
            InvalidateScope::parse("labels").unwrap(),
            InvalidateScope::Labels
        );
    }

    #[test]
    fn parse_scope_rejects_unknown_value() {
        let err = InvalidateScope::parse("ALL").expect_err("err");
        match err {
            Error::InvalidArgument { field, detail } => {
                assert_eq!(field, "scope");
                assert!(detail.contains("ALL"), "detail = {detail}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_empty_account() {
        let gmail = make_gmail(None);
        let err = cache_invalidate(&gmail, "", InvalidateScope::Queries)
            .await
            .expect_err("err");
        match err {
            Error::InvalidArgument { field, .. } => assert_eq!(field, "account"),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_cache_returns_applied_false() {
        let gmail = make_gmail(None);
        let out = cache_invalidate(&gmail, "work", InvalidateScope::All)
            .await
            .expect("ok");
        assert!(!out.applied);
        assert_eq!(out.account, "work");
    }

    #[tokio::test]
    async fn scope_queries_drops_query_cache_only() {
        let cache = open_cache(&["work"]).await;
        seed_two_rows(&cache).await;
        let (q0, lbl0, mlbl0, msgs0) = row_counts(&cache).await;
        assert_eq!((q0, lbl0, mlbl0, msgs0), (1, 1, 1, 1));

        let gmail = make_gmail(Some(Arc::clone(&cache)));
        let out = cache_invalidate(&gmail, "work", InvalidateScope::Queries)
            .await
            .expect("ok");
        assert!(out.applied);

        let (q, lbl, mlbl, msgs) = row_counts(&cache).await;
        assert_eq!(q, 0, "query_cache cleared");
        assert_eq!(lbl, 1, "labels survived");
        assert_eq!(mlbl, 1, "message_labels survived");
        assert_eq!(msgs, 1, "messages survived");
    }

    #[tokio::test]
    async fn scope_labels_drops_labels_only() {
        let cache = open_cache(&["work"]).await;
        seed_two_rows(&cache).await;
        let gmail = make_gmail(Some(Arc::clone(&cache)));

        let out = cache_invalidate(&gmail, "work", InvalidateScope::Labels)
            .await
            .expect("ok");
        assert!(out.applied);

        let (q, lbl, mlbl, msgs) = row_counts(&cache).await;
        assert_eq!(q, 1, "query_cache survived");
        assert_eq!(lbl, 0, "labels cleared");
        assert_eq!(mlbl, 0, "message_labels cleared");
        assert_eq!(msgs, 1, "bodies survived per ADR-0009");
    }

    #[tokio::test]
    async fn scope_all_drops_queries_and_labels_but_not_bodies() {
        let cache = open_cache(&["work"]).await;
        seed_two_rows(&cache).await;
        let gmail = make_gmail(Some(Arc::clone(&cache)));

        let out = cache_invalidate(&gmail, "work", InvalidateScope::All)
            .await
            .expect("ok");
        assert!(out.applied);

        let (q, lbl, mlbl, msgs) = row_counts(&cache).await;
        assert_eq!(q, 0);
        assert_eq!(lbl, 0);
        assert_eq!(mlbl, 0);
        // The load-bearing assertion: scope=all does NOT delete bodies.
        assert_eq!(msgs, 1, "bodies must survive scope=all per ADR-0009");
    }

    #[tokio::test]
    async fn unknown_account_no_ops_but_returns_applied_true() {
        // Mirrors the broader Cache convention (`Cache::lookup_*` returns
        // None for unknown accounts rather than erroring). The applied=true
        // signals "the call ran"; the no-op happens inside Cache.
        let cache = open_cache(&["work"]).await;
        let gmail = make_gmail(Some(cache));
        let out = cache_invalidate(&gmail, "missing", InvalidateScope::All)
            .await
            .expect("ok");
        assert!(out.applied);
    }
}
