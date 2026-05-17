use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Error;

const APP_NAME: &str = "google-personal-mcp";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) accounts: Vec<AccountEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountEntry {
    pub(crate) alias: String,
    pub(crate) email: String,
}

impl Config {
    pub(crate) fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME)
    }

    pub(crate) fn load() -> Result<Self, Error> {
        let path = Self::config_dir().join("config.toml");
        if !path.exists() {
            return Ok(Self { accounts: vec![] });
        }
        let text = std::fs::read_to_string(&path)?;
        toml::from_str(&text).map_err(|e| Error::Internal {
            context: "config::load".into(),
            source: e.into(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(s)
    }

    #[test]
    fn empty_accounts_list() {
        let cfg: Config = parse("accounts = []").expect("valid toml");
        assert!(cfg.accounts.is_empty());
    }

    #[test]
    fn single_account_valid() {
        let cfg: Config = parse(
            r#"
            [[accounts]]
            alias = "personal"
            email = "user@gmail.com"
            "#,
        )
        .expect("valid toml");
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].alias, "personal");
        assert_eq!(cfg.accounts[0].email, "user@gmail.com");
    }

    #[test]
    fn multiple_accounts_valid() {
        let cfg: Config = parse(
            r#"
            [[accounts]]
            alias = "personal"
            email = "a@gmail.com"

            [[accounts]]
            alias = "work"
            email = "b@company.com"
            "#,
        )
        .expect("valid toml");
        assert_eq!(cfg.accounts.len(), 2);
    }

    #[test]
    fn missing_alias_is_error() {
        let result = parse(
            r#"
            [[accounts]]
            email = "user@gmail.com"
            "#,
        );
        assert!(result.is_err(), "expected error for missing alias field");
    }

    #[test]
    fn missing_email_is_error() {
        let result = parse(
            r#"
            [[accounts]]
            alias = "personal"
            "#,
        );
        assert!(result.is_err(), "expected error for missing email field");
    }

    #[test]
    fn unknown_top_level_key_is_error() {
        let result = parse(
            r#"
            accounts = []
            unknown_key = "bad"
            "#,
        );
        assert!(
            result.is_err(),
            "expected error for unknown top-level field"
        );
    }

    #[test]
    fn unknown_account_field_is_error() {
        let result = parse(
            r#"
            [[accounts]]
            alias = "personal"
            email = "user@gmail.com"
            extra = "should fail"
            "#,
        );
        assert!(result.is_err(), "expected error for unknown account field");
    }

    #[test]
    fn completely_empty_config_is_error() {
        // A config with no `accounts` key at all should fail (required field).
        let result = parse("");
        assert!(
            result.is_err(),
            "expected error when accounts key is absent"
        );
    }
}
