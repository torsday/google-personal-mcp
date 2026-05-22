#![allow(dead_code)]

pub mod audit;
pub mod auth;
pub mod cache;
pub mod config;
pub mod error;
pub mod gmail;
pub mod healthz;
pub mod http;
pub mod observability;
pub mod perm_check;
pub mod project_quota;
pub mod rate_limit;
pub mod server;
pub mod tools;
pub mod transport;

// ── Bench-only public wrappers ───────────────────────────────────────────────
//
// Thin pub fns that wrap internal `pub(crate)` items, avoiding the
// "promote everything to `pub`" cascade that comes from exposing the
// internal types directly. Returns a sealed shape (bool or unit) so
// callers can't inspect internals — keeping the actual surface area
// closed even as benches grow. Used by `benches/*.rs` only.

/// Run the MIME parser on `raw_rfc822` and return `true` on success.
/// Bench-only; the internal `gmail::mime::parse_message` stays
/// `pub(crate)`.
#[doc(hidden)]
#[must_use]
pub fn bench_parse_mime(raw_rfc822: &[u8]) -> bool {
    gmail::mime::parse_message(raw_rfc822).is_ok()
}

/// Sealed bench fixture wrapping a fully-wired `GmailClient` pointing at
/// a caller-supplied `base_url` (typically a wiremock server). Used by
/// `benches/tool_latency.rs` to amortize construction cost across the
/// bench's `iter` calls — per-iteration timing reflects tool dispatch,
/// not setup. Internal types stay `pub(crate)`; this is the only
/// bench-visible construction path.
#[doc(hidden)]
pub mod bench_handle {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};

    use crate::auth::tokens::{ReqwestRefreshTransport, TokenManager, TokenState};
    use crate::gmail::client::GmailClient;
    use crate::gmail::service::GmailService;
    use crate::tools::{get_thread, list_labels, search_threads};

    pub struct BenchHandle {
        gmail: Arc<GmailService<ReqwestRefreshTransport>>,
        account: String,
    }

    impl BenchHandle {
        /// Build a handle targeted at `base_url`. Injects a synthetic
        /// `TokenState` whose `expires_at` is one hour out so the bench
        /// loop never triggers a refresh round-trip.
        #[must_use]
        pub fn new(base_url: &str, account: &str) -> Self {
            let state = TokenState {
                access_token: "BENCH".into(),
                refresh_token: "BENCH-REFRESH".into(),
                expires_at: Utc::now() + ChronoDuration::hours(1),
                scopes: vec!["https://www.googleapis.com/auth/gmail.modify".into()],
                client_id: "111-bench.apps.googleusercontent.com".into(),
                client_secret: "BENCH-SECRET".into(),
                failed_until: None,
                consecutive_failures: 0,
                last_refresh_at: None,
            };
            let states = HashMap::from([(account.to_owned(), state)]);
            let tokens_dir = std::env::temp_dir().join(format!("gpm-bench-{}", std::process::id()));
            std::fs::create_dir_all(&tokens_dir).ok();
            let tokens = Arc::new(TokenManager::new(
                states,
                ReqwestRefreshTransport::new(reqwest::Client::new()),
                format!("{base_url}/token"),
                tokens_dir,
            ));
            let client = Arc::new(GmailClient::new(base_url, tokens, reqwest::Client::new()));
            let gmail = Arc::new(GmailService::new(client, None));
            Self {
                gmail,
                account: account.to_owned(),
            }
        }

        /// Run `list_labels(account)` end-to-end.
        pub async fn list_labels(&self) -> bool {
            list_labels::list_labels(&self.gmail, &self.account)
                .await
                .is_ok()
        }

        /// Run `get_thread(account, thread_id)` end-to-end.
        pub async fn get_thread(&self, thread_id: &str) -> bool {
            get_thread::get_thread(&self.gmail, &self.account, thread_id)
                .await
                .is_ok()
        }

        /// Run `search_threads({account, query, max_results=10})`
        /// end-to-end. `max_results=10` keeps per-iteration fan-out cost
        /// bounded.
        pub async fn search_threads(&self, query: &str) -> bool {
            search_threads::search_threads(
                Arc::clone(&self.gmail),
                search_threads::SearchThreadsInput {
                    account: self.account.clone(),
                    query: query.to_owned(),
                    max_results: 10,
                    page_token: None,
                },
            )
            .await
            .is_ok()
        }
    }
}

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::audit::AuditWriter;
use crate::auth::cli::AuthCommand;
use crate::auth::secrets::SecretStore;
use crate::auth::tokens::{ReqwestRefreshTransport, TokenManager, TokenState};
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::service::GmailService;
use crate::server::{run_stdio, GoogleServer};

/// Top-level CLI surface. `serve` is the default; `auth` covers account setup.
#[derive(Parser, Debug)]
#[command(
    name = "google-personal-mcp",
    about = "Personal Google-services MCP daemon",
    long_about = None,
    version,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP daemon over stdio (default if no subcommand given).
    Serve,
    /// OAuth account management.
    Auth {
        #[command(subcommand)]
        sub: AuthCommand,
    },
}

pub fn main_entry() -> ExitCode {
    observability::init();

    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => run_serve_blocking(),
        Command::Auth { sub } => sub.run(&config::config_dir()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = ?e, "command failed");
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Validate startup posture per ADRs 0006 and 0017, build the `GoogleServer`,
/// and drive the rmcp stdio runtime to EOF on a dedicated tokio runtime.
///
/// Returns `Ok(())` on clean client disconnect; surfaces structured errors
/// for startup-posture failures (`InsecurePermissions`, `Config`) and runtime
/// faults (`Internal`).
fn run_serve_blocking() -> Result<(), Error> {
    let dir = config::config_dir();
    perm_check::check(&perm_check::default_subjects(&dir))?;
    let loaded_accounts = config::Accounts::load(&config::accounts_path(&dir))?;
    let cfg = config::Config::load(&config::config_path(&dir))?;

    // Warn when the HTTP bind address is not loopback. Auth tokens flow over
    // this connection on every tool call; non-loopback without TLS termination
    // in front is a credential-exposure risk (ADR-0003 §Risks). The warning is
    // always emitted regardless of RUST_LOG so it surfaces in syslog / systemd
    // journal even when debug logging is off.
    if !config::is_loopback_bind(&cfg.http.bind) {
        tracing::warn!(
            bind = %cfg.http.bind,
            "HTTP transport bound to a non-loopback address without TLS — \
             auth tokens are sent in cleartext on every tool call; \
             place nginx (or another TLS-terminating reverse proxy) in front \
             of this listener; see docs/adr/0003-transport-stdio-and-streamable-http.md"
        );
    }

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(Error::Network)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Internal {
            context: "tokio runtime build".to_owned(),
            source: anyhow::Error::new(e),
        })?;

    // Load each registered account's TokenState from the configured backend
    // (file or Keychain). Missing or malformed tokens are logged at WARN and
    // skipped — the operator runs `auth grant` to repair. Without this load,
    // every Gmail tool call from a freshly-restarted daemon hits
    // `Error::AccountNotFound` because `TokenManager`'s registry would be
    // empty. See #96.
    let tokens_dir = dir.join("tokens");
    let secret_store = auth::secrets::build(cfg.secrets.backend, tokens_dir.clone());
    let token_states = runtime.block_on(load_token_states(
        secret_store.as_ref(),
        &loaded_accounts.accounts,
        &cfg,
    ));

    let tokens = Arc::new(TokenManager::new(
        token_states,
        ReqwestRefreshTransport::new(http_client.clone()),
        "https://oauth2.googleapis.com/token",
        tokens_dir,
    ));
    let gmail_base = "https://gmail.googleapis.com/gmail/v1";
    let gmail_client = Arc::new(GmailClient::new(gmail_base, tokens.clone(), http_client));
    // Phase 0 (#149) of the cache plan: the GmailService wrapper is the seam
    // future cache reads route through. `cache = None` here until Phase 2
    // (#150) wires the Cache constructor into this same path.
    let gmail = Arc::new(GmailService::new(gmail_client, None));

    let accounts = Arc::new(loaded_accounts.accounts);
    let audit = AuditWriter::new(&dir, cfg.audit.rotate.clone());

    // Liveness `/healthz` listener per ADR-0008 (#70). Only spawned when
    // the operator opted in by including `[metrics]` in config.toml. Bind
    // happens on the runtime (eager, so a port collision fails the
    // daemon at startup instead of silently a few seconds later); accept
    // loop lives in a detached task and dies with the runtime.
    if let Some(metrics_cfg) = cfg.metrics.as_ref() {
        let bind = metrics_cfg.bind.clone();
        let state = Arc::new(healthz::HealthState::new(accounts.len()));
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(&bind))
            .map_err(|e| Error::Config {
                path: bind.clone(),
                message: format!("metrics listener could not bind: {e}"),
            })?;
        runtime.spawn(async move {
            if let Err(err) = healthz::run(listener, state).await {
                tracing::error!(error = %err, "healthz listener stopped");
            }
        });
    }

    // Resolve audit verbosity from config and warn when verbose mode is on.
    // Verbose mode logs full query strings, recipient addresses, and body
    // previews — the audit log becomes sensitive (email-metadata equivalent).
    // The WARN is always emitted so it appears in the system journal even
    // when RUST_LOG is at INFO or higher. See ADR-0011 §Configuration.
    let verbosity = if cfg.audit.verbose {
        tracing::warn!(
            "audit verbose mode enabled — the audit log will contain full \
             query strings, recipient addresses, and email body previews; \
             treat the audit log file with the same access controls as token files; \
             see SECURITY.md and docs/adr/0011-audit-log.md"
        );
        audit::Verbosity::Verbose
    } else {
        audit::Verbosity::Redacted
    };

    let server = GoogleServer::new(accounts, tokens, gmail, audit, verbosity);

    runtime.block_on(run_stdio(server))
}

/// Read each registered account's persisted [`TokenState`] from `store`. Emits
/// scope-mismatch warnings via [`config::Config::warn_scope_mismatch`]. Returns
/// the populated registry that [`TokenManager::new`] will index by alias.
///
/// Failure modes are non-fatal: a missing or unreadable token logs WARN and
/// the account is excluded from the registry; the operator re-runs
/// `auth grant`. This matches the existing "auth tooling is interactive;
/// serve is best-effort for what's already on disk" split.
async fn load_token_states(
    store: &dyn SecretStore,
    accounts: &[config::AccountEntry],
    cfg: &config::Config,
) -> HashMap<String, TokenState> {
    let mut states = HashMap::with_capacity(accounts.len());
    for entry in accounts {
        match store.read_token(&entry.alias).await {
            Ok(Some(state)) => {
                cfg.warn_scope_mismatch(&entry.alias, &state.scopes);
                states.insert(entry.alias.clone(), state);
            }
            Ok(None) => {
                tracing::warn!(
                    account = %entry.alias,
                    "account in accounts.toml has no token on disk; \
                     run `google-personal-mcp auth grant --alias {}` to authorize",
                    entry.alias,
                );
            }
            Err(e) => {
                tracing::warn!(
                    account = %entry.alias,
                    error = ?e,
                    "failed to load token; account will be unavailable until repaired"
                );
            }
        }
    }
    states
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    use crate::auth::secrets::file::FileSecretStore;
    use crate::config::AccountEntry;

    fn fixture_state(alias: &str) -> TokenState {
        TokenState {
            access_token: format!("at-{alias}"),
            refresh_token: format!("rt-{alias}"),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            scopes: vec!["https://mail.google.com/".to_owned()],
            client_id: format!("client-{alias}"),
            client_secret: format!("secret-{alias}"),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        }
    }

    fn account(alias: &str, is_default: bool) -> AccountEntry {
        AccountEntry {
            alias: alias.into(),
            email: format!("{alias}@example.com"),
            default: is_default,
        }
    }

    #[tokio::test]
    async fn load_token_states_populates_from_disk() {
        // Without #96's fix, every Gmail tool fails with AccountNotFound on a
        // freshly-started daemon because TokenManager's registry is empty.
        let temp = TempDir::new().unwrap();
        let tokens_dir = temp.path().to_path_buf();
        let store = FileSecretStore::new(tokens_dir);
        store
            .write_token("personal", &fixture_state("personal"))
            .await
            .unwrap();
        store
            .write_token("work", &fixture_state("work"))
            .await
            .unwrap();

        let accounts = vec![account("personal", true), account("work", false)];
        let cfg = config::Config::default();

        let states = load_token_states(&store, &accounts, &cfg).await;

        assert_eq!(states.len(), 2);
        assert!(states.contains_key("personal"));
        assert!(states.contains_key("work"));
        assert_eq!(states["personal"].access_token, "at-personal");
        assert_eq!(states["work"].refresh_token, "rt-work");
    }

    #[tokio::test]
    async fn load_token_states_skips_accounts_without_tokens() {
        // accounts.toml may list an account whose `auth grant` hasn't completed.
        // Skip it with a WARN; don't block startup.
        let temp = TempDir::new().unwrap();
        let tokens_dir = temp.path().to_path_buf();
        let store = FileSecretStore::new(tokens_dir);
        store
            .write_token("have-token", &fixture_state("have-token"))
            .await
            .unwrap();

        let accounts = vec![account("have-token", true), account("no-token", false)];
        let cfg = config::Config::default();

        let states = load_token_states(&store, &accounts, &cfg).await;

        assert_eq!(states.len(), 1);
        assert!(states.contains_key("have-token"));
        assert!(!states.contains_key("no-token"));
    }

    #[tokio::test]
    async fn load_token_states_empty_registry_returns_empty_map() {
        // Fresh install — no accounts yet. Must not panic, must not warn.
        let temp = TempDir::new().unwrap();
        let store = FileSecretStore::new(temp.path().to_path_buf());
        let cfg = config::Config::default();

        let states = load_token_states(&store, &[], &cfg).await;
        assert!(states.is_empty());
    }

    #[tokio::test]
    async fn load_token_states_preserves_token_contents_exactly() {
        // Regression guard: the load path must round-trip every field, not
        // just keys. A subtle bug here would let tools talk to Gmail but with
        // wrong scopes or stale refresh state.
        let temp = TempDir::new().unwrap();
        let store = FileSecretStore::new(temp.path().to_path_buf());
        let original = fixture_state("alpha");
        store.write_token("alpha", &original).await.unwrap();

        let states = load_token_states(
            &store,
            &[account("alpha", true)],
            &config::Config::default(),
        )
        .await;

        let loaded = states.get("alpha").expect("token loaded");
        assert_eq!(loaded.access_token, original.access_token);
        assert_eq!(loaded.refresh_token, original.refresh_token);
        assert_eq!(loaded.scopes, original.scopes);
        assert_eq!(loaded.client_id, original.client_id);
        assert_eq!(loaded.client_secret, original.client_secret);
    }
}
