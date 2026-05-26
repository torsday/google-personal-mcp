//! Opt-in retention sweep for the audit log per
//! [ADR-0019 §"Audit: opt-in automatic deletion"](../docs/adr/0019-data-retention-and-purge.md).
//!
//! Enabled when `[audit] delete_after_days > 0`. A daily background task
//! walks the audit directory, identifies closed rotation files by
//! filename pattern, computes each file's age from the **period encoded
//! in its name** (not mtime), and deletes any whose period end-of-day
//! plus `delete_after_days` falls in the past.
//!
//! ## Filename → period mapping
//!
//! Pattern parsing per the ADR-0011 rotation table:
//!
//! | Pattern                  | Cadence | `period_end` |
//! |--------------------------|---------|--------------|
//! | `audit-YYYY-MM.log`      | monthly | last day of month |
//! | `audit-YYYY-Wnn.log`     | weekly  | Sunday of ISO week |
//! | `audit-YYYY-MM-DD.log`   | daily   | that day |
//! | `audit-<N>.log`          | size    | **None** — exempt from age-based deletion |
//!
//! `period_end` is the **last day of the covered period**. A file is
//! considered eligible for deletion when
//! `(now.date - period_end) ≥ delete_after_days`. Choosing end-of-period
//! over midpoint is the conservative read: a file is only deletable once
//! the entire window it covers has aged past the threshold.
//!
//! ## Current-rotation exclusion
//!
//! The currently-open rotation file is identified by computing what
//! filename the writer *would* produce at `now` (via
//! [`crate::audit::AuditWriter`]'s rotation-mode snapshot) and excluding
//! that exact filename from deletion. The pattern match is the load-
//! bearing guard — even if the threshold-check would otherwise pick the
//! current file up (it won't, because end-of-period is always today or
//! later for the currently-open period, but defence-in-depth).
//!
//! ## Audit-of-audit-deletion
//!
//! Per ADR-0011, deleting an audit file does **not** write a new audit
//! record about the deletion — that would force creating the next
//! rotation purely to record its own forgetting. Instead the deletion is
//! recorded only via a structured tracing log line at INFO.

use std::time::Duration;

use chrono::{DateTime, Datelike as _, NaiveDate, Utc, Weekday};

use crate::audit::AuditWriter;
use crate::config::RotateMode;
use crate::error::Error;

/// Background-loop cadence. Once per day is plenty — the threshold is
/// measured in days and `delete_after_days` is the operator's promise
/// to upstream auditors, not a hard real-time guarantee.
const SWEEP_INTERVAL: Duration = Duration::from_hours(24);

/// One sweep cycle's report. Returned to tests so they can assert on
/// the exact set of files removed; logged at INFO for operator
/// visibility.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SweepReport {
    /// Filenames removed during this sweep, sorted for deterministic
    /// test assertions. Bare names (no directory prefix).
    pub(crate) deleted: Vec<String>,
}

/// Daily retention sweep driver. Cheap to clone (path + integers).
#[derive(Debug, Clone)]
pub(crate) struct RetentionSweep {
    writer: AuditWriter,
    delete_after_days: u32,
}

impl RetentionSweep {
    /// Build a sweep against `writer`. When `delete_after_days == 0`
    /// every call is a no-op (`SweepReport::default()`); construct one
    /// anyway so callers don't need a feature-flag branch around the
    /// background task — the spawn helper short-circuits internally.
    pub(crate) const fn new(writer: AuditWriter, delete_after_days: u32) -> Self {
        Self {
            writer,
            delete_after_days,
        }
    }

    /// Whether this sweep would actually do anything if invoked. Used by
    /// `spawn` to avoid creating an idle task that ticks forever doing
    /// nothing.
    pub(crate) const fn is_enabled(&self) -> bool {
        self.delete_after_days > 0
    }

    /// Run exactly one sweep cycle.
    ///
    /// `now` is supplied so tests can drive deterministic ages; the
    /// production background task passes `Utc::now()`.
    ///
    /// Errors:
    /// - [`Error::Io`] if the audit directory can't be read.
    ///   Individual file-removal failures are logged at WARN and the
    ///   sweep continues — one un-deletable file shouldn't block the
    ///   rest of the cleanup.
    pub(crate) fn sweep_once(&self, now: DateTime<Utc>) -> Result<SweepReport, Error> {
        if !self.is_enabled() {
            return Ok(SweepReport::default());
        }
        let dir = self.writer.audit_dir();
        if !dir.exists() {
            return Ok(SweepReport::default());
        }

        let current = current_filename(self.writer.rotate_mode(), now);
        let today = now.date_naive();
        let threshold_days = i64::from(self.delete_after_days);

        let mut deleted: Vec<String> = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
        // Sort for deterministic iteration order (helps tests; harmless
        // in production).
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            let fname_os = entry.file_name();
            let fname = fname_os.to_string_lossy();

            // Skip the currently-open rotation file by exact-name match.
            // For time-based modes the name is deterministic from `now`;
            // for Size mode `current` is `None` and every size file is
            // already skipped below.
            if let Some(cur) = current.as_deref() {
                if fname == cur {
                    continue;
                }
            }

            // Parse the period encoded in the filename. `None` = either
            // a non-audit file or a size-rotated file with no period; in
            // both cases we leave it alone.
            let Some(period_end) = parse_period_end(&fname) else {
                continue;
            };

            let age_days = (today - period_end).num_days();
            if age_days < threshold_days {
                continue;
            }

            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::info!(
                        audit_retention_purge_file = %path.display(),
                        age_days,
                        "audit_retention_purge",
                    );
                    deleted.push(fname.into_owned());
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "audit retention: failed to remove file; continuing sweep",
                    );
                }
            }
        }
        deleted.sort();
        Ok(SweepReport { deleted })
    }

    /// Spawn a `tokio::task` that runs [`Self::sweep_once`] every
    /// [`SWEEP_INTERVAL`] until the returned handle drops. Returns
    /// `None` when the sweep is disabled (`delete_after_days = 0`).
    pub(crate) fn spawn(self) -> Option<RetentionHandle> {
        if !self.is_enabled() {
            return None;
        }
        let sweep = self;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match sweep.sweep_once(Utc::now()) {
                    Ok(report) if !report.deleted.is_empty() => {
                        tracing::info!(
                            count = report.deleted.len(),
                            "audit retention sweep complete",
                        );
                    }
                    Ok(_) => {} // No files to delete this cycle — silent.
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "audit retention sweep failed; will retry on next tick",
                        );
                    }
                }
            }
        });
        Some(RetentionHandle { task: Some(handle) })
    }
}

/// Owned handle to the background sweep task. Aborts the task on drop.
/// Same shape as [`crate::cache::eviction::EvictionHandle`].
#[must_use = "the retention sweep aborts when this handle is dropped"]
pub(crate) struct RetentionHandle {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RetentionHandle {
    /// Stop the background sweep and wait for the abort to land.
    #[cfg(test)]
    pub(crate) async fn stop(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = t.await;
        }
    }
}

impl Drop for RetentionHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            tracing::debug!("aborting audit retention sweep");
            t.abort();
        }
    }
}

/// Currently-open rotation filename for the given `mode` at `now`.
/// `None` for `RotateMode::Size` — that mode has no deterministic
/// "current name" computable from `now` alone, and the parser below
/// already exempts every `audit-<N>.log` file from deletion.
fn current_filename(mode: &RotateMode, now: DateTime<Utc>) -> Option<String> {
    match mode {
        RotateMode::Monthly => Some(format!("audit-{}-{:02}.log", now.year(), now.month())),
        RotateMode::Weekly => {
            let iso = now.iso_week();
            Some(format!("audit-{}-W{:02}.log", iso.year(), iso.week()))
        }
        RotateMode::Daily => Some(format!(
            "audit-{}-{:02}-{:02}.log",
            now.year(),
            now.month(),
            now.day()
        )),
        // Size mode: no period in filename, no deterministic "current"
        // name. The parser returns None for `audit-<N>.log`, so every
        // size file is skipped by the period-check branch.
        RotateMode::Size(_) => None,
    }
}

/// Parse the period-end date from an audit filename.
///
/// Returns:
/// - `Some(last_day_of_month)` for `audit-YYYY-MM.log`
/// - `Some(sunday_of_iso_week)` for `audit-YYYY-Wnn.log`
/// - `Some(that_day)` for `audit-YYYY-MM-DD.log`
/// - `None` for `audit-<N>.log` (size mode), non-matching names, or
///   malformed components (e.g. `audit-2026-13.log`).
fn parse_period_end(fname: &str) -> Option<NaiveDate> {
    let inner = fname.strip_prefix("audit-")?.strip_suffix(".log")?;

    // Daily: YYYY-MM-DD (3 components).
    if let Some((y_s, rest)) = inner.split_once('-') {
        if let Some((m_s, d_s)) = rest.split_once('-') {
            let y: i32 = y_s.parse().ok()?;
            let m: u32 = m_s.parse().ok()?;
            let d: u32 = d_s.parse().ok()?;
            return NaiveDate::from_ymd_opt(y, m, d);
        }

        // Weekly: YYYY-Wnn — `rest` starts with 'W'.
        if let Some(week_s) = rest.strip_prefix('W') {
            let y: i32 = y_s.parse().ok()?;
            let w: u32 = week_s.parse().ok()?;
            // Sunday is the last day of the ISO week — pick it as
            // end-of-period.
            return NaiveDate::from_isoywd_opt(y, w, Weekday::Sun);
        }

        // Monthly: YYYY-MM — `rest` is just `MM`.
        let y: i32 = y_s.parse().ok()?;
        let m: u32 = rest.parse().ok()?;
        return last_day_of_month(y, m);
    }

    // Size: `audit-<N>.log` → inner is `<N>` (no hyphens). No period.
    None
}

/// Last calendar day of `(year, month)`. `None` for invalid months.
fn last_day_of_month(year: i32, month: u32) -> Option<NaiveDate> {
    // Advance to the first day of the next month, then step back one
    // day. Handles month=12 (year rollover) without special-case math.
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next = NaiveDate::from_ymd_opt(ny, nm, 1)?;
    first_of_next.pred_opt()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs::File;
    use std::path::Path;

    use chrono::{TimeZone as _, Utc};
    use tempfile::TempDir;

    use super::*;

    fn touch(dir: &Path, name: &str) {
        File::create(dir.join(name)).expect("touch");
    }

    fn writer_at(dir: &TempDir, mode: RotateMode) -> AuditWriter {
        AuditWriter::new(dir.path().to_owned(), mode)
    }

    // ── parse_period_end ────────────────────────────────────────────────────

    #[test]
    fn parses_monthly_pattern_to_last_day_of_month() {
        assert_eq!(
            parse_period_end("audit-2026-04.log"),
            NaiveDate::from_ymd_opt(2026, 4, 30),
        );
        // December → year rollover handled by `last_day_of_month`.
        assert_eq!(
            parse_period_end("audit-2026-12.log"),
            NaiveDate::from_ymd_opt(2026, 12, 31),
        );
    }

    #[test]
    fn parses_weekly_pattern_to_iso_sunday() {
        // ISO week 17 of 2026 → Mon Apr 20 .. Sun Apr 26.
        assert_eq!(
            parse_period_end("audit-2026-W17.log"),
            NaiveDate::from_ymd_opt(2026, 4, 26),
        );
    }

    #[test]
    fn parses_daily_pattern_to_that_day() {
        assert_eq!(
            parse_period_end("audit-2026-04-25.log"),
            NaiveDate::from_ymd_opt(2026, 4, 25),
        );
    }

    #[test]
    fn size_pattern_returns_none() {
        // Size-rotated files have no period; the deletion path skips them.
        assert_eq!(parse_period_end("audit-1.log"), None);
        assert_eq!(parse_period_end("audit-42.log"), None);
        assert_eq!(parse_period_end("audit-9999.log"), None);
    }

    #[test]
    fn rejects_non_audit_or_malformed() {
        assert_eq!(parse_period_end("random.txt"), None);
        assert_eq!(parse_period_end("audit-not-a-date.log"), None);
        // Month 13 → NaiveDate::from_ymd_opt returns None.
        assert_eq!(parse_period_end("audit-2026-13.log"), None);
    }

    // ── current_filename ────────────────────────────────────────────────────

    #[test]
    fn current_filename_matches_writer_pattern_per_mode() {
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap();
        assert_eq!(
            current_filename(&RotateMode::Monthly, now).as_deref(),
            Some("audit-2026-04.log"),
        );
        assert_eq!(
            current_filename(&RotateMode::Daily, now).as_deref(),
            Some("audit-2026-04-25.log"),
        );
        // ISO week of 2026-04-25 (Saturday) is W17.
        assert_eq!(
            current_filename(&RotateMode::Weekly, now).as_deref(),
            Some("audit-2026-W17.log"),
        );
        assert_eq!(current_filename(&RotateMode::Size(1024), now), None);
    }

    // ── sweep_once: disabled is a no-op (doesn't even enumerate) ────────────

    #[test]
    fn disabled_sweep_does_not_enumerate_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Seed an old file that *would* match if enabled.
        touch(
            dir.path().join("audit").as_path().parent().unwrap(),
            "ignored.log",
        );
        // Even with a non-existent audit dir, disabled returns empty.
        let sweep = RetentionSweep::new(writer_at(&dir, RotateMode::Monthly), 0);
        let now = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        let report = sweep.sweep_once(now).expect("ok");
        assert!(report.deleted.is_empty());
        assert!(!sweep.is_enabled());
    }

    // ── sweep_once: pattern + current-file exclusion per cadence ────────────

    #[test]
    fn monthly_deletes_old_files_keeps_current_and_recent() {
        let dir = tempfile::tempdir().unwrap();
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir_all(&audit_dir).unwrap();

        // Layout: 2026-01 (old), 2026-02 (old), 2026-03 (recent),
        // 2026-04 (current), plus a noise file that should be left alone.
        touch(&audit_dir, "audit-2026-01.log"); // ends 2026-01-31
        touch(&audit_dir, "audit-2026-02.log"); // ends 2026-02-28
        touch(&audit_dir, "audit-2026-03.log"); // ends 2026-03-31
        touch(&audit_dir, "audit-2026-04.log"); // CURRENT
        touch(&audit_dir, "README.md"); // not an audit file

        let sweep = RetentionSweep::new(writer_at(&dir, RotateMode::Monthly), 30);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap();
        let report = sweep.sweep_once(now).expect("ok");

        // 2026-04-25 - 2026-01-31 = 84 days ≥ 30 → delete
        // 2026-04-25 - 2026-02-28 = 56 days ≥ 30 → delete
        // 2026-04-25 - 2026-03-31 = 25 days < 30 → keep
        // 2026-04         = current → keep
        // README.md       = not an audit file → keep
        assert_eq!(
            report.deleted,
            vec![
                "audit-2026-01.log".to_owned(),
                "audit-2026-02.log".to_owned()
            ],
        );
        assert!(audit_dir.join("audit-2026-03.log").exists());
        assert!(audit_dir.join("audit-2026-04.log").exists());
        assert!(audit_dir.join("README.md").exists());
    }

    #[test]
    fn weekly_excludes_current_iso_week_file() {
        let dir = tempfile::tempdir().unwrap();
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir_all(&audit_dir).unwrap();

        // 2026-04-25 → ISO W17. W10 is well in the past.
        touch(&audit_dir, "audit-2026-W10.log");
        touch(&audit_dir, "audit-2026-W17.log"); // CURRENT

        let sweep = RetentionSweep::new(writer_at(&dir, RotateMode::Weekly), 7);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap();
        let report = sweep.sweep_once(now).expect("ok");

        assert_eq!(report.deleted, vec!["audit-2026-W10.log".to_owned()]);
        assert!(audit_dir.join("audit-2026-W17.log").exists());
    }

    #[test]
    fn daily_excludes_current_day_file() {
        let dir = tempfile::tempdir().unwrap();
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir_all(&audit_dir).unwrap();

        touch(&audit_dir, "audit-2026-04-10.log");
        touch(&audit_dir, "audit-2026-04-25.log"); // CURRENT

        let sweep = RetentionSweep::new(writer_at(&dir, RotateMode::Daily), 7);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap();
        let report = sweep.sweep_once(now).expect("ok");

        assert_eq!(report.deleted, vec!["audit-2026-04-10.log".to_owned()]);
        assert!(audit_dir.join("audit-2026-04-25.log").exists());
    }

    #[test]
    fn size_mode_never_deletes_any_file() {
        let dir = tempfile::tempdir().unwrap();
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir_all(&audit_dir).unwrap();

        // Size-rotated files have no period — no age computable —
        // sweep must leave them alone even with an aggressive threshold.
        touch(&audit_dir, "audit-1.log");
        touch(&audit_dir, "audit-2.log");
        touch(&audit_dir, "audit-3.log"); // would be current

        let sweep = RetentionSweep::new(writer_at(&dir, RotateMode::Size(1024)), 1);
        let now = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        let report = sweep.sweep_once(now).expect("ok");

        assert!(report.deleted.is_empty());
        assert!(audit_dir.join("audit-1.log").exists());
        assert!(audit_dir.join("audit-2.log").exists());
        assert!(audit_dir.join("audit-3.log").exists());
    }

    #[test]
    fn missing_audit_dir_is_a_clean_no_op() {
        let dir = tempfile::tempdir().unwrap();
        // No audit/ subdir created.
        let sweep = RetentionSweep::new(writer_at(&dir, RotateMode::Monthly), 30);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap();
        let report = sweep.sweep_once(now).expect("ok");
        assert!(report.deleted.is_empty());
    }

    /// Layer 2-ish: synthetic mixed-age directory exercised end-to-end,
    /// covering the acceptance criterion "only the old non-current
    /// files are removed".
    #[test]
    fn layer2_mixed_age_directory_only_old_non_current_files_removed() {
        let dir = tempfile::tempdir().unwrap();
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir_all(&audit_dir).unwrap();

        // Mix of cadences shouldn't trip the parser — but the writer
        // mode used to identify "current" is Monthly, so only the
        // 2026-04 monthly file is current.
        for name in [
            "audit-2025-12.log",    // very old monthly
            "audit-2026-01.log",    // old monthly
            "audit-2026-02.log",    // old monthly
            "audit-2026-03.log",    // recent monthly (below threshold)
            "audit-2026-04.log",    // CURRENT (monthly)
            "audit-2026-W05.log",   // very old weekly
            "audit-2026-04-10.log", // old daily
            "audit-2026-04-24.log", // old daily (below threshold? 1 day old)
            "audit-1.log",          // size — exempt
            "leftover.txt",         // not an audit file
        ] {
            touch(&audit_dir, name);
        }

        let sweep = RetentionSweep::new(writer_at(&dir, RotateMode::Monthly), 14);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap();
        let report = sweep.sweep_once(now).expect("ok");

        // Days from now (2026-04-25):
        // 2025-12-31 → 115 days  ≥ 14 → delete
        // 2026-01-31 →  84 days  ≥ 14 → delete
        // 2026-02-28 →  56 days  ≥ 14 → delete
        // 2026-03-31 →  25 days  ≥ 14 → delete
        // 2026-04    → CURRENT          → keep
        // 2026-W05 (ends Sun 2026-02-01) → 83 days ≥ 14 → delete
        // 2026-04-10 →  15 days ≥ 14 → delete
        // 2026-04-24 →   1 day  < 14 → keep
        // audit-1.log → exempt (size mode pattern)
        // leftover.txt → not an audit file
        assert_eq!(
            report.deleted,
            vec![
                "audit-2025-12.log".to_owned(),
                "audit-2026-01.log".to_owned(),
                "audit-2026-02.log".to_owned(),
                "audit-2026-03.log".to_owned(),
                "audit-2026-04-10.log".to_owned(),
                "audit-2026-W05.log".to_owned(),
            ],
        );
        assert!(audit_dir.join("audit-2026-04.log").exists());
        assert!(audit_dir.join("audit-2026-04-24.log").exists());
        assert!(audit_dir.join("audit-1.log").exists());
        assert!(audit_dir.join("leftover.txt").exists());
    }
}
