//! Configuration loading for `accounts.toml` and `config.toml`.
//!
//! Two related but separate state files, per [ADR-0006](../docs/adr/0006-config.md):
//!
//! - `accounts.toml` — CLI-managed registry of Google accounts.
//! - `config.toml`   — operator-managed daemon settings.
//!
//! Both parse with `#[serde(deny_unknown_fields)]` so typos surface loudly
//! rather than being silently ignored. Tilde (`~/`) in path values is expanded
//! at load time via [`shellexpand`]. The bare home form `~user/...` (other
//! users) is rejected explicitly with a clear error.
//!
//! Module layout (see #92):
//!
//! - [`mod.rs`](self) — path helpers, `is_loopback_bind`, re-exports
//! - [`accounts`] — `accounts.toml` schema + load/save/validate
//! - [`types`] — `config.toml` sub-section structs + their defaults
//! - [`parse`] — `Config::load`/`validate`/`warn_scope_mismatch`

mod accounts;
mod parse;
mod types;

pub(crate) use accounts::{AccountEntry, Accounts};
pub(crate) use types::{Config, RotateMode};

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const APP_NAME: &str = "google-personal-mcp";

// ── Path helpers ─────────────────────────────────────────────────────────────

pub(crate) fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

pub(crate) fn accounts_path(dir: &Path) -> PathBuf {
    dir.join("accounts.toml")
}

pub(crate) fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

/// Path to `http_auth.toml` per
/// [ADR-0020](../../docs/adr/0020-http-transport-authentication.md). Only
/// loaded for non-loopback HTTP binds; the file itself is operator-
/// managed (created via `auth bearer generate` + paste).
pub(crate) fn http_auth_path(dir: &Path) -> PathBuf {
    dir.join(crate::http_auth::HTTP_AUTH_FILE)
}

/// Expand a leading `~/` to `$HOME`. Rejects `~<user>/...` form explicitly
/// per ADR-0006 — the daemon is single-user and the other-user form is a
/// foot-gun.
pub(crate) fn expand_tilde(input: &str) -> Result<PathBuf, String> {
    if input.starts_with('~') && !input.starts_with("~/") && input != "~" {
        return Err(format!(
            "`~user/...` form not supported in `{input}`; use `~/` for $HOME or an absolute path"
        ));
    }
    Ok(PathBuf::from(shellexpand::tilde(input).into_owned()))
}

pub(crate) fn deser_tilde_path<'de, D>(de: D) -> Result<PathBuf, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let s = String::deserialize(de)?;
    expand_tilde(&s).map_err(serde::de::Error::custom)
}

/// Returns `true` when `addr` is a valid `host:port` socket address whose IP
/// part is the IPv4 loopback (`127.x.x.x`) or IPv6 loopback (`::1`).
///
/// Any parse failure (invalid address) is treated as **non-loopback** so the
/// caller can surface an error through the normal config-validation path rather
/// than silently suppressing the warning.
pub(crate) fn is_loopback_bind(addr: &str) -> bool {
    addr.parse::<SocketAddr>()
        .is_ok_and(|sa| sa.ip().is_loopback())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // ── is_loopback_bind ────────────────────────────────────────────────────

    #[test]
    fn loopback_ipv4_127_0_0_1_is_loopback() {
        assert!(is_loopback_bind("127.0.0.1:8765"));
    }

    #[test]
    fn loopback_ipv4_127_x_x_x_is_loopback() {
        // The entire 127.0.0.0/8 range is loopback per RFC 5735.
        assert!(is_loopback_bind("127.1.2.3:1234"));
    }

    #[test]
    fn loopback_ipv6_is_loopback() {
        assert!(is_loopback_bind("[::1]:8765"));
    }

    #[test]
    fn non_loopback_ipv4_is_not_loopback() {
        assert!(!is_loopback_bind("0.0.0.0:8765"));
        assert!(!is_loopback_bind("192.168.1.1:8765"));
        assert!(!is_loopback_bind("10.0.0.1:8765"));
    }

    #[test]
    fn non_loopback_ipv6_is_not_loopback() {
        assert!(!is_loopback_bind("[::]:8765"));
        assert!(!is_loopback_bind("[2001:db8::1]:8765"));
    }

    #[test]
    fn invalid_addr_is_not_loopback() {
        assert!(!is_loopback_bind("not-an-address"));
        assert!(!is_loopback_bind(""));
    }

    // ── tilde expansion ──────────────────────────────────────────────────────

    #[test]
    fn expand_tilde_home() {
        let p = expand_tilde("~/foo").expect("ok");
        assert!(
            !p.to_string_lossy().starts_with('~'),
            "tilde should be expanded, got: {}",
            p.display()
        );
        assert!(
            p.to_string_lossy().ends_with("/foo"),
            "got: {}",
            p.display()
        );
    }

    #[test]
    fn expand_tilde_absolute_unchanged() {
        let p = expand_tilde("/etc/foo").expect("ok");
        assert_eq!(p, PathBuf::from("/etc/foo"));
    }

    #[test]
    fn expand_tilde_rejects_other_user() {
        let err = expand_tilde("~root/foo").expect_err("should reject");
        assert!(err.contains("~user/"), "got: {err}");
    }

    #[test]
    fn expand_tilde_bare() {
        let p = expand_tilde("~").expect("ok");
        assert!(
            !p.to_string_lossy().starts_with('~'),
            "got: {}",
            p.display()
        );
    }
}
