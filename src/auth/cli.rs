//! `auth` subcommand surface — adds an account, lists known accounts, switches
//! the default, and performs incremental scope grant / force-refresh / removal.
//! CLI shape from [ADR-0002] and [ADR-0004].

use std::path::Path;

use clap::Subcommand;

use crate::auth::credentials::Credentials;
use crate::auth::pkce_flow::{run_auth_add, run_auth_grant, AuthFlowInputs};
use crate::auth::tokens::TokenState;
use crate::config::{accounts_path, AccountEntry, Accounts, Config};
use crate::error::Error;

/// Subcommands under `google-personal-mcp auth`.
#[derive(Debug, Subcommand)]
pub(crate) enum AuthCommand {
    /// Run the OAuth PKCE flow and store the resulting token under `<alias>`.
    Add {
        /// Local alias for the account (used in `account: ...` MCP arguments).
        #[arg(long)]
        alias: String,
    },
    /// Print the current account registry — aliases, emails, default flag.
    List,
    /// Mark `alias` as the default account.
    SetDefault {
        /// Alias to make default.
        alias: String,
    },
    /// Incrementally grant additional OAuth scopes to an existing account.
    ///
    /// Runs the PKCE flow with `include_granted_scopes=true` so the consent
    /// screen shows only the *delta* between what is already granted and what
    /// is now requested. The union of existing + configured + `--scope` args
    /// becomes the new scope set.
    Grant {
        /// Alias of the account to upgrade.
        alias: String,
        /// Additional scope URIs to request (may be given multiple times).
        #[arg(long = "scope", value_name = "URL")]
        extra_scopes: Vec<String>,
    },
    /// Force-refresh the access token for an account.
    ///
    /// Calls Google's token endpoint with the stored `refresh_token` and
    /// writes the new access token atomically. Use this as a manual recovery
    /// path after a password change or token revocation (`invalid_grant`).
    Refresh {
        /// Alias of the account to refresh.
        alias: String,
    },
    /// Remove an account from the registry and delete its token file.
    ///
    /// Prompts for confirmation unless `--yes` is given. Pass `--revoke` to
    /// also POST to Google's `oauth2/revoke` endpoint so the token is
    /// invalidated server-side.
    Remove {
        /// Alias of the account to remove.
        alias: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Also revoke the token at Google's OAuth endpoint.
        #[arg(long)]
        revoke: bool,
    },
}

impl AuthCommand {
    pub(crate) fn run(self, config_dir: &Path) -> Result<(), Error> {
        match self {
            Self::Add { alias } => run_add(&alias, config_dir),
            Self::List => run_list(config_dir),
            Self::SetDefault { alias } => run_set_default(&alias, config_dir),
            Self::Grant {
                alias,
                extra_scopes,
            } => run_grant(&alias, &extra_scopes, config_dir),
            Self::Refresh { alias } => run_refresh(&alias, config_dir),
            Self::Remove { alias, yes, revoke } => run_remove(&alias, yes, revoke, config_dir),
        }
    }
}

// ── auth add ────────────────────────────────────────────────────────────────

fn run_add(alias: &str, config_dir: &Path) -> Result<(), Error> {
    validate_alias(alias)?;

    let config = Config::load(&crate::config::config_path(config_dir))?;
    let creds = Credentials::load(&config.google.credentials_path)?;
    let gmail = &config.services.gmail;
    if !gmail.enabled {
        return Err(Error::Config {
            path: crate::config::config_path(config_dir).display().to_string(),
            message: "services.gmail is disabled — enable it before running `auth add`".into(),
        });
    }
    if gmail.scopes.is_empty() {
        return Err(Error::Config {
            path: crate::config::config_path(config_dir).display().to_string(),
            message: "services.gmail.scopes is empty — at least one scope is required".into(),
        });
    }

    let inputs = AuthFlowInputs {
        credentials: &creds,
        scopes: &gmail.scopes,
        redirect_port: config.google.oauth.redirect_port,
    };
    let out = run_auth_add(&inputs)?;

    let tokens_dir = config_dir.join("tokens");
    std::fs::create_dir_all(&tokens_dir).map_err(Error::Io)?;
    write_token_file(&tokens_dir, alias, &out.token)?;

    let accounts_file = accounts_path(config_dir);
    let mut accounts = Accounts::load(&accounts_file)?;
    upsert_account(&mut accounts, alias, &out.email);
    accounts.save(&accounts_file)?;

    eprintln!("\nSaved account `{alias}` ({}).", out.email);
    Ok(())
}

fn validate_alias(alias: &str) -> Result<(), Error> {
    if alias.is_empty() {
        return Err(Error::InvalidArgument {
            field: "alias".into(),
            detail: "must not be empty".into(),
        });
    }
    for ch in alias.chars() {
        if !ch.is_ascii_alphanumeric() && !matches!(ch, '-' | '_') {
            return Err(Error::InvalidArgument {
                field: "alias".into(),
                detail: format!("invalid char {ch:?}; allowed: [A-Za-z0-9_-]"),
            });
        }
    }
    Ok(())
}

fn upsert_account(accounts: &mut Accounts, alias: &str, email: &str) {
    // Preserve `default = true` on existing entry; otherwise mark default
    // when this is the first account.
    if let Some(existing) = accounts.accounts.iter_mut().find(|a| a.alias == alias) {
        email.clone_into(&mut existing.email);
        return;
    }
    let is_first = accounts.accounts.is_empty();
    accounts.accounts.push(AccountEntry {
        alias: alias.to_owned(),
        email: email.to_owned(),
        default: is_first,
    });
}

fn write_token_file(tokens_dir: &Path, alias: &str, state: &TokenState) -> Result<(), Error> {
    let final_path = tokens_dir.join(format!("{alias}.json"));
    let tmp_path = tokens_dir.join(format!(".{alias}.json.tmp"));
    let body = serde_json::to_string_pretty(state).map_err(|e| Error::Parse {
        context: "serialize TokenState".into(),
        source: e,
    })?;
    std::fs::write(&tmp_path, body.as_bytes()).map_err(Error::Io)?;
    set_mode_0600(&tmp_path)?;
    std::fs::rename(&tmp_path, &final_path).map_err(Error::Io)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).map_err(Error::Io)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(Error::Io)
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> Result<(), Error> {
    Ok(())
}

// ── auth list ───────────────────────────────────────────────────────────────

fn run_list(config_dir: &Path) -> Result<(), Error> {
    let accounts = Accounts::load(&accounts_path(config_dir))?;
    if accounts.accounts.is_empty() {
        println!(
            "no accounts configured. Run `google-personal-mcp auth add --alias <name>` to add one."
        );
        return Ok(());
    }
    let alias_width = accounts
        .accounts
        .iter()
        .map(|a| a.alias.len())
        .max()
        .unwrap_or(0)
        .max(5);
    println!("{:<alias_width$}  {:<40}  default", "alias", "email");
    println!("{:-<alias_width$}  {:-<40}  -------", "", "");
    for a in &accounts.accounts {
        println!(
            "{:<alias_width$}  {:<40}  {}",
            a.alias,
            a.email,
            if a.default { "yes" } else { "no" }
        );
    }
    Ok(())
}

// ── auth set-default ────────────────────────────────────────────────────────

fn run_set_default(alias: &str, config_dir: &Path) -> Result<(), Error> {
    let accounts_file = accounts_path(config_dir);
    let mut accounts = Accounts::load(&accounts_file)?;
    if !accounts.accounts.iter().any(|a| a.alias == alias) {
        return Err(Error::AccountNotFound {
            account: alias.to_owned(),
        });
    }
    for a in &mut accounts.accounts {
        a.default = a.alias == alias;
    }
    accounts.save(&accounts_file)?;
    eprintln!("default account is now `{alias}`.");
    Ok(())
}

// ── auth grant ──────────────────────────────────────────────────────────────

fn run_grant(alias: &str, extra_scopes: &[String], config_dir: &Path) -> Result<(), Error> {
    validate_alias(alias)?;

    // Load existing token to learn what scopes were already granted.
    let tokens_dir = config_dir.join("tokens");
    let token_path = tokens_dir.join(format!("{alias}.json"));
    let existing: TokenState = {
        let body = std::fs::read_to_string(&token_path).map_err(|_| Error::AccountNotFound {
            account: alias.to_owned(),
        })?;
        serde_json::from_str(&body).map_err(|e| Error::Parse {
            context: "read existing token".into(),
            source: e,
        })?
    };

    let config = Config::load(&crate::config::config_path(config_dir))?;
    let creds = Credentials::load(&config.google.credentials_path)?;
    let gmail = &config.services.gmail;

    // Scope union: existing granted ∪ configured ∪ explicit --scope args.
    let mut scopes: Vec<String> = existing.scopes;
    for s in &gmail.scopes {
        if !scopes.contains(s) {
            scopes.push(s.clone());
        }
    }
    for s in extra_scopes {
        if !scopes.contains(s) {
            scopes.push(s.clone());
        }
    }

    let inputs = AuthFlowInputs {
        credentials: &creds,
        scopes: &scopes,
        redirect_port: config.google.oauth.redirect_port,
    };
    let out = run_auth_grant(&inputs)?;

    write_token_file(&tokens_dir, alias, &out.token)?;

    // Update email in accounts.toml in case it changed (alias-swap scenario).
    let accounts_file = accounts_path(config_dir);
    let mut accounts = Accounts::load(&accounts_file)?;
    upsert_account(&mut accounts, alias, &out.email);
    accounts.save(&accounts_file)?;

    eprintln!(
        "\nScopes for `{alias}` upgraded. Granted: {}",
        scopes.join(", ")
    );
    Ok(())
}

// ── auth refresh ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

fn run_refresh(alias: &str, config_dir: &Path) -> Result<(), Error> {
    validate_alias(alias)?;

    let tokens_dir = config_dir.join("tokens");
    let token_path = tokens_dir.join(format!("{alias}.json"));
    let existing: TokenState = {
        let body = std::fs::read_to_string(&token_path).map_err(|_| Error::AccountNotFound {
            account: alias.to_owned(),
        })?;
        serde_json::from_str(&body).map_err(|e| Error::Parse {
            context: "read existing token".into(),
            source: e,
        })?
    };

    // Build and POST the refresh request using reqwest blocking.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", &existing.refresh_token)
        .append_pair("client_id", &existing.client_id)
        .append_pair("client_secret", &existing.client_secret)
        .finish();

    let client = blocking_reqwest_client()?;
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(Error::Network)?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(Error::Network)?;

    if !(200..300).contains(&status) {
        if text.contains("invalid_grant") {
            return Err(Error::AuthRequired {
                account: alias.to_owned(),
                reason: format!(
                    "refresh_token rejected (invalid_grant) — run \
                     `auth add --alias {alias}` to re-authorize, or \
                     `auth remove --alias {alias} --revoke` then re-add.\nServer: {text}"
                ),
            });
        }
        return Err(Error::upstream("google-oauth", status, text));
    }

    let parsed: RefreshResponse = serde_json::from_str(&text).map_err(|e| Error::Parse {
        context: "OAuth refresh response".into(),
        source: e,
    })?;

    let new_state = TokenState {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token.unwrap_or(existing.refresh_token),
        expires_at: chrono::Utc::now() + chrono::Duration::seconds(parsed.expires_in),
        scopes: existing.scopes,
        client_id: existing.client_id,
        client_secret: existing.client_secret,
        failed_until: None,
        consecutive_failures: 0,
    };
    write_token_file(&tokens_dir, alias, &new_state)?;
    eprintln!("Access token for `{alias}` refreshed successfully.");
    Ok(())
}

// ── auth remove ──────────────────────────────────────────────────────────────

fn run_remove(alias: &str, yes: bool, revoke: bool, config_dir: &Path) -> Result<(), Error> {
    validate_alias(alias)?;

    let accounts_file = accounts_path(config_dir);
    let mut accounts = Accounts::load(&accounts_file)?;
    if !accounts.accounts.iter().any(|a| a.alias == alias) {
        return Err(Error::AccountNotFound {
            account: alias.to_owned(),
        });
    }

    if !yes {
        eprint!("Remove account `{alias}` and delete its token? [y/N] ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(Error::Io)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    // Optionally revoke the token at Google before deleting it.
    let token_path = config_dir.join("tokens").join(format!("{alias}.json"));
    if revoke {
        if let Ok(body) = std::fs::read_to_string(&token_path) {
            if let Ok(state) = serde_json::from_str::<TokenState>(&body) {
                // Try revoke with refresh_token; ignore errors (best-effort).
                let _ = revoke_token_at_google(&state.refresh_token, GOOGLE_REVOKE_URI);
            }
        }
    }

    // Delete the token file (ignore missing-file errors; it might never have
    // been written if `auth add` was aborted).
    if let Err(e) = std::fs::remove_file(&token_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(Error::Io(e));
        }
    }

    // Remove from accounts.toml.  If removed entry was default, promote the
    // first remaining entry to default so the file stays in a valid state.
    let was_default = accounts
        .accounts
        .iter()
        .find(|a| a.alias == alias)
        .is_some_and(|a| a.default);
    accounts.accounts.retain(|a| a.alias != alias);
    if was_default {
        if let Some(first) = accounts.accounts.first_mut() {
            first.default = true;
        }
    }
    accounts.save(&accounts_file)?;

    eprintln!("Account `{alias}` removed.");
    Ok(())
}

const GOOGLE_REVOKE_URI: &str = "https://oauth2.googleapis.com/revoke";

/// POST `token=<refresh_token>` to `revoke_uri` in the **request body**
/// (RFC 7009 §2.1). The token must never appear in the URL — URLs leak via
/// proxy logs, OS crash dumps, and process traces. `revoke_uri` is
/// parameterised so that tests can point at a local mock server.
/// Best-effort: errors are ignored by the caller.
fn revoke_token_at_google(refresh_token: &str, revoke_uri: &str) -> Result<(), Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", refresh_token)
        .finish();
    let client = blocking_reqwest_client()?;
    client
        .post(revoke_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(Error::Network)?;
    Ok(())
}

fn blocking_reqwest_client() -> Result<reqwest::blocking::Client, Error> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(Error::Network)
}

// ── scope union helper ────────────────────────────────────────────────────────

/// Compute the union of two scope lists. Order-preserving; duplicates removed.
/// Exported for unit tests.
pub(crate) fn scope_union(base: &[String], extra: &[String]) -> Vec<String> {
    let mut result = base.to_vec();
    for s in extra {
        if !result.contains(s) {
            result.push(s.clone());
        }
    }
    result
}

// ── Re-export for tests below — internal helpers are module-private. ─────────
#[cfg(test)]
fn upsert_account_test_only(accounts: &mut Accounts, alias: &str, email: &str) {
    upsert_account(accounts, alias, email);
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn fresh_accounts() -> Accounts {
        Accounts {
            accounts: Vec::new(),
        }
    }

    #[test]
    fn alias_validation_accepts_basic_chars() {
        assert!(validate_alias("work").is_ok());
        assert!(validate_alias("personal_2").is_ok());
        assert!(validate_alias("foo-bar").is_ok());
        assert!(validate_alias("ABC123").is_ok());
    }

    #[test]
    fn alias_validation_rejects_special_chars() {
        for bad in ["", "a b", "a.b", "/etc", "a/b", "x@y"] {
            let r = validate_alias(bad);
            assert!(
                matches!(r, Err(Error::InvalidArgument { .. })),
                "should reject {bad:?}: {r:?}"
            );
        }
    }

    #[test]
    fn upsert_first_account_marks_default() {
        let mut a = fresh_accounts();
        upsert_account_test_only(&mut a, "work", "x@example.com");
        assert_eq!(a.accounts.len(), 1);
        assert!(a.accounts[0].default);
    }

    #[test]
    fn upsert_second_account_does_not_steal_default() {
        let mut a = fresh_accounts();
        upsert_account_test_only(&mut a, "work", "x@example.com");
        upsert_account_test_only(&mut a, "personal", "y@example.com");
        assert_eq!(a.accounts.len(), 2);
        assert!(a.accounts[0].default, "first account stays default");
        assert!(!a.accounts[1].default);
    }

    #[test]
    fn upsert_existing_alias_updates_email_preserves_default() {
        let mut a = fresh_accounts();
        upsert_account_test_only(&mut a, "work", "old@example.com");
        upsert_account_test_only(&mut a, "work", "new@example.com");
        assert_eq!(a.accounts.len(), 1);
        assert_eq!(a.accounts[0].email, "new@example.com");
        assert!(a.accounts[0].default);
    }

    // Atomic write + 0600 mode test for the token file path.
    #[test]
    fn write_token_file_writes_atomically_with_0600() {
        let dir = unique_tmp_dir();
        let state = TokenState {
            access_token: "AAA".into(),
            refresh_token: "RRR".into(),
            expires_at: chrono::Utc::now(),
            scopes: vec!["s".into()],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
        };
        write_token_file(&dir, "work", &state).expect("write ok");
        let path = dir.join("work.json");
        let content = std::fs::read_to_string(&path).expect("readable");
        let parsed: TokenState = serde_json::from_str(&content).expect("valid json");
        assert_eq!(parsed.access_token, "AAA");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    fn unique_tmp_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gpm-cli-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    // ── scope_union ──────────────────────────────────────────────────────────

    #[test]
    fn scope_union_merges_without_duplicates() {
        let base = vec!["https://a".to_owned(), "https://b".to_owned()];
        let extra = vec!["https://b".to_owned(), "https://c".to_owned()];
        let result = scope_union(&base, &extra);
        assert_eq!(result, vec!["https://a", "https://b", "https://c"]);
    }

    #[test]
    fn scope_union_empty_extra_returns_base() {
        let base = vec!["https://a".to_owned()];
        let result = scope_union(&base, &[]);
        assert_eq!(result, base);
    }

    #[test]
    fn scope_union_empty_base_returns_extra() {
        let extra = vec!["https://a".to_owned()];
        let result = scope_union(&[], &extra);
        assert_eq!(result, extra);
    }

    #[test]
    fn scope_union_preserves_order() {
        let base = vec!["https://z".to_owned(), "https://a".to_owned()];
        let extra = vec!["https://m".to_owned()];
        let result = scope_union(&base, &extra);
        assert_eq!(result, vec!["https://z", "https://a", "https://m"]);
    }

    // ── run_remove (unit — no network) ───────────────────────────────────────

    #[test]
    fn remove_unknown_alias_returns_account_not_found() {
        let dir = unique_tmp_dir();
        // Write a minimal accounts.toml with one account.
        let accts_path = dir.join("accounts.toml");
        let accts = Accounts {
            accounts: vec![AccountEntry {
                alias: "work".into(),
                email: "w@example.com".into(),
                default: true,
            }],
        };
        accts.save(&accts_path).expect("save");
        let err = run_remove("ghost", true, false, &dir).unwrap_err();
        assert!(
            matches!(err, Error::AccountNotFound { .. }),
            "expected AccountNotFound, got {err:?}"
        );
    }

    #[test]
    fn remove_known_alias_deletes_from_accounts_and_promotes_next_default() {
        let dir = unique_tmp_dir();
        let accts_path = dir.join("accounts.toml");
        let accts = Accounts {
            accounts: vec![
                AccountEntry {
                    alias: "personal".into(),
                    email: "p@example.com".into(),
                    default: true,
                },
                AccountEntry {
                    alias: "work".into(),
                    email: "w@example.com".into(),
                    default: false,
                },
            ],
        };
        accts.save(&accts_path).expect("save");

        // No token file — should not error.
        run_remove("personal", true, false, &dir).expect("remove ok");

        let after = Accounts::load(&accts_path).expect("reload");
        assert_eq!(after.accounts.len(), 1);
        assert_eq!(after.accounts[0].alias, "work");
        assert!(
            after.accounts[0].default,
            "remaining account promoted to default"
        );
    }

    #[test]
    fn remove_deletes_token_file_when_present() {
        let dir = unique_tmp_dir();
        let accts_path = dir.join("accounts.toml");
        let accts = Accounts {
            accounts: vec![AccountEntry {
                alias: "work".into(),
                email: "w@example.com".into(),
                default: true,
            }],
        };
        accts.save(&accts_path).expect("save");

        // Create a fake token file.
        let tokens_dir = dir.join("tokens");
        std::fs::create_dir_all(&tokens_dir).expect("mkdir");
        let token_path = tokens_dir.join("work.json");
        std::fs::write(&token_path, b"{}").expect("write token");

        run_remove("work", true, false, &dir).expect("remove ok");

        assert!(!token_path.exists(), "token file should have been deleted");
    }

    // ── revoke_token_at_google (Layer 2 wiremock) ────────────────────────────

    /// Verifies that `revoke_token_at_google`:
    ///  (a) uses POST
    ///  (b) sends no query parameters in the URL
    ///  (c) puts the token in the request body as `application/x-www-form-urlencoded`
    ///
    /// These three properties are what RFC 7009 §2.1 requires and what the old
    /// URL-query-string implementation violated.
    #[tokio::test]
    async fn revoke_sends_token_in_body_not_url() {
        use wiremock::matchers::{body_string_contains, header, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(header("Content-Type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("token=fake-refresh-token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        // revoke_token_at_google uses reqwest::blocking which cannot run on a
        // tokio thread directly — spawn onto blocking pool.
        let result =
            tokio::task::spawn_blocking(move || revoke_token_at_google("fake-refresh-token", &uri))
                .await
                .expect("spawn_blocking");

        assert!(result.is_ok(), "revoke should succeed: {result:?}");
        // MockServer::verify() is called automatically on drop — the `.expect(1)`
        // assertion fires if the POST was never received.
    }

    /// Token must NOT appear in the URL path or query string.
    #[tokio::test]
    async fn revoke_url_has_no_query_params() {
        use wiremock::matchers::{method, path, query_param_is_missing};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(query_param_is_missing("token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        let _ =
            tokio::task::spawn_blocking(move || revoke_token_at_google("super-secret-token", &uri))
                .await
                .expect("spawn_blocking");
        // MockServer drop verifies the `.expect(1)` — if the token were in the
        // URL as `?token=...`, the `query_param_is_missing("token")` matcher
        // would fail to match and the expectation would not be met.
    }
}
