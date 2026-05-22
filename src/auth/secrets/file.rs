//! File-backed `SecretStore` — atomic tmpfile+rename writes with 0600
//! permissions on Unix. Mirrors the inline logic that previously lived in
//! `TokenManager::persist_atomic` and `cli::write_token_file`; both will
//! migrate to call through this trait in a follow-up PR.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::auth::tokens::TokenState;
use crate::error::Error;

#[derive(Debug, Clone)]
pub(crate) struct FileSecretStore {
    tokens_dir: PathBuf,
}

impl FileSecretStore {
    pub(crate) const fn new(tokens_dir: PathBuf) -> Self {
        Self { tokens_dir }
    }

    fn final_path(&self, alias: &str) -> PathBuf {
        self.tokens_dir.join(format!("{alias}.json"))
    }

    fn tmp_path(&self, alias: &str) -> PathBuf {
        self.tokens_dir.join(format!(".{alias}.json.tmp"))
    }
}

#[async_trait]
impl super::SecretStore for FileSecretStore {
    async fn read_token(&self, alias: &str) -> Result<Option<TokenState>, Error> {
        let path = self.final_path(alias);
        if !path.exists() {
            return Ok(None);
        }
        let body = tokio::fs::read_to_string(&path).await?;
        let state: TokenState = serde_json::from_str(&body).map_err(|e| Error::Parse {
            context: format!("token file {}", path.display()),
            source: e,
        })?;
        Ok(Some(state))
    }

    async fn write_token(&self, alias: &str, state: &TokenState) -> Result<(), Error> {
        tokio::fs::create_dir_all(&self.tokens_dir).await?;
        let body = serde_json::to_string_pretty(state).map_err(|e| Error::Parse {
            context: "serialize TokenState".to_owned(),
            source: e,
        })?;
        let tmp = self.tmp_path(alias);
        let final_path = self.final_path(alias);
        tokio::fs::write(&tmp, body.as_bytes()).await?;
        set_mode_0600(&tmp).await?;
        tokio::fs::rename(&tmp, &final_path).await?;
        Ok(())
    }

    async fn delete_token(&self, alias: &str) -> Result<(), Error> {
        let path = self.final_path(alias);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

#[cfg(unix)]
async fn set_mode_0600(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(path).await?.permissions();
    perms.set_mode(0o600);
    tokio::fs::set_permissions(path, perms).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_mode_0600(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::auth::secrets::SecretStore;
    use chrono::Utc;

    fn sample_state() -> TokenState {
        TokenState {
            access_token: "AAA".into(),
            refresh_token: "RRR".into(),
            expires_at: Utc::now(),
            scopes: vec!["scope.read".into()],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    fn unique_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gpm-secrets-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let store = FileSecretStore::new(unique_dir());
        let state = sample_state();
        store.write_token("work", &state).await.expect("write");
        let read = store.read_token("work").await.expect("read");
        let read = read.expect("present");
        assert_eq!(read.access_token, "AAA");
        assert_eq!(read.refresh_token, "RRR");
    }

    #[tokio::test]
    async fn read_missing_returns_none() {
        let store = FileSecretStore::new(unique_dir());
        let result = store.read_token("ghost").await.expect("ok");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn write_sets_0600_on_unix() {
        let dir = unique_dir();
        let store = FileSecretStore::new(dir.clone());
        store
            .write_token("work", &sample_state())
            .await
            .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("work.json"))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let store = FileSecretStore::new(unique_dir());
        // Deleting a missing token must not error.
        store.delete_token("ghost").await.expect("first delete ok");
        // Now write + delete.
        store
            .write_token("work", &sample_state())
            .await
            .expect("write");
        store.delete_token("work").await.expect("delete present");
        // Second delete still ok.
        store.delete_token("work").await.expect("re-delete ok");
        assert!(store.read_token("work").await.expect("read").is_none());
    }
}
