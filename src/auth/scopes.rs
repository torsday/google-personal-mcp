//! Gmail profile → OAuth scope set mapping, per ADR-0006 / issue #22.
//!
//! The `GmailProfile` enum is the single source of truth for which OAuth
//! scopes a given deployment requests. Keeping scope derivation in one place
//! prevents the "granted scopes don't match requested scopes" class of bugs.

use crate::error::Error;

/// Canonical Gmail OAuth scope URIs.
pub(crate) const SCOPE_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub(crate) const SCOPE_MODIFY: &str = "https://www.googleapis.com/auth/gmail.modify";
pub(crate) const SCOPE_SEND: &str = "https://www.googleapis.com/auth/gmail.send";

/// Operator-selected capability level for a Gmail account.
///
/// The profile maps to the OAuth scopes requested during `auth add` and is
/// enforced at the tool boundary: write tools return `Error::AuthRequired`
/// immediately if the active profile's scope set excludes the required scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum GmailProfile {
    /// `gmail.readonly` — read and search only; no modify or send.
    Readonly,
    /// `gmail.modify` — read, search, label, archive, trash; no send.
    Modify,
    /// `gmail.modify` + `gmail.send` — full access including outbound mail.
    /// This is the default.
    #[default]
    ModifyAndSend,
}

impl GmailProfile {
    /// Parse a profile from a config string value.
    #[allow(clippy::should_implement_trait)]
    pub(crate) fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "readonly" => Ok(Self::Readonly),
            "modify" => Ok(Self::Modify),
            "modify+send" => Ok(Self::ModifyAndSend),
            other => Err(Error::Config {
                path: "[services.gmail].profile".into(),
                message: format!(
                    "unknown profile `{other}`; expected one of: readonly, modify, modify+send"
                ),
            }),
        }
    }

    /// The OAuth scope strings this profile requires.
    pub(crate) const fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::Readonly => &[SCOPE_READONLY],
            Self::Modify => &[SCOPE_MODIFY],
            Self::ModifyAndSend => &[SCOPE_MODIFY, SCOPE_SEND],
        }
    }

    /// True if `scope` is covered by this profile.
    pub(crate) fn includes_scope(self, scope: &str) -> bool {
        self.scopes().contains(&scope)
    }

    /// Guard for write tools that require `gmail.modify`. Returns
    /// `Err(Error::AuthRequired)` when the profile is `readonly`.
    pub(crate) fn require_modify(self, account: &str) -> Result<(), Error> {
        if self == Self::Readonly {
            return Err(Error::AuthRequired {
                account: account.to_owned(),
                reason: "profile is `readonly`; re-authorize with the `modify` or `modify+send` profile to use write tools".into(),
            });
        }
        Ok(())
    }

    /// Guard for send tools that require `gmail.send`. Returns
    /// `Err(Error::AuthRequired)` when the profile is not `modify+send`.
    pub(crate) fn require_send(self, account: &str) -> Result<(), Error> {
        if self != Self::ModifyAndSend {
            return Err(Error::AuthRequired {
                account: account.to_owned(),
                reason: "profile is not `modify+send`; re-authorize with the `modify+send` profile to use send_email".into(),
            });
        }
        Ok(())
    }

    /// Check whether the granted scopes (from the stored token) cover the
    /// profile's required scopes. Returns `true` if they match, `false` if the
    /// token is missing one or more required scopes.
    ///
    /// Called at startup to warn when a token was granted with a different
    /// profile than what is currently configured.
    pub(crate) fn granted_scopes_match(self, granted: &[String]) -> bool {
        self.scopes()
            .iter()
            .all(|required| granted.iter().any(|g| g == required))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    // ── from_str ────────────────────────────────────────────────────────────

    #[test]
    fn from_str_readonly() {
        assert_eq!(
            GmailProfile::from_str("readonly").unwrap(),
            GmailProfile::Readonly
        );
    }

    #[test]
    fn from_str_modify() {
        assert_eq!(
            GmailProfile::from_str("modify").unwrap(),
            GmailProfile::Modify
        );
    }

    #[test]
    fn from_str_modify_send() {
        assert_eq!(
            GmailProfile::from_str("modify+send").unwrap(),
            GmailProfile::ModifyAndSend
        );
    }

    #[test]
    fn from_str_unknown_is_error() {
        let err = GmailProfile::from_str("superuser").unwrap_err();
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("superuser"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── scopes ───────────────────────────────────────────────────────────────

    #[test]
    fn readonly_scopes_contains_only_readonly() {
        let s = GmailProfile::Readonly.scopes();
        assert_eq!(s, &[SCOPE_READONLY]);
    }

    #[test]
    fn modify_scopes_contains_only_modify() {
        let s = GmailProfile::Modify.scopes();
        assert_eq!(s, &[SCOPE_MODIFY]);
    }

    #[test]
    fn modify_send_scopes_contains_both() {
        let s = GmailProfile::ModifyAndSend.scopes();
        assert!(s.contains(&SCOPE_MODIFY), "missing modify");
        assert!(s.contains(&SCOPE_SEND), "missing send");
    }

    // ── includes_scope ────────────────────────────────────────────────────────

    #[test]
    fn readonly_includes_readonly_scope() {
        assert!(GmailProfile::Readonly.includes_scope(SCOPE_READONLY));
    }

    #[test]
    fn readonly_does_not_include_send() {
        assert!(!GmailProfile::Readonly.includes_scope(SCOPE_SEND));
    }

    #[test]
    fn modify_send_includes_send() {
        assert!(GmailProfile::ModifyAndSend.includes_scope(SCOPE_SEND));
    }

    // ── require_modify ────────────────────────────────────────────────────────

    #[test]
    fn readonly_require_modify_is_error() {
        let err = GmailProfile::Readonly
            .require_modify("personal")
            .unwrap_err();
        match err {
            Error::AuthRequired { account, reason } => {
                assert_eq!(account, "personal");
                assert!(reason.contains("readonly"), "got: {reason}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn modify_require_modify_ok() {
        GmailProfile::Modify.require_modify("work").unwrap();
    }

    #[test]
    fn modify_send_require_modify_ok() {
        GmailProfile::ModifyAndSend.require_modify("work").unwrap();
    }

    // ── require_send ──────────────────────────────────────────────────────────

    #[test]
    fn readonly_require_send_is_error() {
        let err = GmailProfile::Readonly.require_send("personal").unwrap_err();
        assert!(matches!(err, Error::AuthRequired { .. }));
    }

    #[test]
    fn modify_require_send_is_error() {
        let err = GmailProfile::Modify.require_send("personal").unwrap_err();
        match err {
            Error::AuthRequired { reason, .. } => {
                assert!(reason.contains("modify+send"), "got: {reason}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn modify_send_require_send_ok() {
        GmailProfile::ModifyAndSend.require_send("work").unwrap();
    }

    // ── granted_scopes_match ─────────────────────────────────────────────────

    #[test]
    fn readonly_profile_matches_readonly_grant() {
        let granted = vec![SCOPE_READONLY.to_owned()];
        assert!(GmailProfile::Readonly.granted_scopes_match(&granted));
    }

    #[test]
    fn readonly_profile_does_not_match_modify_only_grant() {
        let granted = vec![SCOPE_MODIFY.to_owned()];
        assert!(!GmailProfile::Readonly.granted_scopes_match(&granted));
    }

    #[test]
    fn modify_send_profile_matches_full_grant() {
        let granted = vec![SCOPE_MODIFY.to_owned(), SCOPE_SEND.to_owned()];
        assert!(GmailProfile::ModifyAndSend.granted_scopes_match(&granted));
    }

    #[test]
    fn modify_send_profile_does_not_match_readonly_only_grant() {
        let granted = vec![SCOPE_READONLY.to_owned()];
        assert!(!GmailProfile::ModifyAndSend.granted_scopes_match(&granted));
    }

    #[test]
    fn modify_profile_matches_when_modify_in_superset_grant() {
        // A token granted with modify+send also covers a modify profile.
        let granted = vec![SCOPE_MODIFY.to_owned(), SCOPE_SEND.to_owned()];
        assert!(GmailProfile::Modify.granted_scopes_match(&granted));
    }
}
