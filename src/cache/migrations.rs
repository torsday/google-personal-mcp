//! Hand-rolled migration framework per [ADR-0009 §Schema migration
//! mechanism](../../../docs/adr/0009-caching-with-sqlite-and-history-api.md).
//!
//! On open: read `account_state.schema_version` (or treat a missing table as
//! version 0); apply every migration whose `from_version` is `<` the on-disk
//! version up to and including the highest known target; each migration runs
//! in a transaction and updates `schema_version` atomically.
//!
//! Refuses to operate if the on-disk version is *higher* than the highest
//! known migration target — this prevents a downgraded daemon from
//! truncating columns it does not understand. ADR-0009 §"Compatibility rule".

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Error;

/// One forward migration step.
pub(crate) struct Migration {
    /// On-disk version this migration applies to. The migration leaves the
    /// database at `to_version`.
    pub(crate) from_version: u32,
    /// Resulting schema version.
    pub(crate) to_version: u32,
    /// SQL applied in a single transaction.
    pub(crate) sql: &'static str,
}

/// The full migration corpus. Append-only; each entry must be the strict
/// successor of the previous (`to_version` of N must equal `from_version` of
/// N+1).
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        from_version: 0,
        to_version: 1,
        sql: include_str!("migrations/001_initial.sql"),
    },
    Migration {
        from_version: 1,
        to_version: 2,
        sql: include_str!("migrations/002_query_cache_history_watermark.sql"),
    },
    Migration {
        from_version: 2,
        to_version: 3,
        sql: include_str!("migrations/003_messages_purged_at.sql"),
    },
];

/// Highest known schema version. Used for the downgrade-refuse check.
pub(crate) const MAX_KNOWN_VERSION: u32 = match MIGRATIONS.last() {
    Some(m) => m.to_version,
    None => 0,
};

/// Read the on-disk schema version. Returns `0` when the `account_state`
/// table is absent (fresh database).
fn current_version(conn: &Connection) -> Result<u32, Error> {
    // First check the table exists at all. A fresh DB has no tables.
    let has_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='account_state'",
            [],
            |_| Ok(true),
        )
        .optional()
        .map_err(rusqlite_to_internal)?
        .unwrap_or(false);

    if !has_table {
        return Ok(0);
    }

    let v: i64 = conn
        .query_row(
            "SELECT schema_version FROM account_state WHERE rowid = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(rusqlite_to_internal)?
        .ok_or_else(|| Error::Internal {
            context: "cache::migrations".into(),
            source: anyhow::anyhow!(
                "account_state table exists but rowid=1 is missing — corrupt cache DB"
            ),
        })?;

    u32::try_from(v).map_err(|_| Error::Internal {
        context: "cache::migrations".into(),
        source: anyhow::anyhow!("schema_version {v} out of range"),
    })
}

/// Apply every pending migration. Returns the resulting version, which is
/// always [`MAX_KNOWN_VERSION`] on success.
///
/// On a fresh database (`current_version` returns 0) this initializes the
/// schema from `001_initial.sql`. The initial migration `INSERT`s the
/// `account_state` row, so subsequent migrations can rely on its presence.
///
/// Errors:
/// - [`Error::Internal`] on a SQL execution failure.
/// - [`Error::Internal`] with a downgrade-refuse message when the on-disk
///   schema is newer than this binary knows about.
pub(crate) fn apply_pending(conn: &mut Connection) -> Result<u32, Error> {
    let current = current_version(conn)?;

    if current > MAX_KNOWN_VERSION {
        return Err(Error::Internal {
            context: "cache::migrations".into(),
            source: anyhow::anyhow!(
                "on-disk schema version {current} is newer than this binary supports \
                 (max known: {MAX_KNOWN_VERSION}). Refusing to open — upgrade the binary \
                 or remove the cache file."
            ),
        });
    }

    for migration in MIGRATIONS {
        if migration.from_version < current {
            continue;
        }
        let tx = conn.transaction().map_err(rusqlite_to_internal)?;
        tx.execute_batch(migration.sql)
            .map_err(rusqlite_to_internal)?;
        // The 001 migration seeds account_state with schema_version = 1.
        // Every later migration updates the existing row.
        if migration.from_version > 0 {
            tx.execute(
                "UPDATE account_state SET schema_version = ?1 WHERE rowid = 1",
                params![migration.to_version],
            )
            .map_err(rusqlite_to_internal)?;
        }
        tx.commit().map_err(rusqlite_to_internal)?;
    }

    Ok(MAX_KNOWN_VERSION)
}

fn rusqlite_to_internal(e: rusqlite::Error) -> Error {
    Error::Internal {
        context: "cache::migrations".into(),
        source: anyhow::Error::new(e),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn in_memory() -> Connection {
        Connection::open_in_memory().expect("open in-memory sqlite")
    }

    #[test]
    fn fresh_database_initializes_to_max_known_version() {
        let mut conn = in_memory();
        let v = apply_pending(&mut conn).expect("apply");
        assert_eq!(v, MAX_KNOWN_VERSION);
        assert_eq!(current_version(&conn).expect("read"), MAX_KNOWN_VERSION);
    }

    #[test]
    fn fresh_database_creates_all_expected_tables() {
        let mut conn = in_memory();
        apply_pending(&mut conn).expect("apply");
        let expected = [
            "messages",
            "message_labels",
            "threads",
            "labels",
            "account_state",
            "query_cache",
        ];
        for table in expected {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |row| row.get(0),
                )
                .optional()
                .expect("query");
            assert!(exists.is_some(), "expected table `{table}` to exist");
        }
    }

    #[test]
    fn apply_pending_is_idempotent() {
        let mut conn = in_memory();
        apply_pending(&mut conn).expect("first apply");
        let v = apply_pending(&mut conn).expect("second apply");
        assert_eq!(v, MAX_KNOWN_VERSION);
    }

    /// A DB seeded at v1 is upgraded to `MAX_KNOWN_VERSION` on next open,
    /// the way an operator's existing cache file behaves after a binary
    /// upgrade.
    #[test]
    fn upgrades_v1_database_to_current() {
        let mut conn = in_memory();
        // Apply only the first migration to simulate a v1-era DB on disk.
        let only_001 = &MIGRATIONS[0..1];
        for m in only_001 {
            let tx = conn.transaction().expect("tx");
            tx.execute_batch(m.sql).expect("apply 001");
            tx.commit().expect("commit");
        }
        assert_eq!(current_version(&conn).expect("read"), 1);
        // Now apply the full corpus — should walk forward to MAX.
        let v = apply_pending(&mut conn).expect("apply");
        assert_eq!(v, MAX_KNOWN_VERSION);
        // The v2 column must exist on query_cache.
        let col: Option<String> = conn
            .query_row(
                "SELECT name FROM pragma_table_info('query_cache') \
                 WHERE name = 'fetched_at_history_id'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("query");
        assert_eq!(col.as_deref(), Some("fetched_at_history_id"));
        // And the v3 column must exist on messages.
        let col: Option<String> = conn
            .query_row(
                "SELECT name FROM pragma_table_info('messages') \
                 WHERE name = 'purged_at'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("query");
        assert_eq!(col.as_deref(), Some("purged_at"));
    }

    /// A v2-era DB (Phase 2-5 schema with `fetched_at_history_id` already
    /// present) upgrades to v3 cleanly without disturbing v2 rows.
    /// Guards against migration 003's `ALTER TABLE` colliding with the
    /// Phase 4 schema state operators currently have on disk.
    #[test]
    fn upgrades_v2_database_to_current() {
        let mut conn = in_memory();
        // Apply 001 + 002 only.
        let through_v2 = &MIGRATIONS[0..2];
        for m in through_v2 {
            let tx = conn.transaction().expect("tx");
            tx.execute_batch(m.sql).expect("apply");
            if m.from_version > 0 {
                tx.execute(
                    "UPDATE account_state SET schema_version = ?1 WHERE rowid = 1",
                    params![m.to_version],
                )
                .expect("bump");
            }
            tx.commit().expect("commit");
        }
        assert_eq!(current_version(&conn).expect("read"), 2);

        // Seed a row so we can prove the existing data survives the upgrade.
        conn.execute(
            "INSERT INTO messages \
             (id, thread_id, internal_date, headers_json, body_text, has_attachments, fetched_at) \
             VALUES ('m1', 't1', 1, '{}', 'body', 0, 100)",
            [],
        )
        .expect("seed");

        let v = apply_pending(&mut conn).expect("apply");
        assert_eq!(v, MAX_KNOWN_VERSION);

        // purged_at column now exists and the seeded row's value is NULL
        // (no backfill — ADR-0019).
        let purged_at: Option<i64> = conn
            .query_row(
                "SELECT purged_at FROM messages WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(
            purged_at, None,
            "v2 → v3 upgrade must not backfill purged_at"
        );
    }

    #[test]
    fn downgrade_is_refused() {
        let mut conn = in_memory();
        apply_pending(&mut conn).expect("initial migrate");
        // Simulate an on-disk schema written by a newer binary.
        conn.execute(
            "UPDATE account_state SET schema_version = ?1 WHERE rowid = 1",
            params![MAX_KNOWN_VERSION + 1],
        )
        .expect("bump");
        let err = apply_pending(&mut conn).expect_err("expected downgrade refusal");
        let msg = err.to_string();
        assert!(
            msg.contains("newer than this binary supports"),
            "got: {msg}"
        );
    }

    #[test]
    fn migration_corpus_is_well_formed() {
        // Each entry must be the strict successor of the previous one,
        // starting at from_version = 0.
        let mut expected = 0;
        for m in MIGRATIONS {
            assert_eq!(
                m.from_version, expected,
                "migration {}→{} expected from_version={expected}",
                m.from_version, m.to_version
            );
            assert_eq!(
                m.to_version,
                m.from_version + 1,
                "migration {}→{} must advance by exactly one",
                m.from_version,
                m.to_version,
            );
            expected = m.to_version;
        }
        assert_eq!(MAX_KNOWN_VERSION, expected);
    }
}
