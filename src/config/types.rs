//! `config.toml` schema — the top-level [`Config`] struct and every nested
//! `[<section>]` it owns. Pure types + `Default` impls + `default_*` helpers.
//!
//! Load/validate logic lives in [`super::parse`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::auth::scopes::GmailProfile;
use crate::error::Error;

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
    /// `[metrics]` section per [ADR-0008 §`/metrics` endpoint](../docs/adr/0008-observability-and-deployment.md).
    ///
    /// `None` (the default) means **disabled** — no listener is bound, no
    /// background task spawned. Mere presence of `[metrics]` in
    /// `config.toml` flips it on, even with no fields set. That matches the
    /// ADR's "Disabled by default unless the section is present" rule and
    /// matches the v0.x scope (liveness `/healthz` only; Prometheus
    /// `/metrics` is v1.0 — see #70 / #75).
    #[serde(default)]
    pub(crate) metrics: Option<MetricsConfig>,
    /// `[audit]` section per [ADR-0011](../docs/adr/0011-audit-log.md).
    #[serde(default)]
    pub(crate) audit: AuditConfig,
    /// `[cache]` section per [ADR-0009](../docs/adr/0009-caching-with-sqlite-and-history-api.md).
    ///
    /// Default is `enabled = false` during Phases 1–6 of the cache rollout
    /// (see [docs/cache-implementation-plan.md](../docs/cache-implementation-plan.md));
    /// Phase 7 (v1.0) flips it to `true`. Maintainer dogfooding before then is
    /// the only intended `enabled = true` use case.
    #[serde(default)]
    pub(crate) cache: CacheConfig,
}

/// Controls when the audit-log file is rotated per
/// [ADR-0011](../docs/adr/0011-audit-log.md).
///
/// | Config value        | Filename pattern        |
/// |---------------------|-------------------------|
/// | `"monthly"` (default) | `audit-2026-04.log`   |
/// | `"weekly"`          | `audit-2026-W17.log`    |
/// | `"daily"`           | `audit-2026-04-25.log`  |
/// | `"size:<bytes>"`    | `audit-<seq>.log`       |
///
/// Rotation is **lazy**: the filename is computed from the current clock (or
/// current file size) at the moment of the first write in a new period.
/// No background task is required; the old file simply stops receiving writes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum RotateMode {
    /// Rotate once per calendar month (UTC). Default.
    #[default]
    Monthly,
    /// Rotate once per ISO week (UTC, Monday-start per ISO 8601).
    Weekly,
    /// Rotate once per calendar day (UTC).
    Daily,
    /// Rotate when the current file exceeds `n` bytes; sequential numbering.
    Size(u64),
}

impl std::fmt::Display for RotateMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Monthly => write!(f, "monthly"),
            Self::Weekly => write!(f, "weekly"),
            Self::Daily => write!(f, "daily"),
            Self::Size(n) => write!(f, "size:{n}"),
        }
    }
}

impl TryFrom<String> for RotateMode {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "monthly" => Ok(Self::Monthly),
            "weekly" => Ok(Self::Weekly),
            "daily" => Ok(Self::Daily),
            _ if s.starts_with("size:") => {
                let n_str = &s["size:".len()..];
                let n = n_str
                    .parse::<u64>()
                    .map_err(|_| format!("invalid size in `{s}`: expected `size:<bytes>`"))?;
                if n == 0 {
                    return Err(format!("`{s}`: size must be > 0"));
                }
                Ok(Self::Size(n))
            }
            _ => Err(format!(
                "unknown rotate value `{s}`; expected `monthly`, `weekly`, `daily`, or `size:<bytes>`"
            )),
        }
    }
}

impl From<RotateMode> for String {
    fn from(m: RotateMode) -> Self {
        m.to_string()
    }
}

impl<'de> Deserialize<'de> for RotateMode {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::try_from(s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for RotateMode {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_string())
    }
}

/// `[audit]` section — controls verbosity and rotation of the audit log per
/// [ADR-0011 §Configuration](../docs/adr/0011-audit-log.md).
///
/// `verbose = false` (default): attacker-controlled content (queries, subjects,
/// body previews, recipient addresses) is redacted in the audit log. Each tool
/// call is still recorded with its intent and per-item outcomes.
///
/// `verbose = true` (operator opt-in): full content is logged — query strings,
/// full recipient lists, body previews. The audit log file becomes sensitive
/// (equivalent to email metadata). A startup WARN is emitted to ensure the
/// operator's choice is always visible in the system journal.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditConfig {
    /// When `true`, per-tool audit entries include full content (queries,
    /// subjects, recipient addresses, body previews). Default `false`.
    #[serde(default)]
    pub(crate) verbose: bool,
    /// Controls when the audit-log file rotates. Default `"monthly"`.
    ///
    /// See [`RotateMode`] for the full list of accepted values and
    /// corresponding filename patterns.
    #[serde(default)]
    pub(crate) rotate: RotateMode,
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
        deserialize_with = "crate::config::deser_tilde_path"
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
    super::expand_tilde("~/.config/google-personal-mcp/credentials/google.json")
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

pub(super) fn default_services() -> ServicesConfig {
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

/// `[metrics]` section — internal listener for liveness `/healthz` (v0.x)
/// and the Prometheus `/metrics` endpoint (v1.0; #75). Bound to a
/// loopback address by default; never reachable through nginx per
/// [ADR-0008 §nginx termination](../docs/adr/0008-observability-and-deployment.md).
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MetricsConfig {
    #[serde(default = "default_metrics_bind")]
    pub(crate) bind: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            bind: default_metrics_bind(),
        }
    }
}

fn default_metrics_bind() -> String {
    "127.0.0.1:9100".into()
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

/// `[cache]` section per [ADR-0009 §Config additions](../docs/adr/0009-caching-with-sqlite-and-history-api.md).
///
/// `enabled` defaults to `true` as of v1.0 (ADR-0009 Phase 7).  All six
/// prerequisite phases (0-6) have shipped; the cache is production-ready.
/// Set `enabled = false` in your `config.toml` to disable the cache and pass
/// every read through directly to Gmail.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheConfig {
    /// Master switch. `true` (default as of v1.0 Phase 7) constructs the
    /// `SQLite` cache on startup. `false` bypasses the cache entirely —
    /// `Cache::new` is never called and no per-account `.db` files are
    /// created.
    #[serde(default = "default_cache_enabled")]
    pub(crate) enabled: bool,
    /// Directory holding per-account `<alias>.db` files. Created at mode
    /// `0700`; each DB at mode `0600` per [ADR-0017](../docs/adr/0017-secrets-at-rest.md).
    #[serde(
        default = "default_cache_dir",
        deserialize_with = "crate::config::deser_tilde_path"
    )]
    pub(crate) dir: PathBuf,
    /// TTL applied to `query_cache` rows (`list_threads` results). Default
    /// 300 s per ADR-0009.
    #[serde(default = "default_query_ttl")]
    pub(crate) query_ttl_seconds: u64,
    /// TTL applied to label-catalog rows. Default 3600 s per ADR-0009.
    /// (Phase 2 does not yet cache the label catalog; the knob is wired so
    /// Phase 3/4 can use it without a config change.)
    #[serde(default = "default_labels_ttl")]
    pub(crate) labels_ttl_seconds: u64,
    /// Background sync interval per ADR-0009. `0` disables the background
    /// task. (Phase 2 spawns no task; Phase 3 is the consumer.)
    #[serde(default = "default_bg_sync_interval")]
    pub(crate) background_sync_interval_seconds: u64,
    /// When `true`, the background sync runs a catch-up pass before serving
    /// a lookup. (Phase 2 ignores this; the field is wired for Phase 3.)
    #[serde(default = "default_sync_on_read")]
    pub(crate) sync_on_read: bool,
    /// Per-account soft size cap. When a `<account>.db` file exceeds this
    /// value, the next eviction tick deletes LRU rows and `VACUUM`s the
    /// file back below 90 % of the cap. Default 500 MiB per ADR-0009.
    #[serde(default = "default_max_size_bytes_per_account")]
    pub(crate) max_size_bytes_per_account: u64,
    /// Per-account eviction-task cadence in seconds. `0` disables the
    /// background eviction task. Default 300 s (5 min) per ADR-0009.
    #[serde(default = "default_eviction_interval")]
    pub(crate) eviction_interval_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_cache_enabled(),
            dir: default_cache_dir(),
            query_ttl_seconds: default_query_ttl(),
            labels_ttl_seconds: default_labels_ttl(),
            background_sync_interval_seconds: default_bg_sync_interval(),
            sync_on_read: default_sync_on_read(),
            max_size_bytes_per_account: default_max_size_bytes_per_account(),
            eviction_interval_seconds: default_eviction_interval(),
        }
    }
}

const fn default_cache_enabled() -> bool {
    true
}

fn default_cache_dir() -> PathBuf {
    super::expand_tilde("~/.config/google-personal-mcp/cache")
        .unwrap_or_else(|_| PathBuf::from("cache"))
}

const fn default_query_ttl() -> u64 {
    300
}

const fn default_labels_ttl() -> u64 {
    3600
}

const fn default_bg_sync_interval() -> u64 {
    60
}

const fn default_sync_on_read() -> bool {
    true
}

/// 500 MiB per ADR-0009 §"Config additions".
const fn default_max_size_bytes_per_account() -> u64 {
    524_288_000
}

/// 5 minutes per ADR-0009 §"TTLs and eviction".
const fn default_eviction_interval() -> u64 {
    300
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // ── RotateMode ────────────────────────────────────────────────────────────

    #[test]
    fn rotate_mode_default_is_monthly() {
        assert_eq!(RotateMode::default(), RotateMode::Monthly);
    }

    #[test]
    fn rotate_mode_parses_all_variants() {
        assert_eq!(
            RotateMode::try_from("monthly".to_owned()),
            Ok(RotateMode::Monthly)
        );
        assert_eq!(
            RotateMode::try_from("weekly".to_owned()),
            Ok(RotateMode::Weekly)
        );
        assert_eq!(
            RotateMode::try_from("daily".to_owned()),
            Ok(RotateMode::Daily)
        );
        assert_eq!(
            RotateMode::try_from("size:1048576".to_owned()),
            Ok(RotateMode::Size(1_048_576))
        );
        assert_eq!(
            RotateMode::try_from("size:1".to_owned()),
            Ok(RotateMode::Size(1))
        );
    }

    #[test]
    fn rotate_mode_rejects_unknown_values() {
        assert!(RotateMode::try_from("hourly".to_owned()).is_err());
        assert!(RotateMode::try_from(String::new()).is_err());
        assert!(RotateMode::try_from("size:abc".to_owned()).is_err());
        assert!(RotateMode::try_from("size:0".to_owned()).is_err());
        assert!(RotateMode::try_from("size:-1".to_owned()).is_err());
    }

    #[test]
    fn rotate_mode_roundtrips_display() {
        for mode in [
            RotateMode::Monthly,
            RotateMode::Weekly,
            RotateMode::Daily,
            RotateMode::Size(100),
        ] {
            let s: String = mode.clone().into();
            let reparsed = RotateMode::try_from(s.clone()).unwrap();
            assert_eq!(reparsed, mode, "roundtrip failed for `{s}`");
        }
    }
}
