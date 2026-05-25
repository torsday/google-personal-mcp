//! Per-account `SQLite` cache scaffolding per [ADR-0009](../../docs/adr/0009-caching-with-sqlite-and-history-api.md).
//!
//! Phases 1–2 of the cache implementation plan
//! ([docs/cache-implementation-plan.md](../../docs/cache-implementation-plan.md)).
//! This module owns the `SQLite` connections (one per account), the migration
//! framework, file-permission enforcement, and the `lookup_*` / `insert_*`
//! methods that [`crate::gmail::service::GmailService`] calls on each
//! cacheable read. Phase 3 (history-sync, [#80]) will add a background sync
//! loop alongside these primitives. After [`Cache::new`] returns
//! successfully, every per-account DB file exists on disk, has WAL mode
//! enabled, and is at [`migrations::MAX_KNOWN_VERSION`].
//!
//! [#80]: https://github.com/torsday/google-personal-mcp/issues/80
//!
//! Concurrency model: one [`tokio_rusqlite::Connection`] per account. Each
//! `Connection` serializes its SQL through a dedicated background thread, so
//! a `Cache` holding an `Arc` to each connection is safe to share across the
//! async runtime. WAL mode lets many readers run concurrently with one
//! writer. The per-account `HashMap` is built once in [`Cache::new`] and is
//! not mutated after construction; account hot-reload (when it lands) will
//! be a separate ADR-0002-aware path.
//!
//! Per-account file: `<dir>/<account>.db`. Created mode `0600` per
//! [ADR-0017](../../docs/adr/0017-secrets-at-rest.md).

pub(crate) mod metrics;
pub(crate) mod migrations;
pub(crate) mod queries;
pub(crate) mod sync;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_rusqlite::Connection;

use crate::error::Error;
use crate::gmail::threads::{ParsedThread, RawThreadsList, ThreadMetadata};

pub(crate) use self::metrics::CacheMetrics;

/// Filename mode applied to each per-account DB file at creation.
const DB_FILE_MODE: u32 = 0o600;

/// Mode applied to the cache root directory at creation.
const DB_DIR_MODE: u32 = 0o700;

/// Per-account `SQLite` cache holder. Construction opens (creating if needed)
/// one DB file per supplied account alias, applies pending migrations, and
/// enforces filesystem permissions. See module docs for what is and is not
/// in scope as of this phase.
#[derive(Debug)]
pub(crate) struct Cache {
    dir: PathBuf,
    connections: HashMap<String, Arc<Connection>>,
    query_ttl: Duration,
    metrics: CacheMetrics,
}

impl Cache {
    /// Open (or create) one cache DB per account alias under `dir`.
    ///
    /// On a fresh install, each account's DB is created with the [v1
    /// schema](migrations/001_initial.sql) and mode `0600`. On an existing
    /// install, each DB is opened, pending migrations are applied in a
    /// transaction, and the file mode is verified.
    ///
    /// `query_ttl` is applied to `query_cache` rows (`list_threads` results).
    /// Lookups discard expired rows; inserts use the TTL to compute
    /// `expires_at`.
    ///
    /// Errors:
    /// - [`Error::Io`] if `dir` cannot be created.
    /// - [`Error::InsecurePermissions`] if an existing DB file is wider than
    ///   `0600`.
    /// - [`Error::Internal`] for `SQLite` open or migration failures, or when
    ///   the on-disk schema version exceeds [`migrations::MAX_KNOWN_VERSION`]
    ///   (downgrade refusal per ADR-0009).
    pub(crate) async fn new(
        dir: PathBuf,
        accounts: &[String],
        query_ttl: Duration,
    ) -> Result<Self, Error> {
        ensure_cache_dir(&dir)?;

        let mut connections = HashMap::with_capacity(accounts.len());
        for alias in accounts {
            let path = dir.join(format!("{alias}.db"));
            let conn = open_and_migrate(&path).await?;
            connections.insert(alias.clone(), Arc::new(conn));
        }

        Ok(Self {
            dir,
            connections,
            query_ttl,
            metrics: CacheMetrics::default(),
        })
    }

    /// Directory the per-account DBs live under. Stable for the Cache's
    /// lifetime.
    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Aliases this Cache currently holds a connection for, sorted for
    /// deterministic iteration in tests and diagnostics.
    pub(crate) fn account_aliases(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.connections.keys().map(String::as_str).collect();
        out.sort_unstable();
        out
    }

    /// Hit/miss counters. Read by tests and (when it lands) the Prometheus
    /// exporter from [#75].
    ///
    /// [#75]: https://github.com/torsday/google-personal-mcp/issues/75
    pub(crate) const fn metrics(&self) -> &CacheMetrics {
        &self.metrics
    }

    /// Borrow the async connection for `account`, if known. Internal
    /// helper for the `lookup_*` / `insert_*` methods; also exposed for
    /// the Phase 3 history-sync loop ([#80]) and the Layer-4 schema
    /// snapshot tests.
    ///
    /// [#80]: https://github.com/torsday/google-personal-mcp/issues/80
    pub(crate) fn connection(&self, account: &str) -> Option<&Arc<Connection>> {
        self.connections.get(account)
    }

    /// Look up a full thread (body text + attachment metadata + per-message
    /// labels). Returns `Ok(None)` when the thread is absent, when only
    /// metadata-format rows are cached (i.e. `body_text IS NULL`), or when
    /// the account is unknown to this cache. See module docs for the
    /// metadata-vs-full discriminator.
    pub(crate) async fn lookup_thread(
        &self,
        account: &str,
        thread_id: &str,
    ) -> Result<Option<ParsedThread>, Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(None);
        };
        queries::lookup_thread(conn, thread_id).await
    }

    /// Insert (or replace) a full thread. Stores one row per message in
    /// `messages`, links each message's labels in `message_labels`, and
    /// writes a `threads` row for the parent.
    pub(crate) async fn insert_thread(
        &self,
        account: &str,
        thread: &ParsedThread,
    ) -> Result<(), Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(());
        };
        queries::insert_thread(conn, thread).await
    }

    /// Look up metadata-shaped thread data (headers + label ids + size
    /// estimates, no bodies). Returns `Ok(None)` when the thread is absent
    /// or the account is unknown.
    pub(crate) async fn lookup_thread_metadata(
        &self,
        account: &str,
        thread_id: &str,
    ) -> Result<Option<ThreadMetadata>, Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(None);
        };
        queries::lookup_thread_metadata(conn, thread_id).await
    }

    /// Insert (or replace) metadata-shaped thread data. Writes one row per
    /// message to `messages` with `body_text = NULL` (the metadata-only
    /// sentinel) and links labels in `message_labels`. A subsequent
    /// `insert_thread` upgrades these rows in place.
    pub(crate) async fn insert_thread_metadata(
        &self,
        account: &str,
        meta: &ThreadMetadata,
    ) -> Result<(), Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(());
        };
        queries::insert_thread_metadata(conn, meta).await
    }

    /// Look up a memoized `threads.list` result. Returns `Ok(None)` when
    /// the row is absent, expired, or the account is unknown.
    pub(crate) async fn lookup_query(
        &self,
        account: &str,
        query: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> Result<Option<RawThreadsList>, Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(None);
        };
        queries::lookup_query(conn, query, max_results, page_token).await
    }

    /// Cache a `threads.list` result for `query_ttl` (set at construction).
    /// Distinct `(query, max_results, page_token)` tuples cache independently.
    pub(crate) async fn insert_query(
        &self,
        account: &str,
        query: &str,
        max_results: u32,
        page_token: Option<&str>,
        result: &RawThreadsList,
    ) -> Result<(), Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(());
        };
        queries::insert_query(conn, query, max_results, page_token, result, self.query_ttl).await
    }

    // ── Phase 3: history watermark + delta application ───────────────────────

    /// Read the `last_history_id` watermark for `account`. Returns
    /// `Ok(None)` when the account has never been seeded (fresh install)
    /// or when the account is unknown to this cache.
    pub(crate) async fn last_history_id(&self, account: &str) -> Result<Option<i64>, Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(None);
        };
        queries::last_history_id(conn).await
    }

    /// Persist a new `last_history_id` watermark. No-op for unknown
    /// accounts.
    pub(crate) async fn set_last_history_id(
        &self,
        account: &str,
        history_id: i64,
    ) -> Result<(), Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(());
        };
        queries::set_last_history_id(conn, history_id).await
    }

    /// Apply one decoded `history.list` record to the cache. Caller
    /// converts the wire shape into a [`queries::HistoryDelta`].
    pub(crate) async fn apply_history_record(
        &self,
        account: &str,
        delta: queries::HistoryDelta,
    ) -> Result<(), Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(());
        };
        queries::apply_history_record(conn, delta).await
    }

    /// Drop every `query_cache` row for this account. Called after a
    /// history page mutates any messages or labels (Phase 4 will replace
    /// this with finer-grained per-thread invalidation).
    pub(crate) async fn invalidate_all_queries(&self, account: &str) -> Result<(), Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(());
        };
        queries::invalidate_all_queries(conn).await
    }

    /// Reseed path for the 404 `historyNotFound` case. Drops every
    /// cached message / thread / label-link / query-result row for this
    /// account and installs `new_history_id`. The `labels` table
    /// (account-wide catalog) is preserved.
    pub(crate) async fn reseed_account(
        &self,
        account: &str,
        new_history_id: i64,
    ) -> Result<(), Error> {
        let Some(conn) = self.connection(account) else {
            return Ok(());
        };
        queries::reseed_account(conn, new_history_id).await
    }
}

/// Create the cache root directory if it does not exist, with mode `0700`.
/// Verifies the mode of an existing directory does not include any
/// group/other bits.
fn ensure_cache_dir(dir: &Path) -> Result<(), Error> {
    match std::fs::metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(Error::InsecurePermissions {
                    path: dir.display().to_string(),
                    message: "expected a directory but found a file".into(),
                });
            }
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(Error::InsecurePermissions {
                    path: dir.display().to_string(),
                    message: format!(
                        "mode is 0{mode:o}, expected 0{DB_DIR_MODE:o} (owner-only). \
                         Fix with `chmod 0{DB_DIR_MODE:o} {}`.",
                        dir.display()
                    ),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(DB_DIR_MODE))?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Open the `SQLite` file at `path`, set WAL mode, enforce mode 600, and apply
/// pending migrations. Returns the async-wrapped connection ready to use.
async fn open_and_migrate(path: &Path) -> Result<Connection, Error> {
    let existed_before = path.exists();

    if existed_before {
        verify_db_file_mode(path)?;
    }

    let conn = Connection::open(path).await.map_err(|e| Error::Internal {
        context: "cache::open".into(),
        source: anyhow::Error::new(e),
    })?;

    if !existed_before {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(DB_FILE_MODE))?;
    }

    // Set WAL mode and apply pending migrations on the dedicated `SQLite`
    // thread. `journal_mode = WAL` is a persistent file-level setting;
    // setting it on the first open is enough.
    conn.call(|c| -> Result<(), Error> {
        c.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| Error::Internal {
                context: "cache::pragma_wal".into(),
                source: anyhow::Error::new(e),
            })?;
        migrations::apply_pending(c)?;
        Ok(())
    })
    .await
    .map_err(|outer| match outer {
        tokio_rusqlite::Error::Error(inner) => inner,
        other => Error::Internal {
            context: "cache::migrate".into(),
            source: anyhow::Error::new(other),
        },
    })?;

    Ok(conn)
}

fn verify_db_file_mode(path: &Path) -> Result<(), Error> {
    let meta = std::fs::metadata(path)?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::InsecurePermissions {
            path: path.display().to_string(),
            message: format!(
                "mode is 0{mode:o}, expected 0{DB_FILE_MODE:o} (owner-only). \
                 Fix with `chmod 0{DB_FILE_MODE:o} {}`.",
                path.display()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Returns a tempdir already chmod'd to `0o700` so it satisfies the
    /// cache directory's permission invariant. System temp dirs are
    /// typically created at `0o755`.
    fn tmp() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod tempdir to 0700");
        dir
    }

    /// 5-minute TTL — matches the ADR-0009 default — used by tests that
    /// don't care about expiry behavior.
    const TEST_QUERY_TTL: Duration = Duration::from_mins(5);

    async fn open(dir: &Path, accounts: &[&str]) -> Result<Cache, Error> {
        let accounts: Vec<String> = accounts.iter().map(|s| (*s).to_owned()).collect();
        Cache::new(dir.to_owned(), &accounts, TEST_QUERY_TTL).await
    }

    #[tokio::test]
    async fn opens_fresh_db_at_v1_with_mode_600() {
        let dir = tmp();
        let cache = open(dir.path(), &["work"]).await.expect("open cache");

        // DB file exists, mode 600.
        let db_path = dir.path().join("work.db");
        assert!(
            db_path.exists(),
            "expected DB file at {}",
            db_path.display()
        );
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "DB file mode = 0{mode:o}");

        // Dir mode is owner-only (mask bits 0o077 clear).
        let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode & 0o077,
            0,
            "dir mode 0{dir_mode:o} should be owner-only"
        );

        assert_eq!(cache.account_aliases(), vec!["work"]);
        assert!(cache.connection("work").is_some());
        assert!(cache.connection("missing").is_none());

        // Schema is at v1.
        let conn = cache.connection("work").unwrap().clone();
        let version: i64 = conn
            .call(|c| -> rusqlite::Result<i64> {
                c.query_row(
                    "SELECT schema_version FROM account_state WHERE rowid = 1",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .expect("query version");
        assert_eq!(version, 1);
    }

    #[tokio::test]
    async fn opens_multiple_accounts() {
        let dir = tmp();
        let cache = open(dir.path(), &["work", "personal"])
            .await
            .expect("open cache");
        assert_eq!(cache.account_aliases(), vec!["personal", "work"]);
        assert!(dir.path().join("work.db").exists());
        assert!(dir.path().join("personal.db").exists());
    }

    #[tokio::test]
    async fn reopen_existing_database_is_idempotent() {
        let dir = tmp();
        // First open creates the DB.
        open(dir.path(), &["work"]).await.expect("first open");
        // Second open finds the DB, applies zero pending migrations, succeeds.
        let cache = open(dir.path(), &["work"]).await.expect("second open");
        let conn = cache.connection("work").unwrap().clone();
        let version: i64 = conn
            .call(|c| -> rusqlite::Result<i64> {
                c.query_row(
                    "SELECT schema_version FROM account_state WHERE rowid = 1",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .expect("query version");
        assert_eq!(version, 1);
    }

    #[tokio::test]
    async fn rejects_db_file_wider_than_0600() {
        let dir = tmp();
        // Pre-create the DB at mode 0o644 so the second open fails the check.
        open(dir.path(), &["work"]).await.expect("seed");
        let db_path = dir.path().join("work.db");
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let err = open(dir.path(), &["work"])
            .await
            .expect_err("expected mode rejection");
        match err {
            Error::InsecurePermissions { path, message } => {
                assert!(path.ends_with("work.db"), "path = {path}");
                assert!(message.contains("0644"), "msg = {message}");
            }
            other => panic!("expected InsecurePermissions, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_dir_wider_than_0700() {
        let dir = tmp(); // 0o700 by default per `tmp()`
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod to 0755");
        let err = open(dir.path(), &["work"])
            .await
            .expect_err("expected dir mode rejection");
        assert!(matches!(err, Error::InsecurePermissions { .. }));
    }

    #[tokio::test]
    async fn refuses_to_open_db_with_newer_schema() {
        let dir = tmp();
        // Seed a normal v1 DB first.
        open(dir.path(), &["work"]).await.expect("seed");
        // Manually bump schema_version to one beyond what we know.
        let db_path = dir.path().join("work.db");
        let raw = rusqlite::Connection::open(&db_path).expect("raw open");
        raw.execute(
            "UPDATE account_state SET schema_version = ?1 WHERE rowid = 1",
            rusqlite::params![migrations::MAX_KNOWN_VERSION + 1],
        )
        .expect("bump");
        drop(raw);

        let err = open(dir.path(), &["work"])
            .await
            .expect_err("expected downgrade refusal");
        let msg = err.to_string();
        assert!(
            msg.contains("newer than this binary supports"),
            "msg = {msg}"
        );
    }

    #[tokio::test]
    async fn empty_account_list_creates_dir_but_no_dbs() {
        let dir = tmp();
        // Remove the auto-created tempdir so we exercise the create-dir path.
        std::fs::remove_dir(dir.path()).expect("rmdir tempdir");

        let cache = open(dir.path(), &[])
            .await
            .expect("open cache with no accounts");
        assert!(cache.account_aliases().is_empty());
        assert!(dir.path().exists(), "cache dir should be created");
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "cache dir mode = 0{mode:o}");
    }

    /// Layer 4 snapshot test per [ADR-0009 §"Schema migration mechanism"] —
    /// the schema DDL is asserted via `sqlite_master` to detect silent drift.
    #[tokio::test]
    async fn schema_ddl_snapshot() {
        let dir = tmp();
        let cache = open(dir.path(), &["snap"]).await.expect("open cache");
        let conn = cache.connection("snap").unwrap().clone();
        let ddl: String = conn
            .call(|c| -> rusqlite::Result<String> {
                let mut stmt = c.prepare(
                    "SELECT type, name, sql FROM sqlite_master \
                     WHERE name NOT LIKE 'sqlite_%' \
                     ORDER BY type, name",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        let kind: String = row.get(0)?;
                        let name: String = row.get(1)?;
                        let sql: Option<String> = row.get(2)?;
                        Ok(format!("-- {kind}: {name}\n{}", sql.unwrap_or_default()))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.join("\n\n"))
            })
            .await
            .expect("query schema");
        insta::assert_snapshot!("cache_v1_schema", ddl);
    }
}
