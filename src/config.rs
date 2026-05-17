//! Configuration loading for `accounts.toml` and `config.toml`.
//!
//! Two related but separate state files, per [ADR-0006](../docs/adr/0006-config.md):
//!
//! - `accounts.toml` — CLI-managed registry of Google accounts.
//! - `config.toml`   — operator-managed daemon settings.
//!
//! Both parse with `#[serde(deny_unknown_fields)]` so typos surface loudly
//! rather than being silently ignored. Tilde (`~/`) in path values is expanded
//! at load time via [`shellexpand`]. The bare home form `~user/...` (other
//! users) is rejected explicitly with a clear error.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::auth::scopes::GmailProfile;
use crate::error::Error;

const APP_NAME: &str = "google-personal-mcp";

// ── Path helpers ─────────────────────────────────────────────────────────────

pub(crate) fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

pub(crate) fn accounts_path(dir: &Path) -> PathBuf {
    dir.join("accounts.toml")
}

pub(crate) fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

/// Expand a leading `~/` to `$HOME`. Rejects `~<user>/...` form explicitly
/// per ADR-0006 — the daemon is single-user and the other-user form is a
/// foot-gun.
pub(crate) fn expand_tilde(input: &str) -> Result<PathBuf, String> {
    if input.starts_with('~') && !input.starts_with("~/") && input != "~" {
        return Err(format!(
            "`~user/...` form not supported in `{input}`; use `~/` for $HOME or an absolute path"
        ));
    }
    Ok(PathBuf::from(shellexpand::tilde(input).into_owned()))
}

fn deser_tilde_path<'de, D>(de: D) -> Result<PathBuf, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(de)?;
    expand_tilde(&s).map_err(serde::de::Error::custom)
}

// ── accounts.toml ────────────────────────────────────────────────────────────

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

// ── config.toml ──────────────────────────────────────────────────────────────

/// Operator-managed daemon settings per ADR-0006. All fields optional; missing
/// file means "use all defaults".
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) server: ServerConfig,
    #[serde(default)]
    pub(crate) google: GoogleConfig,
    #[serde(default = "default_services")]
    pub(crate) services: ServicesConfig,
    #[serde(default)]
    pub(crate) rate_limit: RateLimitConfig,
    #[serde(default)]
    pub(crate) http: HttpConfig,
    #[serde(default)]
    pub(crate) retry: RetryConfig,
    #[serde(default)]
    pub(crate) secrets: SecretsConfig,
}

/// `[secrets]` section — selects the storage backend per
/// [ADR-0017](../docs/adr/0017-secrets-at-rest.md) extension for #20.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretsConfig {
    /// `"file"` (default elsewhere) or `"keychain"` (default on macOS).
    /// Selection logic lives in [`crate::auth::secrets::build`]; this is
    /// just the operator-visible knob.
    #[serde(default = "default_secrets_backend")]
    pub(crate) backend: crate::auth::secrets::BackendChoice,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            backend: default_secrets_backend(),
        }
    }
}

const fn default_secrets_backend() -> crate::auth::secrets::BackendChoice {
    crate::auth::secrets::BackendChoice::platform_default()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerConfig {
    #[serde(default = "default_log_level")]
    pub(crate) log_level: String,
    #[serde(default = "default_log_format")]
    pub(crate) log_format: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            log_format: default_log_format(),
        }
    }
}

fn default_log_level() -> String {
    "info".into()
}

fn default_log_format() -> String {
    "compact".into()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoogleConfig {
    #[serde(
        default = "default_credentials_path",
        deserialize_with = "deser_tilde_path"
    )]
    pub(crate) credentials_path: PathBuf,
    #[serde(default)]
    pub(crate) oauth: OAuthConfig,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            credentials_path: default_credentials_path(),
            oauth: OAuthConfig::default(),
        }
    }
}

fn default_credentials_path() -> PathBuf {
    expand_tilde("~/.config/google-personal-mcp/credentials/google.json")
        .unwrap_or_else(|_| PathBuf::from("credentials/google.json"))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuthConfig {
    #[serde(default = "default_redirect_port")]
    pub(crate) redirect_port: u16,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            redirect_port: default_redirect_port(),
        }
    }
}

const fn default_redirect_port() -> u16 {
    8080
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServicesConfig {
    #[serde(default = "default_gmail_service")]
    pub(crate) gmail: ServiceEntry,
    #[serde(default)]
    pub(crate) calendar: ServiceEntry,
    #[serde(default)]
    pub(crate) contacts: ServiceEntry,
}

impl Default for ServicesConfig {
    fn default() -> Self {
        default_services()
    }
}

fn default_services() -> ServicesConfig {
    ServicesConfig {
        gmail: default_gmail_service(),
        calendar: ServiceEntry::default(),
        contacts: ServiceEntry::default(),
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceEntry {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) scopes: Vec<String>,
    /// Operator-selected Gmail capability level. Determines which OAuth scopes
    /// are requested and enforced. Defaults to `"modify+send"`.
    #[serde(default = "default_gmail_profile_str")]
    pub(crate) profile: String,
}

impl Default for ServiceEntry {
    fn default() -> Self {
        Self {
            enabled: false,
            scopes: vec![],
            profile: default_gmail_profile_str(),
        }
    }
}

impl ServiceEntry {
    /// Parse `profile` into a typed `GmailProfile`. Returns `Err(Error::Config)`
    /// for unknown values.
    pub(crate) fn gmail_profile(&self) -> Result<GmailProfile, Error> {
        GmailProfile::from_str(&self.profile)
    }
}

fn default_gmail_profile_str() -> String {
    "modify+send".into()
}

fn default_gmail_service() -> ServiceEntry {
    ServiceEntry {
        enabled: true,
        scopes: vec![
            "https://www.googleapis.com/auth/gmail.modify".into(),
            "https://www.googleapis.com/auth/gmail.send".into(),
        ],
        profile: default_gmail_profile_str(),
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RateLimitConfig {
    #[serde(default = "default_gmail_rate")]
    pub(crate) gmail: RateEntry,
    #[serde(default = "default_calendar_rate")]
    pub(crate) calendar: RateEntry,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RateEntry {
    pub(crate) requests_per_second: u32,
    pub(crate) burst: u32,
    /// Per-GCP-project daily quota cap in units. Gmail's documented limit
    /// is 1,200,000 units/day per project. Set to `0` to disable the
    /// per-project bucket (per-account bucket still applies). See #30.
    #[serde(default = "default_per_project_daily_units")]
    pub(crate) per_project_daily_units: u64,
}

impl Default for RateEntry {
    fn default() -> Self {
        Self {
            requests_per_second: 5,
            burst: 20,
            per_project_daily_units: default_per_project_daily_units(),
        }
    }
}

const fn default_per_project_daily_units() -> u64 {
    crate::project_quota::GMAIL_DEFAULT_PROJECT_DAILY_UNITS
}

fn default_gmail_rate() -> RateEntry {
    RateEntry::default()
}

fn default_calendar_rate() -> RateEntry {
    RateEntry::default()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpConfig {
    #[serde(default = "default_bind")]
    pub(crate) bind: String,
    #[serde(default = "default_idle_timeout")]
    pub(crate) session_idle_timeout_secs: u64,
    #[serde(default = "default_max_sessions")]
    pub(crate) max_concurrent_sessions: u32,
    #[serde(default = "default_require_loopback_or_tls")]
    pub(crate) require_loopback_or_tls: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            session_idle_timeout_secs: default_idle_timeout(),
            max_concurrent_sessions: default_max_sessions(),
            require_loopback_or_tls: default_require_loopback_or_tls(),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1:8765".into()
}

const fn default_idle_timeout() -> u64 {
    3600
}

const fn default_max_sessions() -> u32 {
    50
}

const fn default_require_loopback_or_tls() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetryConfig {
    #[serde(default = "default_max_5xx")]
    pub(crate) max_attempts_5xx: u32,
    #[serde(default = "default_max_429")]
    pub(crate) max_attempts_429: u32,
    #[serde(default = "default_max_network")]
    pub(crate) max_attempts_network: u32,
    #[serde(default = "default_backoff_base")]
    pub(crate) backoff_base_ms: u64,
    #[serde(default = "default_backoff_cap")]
    pub(crate) backoff_cap_ms: u64,
    #[serde(default = "default_max_total")]
    pub(crate) max_total_duration_seconds: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts_5xx: default_max_5xx(),
            max_attempts_429: default_max_429(),
            max_attempts_network: default_max_network(),
            backoff_base_ms: default_backoff_base(),
            backoff_cap_ms: default_backoff_cap(),
            max_total_duration_seconds: default_max_total(),
        }
    }
}

const fn default_max_5xx() -> u32 {
    3
}

const fn default_max_429() -> u32 {
    5
}

const fn default_max_network() -> u32 {
    3
}

const fn default_backoff_base() -> u64 {
    100
}

const fn default_backoff_cap() -> u64 {
    5000
}

const fn default_max_total() -> u64 {
    30
}

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
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // ── tilde expansion ──────────────────────────────────────────────────────

    #[test]
    fn expand_tilde_home() {
        let p = expand_tilde("~/foo").expect("ok");
        assert!(
            !p.to_string_lossy().starts_with('~'),
            "tilde should be expanded, got: {}",
            p.display()
        );
        assert!(
            p.to_string_lossy().ends_with("/foo"),
            "got: {}",
            p.display()
        );
    }

    #[test]
    fn expand_tilde_absolute_unchanged() {
        let p = expand_tilde("/etc/foo").expect("ok");
        assert_eq!(p, PathBuf::from("/etc/foo"));
    }

    #[test]
    fn expand_tilde_rejects_other_user() {
        let err = expand_tilde("~root/foo").expect_err("should reject");
        assert!(err.contains("~user/"), "got: {err}");
    }

    #[test]
    fn expand_tilde_bare() {
        let p = expand_tilde("~").expect("ok");
        assert!(
            !p.to_string_lossy().starts_with('~'),
            "got: {}",
            p.display()
        );
    }

    // ── accounts.toml ────────────────────────────────────────────────────────

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

    // ── config.toml ──────────────────────────────────────────────────────────

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
}
