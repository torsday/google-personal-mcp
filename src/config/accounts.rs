//! `accounts.toml` — CLI-managed registry of Google accounts. Hot-reloaded
//! per ADR-0002; managed via the `auth add`/`auth remove`/`auth set-default`
//! CLI subcommands.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Registry of Google accounts the daemon knows about. Managed by the
/// `auth add`/`auth remove`/`auth set-default` CLI subcommands; hot-reloaded
/// per ADR-0002.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Accounts {
    #[serde(default)]
    pub(crate) accounts: Vec<AccountEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountEntry {
    pub(crate) alias: String,
    pub(crate) email: String,
    /// Exactly one entry should have `default = true`. The CLI enforces this
    /// on write; [`Accounts::validate`] enforces it on read.
    #[serde(default)]
    pub(crate) default: bool,
}

impl Accounts {
    /// Parse the file at `path`. Missing file returns an empty registry —
    /// fresh installs have no accounts yet.
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Ok(Self { accounts: vec![] });
        }
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text).map_err(|e| Error::Config {
            path: path.display().to_string(),
            message: format!("toml parse failed: {e}"),
        })?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// Serialize and atomically write the registry to `path` (tmpfile + rename).
    /// Validates first so we never persist a broken file. Used by `auth add` /
    /// `auth set-default`; not used at daemon startup (which is read-only).
    pub(crate) fn save(&self, path: &Path) -> Result<(), Error> {
        self.validate(path)?;
        let body = toml::to_string_pretty(self).map_err(|e| Error::Config {
            path: path.display().to_string(),
            message: format!("toml serialize failed: {e}"),
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn validate(&self, path: &Path) -> Result<(), Error> {
        let defaults: Vec<&str> = self
            .accounts
            .iter()
            .filter(|a| a.default)
            .map(|a| a.alias.as_str())
            .collect();
        if defaults.len() > 1 {
            return Err(Error::Config {
                path: path.display().to_string(),
                message: format!(
                    "exactly one account may have `default = true`; found {} ({})",
                    defaults.len(),
                    defaults.join(", ")
                ),
            });
        }
        if !self.accounts.is_empty() && defaults.is_empty() {
            return Err(Error::Config {
                path: path.display().to_string(),
                message: "exactly one account must have `default = true`".into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn accounts_missing_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("accounts.toml");
        let cfg = Accounts::load(&path).unwrap();
        assert!(cfg.accounts.is_empty());
    }

    #[test]
    fn accounts_empty_list_is_valid() {
        let tmp = TempDir::new().unwrap();
        let path = write(&tmp, "accounts.toml", "accounts = []\n");
        let cfg = Accounts::load(&path).unwrap();
        assert!(cfg.accounts.is_empty());
    }

    #[test]
    fn accounts_single_with_default() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "accounts.toml",
            r#"
            [[accounts]]
            alias = "personal"
            email = "user@gmail.com"
            default = true
            "#,
        );
        let cfg = Accounts::load(&path).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].alias, "personal");
        assert!(cfg.accounts[0].default);
    }

    #[test]
    fn accounts_default_omitted_means_false_and_invalid_when_alone() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "accounts.toml",
            r#"
            [[accounts]]
            alias = "personal"
            email = "user@gmail.com"
            "#,
        );
        let err = Accounts::load(&path).expect_err("missing default should fail validation");
        match err {
            Error::Config { message, .. } => assert!(
                message.contains("must have `default = true`"),
                "got: {message}"
            ),
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn accounts_two_defaults_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "accounts.toml",
            r#"
            [[accounts]]
            alias = "personal"
            email = "a@gmail.com"
            default = true

            [[accounts]]
            alias = "work"
            email = "b@company.com"
            default = true
            "#,
        );
        let err = Accounts::load(&path).expect_err("two defaults should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("exactly one"), "got: {message}");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn accounts_unknown_field_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "accounts.toml",
            r#"
            [[accounts]]
            alias = "personal"
            email = "user@gmail.com"
            default = true
            mystery = "x"
            "#,
        );
        let err = Accounts::load(&path).expect_err("unknown field should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("toml parse"), "got: {message}");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn accounts_missing_alias_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "accounts.toml",
            r#"
            [[accounts]]
            email = "user@gmail.com"
            default = true
            "#,
        );
        assert!(Accounts::load(&path).is_err());
    }
}
