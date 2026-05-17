use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Error;

const APP_NAME: &str = "google-personal-mcp";

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    pub(crate) accounts: Vec<AccountEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
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
