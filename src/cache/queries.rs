//! `SQLite` round-trip logic for the Phase 2 on-demand cache.
//!
//! Each `lookup_*` / `insert_*` pair maps a Gmail response type to one or
//! more rows in the v1 schema ([`super::migrations`]) and back. The
//! mapping is the *only* place that needs to know which column means what
//! — [`super::Cache`] just delegates here.
//!
//! Metadata-vs-full discriminator (per the module docs on
//! [`super::Cache::lookup_thread`]): rows inserted via
//! [`insert_thread_metadata`] write `body_text = NULL` and
//! `attachments_json = NULL`. [`lookup_thread`] refuses to return a thread
//! when any row in it has `body_text IS NULL` — the caller sees a miss
//! and re-fetches via the API.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_rusqlite::Connection;

use crate::error::Error;
use crate::gmail::threads::{
    ParsedAttachment, ParsedMessage, ParsedThread, RawListedThread, RawThreadsList, ThreadMetadata,
    ThreadMetadataMessage,
};

/// Header bundle persisted alongside each cached message. The same shape
/// services full and metadata reads — metadata-only inserts populate
/// `subject` + `from` and leave `to`/`cc` as empty vecs.
#[derive(Debug, Serialize, Deserialize)]
struct HeadersJson {
    #[serde(default)]
    subject: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AttachmentJson {
    attachment_id: String,
    filename: String,
    mime_type: String,
    size_bytes: u64,
}

/// JSON envelope stored in `query_cache.result_ids_json`. Encodes the
/// thread IDs *and* their accompanying snippet + `historyId` so a
/// `list_threads` hit reconstructs the exact `RawThreadsList` Gmail
/// returned, including pagination.
#[derive(Debug, Serialize, Deserialize)]
struct QueryResultJson {
    threads: Vec<QueryResultThreadJson>,
    next_page_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueryResultThreadJson {
    id: String,
    snippet: String,
    history_id: String,
}

// ── thread (full) ─────────────────────────────────────────────────────────────

pub(super) async fn lookup_thread(
    conn: &Arc<Connection>,
    thread_id: &str,
) -> Result<Option<ParsedThread>, Error> {
    let thread_id_owned = thread_id.to_owned();
    let messages: Vec<MessageRow> = conn
        .call(move |c| -> rusqlite::Result<Vec<MessageRow>> {
            let mut stmt = c.prepare(
                "SELECT id, internal_date, headers_json, body_text, attachments_json \
                 FROM messages WHERE thread_id = ?1 AND deleted_at IS NULL \
                 ORDER BY internal_date ASC",
            )?;
            let rows: rusqlite::Result<Vec<MessageRow>> = stmt
                .query_map(rusqlite::params![thread_id_owned], |row| {
                    Ok(MessageRow {
                        id: row.get(0)?,
                        internal_date: row.get(1)?,
                        headers_json: row.get(2)?,
                        body_text: row.get(3)?,
                        attachments_json: row.get(4)?,
                    })
                })?
                .collect();
            rows
        })
        .await
        .map_err(map_tokio_err)?;

    if messages.is_empty() {
        return Ok(None);
    }
    // Metadata-only rows have body_text = NULL; a full lookup refuses to
    // serve a thread that has *any* incomplete message.
    if messages.iter().any(|m| m.body_text.is_none()) {
        return Ok(None);
    }

    let mut parsed = Vec::with_capacity(messages.len());
    for m in messages {
        let labels = fetch_labels(conn, &m.id).await?;
        let headers: HeadersJson = parse_headers(&m.headers_json)?;
        let attachments: Vec<ParsedAttachment> = match m.attachments_json.as_deref() {
            Some(raw) => parse_attachments(raw)?,
            None => Vec::new(),
        };
        parsed.push(ParsedMessage {
            message_id: m.id,
            internal_date_ms: m.internal_date.to_string(),
            label_ids: labels,
            subject: headers.subject,
            from: headers.from,
            to: headers.to,
            cc: headers.cc,
            body_text: m.body_text.unwrap_or_default(),
            attachments,
        });
    }

    Ok(Some(ParsedThread {
        thread_id: thread_id.to_owned(),
        messages: parsed,
    }))
}

pub(super) async fn insert_thread(
    conn: &Arc<Connection>,
    thread: &ParsedThread,
) -> Result<(), Error> {
    let rows = encode_full_thread(thread)?;
    let thread_id = thread.thread_id.clone();
    let now_ms = now_ms();

    conn.call(move |c| -> rusqlite::Result<()> {
        let tx = c.transaction()?;
        // FULL is authoritative. Wipe any prior rows (metadata or stale
        // full) for this thread so the row set is exactly the just-fetched
        // messages. message_labels rows cascade-delete via FK.
        tx.execute(
            "DELETE FROM messages WHERE thread_id = ?1",
            rusqlite::params![thread_id],
        )?;
        tx.execute(
            "INSERT INTO threads (id, snippet, history_id, fetched_at) \
             VALUES (?1, NULL, NULL, ?2) \
             ON CONFLICT(id) DO UPDATE SET fetched_at = excluded.fetched_at",
            rusqlite::params![thread_id, now_ms],
        )?;

        for row in &rows {
            tx.execute(
                "INSERT INTO messages \
                 (id, thread_id, internal_date, headers_json, body_text, body_html, \
                  snippet, has_attachments, attachments_json, raw_size, fetched_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, NULL, ?8, NULL)",
                rusqlite::params![
                    row.id,
                    thread_id,
                    row.internal_date,
                    row.headers_json,
                    row.body_text,
                    row.has_attachments,
                    row.attachments_json,
                    now_ms,
                ],
            )?;
            for label_id in &row.label_ids {
                tx.execute(
                    "INSERT INTO message_labels (message_id, label_id) VALUES (?1, ?2)",
                    rusqlite::params![row.id, label_id],
                )?;
            }
        }
        tx.commit()
    })
    .await
    .map_err(map_tokio_err)?;
    Ok(())
}

// ── thread (metadata) ─────────────────────────────────────────────────────────

pub(super) async fn lookup_thread_metadata(
    conn: &Arc<Connection>,
    thread_id: &str,
) -> Result<Option<ThreadMetadata>, Error> {
    let thread_id_owned = thread_id.to_owned();
    let messages: Vec<MetadataRow> = conn
        .call(move |c| -> rusqlite::Result<Vec<MetadataRow>> {
            let mut stmt = c.prepare(
                "SELECT id, internal_date, headers_json, raw_size \
                 FROM messages WHERE thread_id = ?1 AND deleted_at IS NULL \
                 ORDER BY internal_date ASC",
            )?;
            let rows: rusqlite::Result<Vec<MetadataRow>> = stmt
                .query_map(rusqlite::params![thread_id_owned], |row| {
                    Ok(MetadataRow {
                        id: row.get(0)?,
                        internal_date: row.get(1)?,
                        headers_json: row.get(2)?,
                        raw_size: row.get(3).unwrap_or(0),
                    })
                })?
                .collect();
            rows
        })
        .await
        .map_err(map_tokio_err)?;

    if messages.is_empty() {
        return Ok(None);
    }

    let mut parsed = Vec::with_capacity(messages.len());
    for m in messages {
        let labels = fetch_labels(conn, &m.id).await?;
        let headers: HeadersJson = parse_headers(&m.headers_json)?;
        parsed.push(ThreadMetadataMessage {
            internal_date_ms: m.internal_date.to_string(),
            label_ids: labels,
            size_estimate: m.raw_size.try_into().unwrap_or(0),
            subject: headers.subject,
            from: headers.from,
        });
    }
    Ok(Some(ThreadMetadata {
        thread_id: thread_id.to_owned(),
        messages: parsed,
    }))
}

pub(super) async fn insert_thread_metadata(
    conn: &Arc<Connection>,
    meta: &ThreadMetadata,
) -> Result<(), Error> {
    let rows = encode_metadata_thread(meta)?;
    let thread_id = meta.thread_id.clone();
    let now_ms = now_ms();

    conn.call(move |c| -> rusqlite::Result<()> {
        let tx = c.transaction()?;
        // No-downgrade rule: if any FULL row exists for this thread, leave
        // everything as-is. A metadata read after a full fetch must not
        // contaminate the row set with synthetic-id rows (which would
        // make lookup_thread mistakenly conclude the cache is partial).
        let full_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages \
             WHERE thread_id = ?1 AND body_text IS NOT NULL",
            rusqlite::params![thread_id],
            |row| row.get(0),
        )?;
        if full_count > 0 {
            return tx.commit();
        }
        // Otherwise, replace the metadata row set so a refreshed metadata
        // fetch (e.g. updated labels) doesn't leave stale rows behind.
        tx.execute(
            "DELETE FROM messages WHERE thread_id = ?1",
            rusqlite::params![thread_id],
        )?;
        tx.execute(
            "INSERT INTO threads (id, snippet, history_id, fetched_at) \
             VALUES (?1, NULL, NULL, ?2) \
             ON CONFLICT(id) DO UPDATE SET fetched_at = excluded.fetched_at",
            rusqlite::params![thread_id, now_ms],
        )?;

        for row in &rows {
            tx.execute(
                "INSERT INTO messages \
                 (id, thread_id, internal_date, headers_json, body_text, body_html, \
                  snippet, has_attachments, attachments_json, raw_size, fetched_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 0, NULL, ?5, ?6, NULL)",
                rusqlite::params![
                    row.id,
                    thread_id,
                    row.internal_date,
                    row.headers_json,
                    row.raw_size,
                    now_ms,
                ],
            )?;
            for label_id in &row.label_ids {
                tx.execute(
                    "INSERT INTO message_labels (message_id, label_id) VALUES (?1, ?2)",
                    rusqlite::params![row.id, label_id],
                )?;
            }
        }
        tx.commit()
    })
    .await
    .map_err(map_tokio_err)?;
    Ok(())
}

// ── query_cache (threads.list) ────────────────────────────────────────────────

pub(super) async fn lookup_query(
    conn: &Arc<Connection>,
    query: &str,
    max_results: u32,
    page_token: Option<&str>,
) -> Result<Option<RawThreadsList>, Error> {
    let hash = query_hash(query, max_results, page_token);
    let now = now_ms();

    let raw: Option<String> = conn
        .call(move |c| -> rusqlite::Result<Option<String>> {
            c.query_row(
                "SELECT result_ids_json FROM query_cache \
                 WHERE query_hash = ?1 AND expires_at > ?2",
                rusqlite::params![hash, now],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await
        .map_err(map_tokio_err)?;

    let Some(json) = raw else { return Ok(None) };
    let decoded: QueryResultJson = serde_json::from_str(&json).map_err(|e| Error::Internal {
        context: "cache::lookup_query: decode result_ids_json".into(),
        source: anyhow::Error::new(e),
    })?;
    Ok(Some(RawThreadsList {
        threads: decoded
            .threads
            .into_iter()
            .map(|t| RawListedThread {
                id: t.id,
                snippet: t.snippet,
                history_id: t.history_id,
            })
            .collect(),
        next_page_token: decoded.next_page_token,
    }))
}

pub(super) async fn insert_query(
    conn: &Arc<Connection>,
    query: &str,
    max_results: u32,
    page_token: Option<&str>,
    result: &RawThreadsList,
    ttl: Duration,
) -> Result<(), Error> {
    let hash = query_hash(query, max_results, page_token);
    let payload = QueryResultJson {
        threads: result
            .threads
            .iter()
            .map(|t| QueryResultThreadJson {
                id: t.id.clone(),
                snippet: t.snippet.clone(),
                history_id: t.history_id.clone(),
            })
            .collect(),
        next_page_token: result.next_page_token.clone(),
    };
    let json = serde_json::to_string(&payload).map_err(|e| Error::Internal {
        context: "cache::insert_query: encode result_ids_json".into(),
        source: anyhow::Error::new(e),
    })?;
    let now = now_ms();
    let expires_at = now.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));
    let query_owned = query.to_owned();
    let token_owned = page_token.map(str::to_owned);
    let max_results_i64 = i64::from(max_results);

    conn.call(move |c| -> rusqlite::Result<()> {
        c.execute(
            "INSERT INTO query_cache \
                (query_hash, query, max_results, page_token, result_ids_json, \
                 cached_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(query_hash) DO UPDATE SET \
                result_ids_json = excluded.result_ids_json, \
                cached_at = excluded.cached_at, \
                expires_at = excluded.expires_at",
            rusqlite::params![
                hash,
                query_owned,
                max_results_i64,
                token_owned,
                json,
                now,
                expires_at
            ],
        )?;
        Ok(())
    })
    .await
    .map_err(map_tokio_err)?;
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

struct MessageRow {
    id: String,
    internal_date: i64,
    headers_json: String,
    body_text: Option<String>,
    attachments_json: Option<String>,
}

struct MetadataRow {
    id: String,
    internal_date: i64,
    headers_json: String,
    raw_size: i64,
}

struct EncodedFullMessage {
    id: String,
    internal_date: i64,
    headers_json: String,
    body_text: String,
    has_attachments: i64,
    attachments_json: String,
    label_ids: Vec<String>,
}

struct EncodedMetadataMessage {
    id: String,
    internal_date: i64,
    headers_json: String,
    raw_size: i64,
    label_ids: Vec<String>,
}

fn encode_full_thread(thread: &ParsedThread) -> Result<Vec<EncodedFullMessage>, Error> {
    thread
        .messages
        .iter()
        .map(|m| {
            let headers = HeadersJson {
                subject: m.subject.clone(),
                from: m.from.clone(),
                to: m.to.clone(),
                cc: m.cc.clone(),
            };
            let attachments_json: Vec<AttachmentJson> = m
                .attachments
                .iter()
                .map(|a| AttachmentJson {
                    attachment_id: a.attachment_id.clone(),
                    filename: a.filename.clone(),
                    mime_type: a.mime_type.clone(),
                    size_bytes: a.size_bytes,
                })
                .collect();
            Ok(EncodedFullMessage {
                id: m.message_id.clone(),
                internal_date: parse_internal_date(&m.internal_date_ms),
                headers_json: serde_json::to_string(&headers).map_err(encode_err)?,
                body_text: m.body_text.clone(),
                has_attachments: i64::from(!m.attachments.is_empty()),
                attachments_json: serde_json::to_string(&attachments_json).map_err(encode_err)?,
                label_ids: m.label_ids.clone(),
            })
        })
        .collect()
}

fn encode_metadata_thread(meta: &ThreadMetadata) -> Result<Vec<EncodedMetadataMessage>, Error> {
    meta.messages
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let headers = HeadersJson {
                subject: m.subject.clone(),
                from: m.from.clone(),
                to: Vec::new(),
                cc: Vec::new(),
            };
            // Metadata response from Gmail does not include per-message id
            // for thread.get(format=metadata) calls — ThreadMetadataMessage
            // omits it. Synthesize a stable per-thread index-based key so
            // distinct messages in the same thread don't collide.
            let id = format!("{}::{idx}", meta.thread_id);
            Ok(EncodedMetadataMessage {
                id,
                internal_date: parse_internal_date(&m.internal_date_ms),
                headers_json: serde_json::to_string(&headers).map_err(encode_err)?,
                raw_size: i64::try_from(m.size_estimate).unwrap_or(i64::MAX),
                label_ids: m.label_ids.clone(),
            })
        })
        .collect()
}

async fn fetch_labels(conn: &Arc<Connection>, message_id: &str) -> Result<Vec<String>, Error> {
    let id_owned = message_id.to_owned();
    conn.call(move |c| -> rusqlite::Result<Vec<String>> {
        let mut stmt = c.prepare(
            "SELECT label_id FROM message_labels WHERE message_id = ?1 ORDER BY label_id",
        )?;
        let rows: rusqlite::Result<Vec<String>> = stmt
            .query_map(rusqlite::params![id_owned], |row| row.get::<_, String>(0))?
            .collect();
        rows
    })
    .await
    .map_err(map_tokio_err)
}

fn parse_headers(raw: &str) -> Result<HeadersJson, Error> {
    serde_json::from_str(raw).map_err(|e| Error::Internal {
        context: "cache: decode headers_json".into(),
        source: anyhow::Error::new(e),
    })
}

fn parse_attachments(raw: &str) -> Result<Vec<ParsedAttachment>, Error> {
    let decoded: Vec<AttachmentJson> = serde_json::from_str(raw).map_err(|e| Error::Internal {
        context: "cache: decode attachments_json".into(),
        source: anyhow::Error::new(e),
    })?;
    Ok(decoded
        .into_iter()
        .map(|a| ParsedAttachment {
            attachment_id: a.attachment_id,
            filename: a.filename,
            mime_type: a.mime_type,
            size_bytes: a.size_bytes,
        })
        .collect())
}

/// Parse Gmail's `internalDate` (decimal ms-epoch string) into an i64.
/// Falls back to 0 on parse failure; the column is NOT NULL but the
/// concrete value isn't load-bearing in Phase 2.
fn parse_internal_date(s: &str) -> i64 {
    s.parse::<i64>().unwrap_or(0)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// SHA-256 of `query || "\0" || max_results || "\0" || page_token`,
/// hex-encoded. Distinguishes paginated reads of the same query.
fn query_hash(query: &str, max_results: u32, page_token: Option<&str>) -> String {
    let mut h = Sha256::new();
    h.update(query.as_bytes());
    h.update([0u8]);
    h.update(max_results.to_le_bytes());
    h.update([0u8]);
    if let Some(t) = page_token {
        h.update(t.as_bytes());
    }
    hex::encode(h.finalize())
}

fn encode_err(e: serde_json::Error) -> Error {
    Error::Internal {
        context: "cache: encode json".into(),
        source: anyhow::Error::new(e),
    }
}

fn map_tokio_err(e: tokio_rusqlite::Error) -> Error {
    Error::Internal {
        context: "cache: sql".into(),
        source: anyhow::Error::new(e),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        d
    }

    async fn open_cache(dir: &TempDir) -> Cache {
        Cache::new(
            dir.path().to_owned(),
            &["work".to_owned()],
            Duration::from_mins(5),
        )
        .await
        .expect("open")
    }

    fn sample_thread() -> ParsedThread {
        ParsedThread {
            thread_id: "tid-1".into(),
            messages: vec![ParsedMessage {
                message_id: "m1".into(),
                internal_date_ms: "1717200000000".into(),
                label_ids: vec!["INBOX".into(), "UNREAD".into()],
                subject: "hello".into(),
                from: "alice@example.com".into(),
                to: vec!["bob@example.com".into()],
                cc: vec![],
                body_text: "the body".into(),
                attachments: vec![ParsedAttachment {
                    attachment_id: "att1".into(),
                    filename: "doc.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size_bytes: 1024,
                }],
            }],
        }
    }

    #[tokio::test]
    async fn full_insert_then_lookup_round_trips() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let thread = sample_thread();
        cache.insert_thread("work", &thread).await.expect("insert");
        let got = cache
            .lookup_thread("work", "tid-1")
            .await
            .expect("lookup")
            .expect("hit");
        assert_eq!(got.thread_id, "tid-1");
        assert_eq!(got.messages.len(), 1);
        let m = &got.messages[0];
        assert_eq!(m.message_id, "m1");
        assert_eq!(m.subject, "hello");
        assert_eq!(m.from, "alice@example.com");
        assert_eq!(m.to, vec!["bob@example.com".to_owned()]);
        assert_eq!(m.body_text, "the body");
        assert_eq!(m.label_ids, vec!["INBOX".to_owned(), "UNREAD".to_owned()]);
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].filename, "doc.pdf");
    }

    #[tokio::test]
    async fn lookup_thread_miss_when_absent() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let got = cache.lookup_thread("work", "ghost").await.expect("lookup");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn lookup_thread_miss_when_unknown_account() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let got = cache
            .lookup_thread("unknown-account", "tid-1")
            .await
            .expect("lookup");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn metadata_round_trips_and_does_not_satisfy_full_lookup() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let meta = ThreadMetadata {
            thread_id: "tid-meta".into(),
            messages: vec![ThreadMetadataMessage {
                internal_date_ms: "1717200000000".into(),
                label_ids: vec!["INBOX".into()],
                size_estimate: 4096,
                subject: "meta only".into(),
                from: "carol@example.com".into(),
            }],
        };
        cache
            .insert_thread_metadata("work", &meta)
            .await
            .expect("insert meta");

        let meta_back = cache
            .lookup_thread_metadata("work", "tid-meta")
            .await
            .expect("lookup meta")
            .expect("meta hit");
        assert_eq!(meta_back.messages.len(), 1);
        assert_eq!(meta_back.messages[0].subject, "meta only");
        assert_eq!(meta_back.messages[0].size_estimate, 4096);
        assert_eq!(meta_back.messages[0].label_ids, vec!["INBOX".to_owned()]);

        // The same thread must NOT satisfy lookup_thread (body_text is NULL).
        let full = cache
            .lookup_thread("work", "tid-meta")
            .await
            .expect("lookup");
        assert!(
            full.is_none(),
            "metadata-only cached thread must not satisfy a FULL lookup",
        );
    }

    #[tokio::test]
    async fn full_insert_after_metadata_upgrades_in_place() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        // Same thread_id used by sample_thread() — but seed metadata first.
        let meta = ThreadMetadata {
            thread_id: "tid-1".into(),
            messages: vec![ThreadMetadataMessage {
                internal_date_ms: "1717200000000".into(),
                label_ids: vec!["INBOX".into()],
                size_estimate: 1024,
                subject: "before".into(),
                from: "alice@example.com".into(),
            }],
        };
        cache
            .insert_thread_metadata("work", &meta)
            .await
            .expect("seed meta");

        let thread = sample_thread();
        cache.insert_thread("work", &thread).await.expect("upgrade");

        let full = cache
            .lookup_thread("work", "tid-1")
            .await
            .expect("lookup")
            .expect("hit");
        assert_eq!(full.messages[0].body_text, "the body");
        assert_eq!(full.messages[0].subject, "hello"); // headers refreshed
    }

    #[tokio::test]
    async fn metadata_insert_is_noop_when_full_row_exists() {
        // No-downgrade invariant: once FULL data is cached for a thread,
        // a subsequent metadata fetch for the same thread must not
        // contaminate the row set. lookup_thread must keep returning the
        // FULL data unchanged.
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let thread = sample_thread();
        cache
            .insert_thread("work", &thread)
            .await
            .expect("seed full");

        let meta = ThreadMetadata {
            thread_id: "tid-1".into(),
            messages: vec![ThreadMetadataMessage {
                internal_date_ms: "1717200000000".into(),
                label_ids: vec!["IMPORTANT".into()],
                size_estimate: 9999,
                subject: "different subject".into(),
                from: "eve@example.com".into(),
            }],
        };
        cache
            .insert_thread_metadata("work", &meta)
            .await
            .expect("noop");

        let full = cache
            .lookup_thread("work", "tid-1")
            .await
            .expect("lookup")
            .expect("hit");
        assert_eq!(full.messages.len(), 1, "no extra rows from metadata insert");
        assert_eq!(full.messages[0].message_id, "m1");
        assert_eq!(full.messages[0].body_text, "the body");
        assert_eq!(full.messages[0].subject, "hello"); // FULL headers preserved
        assert_eq!(
            full.messages[0].label_ids,
            vec!["INBOX".to_owned(), "UNREAD".to_owned()],
        );
    }

    #[tokio::test]
    async fn query_round_trip_returns_threads_and_token() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let result = RawThreadsList {
            threads: vec![
                RawListedThread {
                    id: "tid-a".into(),
                    snippet: "preview a".into(),
                    history_id: "100".into(),
                },
                RawListedThread {
                    id: "tid-b".into(),
                    snippet: "preview b".into(),
                    history_id: "101".into(),
                },
            ],
            next_page_token: Some("pg2".into()),
        };
        cache
            .insert_query("work", "from:alice", 10, None, &result)
            .await
            .expect("insert");

        let got = cache
            .lookup_query("work", "from:alice", 10, None)
            .await
            .expect("lookup")
            .expect("hit");
        assert_eq!(got.threads.len(), 2);
        assert_eq!(got.threads[0].id, "tid-a");
        assert_eq!(got.threads[0].snippet, "preview a");
        assert_eq!(got.threads[0].history_id, "100");
        assert_eq!(got.next_page_token.as_deref(), Some("pg2"));
    }

    #[tokio::test]
    async fn query_miss_when_query_or_max_results_or_page_token_differ() {
        let dir = tmp();
        let cache = open_cache(&dir).await;
        let result = RawThreadsList {
            threads: vec![],
            next_page_token: None,
        };
        cache
            .insert_query("work", "is:unread", 25, None, &result)
            .await
            .expect("insert");

        assert!(
            cache
                .lookup_query("work", "is:read", 25, None)
                .await
                .expect("lookup")
                .is_none(),
            "different query string must miss",
        );
        assert!(
            cache
                .lookup_query("work", "is:unread", 50, None)
                .await
                .expect("lookup")
                .is_none(),
            "different max_results must miss",
        );
        assert!(
            cache
                .lookup_query("work", "is:unread", 25, Some("pg2"))
                .await
                .expect("lookup")
                .is_none(),
            "different page_token must miss",
        );
        assert!(
            cache
                .lookup_query("work", "is:unread", 25, None)
                .await
                .expect("lookup")
                .is_some(),
            "exact match must hit",
        );
    }

    #[tokio::test]
    async fn query_lookup_misses_after_ttl_expires() {
        let dir = tmp();
        // 0-ms TTL → every entry is expired the moment it's written.
        let cache = Cache::new(
            dir.path().to_owned(),
            &["work".to_owned()],
            Duration::from_millis(0),
        )
        .await
        .expect("open");
        let result = RawThreadsList {
            threads: vec![],
            next_page_token: None,
        };
        cache
            .insert_query("work", "stale", 5, None, &result)
            .await
            .expect("insert");
        let got = cache
            .lookup_query("work", "stale", 5, None)
            .await
            .expect("lookup");
        assert!(got.is_none(), "expired row should not be returned");
    }
}
