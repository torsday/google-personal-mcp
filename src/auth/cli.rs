//! `auth` subcommand surface — adds an account, lists known accounts, switches
//! the default. The v0.2 CLI shape from [ADR-0002].

use std::path::Path;

use clap::Subcommand;

use crate::auth::credentials::Credentials;
use crate::auth::pkce_flow::{run_auth_add, AuthFlowInputs};
use crate::auth::tokens::TokenState;
use crate::config::{accounts_path, AccountEntry, Accounts, Config};
use crate::error::Error;

/// Subcommands under `google-personal-mcp auth`. `auth grant`, `auth refresh`,
/// and `auth remove` are tracked separately in issue #27.
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
}

impl AuthCommand {
    pub(crate) fn run(self, config_dir: &Path) -> Result<(), Error> {
        match self {
            Self::Add { alias } => run_add(&alias, config_dir),
            Self::List => run_list(config_dir),
            Self::SetDefault { alias } => run_set_default(&alias, config_dir),
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

// Re-export for tests below — internal helpers are module-private.
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
}
