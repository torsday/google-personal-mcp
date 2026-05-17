//! Startup permission check for `~/.config/google-personal-mcp/` per
//! [ADR-0017](../docs/adr/0017-secrets-at-rest.md).
//!
//! Refuses to start when token or credentials files are wider than `0600`,
//! when their parent directories are wider than `0700`, or when any of those
//! paths is a symlink. Operator-managed `accounts.toml` / `config.toml` are
//! warn-only — they reference aliases, not secrets.
//!
//! Escape hatch: setting `GOOGLE_PERSONAL_MCP_SKIP_PERM_CHECK=1` bypasses the
//! entire check and logs a single WARN. Intended for edge cases like WSL on a
//! mounted drive where ownership semantics don't match POSIX — not for
//! routine use.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Env var that, when set to `1`, skips the entire startup permission check.
pub(crate) const SKIP_ENV_VAR: &str = "GOOGLE_PERSONAL_MCP_SKIP_PERM_CHECK";

/// What kind of path we're checking — drives both the expected mode and the
/// failure message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Directory — expected mode `0700`.
    Dir,
    /// File holding secret material (token, OAuth client credentials) —
    /// expected mode `0600`. Failure refuses startup.
    SecretFile,
    /// Operator-managed config file (`accounts.toml`, `config.toml`) —
    /// expected mode `0600`. Failure logs a WARN only; these reference
    /// aliases, not secrets.
    ConfigFile,
}

impl Kind {
    const fn expected_mode(self) -> u32 {
        match self {
            Self::Dir => 0o700,
            Self::SecretFile | Self::ConfigFile => 0o600,
        }
    }

    const fn refuse_on_violation(self) -> bool {
        matches!(self, Self::Dir | Self::SecretFile)
    }

    const fn reject_symlink(self) -> bool {
        // ADR-0017: "If any of the above paths is a symlink, startup fails."
        // Operator config files (accounts.toml / config.toml) are explicitly
        // listed under the same rule.
        matches!(self, Self::Dir | Self::SecretFile | Self::ConfigFile)
    }
}

/// A single path to check.
#[derive(Debug, Clone)]
pub(crate) struct Subject {
    pub(crate) path: PathBuf,
    pub(crate) kind: Kind,
}

impl Subject {
    pub(crate) fn new(path: impl Into<PathBuf>, kind: Kind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }
}

/// Build the default subject list rooted at `config_dir` per the ADR-0017
/// table. Existing token files (`tokens/*.json`) are enumerated; the dir need
/// not contain any yet.
pub(crate) fn default_subjects(config_dir: &Path) -> Vec<Subject> {
    let mut out = vec![
        Subject::new(config_dir.to_owned(), Kind::Dir),
        Subject::new(config_dir.join("tokens"), Kind::Dir),
        Subject::new(
            config_dir.join("credentials").join("google.json"),
            Kind::SecretFile,
        ),
        Subject::new(config_dir.join("accounts.toml"), Kind::ConfigFile),
        Subject::new(config_dir.join("config.toml"), Kind::ConfigFile),
    ];
    if let Ok(rd) = std::fs::read_dir(config_dir.join("tokens")) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "json") {
                out.push(Subject::new(p, Kind::SecretFile));
            }
        }
    }
    out
}

/// Returns `true` when the escape-hatch env var is set to `"1"`. Logged once
/// at WARN by [`check`] on first invocation.
pub(crate) fn escape_hatch_enabled() -> bool {
    matches!(std::env::var(SKIP_ENV_VAR).as_deref(), Ok("1"))
}

/// Run the startup permission check over `subjects`. Reads the escape-hatch
/// env var via [`escape_hatch_enabled`]; see [`check_with`] for the
/// dependency-injected form used by tests.
///
/// Returns `Ok(())` when every refuse-on-violation subject is either missing
/// (e.g., no tokens yet on a fresh install) or has acceptable permissions.
/// Warn-only subjects with wider perms log a WARN and continue.
pub(crate) fn check(subjects: &[Subject]) -> Result<(), Error> {
    check_with(subjects, escape_hatch_enabled())
}

/// The testable inner form of [`check`]. `skip` is the escape-hatch flag, so
/// callers can verify the escape-hatch path without mutating process env.
pub(crate) fn check_with(subjects: &[Subject], skip: bool) -> Result<(), Error> {
    if skip {
        tracing::warn!(
            env_var = %SKIP_ENV_VAR,
            "permission check disabled via env var — tokens may be readable by other local users/processes",
        );
        return Ok(());
    }
    for s in subjects {
        check_one(s)?;
    }
    Ok(())
}

fn check_one(subject: &Subject) -> Result<(), Error> {
    let path = &subject.path;
    let path_str = path.display().to_string();

    // `symlink_metadata` does not traverse the final component — that is what
    // lets us detect a symlinked secret file before reading its contents.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    if meta.file_type().is_symlink() && subject.kind.reject_symlink() {
        return Err(Error::InsecurePermissions {
            path: path_str,
            message: "path is a symlink — refusing to follow. \
                Replace the symlink with the actual file/directory \
                so permissions can be enforced directly."
                .into(),
        });
    }

    let kind = subject.kind;
    let expected = kind.expected_mode();
    let actual = meta.permissions().mode() & 0o777;

    // Type mismatch — refuse loudly. A token file that is actually a
    // directory (or vice versa) is a corrupt state, not a perm issue.
    match kind {
        Kind::Dir if !meta.file_type().is_dir() => {
            return Err(Error::InsecurePermissions {
                path: path_str,
                message: "expected a directory but found a file".into(),
            });
        }
        Kind::SecretFile | Kind::ConfigFile if !meta.file_type().is_file() => {
            return Err(Error::InsecurePermissions {
                path: path_str,
                message: "expected a regular file but found a directory".into(),
            });
        }
        _ => {}
    }

    // "Wider" = any group/other permission bit set. Owner-side bits are not
    // a leak; the ADR threat is "another local user/process reads my tokens".
    if actual & 0o077 != 0 {
        let message = format!(
            "mode is 0{actual:o}, expected 0{expected:o} (owner-only). \
             Fix with `chmod 0{expected:o} {path_str}`."
        );
        if kind.refuse_on_violation() {
            return Err(Error::InsecurePermissions {
                path: path_str,
                message,
            });
        }
        tracing::warn!(
            path = %path_str,
            mode = format_args!("0{actual:o}"),
            "{message}",
        );
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn set_mode(p: &Path, mode: u32) {
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(p, perms).unwrap();
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"").unwrap();
        p
    }

    // ── Happy path ───────────────────────────────────────────────────────────

    #[test]
    fn dir_at_0700_passes() {
        let tmp = TempDir::new().unwrap();
        set_mode(tmp.path(), 0o700);
        let subjects = vec![Subject::new(tmp.path().to_owned(), Kind::Dir)];
        assert!(check(&subjects).is_ok());
    }

    #[test]
    fn secret_file_at_0600_passes() {
        let tmp = TempDir::new().unwrap();
        let f = touch(tmp.path(), "tok.json");
        set_mode(&f, 0o600);
        let subjects = vec![Subject::new(f, Kind::SecretFile)];
        assert!(check(&subjects).is_ok());
    }

    #[test]
    fn missing_path_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let subjects = vec![Subject::new(tmp.path().join("nope.json"), Kind::SecretFile)];
        assert!(check(&subjects).is_ok());
    }

    // ── Wider-than-required → refuse ────────────────────────────────────────

    #[test]
    fn dir_world_readable_refuses() {
        let tmp = TempDir::new().unwrap();
        set_mode(tmp.path(), 0o755);
        let subjects = vec![Subject::new(tmp.path().to_owned(), Kind::Dir)];
        let err = check(&subjects).expect_err("should refuse");
        match err {
            Error::InsecurePermissions { message, .. } => {
                assert!(message.contains("0700"), "got: {message}");
                assert!(message.contains("chmod 0700"), "got: {message}");
                assert!(message.contains("0755"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn secret_file_group_readable_refuses() {
        let tmp = TempDir::new().unwrap();
        let f = touch(tmp.path(), "tok.json");
        set_mode(&f, 0o640);
        let subjects = vec![Subject::new(f, Kind::SecretFile)];
        let err = check(&subjects).expect_err("should refuse");
        match err {
            Error::InsecurePermissions { message, .. } => {
                assert!(message.contains("0600"), "got: {message}");
                assert!(message.contains("chmod 0600"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── Symlinks ────────────────────────────────────────────────────────────

    #[test]
    fn symlinked_secret_file_refuses() {
        let tmp = TempDir::new().unwrap();
        let target = touch(tmp.path(), "actual.json");
        set_mode(&target, 0o600);
        let link = tmp.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let subjects = vec![Subject::new(link, Kind::SecretFile)];
        let err = check(&subjects).expect_err("should refuse symlink");
        match err {
            Error::InsecurePermissions { message, .. } => {
                assert!(message.contains("symlink"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn symlinked_dir_refuses() {
        let tmp = TempDir::new().unwrap();
        let real_dir = tmp.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        set_mode(&real_dir, 0o700);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        let subjects = vec![Subject::new(link, Kind::Dir)];
        let err = check(&subjects).expect_err("should refuse symlink");
        match err {
            Error::InsecurePermissions { message, .. } => {
                assert!(message.contains("symlink"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── Type mismatch ───────────────────────────────────────────────────────

    #[test]
    fn dir_kind_against_file_refuses() {
        let tmp = TempDir::new().unwrap();
        let f = touch(tmp.path(), "not-a-dir");
        set_mode(&f, 0o600);
        let subjects = vec![Subject::new(f, Kind::Dir)];
        let err = check(&subjects).expect_err("should refuse type mismatch");
        match err {
            Error::InsecurePermissions { message, .. } => {
                assert!(message.contains("expected a directory"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── Warn-only ConfigFile ────────────────────────────────────────────────

    #[test]
    fn config_file_wider_perms_warn_does_not_refuse() {
        let tmp = TempDir::new().unwrap();
        let f = touch(tmp.path(), "accounts.toml");
        set_mode(&f, 0o644);
        let subjects = vec![Subject::new(f, Kind::ConfigFile)];
        // Wider than 0600 — the ADR says warn-only.
        assert!(check(&subjects).is_ok());
    }

    // ── Escape hatch ────────────────────────────────────────────────────────
    // Tested via `check_with` so we don't mutate process env (which would
    // race other tests and trip the crate's `unsafe_code = "forbid"` lint).
    // The thin wrapper `check` over `check_with` is trivially correct.

    #[test]
    fn escape_hatch_skips_failing_checks() {
        let tmp = TempDir::new().unwrap();
        set_mode(tmp.path(), 0o777); // would normally refuse
        let subjects = vec![Subject::new(tmp.path().to_owned(), Kind::Dir)];
        assert!(
            check_with(&subjects, true).is_ok(),
            "escape hatch should skip the check"
        );
        assert!(
            check_with(&subjects, false).is_err(),
            "without escape hatch, world-readable dir should refuse"
        );
    }
}
