//! macOS Keychain-backed `SecretStore`. Behind the `macos-keychain` feature.
//!
//! Per [ADR-0017](../../docs/adr/0017-secrets-at-rest.md): on a managed Mac,
//! Keychain is the right place for these tokens — the OS owns the encryption
//! key, EDR/DLP filesystem scans see nothing. Headless VPS deployments
//! continue using [`super::file::FileSecretStore`].
//!
//! The Keychain serializes one entry per account alias under a single
//! service identifier (`SERVICE`); each entry's contents are the JSON
//! encoding of [`TokenState`], same as the file backend.
//!
//! First-run on macOS produces the standard Keychain consent dialog. The
//! `keyring` crate handles all the platform-specific protocol; we just
//! call `set_password` / `get_password` / `delete_credential`.

use apple_native_keyring_store::keychain::{Cred, MacKeychainDomain};
use async_trait::async_trait;
use keyring_core::Entry;

use crate::auth::tokens::TokenState;
use crate::error::Error;

/// Service identifier under which all this app's tokens live. Reverse-DNS
/// per Apple convention.
pub(crate) const SERVICE: &str = "org.torsday.google-personal-mcp";

#[derive(Debug, Default)]
pub(crate) struct KeychainSecretStore;

impl KeychainSecretStore {
    fn entry(alias: &str) -> Result<Entry, Error> {
        Cred::build(MacKeychainDomain::User, SERVICE, alias).map_err(|e| Error::Config {
            path: format!("keychain://{SERVICE}/{alias}"),
            message: format!("could not open Keychain entry: {e}"),
        })
    }
}

#[async_trait]
impl super::SecretStore for KeychainSecretStore {
    async fn read_token(&self, alias: &str) -> Result<Option<TokenState>, Error> {
        let alias = alias.to_owned();
        let body = tokio::task::spawn_blocking(move || {
            let entry = Self::entry(&alias)?;
            match entry.get_password() {
                Ok(s) => Ok(Some(s)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(Error::Config {
                    path: format!("keychain://{SERVICE}/{alias}"),
                    message: format!("Keychain read failed: {e}"),
                }),
            }
        })
        .await
        .map_err(|e| Error::Internal {
            context: "spawn_blocking(Keychain read)".to_owned(),
            source: anyhow::Error::new(e),
        })??;

        match body {
            Some(s) => Ok(Some(serde_json::from_str(&s).map_err(|e| {
                Error::Parse {
                    context: "Keychain token JSON".to_owned(),
                    source: e,
                }
            })?)),
            None => Ok(None),
        }
    }

    async fn write_token(&self, alias: &str, state: &TokenState) -> Result<(), Error> {
        let body = serde_json::to_string(state).map_err(|e| Error::Parse {
            context: "serialize TokenState".to_owned(),
            source: e,
        })?;
        let alias = alias.to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Self::entry(&alias)?;
            entry.set_password(&body).map_err(|e| Error::Config {
                path: format!("keychain://{SERVICE}/{alias}"),
                message: format!("Keychain write failed: {e}"),
            })
        })
        .await
        .map_err(|e| Error::Internal {
            context: "spawn_blocking(Keychain write)".to_owned(),
            source: anyhow::Error::new(e),
        })?
    }

    async fn delete_token(&self, alias: &str) -> Result<(), Error> {
        let alias = alias.to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Self::entry(&alias)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
                Err(e) => Err(Error::Config {
                    path: format!("keychain://{SERVICE}/{alias}"),
                    message: format!("Keychain delete failed: {e}"),
                }),
            }
        })
        .await
        .map_err(|e| Error::Internal {
            context: "spawn_blocking(Keychain delete)".to_owned(),
            source: anyhow::Error::new(e),
        })?
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // Real Keychain access in tests requires a logged-in macOS GUI session
    // and the `--ignored` flag — CI uses a non-GUI runner. The tests here
    // only verify constants / construction without touching the live
    // Keychain. Full round-trips live in `tests/keychain_integration.rs`
    // (ignored by default; run via `just test-e2e` on a real Mac).

    #[test]
    fn service_identifier_is_reverse_dns() {
        assert!(SERVICE.contains('.'));
        assert!(SERVICE.starts_with("org.torsday."));
    }

    #[test]
    fn entry_construction_does_not_panic() {
        // `Entry::new` is stateless — it should not touch Keychain.
        let result = KeychainSecretStore::entry("test-alias");
        // Either Ok (entry handle constructed) or a Config error — never panic.
        match result {
            Ok(_) | Err(Error::Config { .. }) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }
}
