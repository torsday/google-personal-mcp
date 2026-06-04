//! Drive OAuth scope vocabulary per
//! [ADR-0025](../../docs/adr/0025-drive-service-surface.md).
//!
//! Drive is the first service to support a **per-account scope override**
//! (`[services.drive.accounts.<alias>] scopes = [...]`). The resolver that
//! chooses between the per-account override and the service-level default lives
//! in [`crate::config`] (it reads config types); this module owns only the
//! Drive-specific *vocabulary* — which scope strings are legal — so the
//! load-time validator in [`crate::config`] has a single source of truth to
//! check against.
//!
//! Only three scopes are accepted (ADR-0025 §"Scopes"):
//!
//! - `drive.file` — per-file access to files the app created or the user opened
//! - `drive.readonly` — read-only access to all of the user's files
//! - `drive` — full read/write access to all of the user's files
//!
//! The broader `drive.metadata`/`drive.appdata`/`drive.scripts` scopes are out
//! of scope for v1.1 and are rejected so a typo or an over-broad grant surfaces
//! at startup rather than at first call.

/// `drive.file` — per-file access (files created or opened by the app).
pub(crate) const SCOPE_DRIVE_FILE: &str = "https://www.googleapis.com/auth/drive.file";
/// `drive.readonly` — read-only access to all of the user's Drive files.
pub(crate) const SCOPE_DRIVE_READONLY: &str = "https://www.googleapis.com/auth/drive.readonly";
/// `drive` — full read/write access to all of the user's Drive files.
pub(crate) const SCOPE_DRIVE: &str = "https://www.googleapis.com/auth/drive";

/// The complete set of Drive scopes a deployment may configure (ADR-0025).
/// Order is least- to most-privileged for readability; lookups are membership
/// tests so order is not significant.
pub(crate) const ALLOWED_DRIVE_SCOPES: &[&str] =
    &[SCOPE_DRIVE_FILE, SCOPE_DRIVE_READONLY, SCOPE_DRIVE];

/// True if `scope` is a Drive scope this server accepts in config.
pub(crate) fn is_allowed_drive_scope(scope: &str) -> bool {
    ALLOWED_DRIVE_SCOPES.contains(&scope)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn each_allowed_scope_is_accepted() {
        for scope in ALLOWED_DRIVE_SCOPES {
            assert!(is_allowed_drive_scope(scope), "should accept {scope}");
        }
    }

    #[test]
    fn the_three_canonical_scopes_are_present() {
        assert!(is_allowed_drive_scope(SCOPE_DRIVE_FILE));
        assert!(is_allowed_drive_scope(SCOPE_DRIVE_READONLY));
        assert!(is_allowed_drive_scope(SCOPE_DRIVE));
        assert_eq!(ALLOWED_DRIVE_SCOPES.len(), 3);
    }

    #[test]
    fn out_of_scope_drive_variants_are_rejected() {
        // Real Google scopes that v1.1 deliberately does not accept.
        assert!(!is_allowed_drive_scope(
            "https://www.googleapis.com/auth/drive.metadata.readonly"
        ));
        assert!(!is_allowed_drive_scope(
            "https://www.googleapis.com/auth/drive.appdata"
        ));
    }

    #[test]
    fn unrelated_or_malformed_strings_are_rejected() {
        assert!(!is_allowed_drive_scope(""));
        assert!(!is_allowed_drive_scope("drive"));
        assert!(!is_allowed_drive_scope(
            "https://www.googleapis.com/auth/gmail.readonly"
        ));
    }
}
