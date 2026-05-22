//! Atomic on-disk persistence for `TokenState`. ADR-0017.
//!
//! Writes go to a temp file in the same directory, get chmod 0600 on Unix,
//! then atomic-rename into place. The rename is the durability boundary —
//! callers see either the prior version or the new one, never a partial.

use std::path::Path;

use crate::error::Error;

pub(super) async fn write_atomic_0600(
    tmp: &Path,
    final_path: &Path,
    bytes: &[u8],
) -> Result<(), Error> {
    tokio::fs::write(tmp, bytes).await?;
    set_mode_0600(tmp).await?;
    tokio::fs::rename(tmp, final_path).await?;
    Ok(())
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
    // No-op on non-Unix; ADR-0017's permission check is Unix-only.
    Ok(())
}
