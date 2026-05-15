use anyhow::{anyhow, bail, Context, Result};
use oauth2::{
    basic::BasicClient, AuthUrl, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenUrl,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

const TOKEN_FILE: &str = "token.json";
const CONFIG_DIR: &str = "google-personal-mcp";
const REDIRECT_PORT: u16 = 8080;

/// Persisted token data
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct StoredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub client_id: String,
    pub client_secret: String,
}

fn config_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|p| p.join(CONFIG_DIR))
        .ok_or_else(|| {
            anyhow!(
                "could not determine the user config directory \
                 (HOME unset?); cannot locate {CONFIG_DIR} state"
            )
        })
}

fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(TOKEN_FILE))
}

fn credentials_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("credentials.json"))
}

/// Shape of Google's credentials.json (downloaded from Cloud Console)
#[derive(Debug, Deserialize)]
struct GoogleCredentials {
    installed: InstalledCredentials,
}

#[derive(Debug, Deserialize)]
struct InstalledCredentials {
    client_id: String,
    client_secret: String,
    auth_uri: String,
    token_uri: String,
}

/// Google's token endpoint response
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

pub(crate) async fn load_token() -> Result<StoredToken> {
    let path = token_path()?;
    if !path.exists() {
        bail!(
            "Not authenticated. Run `google-personal-mcp auth` first.\n\
             Ensure credentials.json is at: {}",
            credentials_path()?.display()
        );
    }
    let data = tokio::fs::read_to_string(&path).await?;
    serde_json::from_str(&data).context("Failed to parse token.json")
}

pub(crate) async fn run_auth_flow() -> Result<()> {
    let creds_path = credentials_path()?;
    if !creds_path.exists() {
        bail!(
            "credentials.json not found at {}\n\n\
             Setup steps:\n\
             1. Go to https://console.cloud.google.com/\n\
             2. Create/select a project and enable the Gmail API\n\
             3. Create OAuth 2.0 credentials → Desktop app\n\
             4. Download credentials.json and place it at the path above",
            creds_path.display()
        );
    }

    let raw = tokio::fs::read_to_string(&creds_path).await?;
    let creds: GoogleCredentials =
        serde_json::from_str(&raw).context("Failed to parse credentials.json")?;
    let ic = creds.installed;

    // Build client (oauth2 v5 builder API)
    let client = BasicClient::new(ClientId::new(ic.client_id.clone()))
        .set_client_secret(ClientSecret::new(ic.client_secret.clone()))
        .set_auth_uri(AuthUrl::new(ic.auth_uri)?)
        .set_token_uri(TokenUrl::new(ic.token_uri.clone())?)
        .set_redirect_uri(RedirectUrl::new(format!(
            "http://localhost:{REDIRECT_PORT}"
        ))?);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(
            "https://www.googleapis.com/auth/gmail.modify".to_owned(),
        ))
        .add_scope(Scope::new(
            "https://www.googleapis.com/auth/gmail.send".to_owned(),
        ))
        .set_pkce_challenge(pkce_challenge)
        .url();

    eprintln!("Opening browser for Gmail authorization...");
    eprintln!("If it doesn't open automatically, visit:\n{auth_url}");
    let _ = open::that(auth_url.to_string());

    let (code, state) = wait_for_redirect(REDIRECT_PORT).await?;

    if state != *csrf_token.secret() {
        bail!("CSRF token mismatch — aborting for security");
    }

    // Exchange code for token using reqwest directly (avoids oauth2 reqwest version conflict).
    let token = exchange_code(
        &ic.token_uri,
        &ic.client_id,
        &ic.client_secret,
        &code,
        pkce_verifier,
        &format!("http://localhost:{REDIRECT_PORT}"),
    )
    .await?;

    let stored = StoredToken {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        client_id: ic.client_id,
        client_secret: ic.client_secret,
    };

    let path = token_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("token path has no parent: {}", path.display()))?;
    tokio::fs::create_dir_all(parent).await?;
    tokio::fs::write(&path, serde_json::to_string_pretty(&stored)?).await?;

    eprintln!("✓ Token saved to {}", path.display());
    Ok(())
}

/// Exchange the authorization code for tokens via a direct reqwest POST.
///
/// Captures the response body BEFORE the status check (per ADR-0005) so
/// Google's actual error description (e.g. `invalid_grant`, `redirect_uri_mismatch`)
/// is preserved in the error chain.
async fn exchange_code(
    token_uri: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    pkce_verifier: PkceCodeVerifier,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", client_id)
        .append_pair("client_secret", client_secret)
        .append_pair("code_verifier", pkce_verifier.secret())
        .finish();

    let http = reqwest::Client::new();
    let resp = http
        .post(token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("Token endpoint request failed")?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .context("Failed to read token endpoint response body")?;

    if !status.is_success() {
        bail!("Token endpoint returned HTTP {status}: {body}");
    }

    serde_json::from_str(&body).context("Failed to parse token response")
}

/// One-shot HTTP listener to capture the `OAuth2` redirect.
async fn wait_for_redirect(port: u16) -> Result<(String, String)> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    eprintln!("Waiting for OAuth redirect on http://localhost:{port} ...");

    let (stream, _) = listener.accept().await?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    // Request line: GET /?code=...&state=... HTTP/1.1
    let path = request_line
        .split_whitespace()
        .nth(1)
        .context("Malformed HTTP request from browser")?;

    let url = url::Url::parse(&format!("http://localhost{path}"))?;
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();

    let code = params
        .get("code")
        .context("No 'code' in OAuth redirect")?
        .to_string();
    let state = params
        .get("state")
        .context("No 'state' in OAuth redirect")?
        .to_string();

    Ok((code, state))
}
