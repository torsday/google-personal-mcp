//! `purge_account` "right to forget" tool per
//! [ADR-0019 §`purge_account`](../../docs/adr/0019-data-retention-and-purge.md).
//!
//! Atomically (best-effort) drops one account's persistent state:
//!
//! 1. `<config>/cache/<account>.db` (+ WAL/SHM sidecars)
//! 2. `<config>/tokens/<account>.json`
//! 3. The entry in `<config>/accounts.toml`
//!
//! Audit log files are **not** modified — the historical record persists
//! per [ADR-0011](../../docs/adr/0011-audit-log.md)'s tamper-resistance
//! contract.
//!
//! ## Confirmation guard
//!
//! Callers must pass `confirm = "yes-purge-<account>"` (the literal
//! string with the account name interpolated). Any other value is
//! rejected before any disk I/O. This is in addition to host-side
//! confirmation gating recommended by the issue.
//!
//! ## Atomicity caveat
//!
//! The three unlink + the accounts.toml rewrite cannot be one syscall.
//! A daemon crash mid-purge can leave partial state — the unlink order
//! (cache → token → registry) is chosen so that *retrying* against a
//! half-purged account completes the work without surprises (idempotency
//! invariant: all three "existed" booleans become `false` once purge
//! finishes regardless of starting state).
//!
//! ## In-flight Arc<TokenManager> snapshot
//!
//! Per [ADR-0002 §Multi-account snapshot pattern](../../docs/adr/0002-multi-account-architecture.md),
//! readers hold an `Arc<TokenState>` captured before the purge. They may
//! still successfully complete in-flight calls after the on-disk state
//! is gone. The next call on a freshly-snapshotted manager returns
//! [`Error::AccountNotFound`]. The daemon does **not** notify the cache
//! of the .db removal; the per-account connection will fail its next
//! SQL call. Operator should restart the daemon for a clean state.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{accounts_path, Accounts};
use crate::error::Error;

/// Concrete paths the tool needs. Built from the live `Config` at
/// startup and held by `GoogleServer`. Passed explicitly to
/// [`purge_account`] so unit tests can target a tempdir.
#[derive(Debug, Clone)]
pub(crate) struct PurgePaths {
    /// Top-level config directory (`~/.config/google-personal-mcp`).
    /// `accounts.toml` and `tokens/` live directly under this.
    pub(crate) config_dir: PathBuf,
    /// Cache directory from `[cache] dir`. Per-account `.db` files
    /// (`<alias>.db`, `<alias>.db-wal`, `<alias>.db-shm`) live here.
    pub(crate) cache_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct PurgeAccountInput {
    pub account: String,
    pub dry_run: bool,
    pub confirm: String,
}

// The three `*_existed` fields are distinct domain signals (each names
// a different on-disk artefact) — not flag-soup. Suppress
// `struct_excessive_bools` rather than collapsing into a sub-struct
// that would change the operator-visible JSON shape per the issue.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct PurgeAccountOutput {
    pub account: String,
    /// Echoed back so the audit row and the host can disambiguate
    /// `dry_run` from real purges.
    pub dry_run: bool,
    /// `true` iff `<cache_dir>/<account>.db` existed at the moment of
    /// the probe. WAL/SHM sidecars are removed alongside the .db when
    /// `dry_run = false` but are not surfaced separately.
    pub cache_db_existed: bool,
    /// `true` iff `<config>/tokens/<account>.json` existed at probe time.
    pub token_existed: bool,
    /// `true` iff the alias was present in `accounts.toml` at probe time.
    pub registry_entry_existed: bool,
}

/// Execute the purge. The dispatcher writes the audit `intent` row
/// before calling and the post-call `applied` row after; this function
/// is pure I/O against the supplied paths.
///
/// Errors:
/// - [`Error::InvalidArgument`] when `account` is empty, contains
///   unsafe characters, or matches the fan-out marker `"*"`.
/// - [`Error::Config`] when `confirm` doesn't match `yes-purge-<account>`.
/// - [`Error::Io`] when one of the file operations fails for a reason
///   other than `NotFound` (which is treated as idempotent success).
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "purge_account",
        tool.account = %input.account,
        tool.dry_run = input.dry_run,
    ),
)]
pub(crate) fn purge_account(
    input: PurgeAccountInput,
    paths: &PurgePaths,
) -> Result<PurgeAccountOutput, Error> {
    validate_account(&input.account)?;
    validate_confirm(&input.account, &input.confirm)?;

    let cache_db = paths.cache_dir.join(format!("{}.db", input.account));
    let token_path = paths
        .config_dir
        .join("tokens")
        .join(format!("{}.json", input.account));
    let accounts_file = accounts_path(&paths.config_dir);

    let cache_db_existed = cache_db.exists();
    let token_existed = token_path.exists();
    let registry_entry_existed = registry_has(&accounts_file, &input.account)?;

    if !input.dry_run {
        if cache_db_existed {
            remove_cache_db_and_sidecars(&cache_db)?;
        }
        if token_existed {
            try_unlink(&token_path)?;
        }
        if registry_entry_existed {
            remove_from_accounts_toml(&accounts_file, &input.account)?;
        }
    }

    Ok(PurgeAccountOutput {
        account: input.account,
        dry_run: input.dry_run,
        cache_db_existed,
        token_existed,
        registry_entry_existed,
    })
}

/// Reject empty/whitespace, the fan-out marker, and any non-portable
/// alias characters. Mirrors the safe-alias rule used by the audit
/// writer (`[A-Za-z0-9_-]+`).
fn validate_account(account: &str) -> Result<(), Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    if account == crate::tools::fanout::FANOUT_MARKER {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "`purge_account` is not fan-out-eligible per ADR-0013; \
                     pass a single account alias, not the `*` marker"
                .into(),
        });
    }
    if !account
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: format!("account alias {account:?} must match [A-Za-z0-9_-]+"),
        });
    }
    Ok(())
}

/// Require `confirm == "yes-purge-<account>"` exactly. Per the issue,
/// any other value returns a `ConfigInvalid`-shaped response — we use
/// [`Error::Config`] for that.
fn validate_confirm(account: &str, confirm: &str) -> Result<(), Error> {
    let expected = format!("yes-purge-{account}");
    if confirm == expected {
        return Ok(());
    }
    Err(Error::Config {
        path: "purge_account/confirm".into(),
        message: format!(
            "confirm parameter must be exactly {expected:?} to authorize \
             irreversible purge of account {account:?}",
        ),
    })
}

/// True iff `account` has an entry in `accounts.toml`.
fn registry_has(accounts_file: &Path, account: &str) -> Result<bool, Error> {
    let accounts = Accounts::load(accounts_file)?;
    Ok(accounts.accounts.iter().any(|a| a.alias == account))
}

/// Remove the .db file plus its `.db-wal` and `.db-shm` companions
/// (`SQLite` WAL mode). Each absent file is a clean idempotent no-op;
/// other I/O errors propagate.
fn remove_cache_db_and_sidecars(db_path: &Path) -> Result<(), Error> {
    try_unlink(db_path)?;
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    try_unlink(Path::new(&wal))?;
    let mut shm = db_path.as_os_str().to_owned();
    shm.push("-shm");
    try_unlink(Path::new(&shm))?;
    Ok(())
}

/// `unlink(2)` that swallows `NotFound` (idempotency invariant per the
/// acceptance criteria) and propagates other errors.
fn try_unlink(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Remove `account` from `accounts.toml`, promoting the first remaining
/// account to default if the removed one was the default. Mirrors the
/// behavior of `auth remove --alias` (`src/auth/cli.rs::run_remove`).
fn remove_from_accounts_toml(accounts_file: &Path, account: &str) -> Result<(), Error> {
    let mut accounts = Accounts::load(accounts_file)?;
    let was_default = accounts
        .accounts
        .iter()
        .find(|a| a.alias == account)
        .is_some_and(|a| a.default);
    accounts.accounts.retain(|a| a.alias != account);
    if was_default {
        if let Some(first) = accounts.accounts.first_mut() {
            first.default = true;
        }
    }
    accounts.save(accounts_file)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs::File;
    use std::io::Write as _;

    use tempfile::TempDir;

    use super::*;

    fn paths(dir: &TempDir) -> PurgePaths {
        let cfg = dir.path().to_owned();
        let cache = cfg.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(cfg.join("tokens")).unwrap();
        PurgePaths {
            config_dir: cfg,
            cache_dir: cache,
        }
    }

    fn write_accounts_toml(dir: &TempDir, body: &str) {
        let p = dir.path().join("accounts.toml");
        let mut f = File::create(p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    fn seed_account(dir: &TempDir, alias: &str) {
        // Cache .db + WAL + SHM
        File::create(dir.path().join("cache").join(format!("{alias}.db"))).unwrap();
        File::create(dir.path().join("cache").join(format!("{alias}.db-wal"))).unwrap();
        File::create(dir.path().join("cache").join(format!("{alias}.db-shm"))).unwrap();
        // Token
        File::create(dir.path().join("tokens").join(format!("{alias}.json"))).unwrap();
    }

    // ── Confirmation guard ──────────────────────────────────────────────────

    #[test]
    fn rejects_wrong_confirm_string() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        let err = purge_account(
            PurgeAccountInput {
                account: "work".into(),
                dry_run: false,
                confirm: "yes-purge-personal".into(), // wrong account
            },
            &p,
        )
        .expect_err("must reject mismatched confirm");
        match err {
            Error::Config { path, message } => {
                assert!(path.contains("confirm"));
                assert!(message.contains("yes-purge-work"));
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_confirm() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        let err = purge_account(
            PurgeAccountInput {
                account: "work".into(),
                dry_run: false,
                confirm: String::new(),
            },
            &p,
        )
        .expect_err("must reject empty confirm");
        assert!(matches!(err, Error::Config { .. }));
    }

    // ── Account validation ──────────────────────────────────────────────────

    #[test]
    fn rejects_fanout_marker() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        let err = purge_account(
            PurgeAccountInput {
                account: "*".into(),
                dry_run: false,
                confirm: "yes-purge-*".into(),
            },
            &p,
        )
        .expect_err("must reject fanout marker");
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "account"),
            "got {err:?}",
        );
    }

    #[test]
    fn rejects_path_traversal_alias() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        let err = purge_account(
            PurgeAccountInput {
                account: "../etc".into(),
                dry_run: false,
                confirm: "yes-purge-../etc".into(),
            },
            &p,
        )
        .expect_err("must reject traversal");
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn rejects_empty_account() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        let err = purge_account(
            PurgeAccountInput {
                account: String::new(),
                dry_run: false,
                confirm: "yes-purge-".into(),
            },
            &p,
        )
        .expect_err("must reject empty");
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "account"));
    }

    // ── Dry-run reports but doesn't delete ──────────────────────────────────

    #[test]
    fn dry_run_probes_existence_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        seed_account(&dir, "work");
        write_accounts_toml(
            &dir,
            r#"
[[accounts]]
alias = "work"
email = "alice@example.com"
default = true
"#,
        );

        let out = purge_account(
            PurgeAccountInput {
                account: "work".into(),
                dry_run: true,
                confirm: "yes-purge-work".into(),
            },
            &p,
        )
        .expect("ok");
        assert!(out.dry_run);
        assert!(out.cache_db_existed);
        assert!(out.token_existed);
        assert!(out.registry_entry_existed);

        // Nothing actually removed.
        assert!(dir.path().join("cache/work.db").exists());
        assert!(dir.path().join("tokens/work.json").exists());
        let registry = std::fs::read_to_string(dir.path().join("accounts.toml")).unwrap();
        assert!(registry.contains("work"));
    }

    // ── Full purge end-to-end ───────────────────────────────────────────────

    #[test]
    fn real_purge_removes_all_three_locations() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        seed_account(&dir, "work");
        write_accounts_toml(
            &dir,
            r#"
[[accounts]]
alias = "work"
email = "alice@example.com"
default = true

[[accounts]]
alias = "personal"
email = "bob@example.com"
"#,
        );

        let out = purge_account(
            PurgeAccountInput {
                account: "work".into(),
                dry_run: false,
                confirm: "yes-purge-work".into(),
            },
            &p,
        )
        .expect("ok");
        assert!(!out.dry_run);
        assert!(out.cache_db_existed);
        assert!(out.token_existed);
        assert!(out.registry_entry_existed);

        // Cache .db + WAL/SHM sidecars all gone.
        assert!(!dir.path().join("cache/work.db").exists());
        assert!(!dir.path().join("cache/work.db-wal").exists());
        assert!(!dir.path().join("cache/work.db-shm").exists());
        // Token gone.
        assert!(!dir.path().join("tokens/work.json").exists());

        // Registry: `work` removed, `personal` promoted to default.
        let accts = Accounts::load(&accounts_path(&p.config_dir)).expect("reload");
        assert_eq!(accts.accounts.len(), 1);
        assert_eq!(accts.accounts[0].alias, "personal");
        assert!(accts.accounts[0].default, "default promoted on removal");
    }

    // ── Idempotency ─────────────────────────────────────────────────────────

    #[test]
    fn idempotent_against_absent_account() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        // Nothing seeded — no cache, no token, no registry. Still must
        // succeed.
        let out = purge_account(
            PurgeAccountInput {
                account: "ghost".into(),
                dry_run: false,
                confirm: "yes-purge-ghost".into(),
            },
            &p,
        )
        .expect("idempotent ok");
        assert!(!out.cache_db_existed);
        assert!(!out.token_existed);
        assert!(!out.registry_entry_existed);
    }

    /// Layer-2-ish: full purge of a populated account, then a second
    /// invocation that observes the all-false absent-state outcome.
    /// Verifies the idempotency invariant the issue calls out.
    #[test]
    fn second_purge_after_full_purge_is_clean_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        seed_account(&dir, "work");
        write_accounts_toml(
            &dir,
            r#"
[[accounts]]
alias = "work"
email = "alice@example.com"
default = true
"#,
        );

        let first = purge_account(
            PurgeAccountInput {
                account: "work".into(),
                dry_run: false,
                confirm: "yes-purge-work".into(),
            },
            &p,
        )
        .expect("first ok");
        assert!(first.cache_db_existed && first.token_existed && first.registry_entry_existed);

        let second = purge_account(
            PurgeAccountInput {
                account: "work".into(),
                dry_run: false,
                confirm: "yes-purge-work".into(),
            },
            &p,
        )
        .expect("second ok");
        assert!(
            !second.cache_db_existed && !second.token_existed && !second.registry_entry_existed,
            "second purge must observe all-false state",
        );
    }

    // ── Partial-state recovery ──────────────────────────────────────────────

    /// Simulates a daemon crash that wiped the cache .db before the
    /// token unlink. Running purge again must finish the work and
    /// report `cache_db_existed = false, token_existed = true`.
    #[test]
    fn partial_state_post_crash_completes_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(&dir);
        // No cache file, but token + registry still present (the
        // post-cache-unlink, pre-token-unlink crash window).
        File::create(dir.path().join("tokens/work.json")).unwrap();
        write_accounts_toml(
            &dir,
            r#"
[[accounts]]
alias = "work"
email = "alice@example.com"
default = true
"#,
        );

        let out = purge_account(
            PurgeAccountInput {
                account: "work".into(),
                dry_run: false,
                confirm: "yes-purge-work".into(),
            },
            &p,
        )
        .expect("ok");
        assert!(!out.cache_db_existed);
        assert!(out.token_existed);
        assert!(out.registry_entry_existed);

        // Token and registry now cleaned up.
        assert!(!dir.path().join("tokens/work.json").exists());
        let accts = Accounts::load(&accounts_path(&p.config_dir)).expect("reload");
        assert!(accts.accounts.is_empty());
    }
}
