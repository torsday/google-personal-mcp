//! Append-only JSONL audit log per
//! [ADR-0011](../docs/adr/0011-audit-log.md) v0.2 subset.
//!
//! One JSONL record is written per destructive tool invocation (including
//! `dry_run` calls). Audit writes are best-effort in v0.2 — a write failure
//! logs a warning but does **not** block the operation. The v1.0 fail-closed
//! and fsync-per-record semantics are deferred.
//!
//! # File layout
//!
//! ```text
//! <config_dir>/audit/<account>.jsonl
//! ```
//!
//! The file is created with mode `0600` on first write.

use std::io::Write as _;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

// ── Entry ─────────────────────────────────────────────────────────────────────

/// One audit record. Written as a single JSON line (no newline in values).
#[derive(Debug, Serialize)]
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

    /// Append `entry` to `<audit_dir>/<account>.jsonl`.
    ///
    /// Write failures are logged as warnings; they never return an error so
    /// callers don't need to handle them (v0.2 best-effort semantics).
    pub(crate) fn write(&self, entry: &AuditEntry) {
        if let Err(e) = self.try_write(entry) {
            tracing::warn!(error = %e, account = %entry.account, tool = %entry.tool, "audit write failed");
        }
    }

    fn try_write(&self, entry: &AuditEntry) -> std::io::Result<()> {
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a `params_summary` for non-send destructive tools (archive, trash,
/// `modify_labels`). Thread IDs and label IDs are operator-controlled and
/// included as-is; no redaction needed.
pub(crate) fn summarize_thread_op(
    thread_ids: &[String],
    dry_run: bool,
    extra: Option<Value>,
) -> Value {
    let mut m = serde_json::Map::new();
    if thread_ids.len() == 1 {
        m.insert("thread_id".into(), Value::String(thread_ids[0].clone()));
    } else {
        m.insert(
            "thread_ids".into(),
            Value::Array(
                thread_ids
                    .iter()
                    .map(|t| Value::String(t.clone()))
                    .collect(),
            ),
        );
    }
    m.insert("dry_run".into(), Value::Bool(dry_run));
    if let Some(extra) = extra {
        if let Some(obj) = extra.as_object() {
            m.extend(obj.clone());
        }
    }
    Value::Object(m)
}

/// Build a `params_summary` for `send_email`. The body text is redacted to
/// `{length, sha256_prefix}` — never logged as plaintext.
pub(crate) fn summarize_send(
    to: &[String],
    subject_len: usize,
    body_len: usize,
    body_sha256_prefix: &str,
    dry_run: bool,
) -> Value {
    serde_json::json!({
        "to_count": to.len(),
        "subject_len": subject_len,
        "body_len": body_len,
        "body_sha256_prefix": body_sha256_prefix,
        "dry_run": dry_run,
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

    #[test]
    fn send_email_params_summary_redacts_body() {
        let summary = summarize_send(
            &["alice@example.com".to_owned()],
            14,   // subject_len
            1024, // body_len
            "abc123",
            false,
        );
        assert_eq!(summary["body_len"], 1024);
        assert_eq!(summary["body_sha256_prefix"], "abc123");
        // body text must NOT appear in the summary
        assert!(summary.get("body").is_none());
        assert!(summary.get("body_text").is_none());
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
        writer.write(&make_entry("/tmp/pwn-test-audit", "archive_thread", "applied"));

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
                .filter_map(|e| e.ok())
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
                .filter_map(|e| e.ok())
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
