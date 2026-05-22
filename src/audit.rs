//! Append-only JSONL audit log per
//! [ADR-0011](../docs/adr/0011-audit-log.md).
//!
//! ## Write modes
//!
//! Two write paths cover the v0.x audit needs:
//!
//! - [`AuditWriter::write`] — best-effort. Failures log a warning and the
//!   caller continues. Used for read-only / non-destructive tools and for
//!   the **outcome** record of destructive tools (after the API call).
//! - [`AuditWriter::write_synced`] — fail-closed + `fsync`. Returns an
//!   [`Error::Internal`] on any I/O failure. Used for the **intent**
//!   record of destructive tools, written *before* the Gmail API call
//!   per [ADR-0011 lines 83-86](../docs/adr/0011-audit-log.md) (#66) —
//!   the trust property is that even if the daemon crashes during the
//!   API call, the audit log shows the intent.
//!
//! ## File layout
//!
//! ```text
//! <config_dir>/audit/<account>.jsonl
//! ```
//!
//! The file is created with mode `0600` on first write.

use std::io::Write as _;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Entry ─────────────────────────────────────────────────────────────────────

/// One audit record. Written as a single JSON line (no newline in values).
/// `Deserialize` is for the reader path used by `audit_summary` (#65).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AuditEntry {
    /// RFC 3339 UTC timestamp of the invocation.
    pub timestamp: DateTime<Utc>,
    /// Account alias from `accounts.toml`.
    pub account: String,
    /// MCP tool name (e.g. `"archive_thread"`, `"send_email"`).
    pub tool: String,
    /// Tool-specific parameter summary. Attacker-controlled body content is
    /// redacted (length + sha256 prefix only). Thread IDs and label IDs are
    /// operator-assigned and included as-is.
    pub params_summary: Value,
    /// Outcome of the dedup / dry-run check.
    /// - `"applied"` — Gmail API call was made.
    /// - `"dry_run"` — `dry_run=true`; no API call.
    /// - `"deduped:<prior_message_id>"` — `send_email` detected a duplicate.
    pub action: String,
    /// `"ok"` on success, or `"error: <short description>"` on failure.
    pub result: String,
}

// ── Writer ────────────────────────────────────────────────────────────────────

/// Best-effort audit log writer. `Clone` is cheap (path + flag only).
#[derive(Debug, Clone)]
pub(crate) struct AuditWriter {
    audit_dir: PathBuf,
}

impl AuditWriter {
    /// Create a writer rooted at `<config_dir>/audit/`.
    pub(crate) fn new(config_dir: impl Into<PathBuf>) -> Self {
        let mut audit_dir = config_dir.into();
        audit_dir.push("audit");
        Self { audit_dir }
    }

    /// Path the writer stores JSONL files under. Used by `audit_summary`
    /// (#65) to read back the same directory.
    pub(crate) fn audit_dir(&self) -> &std::path::Path {
        &self.audit_dir
    }

    /// Append `entry` to `<audit_dir>/<account>.jsonl`.
    ///
    /// Best-effort: failures log a warning and return without raising.
    /// Used for outcome records and non-destructive tools, where a
    /// lost audit line is regrettable but not a trust violation.
    pub(crate) fn write(&self, entry: &AuditEntry) {
        if let Err(e) = self.try_write(entry, /* sync = */ false) {
            tracing::warn!(error = %e, account = %entry.account, tool = %entry.tool, "audit write failed");
        }
    }

    /// Append `entry` to `<audit_dir>/<account>.jsonl` and `fsync` the
    /// file before returning.
    ///
    /// On any I/O failure returns [`Error::Internal`] so callers can
    /// **refuse the destructive op** rather than silently proceeding —
    /// this is the fail-closed half of ADR-0011's intent-record trust
    /// property (#66). The synced write happens *before* the Google
    /// API call; if the daemon crashes between this fsync and the API
    /// returning, the operator can reconcile from the durable intent.
    pub(crate) fn write_synced(&self, entry: &AuditEntry) -> Result<(), crate::error::Error> {
        self.try_write(entry, /* sync = */ true).map_err(|e| {
            tracing::error!(
                error = %e,
                account = %entry.account,
                tool = %entry.tool,
                "audit fsync failed — refusing destructive op"
            );
            crate::error::Error::Internal {
                context: format!("audit write_synced for tool `{}`", entry.tool),
                source: anyhow::Error::new(e),
            }
        })
    }

    fn try_write(&self, entry: &AuditEntry, sync: bool) -> std::io::Result<()> {
        // Reject account values that could escape `audit_dir` via path traversal
        // or absolute-path replacement. The MCP layer should validate too, but
        // this is the last line of defense before disk I/O. See issue #101.
        if !is_safe_account_alias(&entry.account) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to write audit entry: account alias {:?} fails [A-Za-z0-9_-]+ rule",
                    entry.account
                ),
            ));
        }

        std::fs::create_dir_all(&self.audit_dir)?;

        let path = self.audit_dir.join(format!("{}.jsonl", entry.account));

        // Serialize to a single JSON line (no embedded newlines in values since
        // serde_json compact serialization never emits them).
        let line =
            serde_json::to_string(entry).map_err(|e| std::io::Error::other(e.to_string()))?;

        // Open with append + create; set 0600 on first creation.
        let mut file = open_append(&path)?;
        writeln!(file, "{line}")?;
        if sync {
            // fsync the file descriptor so the intent record is on
            // durable storage before we return — per ADR-0011 lines
            // 83-86 (#66). `sync_data` skips the metadata flush that
            // `sync_all` would do; append-only files don't carry
            // metadata an operator would need across a crash.
            file.sync_data()?;
        }
        Ok(())
    }
}

/// Validate that `account` is a safe alias: non-empty and matches `[A-Za-z0-9_-]+`.
///
/// Mirrors `validate_alias` in [`src/auth/cli.rs`](crate::auth::cli). Used to
/// prevent path traversal (`"../.."`) and absolute-path replacement (`"/etc/x"`)
/// in audit-log filenames — see issue #101. `PathBuf::join` discards the base
/// when joined with an absolute path, and does not normalize `..`.
fn is_safe_account_alias(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// Open `path` in append mode (create if absent, mode 0600 on Unix).
fn open_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
    }
}

// ── Per-tool params_summary builders ─────────────────────────────────────────
//
// One builder per v0.2 tool, matching the redaction table in
// [ADR-0011 §Redaction rules per tool](../docs/adr/0011-audit-log.md).
//
// Each builder either takes no `verbosity` argument (the redacted form is the
// only sane form — e.g. IDs aren't content) or accepts a [`Verbosity`] flag
// so the operator-opt-in verbose mode can be wired by a future ticket (#68)
// without changing call sites again.

/// Operator-facing audit verbosity per ADR-0011. Default redacted; verbose
/// is opt-in via the (forthcoming) `[audit] verbose = true` config flag (#68).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verbosity {
    /// Redact attacker-controlled content (default).
    Redacted,
    /// Operator opt-in: full content for the operator's own audit log.
    Verbose,
}

// ── Single-thread tools — no content to redact ───────────────────────────────

/// `archive_thread` params: just `thread_id` + `dry_run`. Both are
/// operator-controlled IDs, not content — no verbosity dimension.
pub(crate) fn summarize_archive_thread(thread_id: &str, dry_run: bool) -> Value {
    serde_json::json!({
        "thread_id": thread_id,
        "dry_run": dry_run,
    })
}

/// `trash_thread` params: same shape as `archive_thread`.
pub(crate) fn summarize_trash_thread(thread_id: &str, dry_run: bool) -> Value {
    serde_json::json!({
        "thread_id": thread_id,
        "dry_run": dry_run,
    })
}

/// `modify_thread_labels` params: thread + label IDs + `dry_run`. Label IDs
/// are operator-assigned (system labels like `INBOX` or user-created); not
/// considered content.
pub(crate) fn summarize_modify_thread_labels(
    thread_id: &str,
    add_label_ids: &[String],
    remove_label_ids: &[String],
    dry_run: bool,
) -> Value {
    serde_json::json!({
        "thread_id": thread_id,
        "add_label_ids": add_label_ids,
        "remove_label_ids": remove_label_ids,
        "dry_run": dry_run,
    })
}

/// `get_thread` params: `thread_id` only — no content even in verbose mode.
pub(crate) fn summarize_get_thread(thread_id: &str) -> Value {
    serde_json::json!({ "thread_id": thread_id })
}

// ── Batch tools — `thread_ids` redacts to count + first/last ─────────────────

/// `batch_archive` params. Redacted: `{thread_ids_count, thread_ids_first,
/// thread_ids_last, dry_run}`. Verbose: full `thread_ids` array.
pub(crate) fn summarize_batch_archive(
    thread_ids: &[String],
    verbosity: Verbosity,
    dry_run: bool,
) -> Value {
    let mut m = serde_json::Map::new();
    redact_thread_ids(&mut m, thread_ids, verbosity);
    m.insert("dry_run".into(), Value::Bool(dry_run));
    Value::Object(m)
}

/// `batch_trash` params — same redaction shape as `batch_archive`.
pub(crate) fn summarize_batch_trash(
    thread_ids: &[String],
    verbosity: Verbosity,
    dry_run: bool,
) -> Value {
    let mut m = serde_json::Map::new();
    redact_thread_ids(&mut m, thread_ids, verbosity);
    m.insert("dry_run".into(), Value::Bool(dry_run));
    Value::Object(m)
}

/// `batch_modify_thread_labels` params. Same `thread_ids` redaction; label
/// IDs are always included regardless of verbosity per the ADR.
pub(crate) fn summarize_batch_modify_thread_labels(
    thread_ids: &[String],
    add_label_ids: &[String],
    remove_label_ids: &[String],
    verbosity: Verbosity,
    dry_run: bool,
) -> Value {
    let mut m = serde_json::Map::new();
    redact_thread_ids(&mut m, thread_ids, verbosity);
    m.insert(
        "add_label_ids".into(),
        Value::Array(
            add_label_ids
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    m.insert(
        "remove_label_ids".into(),
        Value::Array(
            remove_label_ids
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    m.insert("dry_run".into(), Value::Bool(dry_run));
    Value::Object(m)
}

/// Populate `m` with redacted `thread_ids_*` fields, or the full `thread_ids`
/// array in verbose mode. Shared by every batch tool's summarizer.
fn redact_thread_ids(
    m: &mut serde_json::Map<String, Value>,
    thread_ids: &[String],
    verbosity: Verbosity,
) {
    if verbosity == Verbosity::Verbose {
        m.insert(
            "thread_ids".into(),
            Value::Array(
                thread_ids
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
        return;
    }
    m.insert(
        "thread_ids_count".into(),
        Value::Number(serde_json::Number::from(thread_ids.len())),
    );
    if let Some(first) = thread_ids.first() {
        m.insert("thread_ids_first".into(), Value::String(first.clone()));
    }
    if thread_ids.len() > 1 {
        if let Some(last) = thread_ids.last() {
            m.insert("thread_ids_last".into(), Value::String(last.clone()));
        }
    }
}

// ── search_threads — query is potentially sensitive content ──────────────────

/// `search_threads` params. Redacted: `query_len` + `query_token_count`
/// only. Verbose: full `query`. Token count uses whitespace splitting — a
/// readable signal of search complexity without echoing the operator's
/// terms.
pub(crate) fn summarize_search_threads(query: &str, verbosity: Verbosity) -> Value {
    let mut m = serde_json::Map::new();
    if verbosity == Verbosity::Verbose {
        m.insert("query".into(), Value::String(query.to_owned()));
    } else {
        m.insert(
            "query_len".into(),
            Value::Number(serde_json::Number::from(query.len())),
        );
        m.insert(
            "query_token_count".into(),
            Value::Number(serde_json::Number::from(query.split_whitespace().count())),
        );
    }
    Value::Object(m)
}

// ── send_email — most attacker-controlled fields; tightest redaction ─────────

/// `send_email` params per ADR-0011. Redacted mode:
/// - `to` collapses to one entry per *unique domain* (local-parts stripped)
/// - `subject` → length only
/// - `body` → length + sha256 prefix only (the hash supports correlating
///   audit records with the dedup cache per ADR-0012 without leaking content)
/// - `cc` → count only
///
/// Verbose mode: full `to`, full `subject`, first 200 chars of body
/// preview (truncated, char-boundary safe), full `cc`. The 200-char ceiling
/// matches the ADR.
pub(crate) fn summarize_send_email(
    to: &[String],
    subject: &str,
    body: &str,
    body_sha256_prefix: &str,
    cc: &[String],
    verbosity: Verbosity,
    dry_run: bool,
) -> Value {
    let mut m = serde_json::Map::new();
    match verbosity {
        Verbosity::Redacted => {
            m.insert("to_domains".into(), Value::Array(to_domains(to)));
            m.insert(
                "subject_len".into(),
                Value::Number(serde_json::Number::from(subject.len())),
            );
            m.insert(
                "body_len".into(),
                Value::Number(serde_json::Number::from(body.len())),
            );
            m.insert(
                "body_sha256_prefix".into(),
                Value::String(body_sha256_prefix.to_owned()),
            );
            m.insert(
                "cc_count".into(),
                Value::Number(serde_json::Number::from(cc.len())),
            );
        }
        Verbosity::Verbose => {
            m.insert(
                "to".into(),
                Value::Array(to.iter().map(|s| Value::String(s.clone())).collect()),
            );
            m.insert("subject".into(), Value::String(subject.to_owned()));
            m.insert("body_preview".into(), Value::String(truncate_body(body)));
            m.insert(
                "body_len".into(),
                Value::Number(serde_json::Number::from(body.len())),
            );
            m.insert(
                "body_sha256_prefix".into(),
                Value::String(body_sha256_prefix.to_owned()),
            );
            m.insert(
                "cc".into(),
                Value::Array(cc.iter().map(|s| Value::String(s.clone())).collect()),
            );
        }
    }
    m.insert("dry_run".into(), Value::Bool(dry_run));
    Value::Object(m)
}

/// Extract the deduplicated set of domains from a list of addresses. Output
/// is in first-occurrence order so the audit log is deterministic. Inputs
/// without an `@` surface as `<unparseable>` so the operator can still tell
/// *something* was passed.
fn to_domains(to: &[String]) -> Vec<Value> {
    let mut seen: Vec<String> = Vec::with_capacity(to.len());
    for addr in to {
        let domain = addr
            .rsplit_once('@')
            .map_or("<unparseable>", |(_local, dom)| dom)
            .to_owned();
        if !seen.contains(&domain) {
            seen.push(domain);
        }
    }
    seen.into_iter().map(Value::String).collect()
}

/// Truncate `body` to the first 200 chars (char-boundary safe). ADR-0011
/// caps the verbose-mode body preview at 200 chars; longer bodies get an
/// ellipsis appended.
fn truncate_body(body: &str) -> String {
    const CAP: usize = 200;
    if body.chars().count() <= CAP {
        return body.to_owned();
    }
    body.chars().take(CAP).collect::<String>() + "…"
}

// ── download_attachment — paths are operator-chosen; no content ──────────────

/// `download_attachment` params. The `save_to` path is operator-chosen on
/// every call, so it's not redacted; the ADR notes the operator owns this
/// choice. No verbosity dimension.
pub(crate) fn summarize_download_attachment(
    attachment_id: &str,
    mime_type: &str,
    size_bytes: u64,
    save_to: &str,
) -> Value {
    serde_json::json!({
        "attachment_id": attachment_id,
        "mime_type": mime_type,
        "size_bytes": size_bytes,
        "save_to": save_to,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(account: &str, tool: &str, action: &str) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            account: account.into(),
            tool: tool.into(),
            params_summary: serde_json::json!({"thread_id": "tid1", "dry_run": false}),
            action: action.into(),
            result: "ok".into(),
        }
    }

    #[test]
    fn write_creates_file_and_appends() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        writer.write(&make_entry("personal", "archive_thread", "applied"));
        writer.write(&make_entry("personal", "trash_thread", "applied"));

        let path = dir.path().join("audit/personal.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"archive_thread\""));
        assert!(lines[1].contains("\"trash_thread\""));
    }

    #[test]
    fn each_line_is_valid_json() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        writer.write(&make_entry("work", "modify_thread_labels", "applied"));

        let path = dir.path().join("audit/work.jsonl");
        let line = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(line.trim()).expect("valid JSON");
        assert_eq!(parsed["tool"], "modify_thread_labels");
        assert_eq!(parsed["account"], "work");
        assert_eq!(parsed["action"], "applied");
        assert_eq!(parsed["result"], "ok");
    }

    #[test]
    fn line_does_not_contain_embedded_newlines() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());
        writer.write(&make_entry("personal", "archive_thread", "applied"));

        let path = dir.path().join("audit/personal.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        // The file should have exactly one \n (the trailing newline from writeln!)
        assert_eq!(content.matches('\n').count(), 1);
    }

    // ── write_synced + fail-closed semantics (#66, ADR-0011) ──────────────────

    #[test]
    fn write_synced_persists_intent_record() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        let entry = AuditEntry {
            timestamp: Utc::now(),
            account: "personal".into(),
            tool: "archive_thread".into(),
            params_summary: serde_json::json!({"thread_id": "tid1", "dry_run": false}),
            action: "intent".into(),
            result: "pending".into(),
        };
        writer
            .write_synced(&entry)
            .expect("synced write should succeed");

        // Intent record durable on disk before we return — that's the
        // trust property under test.
        let line = std::fs::read_to_string(dir.path().join("audit/personal.jsonl")).unwrap();
        let parsed: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["action"], "intent");
        assert_eq!(parsed["result"], "pending");
        assert_eq!(parsed["tool"], "archive_thread");
    }

    #[test]
    fn write_synced_returns_internal_error_on_unsafe_account() {
        // Path-traversal-shaped alias triggers the is_safe_account_alias
        // guard, which we treat as a synthetic I/O failure — the
        // destructive op must refuse rather than swallow.
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());
        let entry = AuditEntry {
            timestamp: Utc::now(),
            account: "../escape".into(),
            tool: "archive_thread".into(),
            params_summary: serde_json::json!({}),
            action: "intent".into(),
            result: "pending".into(),
        };
        let err = writer.write_synced(&entry).expect_err("must fail closed");
        assert!(matches!(err, crate::error::Error::Internal { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn write_synced_returns_internal_error_when_audit_dir_unwritable() {
        // Simulate a real I/O failure: point the writer at a read-only
        // directory and prove the synced write surfaces Error::Internal
        // rather than silently dropping the intent record.
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        // Pre-create the audit subdir as read-only so create_dir_all
        // succeeds (idempotent) but the file open fails with EACCES.
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir(&audit_dir).unwrap();
        let mut perms = std::fs::metadata(&audit_dir).unwrap().permissions();
        perms.set_mode(0o500); // r-x — no write
        std::fs::set_permissions(&audit_dir, perms).unwrap();

        let writer = AuditWriter::new(dir.path());
        let entry = AuditEntry {
            timestamp: Utc::now(),
            account: "personal".into(),
            tool: "archive_thread".into(),
            params_summary: serde_json::json!({}),
            action: "intent".into(),
            result: "pending".into(),
        };
        let result = writer.write_synced(&entry);

        // Restore perms so TempDir's Drop can clean up regardless.
        let audit_dir = dir.path().join("audit");
        let mut perms = std::fs::metadata(&audit_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&audit_dir, perms).ok();

        let err = result.expect_err("must fail closed on EACCES");
        assert!(matches!(err, crate::error::Error::Internal { .. }));
    }

    #[test]
    fn separate_accounts_use_separate_files() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        writer.write(&make_entry("personal", "archive_thread", "applied"));
        writer.write(&make_entry("work", "trash_thread", "applied"));

        assert!(dir.path().join("audit/personal.jsonl").exists());
        assert!(dir.path().join("audit/work.jsonl").exists());
    }

    #[test]
    fn dry_run_entries_recorded() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        let entry = AuditEntry {
            timestamp: Utc::now(),
            account: "personal".into(),
            tool: "archive_thread".into(),
            params_summary: serde_json::json!({"thread_id": "tid1", "dry_run": true}),
            action: "dry_run".into(),
            result: "ok".into(),
        };
        writer.write(&entry);

        let path = dir.path().join("audit/personal.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"dry_run\""));
        assert!(content.contains("\"action\":\"dry_run\""));
    }

    // ── Per-tool extra-builder tests (ADR-0011 §Redaction rules per tool) ──

    // archive_thread / trash_thread — IDs only, no verbosity dimension.

    #[test]
    fn archive_thread_summary_has_id_and_dry_run() {
        let s = summarize_archive_thread("tid1", false);
        assert_eq!(s["thread_id"], "tid1");
        assert_eq!(s["dry_run"], false);
    }

    #[test]
    fn trash_thread_summary_has_id_and_dry_run() {
        let s = summarize_trash_thread("tid7", true);
        assert_eq!(s["thread_id"], "tid7");
        assert_eq!(s["dry_run"], true);
    }

    #[test]
    fn modify_thread_labels_summary_has_id_and_labels() {
        let s = summarize_modify_thread_labels(
            "tid",
            &["INBOX".into()],
            &["STARRED".into(), "UNREAD".into()],
            false,
        );
        assert_eq!(s["thread_id"], "tid");
        assert_eq!(s["add_label_ids"], serde_json::json!(["INBOX"]));
        assert_eq!(
            s["remove_label_ids"],
            serde_json::json!(["STARRED", "UNREAD"])
        );
    }

    #[test]
    fn get_thread_summary_has_id_only() {
        let s = summarize_get_thread("tid42");
        assert_eq!(s["thread_id"], "tid42");
        // No other fields — get_thread is read-only with no other meaningful params.
        assert_eq!(s.as_object().unwrap().len(), 1);
    }

    // batch tools — redact thread_ids to count + first/last by default.

    #[test]
    fn batch_archive_redacted_omits_full_list() {
        let ids: Vec<String> = (0..10).map(|i| format!("tid{i}")).collect();
        let s = summarize_batch_archive(&ids, Verbosity::Redacted, false);
        assert_eq!(s["thread_ids_count"], 10);
        assert_eq!(s["thread_ids_first"], "tid0");
        assert_eq!(s["thread_ids_last"], "tid9");
        assert!(
            s.get("thread_ids").is_none(),
            "redacted must not emit full list"
        );
    }

    #[test]
    fn batch_archive_redacted_single_id_omits_last() {
        let s = summarize_batch_archive(&["only".into()], Verbosity::Redacted, false);
        assert_eq!(s["thread_ids_count"], 1);
        assert_eq!(s["thread_ids_first"], "only");
        // last is suppressed when count == 1 (it would duplicate first).
        assert!(s.get("thread_ids_last").is_none());
    }

    #[test]
    fn batch_archive_redacted_empty_ok() {
        let s = summarize_batch_archive(&[], Verbosity::Redacted, false);
        assert_eq!(s["thread_ids_count"], 0);
        assert!(s.get("thread_ids_first").is_none());
        assert!(s.get("thread_ids_last").is_none());
    }

    #[test]
    fn batch_archive_verbose_includes_full_list() {
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let s = summarize_batch_archive(&ids, Verbosity::Verbose, true);
        assert_eq!(s["thread_ids"], serde_json::json!(["a", "b", "c"]));
        assert_eq!(s["dry_run"], true);
        assert!(
            s.get("thread_ids_count").is_none(),
            "verbose must not emit the redacted-form count field"
        );
    }

    #[test]
    fn batch_trash_uses_same_redaction_as_batch_archive() {
        let ids: Vec<String> = vec!["x".into(), "y".into()];
        let s = summarize_batch_trash(&ids, Verbosity::Redacted, false);
        assert_eq!(s["thread_ids_count"], 2);
        assert_eq!(s["thread_ids_first"], "x");
        assert_eq!(s["thread_ids_last"], "y");
    }

    #[test]
    fn batch_modify_thread_labels_redacts_ids_keeps_labels() {
        let ids: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let s = summarize_batch_modify_thread_labels(
            &ids,
            &["INBOX".into()],
            &[],
            Verbosity::Redacted,
            false,
        );
        assert_eq!(s["thread_ids_count"], 5);
        assert_eq!(s["thread_ids_first"], "t0");
        assert_eq!(s["thread_ids_last"], "t4");
        assert_eq!(s["add_label_ids"], serde_json::json!(["INBOX"]));
        assert_eq!(s["remove_label_ids"], serde_json::json!([]));
    }

    // search_threads — query content is potentially sensitive.

    #[test]
    fn search_threads_redacted_emits_len_and_token_count_only() {
        let query = "from:alice subject:secret-project";
        let s = summarize_search_threads(query, Verbosity::Redacted);
        assert_eq!(s["query_len"], query.len());
        assert_eq!(s["query_token_count"], 2);
        assert!(s.get("query").is_none(), "redacted must not echo query");
    }

    #[test]
    fn search_threads_verbose_emits_full_query() {
        let s = summarize_search_threads("from:alice", Verbosity::Verbose);
        assert_eq!(s["query"], "from:alice");
        assert!(s.get("query_len").is_none());
    }

    #[test]
    fn search_threads_redacted_does_not_leak_content() {
        // Sentinel content must not appear anywhere in the serialized output.
        let sentinel = "S3CRET-PROJECT-NAME";
        let s = summarize_search_threads(&format!("subject:{sentinel}"), Verbosity::Redacted);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains(sentinel), "redacted leaked query: {json}");
    }

    // send_email — strictest redaction; PII never appears in default mode.

    #[test]
    fn send_email_redacted_strips_local_parts_and_subject() {
        let subject = "Q3 forecast";
        let body = "Hi team, attached is the forecast for Q3. Confidential.";
        let s = summarize_send_email(
            &[
                "alice@example.com".into(),
                "bob@example.com".into(),
                "carol@other.org".into(),
            ],
            subject,
            body,
            "deadbeef",
            &["cc1@x.com".into(), "cc2@y.com".into()],
            Verbosity::Redacted,
            false,
        );
        // Domains are deduped + in first-occurrence order.
        assert_eq!(
            s["to_domains"],
            serde_json::json!(["example.com", "other.org"])
        );
        assert_eq!(s["subject_len"], subject.len());
        assert_eq!(s["body_len"], body.len());
        assert_eq!(s["body_sha256_prefix"], "deadbeef");
        assert_eq!(s["cc_count"], 2);
        assert!(s.get("subject").is_none());
        assert!(s.get("body").is_none());
        assert!(s.get("body_preview").is_none());
        assert!(s.get("to").is_none());
        assert!(s.get("cc").is_none());
    }

    #[test]
    fn send_email_redacted_does_not_leak_pii() {
        // The most important assertion in this whole file: no recipient
        // local-part, no subject text, no body text in default-mode output.
        let sentinels = [
            "alice",
            "S3CRET-DEAL",
            "highly-confidential-body-content",
            "bob",
            "carol",
        ];
        let s = summarize_send_email(
            &["alice@x.example.com".into(), "bob@x.example.com".into()],
            "Re: S3CRET-DEAL",
            "Body: highly-confidential-body-content",
            "abc",
            &["carol@cc.example".into()],
            Verbosity::Redacted,
            false,
        );
        let json = serde_json::to_string(&s).unwrap();
        for sentinel in sentinels {
            assert!(
                !json.contains(sentinel),
                "redacted send_email leaked {sentinel:?}: {json}"
            );
        }
    }

    #[test]
    fn send_email_verbose_includes_full_to_and_subject_and_body_preview() {
        let s = summarize_send_email(
            &["alice@x.com".into()],
            "Hello",
            "The full body content here.",
            "abc",
            &[],
            Verbosity::Verbose,
            false,
        );
        assert_eq!(s["to"], serde_json::json!(["alice@x.com"]));
        assert_eq!(s["subject"], "Hello");
        assert_eq!(s["body_preview"], "The full body content here.");
        assert_eq!(s["body_len"], 27);
    }

    #[test]
    fn send_email_verbose_truncates_body_at_200_chars() {
        // 250 'a' chars; verbose preview must be exactly 200 + ellipsis.
        let body = "a".repeat(250);
        let s = summarize_send_email(
            &["x@y.com".into()],
            "",
            &body,
            "",
            &[],
            Verbosity::Verbose,
            false,
        );
        let preview = s["body_preview"].as_str().unwrap();
        let count = preview.chars().count();
        // 200 'a' chars + the ellipsis character = 201 chars.
        assert_eq!(count, 201, "got {count} chars: {preview}");
        assert!(preview.ends_with('…'));
        assert_eq!(s["body_len"], 250);
    }

    #[test]
    fn send_email_truncation_is_char_boundary_safe() {
        // 200 multi-byte chars, each 3 bytes (CJK). String truncation via
        // byte slicing would panic; char-boundary-safe truncation must not.
        let body = "好".repeat(250);
        let s = summarize_send_email(
            &["x@y.com".into()],
            "",
            &body,
            "",
            &[],
            Verbosity::Verbose,
            false,
        );
        let preview = s["body_preview"].as_str().unwrap();
        assert_eq!(preview.chars().count(), 201);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn send_email_redacted_handles_unparseable_address() {
        let s = summarize_send_email(
            &["no-at-sign".into(), "valid@example.com".into()],
            "x",
            "y",
            "z",
            &[],
            Verbosity::Redacted,
            false,
        );
        // <unparseable> stays in the output as a signal that something was
        // there, but local part never leaks.
        assert_eq!(
            s["to_domains"],
            serde_json::json!(["<unparseable>", "example.com"])
        );
    }

    #[test]
    fn send_email_redacted_dedupes_repeated_domains() {
        let s = summarize_send_email(
            &[
                "a@example.com".into(),
                "b@example.com".into(),
                "c@example.com".into(),
            ],
            "x",
            "y",
            "z",
            &[],
            Verbosity::Redacted,
            false,
        );
        assert_eq!(s["to_domains"], serde_json::json!(["example.com"]));
    }

    // download_attachment — operator-controlled fields only.

    #[test]
    fn download_attachment_summary_has_all_fields() {
        let s = summarize_download_attachment(
            "att1",
            "application/pdf",
            4096,
            "/Users/me/Downloads/report.pdf",
        );
        assert_eq!(s["attachment_id"], "att1");
        assert_eq!(s["mime_type"], "application/pdf");
        assert_eq!(s["size_bytes"], 4096);
        assert_eq!(s["save_to"], "/Users/me/Downloads/report.pdf");
    }

    #[cfg(unix)]
    #[test]
    fn file_created_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());
        writer.write(&make_entry("personal", "archive_thread", "applied"));

        let path = dir.path().join("audit/personal.jsonl");
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "audit file should be 0600, got {mode:o}");
    }

    // ── Path-traversal defense (#101) ──────────────────────────────────────

    #[test]
    fn is_safe_account_alias_accepts_valid_names() {
        for ok in [
            "personal",
            "work",
            "a",
            "user-1",
            "user_1",
            "A1",
            "ABC_def-123",
        ] {
            assert!(is_safe_account_alias(ok), "should accept {ok:?}");
        }
    }

    #[test]
    fn is_safe_account_alias_rejects_traversal_and_unsafe() {
        for bad in [
            "",                // empty
            "..",              // pure parent
            "../etc/passwd",   // relative traversal
            "/tmp/evil",       // absolute path
            "/etc/cron.daily", // absolute path 2
            "a/b",             // path separator
            "a\\b",            // backslash
            "a.b",             // dot — could compose with `.jsonl`
            "a\x00b",          // null byte
            "a\nb",            // newline (would break JSONL grep too)
            "a b",             // whitespace
            " leading",        // leading space
            "trailing ",       // trailing space
            "café",            // non-ASCII
        ] {
            assert!(!is_safe_account_alias(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_relative_traversal_no_file_created() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        writer.write(&make_entry("../etc/x", "archive_thread", "applied"));

        // Neither the bogus traversal path nor the audit_dir's own file should exist.
        assert!(
            !dir.path().join("etc/x.jsonl").exists(),
            "traversal must not write outside audit_dir"
        );
        assert!(
            !dir.path().join("audit/../etc/x.jsonl").exists(),
            "traversal must not resolve into audit_dir"
        );
    }

    #[test]
    fn rejects_absolute_path_no_file_created() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        // The dangerous shape — `PathBuf::join("/tmp/x")` discards the base.
        // The validator must catch this before `join` is reached.
        writer.write(&make_entry(
            "/tmp/pwn-test-audit",
            "archive_thread",
            "applied",
        ));

        assert!(
            !std::path::Path::new("/tmp/pwn-test-audit.jsonl").exists(),
            "absolute-path account must not create a file outside audit_dir"
        );
    }

    #[test]
    fn rejects_empty_account_no_file_created() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        writer.write(&make_entry("", "archive_thread", "applied"));

        // Audit dir may be created, but no `.jsonl` should land in it.
        let audit_dir = dir.path().join("audit");
        if audit_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&audit_dir)
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            assert!(
                entries.is_empty(),
                "empty account must not create any file; got {entries:?}"
            );
        }
    }

    #[test]
    fn rejects_path_separator_no_file_created() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        writer.write(&make_entry("a/b", "archive_thread", "applied"));

        assert!(
            !dir.path().join("audit/a/b.jsonl").exists(),
            "path-separator account must not create nested file"
        );
    }

    #[test]
    fn rejects_null_byte_no_file_created() {
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());

        writer.write(&make_entry("a\x00b", "archive_thread", "applied"));

        // Null byte would fail at the OS layer anyway, but the validator catches it first.
        let audit_dir = dir.path().join("audit");
        if audit_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&audit_dir)
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            assert!(
                entries.is_empty(),
                "null-byte account must not create any file; got {entries:?}"
            );
        }
    }

    #[test]
    fn valid_account_still_writes_after_validator_added() {
        // Regression guard: the validator must not break the happy path.
        let dir = TempDir::new().unwrap();
        let writer = AuditWriter::new(dir.path());
        writer.write(&make_entry("work-1_personal", "send_email", "applied"));

        let path = dir.path().join("audit/work-1_personal.jsonl");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"send_email\""));
    }
}
