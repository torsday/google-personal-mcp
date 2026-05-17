//! Google OAuth client credentials — the `credentials.json` file the operator
//! downloads from the GCP console for a "Desktop" OAuth client.
//!
//! Per [ADR-0004](../../docs/adr/0004-oauth-token-refresh.md), the Desktop
//! client type's `client_secret` is not actually secret (Google's own
//! documentation), but we still chmod-protect the file and never log its
//! contents.

use std::path::Path;

use serde::Deserialize;

use crate::error::Error;

/// Decoded Google OAuth client credentials. Either `installed` (Desktop) or
/// `web` (server) shape from Google's downloaded JSON is accepted; both have
/// the same field set in practice.
#[derive(Debug, Clone)]
pub(crate) struct Credentials {
    pub client_id: String,
    pub client_secret: String,
    pub auth_uri: String,
    pub token_uri: String,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(default)]
    installed: Option<RawClient>,
    #[serde(default)]
    web: Option<RawClient>,
}

#[derive(Deserialize)]
struct RawClient {
    client_id: String,
    client_secret: String,
    #[serde(default = "default_auth_uri")]
    auth_uri: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_auth_uri() -> String {
    "https://accounts.google.com/o/oauth2/auth".into()
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

impl Credentials {
    /// Load and parse the credentials file. Returns `Error::Config` for
    /// missing-file or schema-mismatch — the operator's likely next action
    /// is "go download the right file from GCP".
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Config {
            path: path.display().to_string(),
            message: format!("read failed: {e}"),
        })?;
        Self::parse(&text, path)
    }

    pub(crate) fn parse(text: &str, path: &Path) -> Result<Self, Error> {
        let raw: RawFile = serde_json::from_str(text).map_err(|e| Error::Config {
            path: path.display().to_string(),
            message: format!("json parse failed: {e}"),
        })?;
        let c = raw.installed.or(raw.web).ok_or_else(|| Error::Config {
            path: path.display().to_string(),
            message: "expected top-level `installed` or `web` object \
                      — download the Desktop OAuth client JSON from GCP"
                .into(),
        })?;
        Ok(Self {
            client_id: c.client_id,
            client_secret: c.client_secret,
            auth_uri: c.auth_uri,
            token_uri: c.token_uri,
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_path() -> PathBuf {
        PathBuf::from("/tmp/credentials.json")
    }

    #[test]
    fn parses_installed_shape() {
        let json = r#"{"installed":{"client_id":"abc.apps.googleusercontent.com","client_secret":"sssh","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","redirect_uris":["http://localhost"]}}"#;
        let c = Credentials::parse(json, &dummy_path()).expect("parse ok");
        assert_eq!(c.client_id, "abc.apps.googleusercontent.com");
        assert_eq!(c.client_secret, "sssh");
        assert!(c.auth_uri.contains("accounts.google.com"));
    }

    #[test]
    fn parses_web_shape() {
        let json = r#"{"web":{"client_id":"xyz","client_secret":"sec","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token"}}"#;
        let c = Credentials::parse(json, &dummy_path()).expect("parse ok");
        assert_eq!(c.client_id, "xyz");
    }

    #[test]
    fn defaults_endpoints_when_absent() {
        let json = r#"{"installed":{"client_id":"x","client_secret":"y"}}"#;
        let c = Credentials::parse(json, &dummy_path()).expect("parse ok");
        assert_eq!(c.auth_uri, "https://accounts.google.com/o/oauth2/auth");
        assert_eq!(c.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn rejects_unknown_top_level_shape() {
        let json = r#"{"random":{"client_id":"x","client_secret":"y"}}"#;
        let err = Credentials::parse(json, &dummy_path()).expect_err("must fail");
        assert!(matches!(err, Error::Config { .. }));
    }

    #[test]
    fn rejects_malformed_json() {
        let err = Credentials::parse("not json", &dummy_path()).expect_err("must fail");
        assert!(matches!(err, Error::Config { .. }));
    }
}
