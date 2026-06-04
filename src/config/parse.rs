//! `Config::load` + validation. Pure I/O + invariant checks; the type
//! definitions live in [`super::types`].

use std::net::SocketAddr;
use std::path::Path;

use crate::error::Error;

use super::types::{Config, SANCTIONED_TOOL_OVERRIDES};

impl Config {
    /// Parse the file at `path`. Missing file returns default config — the
    /// daemon runs with the ADR-0006 defaults when no operator overrides exist.
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text).map_err(|e| Error::Config {
            path: path.display().to_string(),
            message: format!("toml parse failed: {e}"),
        })?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// Emit a `WARN` log for each account whose granted token scopes don't cover
    /// the configured Gmail profile's required scopes. Called at startup after
    /// loading both `config.toml` and all token files.
    ///
    /// A mismatch is not fatal — the operator may have intentionally changed the
    /// profile without re-authenticating. The warning surfaces the discrepancy so
    /// they know to run `auth refresh` or `auth grant` for that account.
    pub(crate) fn warn_scope_mismatch(&self, account: &str, granted: &[String]) {
        if !self.services.gmail.enabled {
            return;
        }
        let profile = match self.services.gmail.gmail_profile() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(account = %account, error = %e, "could not parse gmail profile for scope-mismatch check");
                return;
            }
        };
        if !profile.granted_scopes_match(granted) {
            tracing::warn!(
                account = %account,
                profile = %self.services.gmail.profile,
                ?granted,
                required = ?profile.scopes(),
                "token scopes do not match configured gmail profile — run `google-personal-mcp auth refresh {account}` to re-authorize"
            );
        }
    }

    fn validate(&self, path: &Path) -> Result<(), Error> {
        let display = || path.display().to_string();
        // Validate the gmail profile string so a typo surfaces at startup.
        if self.services.gmail.enabled {
            self.services
                .gmail
                .gmail_profile()
                .map_err(|e| Error::Config {
                    path: display(),
                    message: format!("invalid [services.gmail].profile: {e}"),
                })?;
        }
        if self.http.bind.parse::<SocketAddr>().is_err() {
            return Err(Error::Config {
                path: display(),
                message: format!(
                    "`http.bind` value `{}` is not a valid socket address (e.g. `127.0.0.1:8765`)",
                    self.http.bind
                ),
            });
        }
        if self.rate_limit.gmail.requests_per_second == 0 {
            return Err(Error::Config {
                path: display(),
                message: "`rate_limit.gmail.requests_per_second` must be > 0".into(),
            });
        }
        if self.rate_limit.calendar.requests_per_second == 0 {
            return Err(Error::Config {
                path: display(),
                message: "`rate_limit.calendar.requests_per_second` must be > 0".into(),
            });
        }
        // Per-tool capability overrides are a bounded exception list (ADR-0022
        // §Per-tool override) — reject any override naming a tool outside the
        // sanctioned set so the escape hatch can't be widened by config alone.
        for (service_name, entry) in self.services.all() {
            for tool in entry.tools.keys() {
                if !SANCTIONED_TOOL_OVERRIDES.contains(&tool.as_str()) {
                    return Err(Error::Config {
                        path: display(),
                        message: format!(
                            "[services.{service_name}.tools.{tool}] is not a sanctioned per-tool \
                             override; only {SANCTIONED_TOOL_OVERRIDES:?} may be overridden \
                             (ADR-0022 — adding one requires an ADR amendment)"
                        ),
                    });
                }
            }
        }
        // Drive scope vocabulary (ADR-0025). Both the service-level
        // `[services.drive] scopes` and every per-account override
        // `[services.drive.accounts.<alias>] scopes` may name only scopes in the
        // allowed Drive set — reject anything else at load time so a typo or an
        // over-broad grant fails the daemon at startup, not at first call.
        self.validate_drive_scopes(&display)?;
        Ok(())
    }

    /// Reject any configured Drive scope outside the allowed set
    /// ([`crate::drive::scopes::ALLOWED_DRIVE_SCOPES`]). Checks the
    /// service-level scopes and each per-account override. Split out of
    /// [`Self::validate`] so the (otherwise long) validator stays readable.
    fn validate_drive_scopes(&self, display: &impl Fn() -> String) -> Result<(), Error> {
        use crate::drive::scopes::{is_allowed_drive_scope, ALLOWED_DRIVE_SCOPES};

        let reject = |location: String, scope: &str| Error::Config {
            path: display(),
            message: format!(
                "[services.drive{location}].scopes contains `{scope}`, which is not an \
                 allowed Drive scope; expected one of {ALLOWED_DRIVE_SCOPES:?} (ADR-0025)"
            ),
        };

        let drive = &self.services.drive;
        for scope in &drive.scopes {
            if !is_allowed_drive_scope(scope) {
                return Err(reject(String::new(), scope));
            }
        }
        for (alias, over) in &drive.accounts {
            for scope in &over.scopes {
                if !is_allowed_drive_scope(scope) {
                    return Err(reject(format!(".accounts.{alias}"), scope));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::auth::scopes::GmailProfile;
    use crate::config::RotateMode;
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
    fn config_missing_file_uses_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.log_level, "info");
        assert!(cfg.services.gmail.enabled);
        assert!(!cfg.services.calendar.enabled);
        assert_eq!(cfg.http.bind, "127.0.0.1:8765");
    }

    #[test]
    fn config_empty_file_is_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = write(&tmp, "config.toml", "");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.log_level, "info");
    }

    #[test]
    fn config_overrides() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [server]
            log_level = "debug"

            [services.gmail]
            enabled = false
            scopes = ["https://www.googleapis.com/auth/gmail.readonly"]
            "#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.log_level, "debug");
        assert!(!cfg.services.gmail.enabled);
        assert_eq!(cfg.services.gmail.scopes.len(), 1);
    }

    #[test]
    fn config_tilde_in_path_value_expanded() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [google]
            credentials_path = "~/somewhere/google.json"
            "#,
        );
        let cfg = Config::load(&path).unwrap();
        assert!(
            !cfg.google
                .credentials_path
                .to_string_lossy()
                .starts_with('~'),
            "tilde should be expanded, got: {}",
            cfg.google.credentials_path.display()
        );
        assert!(
            cfg.google
                .credentials_path
                .to_string_lossy()
                .ends_with("/somewhere/google.json"),
            "got: {}",
            cfg.google.credentials_path.display()
        );
    }

    #[test]
    fn config_other_user_tilde_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [google]
            credentials_path = "~root/google.json"
            "#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn config_unknown_top_level_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(&tmp, "config.toml", "mystery = true\n");
        let err = Config::load(&path).expect_err("unknown key should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("toml parse"), "got: {message}");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn config_unknown_nested_key_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [server]
            log_level = "info"
            log_styyle = "compact"
            "#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn config_bad_bind_address_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [http]
            bind = "not-an-address"
            "#,
        );
        let err = Config::load(&path).expect_err("bad bind should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("http.bind"), "got: {message}");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn config_zero_rate_limit_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r"
            [rate_limit.gmail]
            requests_per_second = 0
            burst = 20
            ",
        );
        let err = Config::load(&path).expect_err("zero rate should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("requests_per_second"), "got: {message}");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    // ── drive scopes + per-account override (ADR-0025) ────────────────────────

    #[test]
    fn config_drive_valid_scopes_load_and_resolve() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [services.drive]
            enabled = true
            scopes = ["https://www.googleapis.com/auth/drive.readonly"]

            [services.drive.accounts.work]
            scopes = ["https://www.googleapis.com/auth/drive"]
            "#,
        );
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.services.drive.enabled);
        // `work` has an override → it wins over the service-level scope.
        assert_eq!(
            cfg.services.drive.resolve_account_scopes("work"),
            &["https://www.googleapis.com/auth/drive".to_owned()]
        );
        // `personal` has no override → inherits the service-level scope.
        assert_eq!(
            cfg.services.drive.resolve_account_scopes("personal"),
            &["https://www.googleapis.com/auth/drive.readonly".to_owned()]
        );
    }

    #[test]
    fn config_drive_invalid_service_scope_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [services.drive]
            enabled = true
            scopes = ["https://www.googleapis.com/auth/drive.metadata.readonly"]
            "#,
        );
        let err = Config::load(&path).expect_err("disallowed drive scope should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(
                    message.contains("[services.drive].scopes"),
                    "got: {message}"
                );
                assert!(
                    message.contains("not an allowed Drive scope"),
                    "got: {message}"
                );
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn config_drive_invalid_account_override_scope_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [services.drive]
            enabled = true
            scopes = ["https://www.googleapis.com/auth/drive.readonly"]

            [services.drive.accounts.work]
            scopes = ["https://example.com/not-a-drive-scope"]
            "#,
        );
        let err = Config::load(&path).expect_err("disallowed per-account scope should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(
                    message.contains("[services.drive.accounts.work].scopes"),
                    "got: {message}"
                );
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn config_drive_empty_account_override_inherits_service_scopes() {
        // An account block with no `scopes` (only a capabilities override) must
        // inherit the service-level scopes, not resolve to empty.
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [services.drive]
            enabled = true
            scopes = ["https://www.googleapis.com/auth/drive.file"]

            [services.drive.accounts.work.capabilities]
            write = true
            "#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.services.drive.resolve_account_scopes("work"),
            &["https://www.googleapis.com/auth/drive.file".to_owned()]
        );
    }

    // ── profile field ────────────────────────────────────────────────────────

    #[test]
    fn config_default_profile_is_modify_send() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.services.gmail.profile, "modify+send");
        assert_eq!(
            cfg.services.gmail.gmail_profile().unwrap(),
            GmailProfile::ModifyAndSend
        );
    }

    #[test]
    fn config_profile_readonly_parsed() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [services.gmail]
            enabled = true
            profile = "readonly"
            "#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.services.gmail.gmail_profile().unwrap(),
            GmailProfile::Readonly
        );
    }

    #[test]
    fn config_profile_modify_parsed() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [services.gmail]
            enabled = true
            profile = "modify"
            "#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.services.gmail.gmail_profile().unwrap(),
            GmailProfile::Modify
        );
    }

    #[test]
    fn config_invalid_profile_rejected_at_load() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r#"
            [services.gmail]
            enabled = true
            profile = "superuser"
            "#,
        );
        let err = Config::load(&path).expect_err("invalid profile should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(
                    message.contains("superuser") || message.contains("profile"),
                    "got: {message}"
                );
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    // ── warn_scope_mismatch ──────────────────────────────────────────────────

    #[test]
    fn scope_mismatch_check_readonly_profile_with_readonly_grant() {
        let cfg = Config::default();
        // readonly profile, token has readonly scope — no mismatch
        let mut readonly_cfg = cfg;
        readonly_cfg.services.gmail.profile = "readonly".into();
        readonly_cfg.services.gmail.enabled = true;
        let granted = vec![crate::auth::scopes::SCOPE_READONLY.to_owned()];
        // Should not panic; just logs (can't assert on warn logs in unit tests)
        readonly_cfg.warn_scope_mismatch("personal", &granted);
    }

    #[test]
    fn scope_mismatch_skipped_when_gmail_disabled() {
        let mut cfg = Config::default();
        cfg.services.gmail.enabled = false;
        // Even with mismatched scopes, disabled service → no warning emitted
        // (Can only verify it doesn't panic here)
        cfg.warn_scope_mismatch("personal", &[]);
    }

    // ── AuditConfig ──────────────────────────────────────────────────────────

    #[test]
    fn audit_verbose_defaults_to_false() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let cfg = Config::load(&path).unwrap();
        assert!(!cfg.audit.verbose, "verbose should default to false");
    }

    #[test]
    fn audit_verbose_true_parsed() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            "
            [audit]
            verbose = true
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.audit.verbose);
    }

    #[test]
    fn audit_verbose_false_explicit_parsed() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            "
            [audit]
            verbose = false
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert!(!cfg.audit.verbose);
    }

    #[test]
    fn audit_rotate_parses_from_config_toml() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            "
            [audit]
            rotate = \"weekly\"
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.audit.rotate, RotateMode::Weekly);
    }

    #[test]
    fn audit_rotate_defaults_to_monthly_when_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.audit.rotate, RotateMode::Monthly);
    }

    #[test]
    fn audit_rotate_size_parses_from_config_toml() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            "
            [audit]
            rotate = \"size:10485760\"
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.audit.rotate, RotateMode::Size(10_485_760));
    }

    // ── capability gating (ADR-0022) ──────────────────────────────────────────

    use crate::tools::aspect::Aspect;

    #[test]
    fn capabilities_gmail_enabled_without_block_is_all_on() {
        // The grandfathered-default guard: `[services.gmail]` present, enabled,
        // but no `capabilities` block must resolve all-on — never the
        // conservative read-only default.
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r"
            [services.gmail]
            enabled = true
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.resolve_capability("personal", "gmail", Aspect::Write));
        assert!(cfg.resolve_capability("personal", "gmail", Aspect::Destructive));
    }

    #[test]
    fn capabilities_service_block_parsed_and_resolved() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r"
            [services.calendar]
            enabled = true

            [services.calendar.capabilities]
            read        = true
            write       = true
            destructive = false
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.resolve_capability("personal", "calendar", Aspect::Write));
        assert!(!cfg.resolve_capability("personal", "calendar", Aspect::Destructive));
    }

    #[test]
    fn capabilities_per_account_override_parsed_and_resolved() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r"
            [services.calendar]
            enabled = true

            [services.calendar.capabilities]
            destructive = false

            [services.calendar.accounts.work.capabilities]
            destructive = true
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.resolve_capability("work", "calendar", Aspect::Destructive));
        assert!(!cfg.resolve_capability("personal", "calendar", Aspect::Destructive));
    }

    #[test]
    fn capabilities_sanctioned_per_tool_override_accepted() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r"
            [services.contacts]
            enabled = true

            [services.contacts.tools.list_directory_people]
            enabled = false
            ",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.services
                .contacts
                .tools
                .get("list_directory_people")
                .map(|t| t.enabled),
            Some(false)
        );
    }

    #[test]
    fn capabilities_unsanctioned_per_tool_override_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r"
            [services.gmail]
            enabled = true

            [services.gmail.tools.send_email]
            enabled = false
            ",
        );
        let err = Config::load(&path).expect_err("unsanctioned tool override should fail");
        match err {
            Error::Config { message, .. } => {
                assert!(
                    message.contains("send_email") && message.contains("sanctioned"),
                    "got: {message}"
                );
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn capabilities_unknown_aspect_key_rejected() {
        // deny_unknown_fields: a typo in a capability key must be loud.
        let tmp = TempDir::new().unwrap();
        let path = write(
            &tmp,
            "config.toml",
            r"
            [services.calendar.capabilities]
            reed = true
            ",
        );
        assert!(
            Config::load(&path).is_err(),
            "typo'd capability key should be rejected"
        );
    }
}
