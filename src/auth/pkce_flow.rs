//! OAuth 2.0 Authorization-Code-with-PKCE flow for installed apps.
//!
//! Implements the `auth add` command per
//! [ADR-0004](../../docs/adr/0004-oauth-token-refresh.md) §"PKCE flow" and
//! [ADR-0017](../../docs/adr/0017-secrets-at-rest.md) for the on-disk write.
//!
//! Flow:
//!
//! 1. Generate a PKCE verifier+challenge (`S256`) and a CSRF `state` token.
//! 2. Bind a one-shot listener on `127.0.0.1:<redirect_port>`.
//! 3. Open the consent URL in the operator's browser (falls back to printing
//!    the URL when no browser can be launched).
//! 4. Receive the redirect at the listener; verify the `state` parameter.
//! 5. Exchange the authorization code for tokens via direct `reqwest` POST —
//!    ADR-0004 forbids using `oauth2::Client::exchange_*` because of a
//!    version conflict between `oauth2` v5 and `reqwest` v0.13 (see prototype
//!    commit `377b558`).
//! 6. Hit Google's `userinfo` endpoint to retrieve the account email.
//! 7. Return a populated [`TokenState`] ready for atomic persistence.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};
use oauth2::{CsrfToken, PkceCodeChallenge, PkceCodeVerifier};
use serde::Deserialize;
use url::Url;

use crate::auth::credentials::Credentials;
use crate::auth::tokens::TokenState;
use crate::error::Error;

/// How long to wait for the operator to complete the browser flow before
/// giving up. Aligned with ADR-0006's auth-timeout default.
const LISTENER_TIMEOUT: StdDuration = StdDuration::from_mins(5);

/// User-facing HTML returned to the browser after the redirect lands.
/// Plain HTML — no scripts, no external resources. The window is the
/// operator's confirmation that auth completed.
const SUCCESS_HTML: &str = "<!doctype html><html><body>\
<h2>google-personal-mcp · authorization complete</h2>\
<p>You can close this tab and return to the terminal.</p>\
</body></html>";

const ERROR_HTML_PREFIX: &str = "<!doctype html><html><body>\
<h2>google-personal-mcp · authorization failed</h2><pre>";

/// All inputs needed to run the auth flow once. Pulled out so the function
/// signature stays narrow and tests can construct a fixture struct.
pub(crate) struct AuthFlowInputs<'a> {
    pub credentials: &'a Credentials,
    pub scopes: &'a [String],
    pub redirect_port: u16,
}

/// What the redirect listener captured.
#[derive(Debug)]
pub(crate) struct RedirectResult {
    pub code: String,
    pub state: String,
}

/// Result of a successful `auth add` flow.
pub(crate) struct AuthFlowOutput {
    pub email: String,
    pub token: TokenState,
}

/// Run the full PKCE flow synchronously. Blocks the calling thread on the
/// listener; intended to be called from a CLI subcommand. Returns when the
/// browser redirect arrives, tokens exchange, and userinfo resolves.
pub(crate) fn run_auth_add(inputs: &AuthFlowInputs<'_>) -> Result<AuthFlowOutput, Error> {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let csrf = CsrfToken::new_random();
    let redirect_uri = format!("http://127.0.0.1:{}", inputs.redirect_port);

    let listener = bind_listener(inputs.redirect_port)?;

    let auth_url = build_auth_url(
        &inputs.credentials.auth_uri,
        &inputs.credentials.client_id,
        &redirect_uri,
        inputs.scopes,
        challenge.as_str(),
        csrf.secret(),
    )?;

    print_or_open_browser(&auth_url);

    let redirect = wait_for_redirect(&listener, LISTENER_TIMEOUT)?;

    if redirect.state != *csrf.secret() {
        return Err(Error::AuthRequired {
            account: "(pre-auth)".into(),
            reason: "CSRF state mismatch in OAuth redirect — possible replay or hijack".into(),
        });
    }

    let token = exchange_code(
        inputs.credentials,
        &redirect.code,
        &redirect_uri,
        &verifier,
        inputs.scopes,
    )?;
    let email = fetch_userinfo_email(&token.access_token)?;

    Ok(AuthFlowOutput { email, token })
}

/// Like [`run_auth_add`] but adds `include_granted_scopes=true` to the auth
/// URL, so Google's consent screen only shows the *delta* scopes. Used by
/// `auth grant` to incrementally extend an existing account's permission set.
pub(crate) fn run_auth_grant(inputs: &AuthFlowInputs<'_>) -> Result<AuthFlowOutput, Error> {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let csrf = CsrfToken::new_random();
    let redirect_uri = format!("http://127.0.0.1:{}", inputs.redirect_port);

    let listener = bind_listener(inputs.redirect_port)?;

    let auth_url = build_auth_url_with_opts(
        &inputs.credentials.auth_uri,
        &inputs.credentials.client_id,
        &redirect_uri,
        inputs.scopes,
        challenge.as_str(),
        csrf.secret(),
        true, // include_granted_scopes
    )?;

    print_or_open_browser(&auth_url);

    let redirect = wait_for_redirect(&listener, LISTENER_TIMEOUT)?;

    if redirect.state != *csrf.secret() {
        return Err(Error::AuthRequired {
            account: "(pre-auth)".into(),
            reason: "CSRF state mismatch in OAuth redirect — possible replay or hijack".into(),
        });
    }

    let token = exchange_code(
        inputs.credentials,
        &redirect.code,
        &redirect_uri,
        &verifier,
        inputs.scopes,
    )?;
    let email = fetch_userinfo_email(&token.access_token)?;

    Ok(AuthFlowOutput { email, token })
}

// ── Auth URL construction (pure) ─────────────────────────────────────────────

fn build_auth_url(
    auth_uri: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    code_challenge: &str,
    state: &str,
) -> Result<Url, Error> {
    build_auth_url_with_opts(
        auth_uri,
        client_id,
        redirect_uri,
        scopes,
        code_challenge,
        state,
        false,
    )
}

fn build_auth_url_with_opts(
    auth_uri: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    code_challenge: &str,
    state: &str,
    include_granted_scopes: bool,
) -> Result<Url, Error> {
    let mut url = Url::parse(auth_uri).map_err(|e| Error::Config {
        path: auth_uri.to_owned(),
        message: format!("invalid auth_uri: {e}"),
    })?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("response_type", "code");
        q.append_pair("scope", &scopes.join(" "));
        q.append_pair("state", state);
        q.append_pair("code_challenge", code_challenge);
        q.append_pair("code_challenge_method", "S256");
        // access_type=offline + prompt=consent is what Google requires to
        // reliably get a refresh_token back. Without prompt=consent, Google
        // skips the refresh_token on re-auth of the same account.
        q.append_pair("access_type", "offline");
        q.append_pair("prompt", "consent");
        if include_granted_scopes {
            // Google-specific extension: consent screen shows only the *delta*
            // between what is already granted and what is now requested.
            q.append_pair("include_granted_scopes", "true");
        }
    }
    Ok(url)
}

// ── Browser launch ──────────────────────────────────────────────────────────

fn print_or_open_browser(url: &Url) {
    let s = url.as_str();
    eprintln!("\nOpening authorization URL in your browser:\n  {s}\n");
    if open::that(s).is_err() {
        eprintln!("(could not auto-open browser — paste the URL above into your browser manually)");
    }
}

// ── Local redirect listener ─────────────────────────────────────────────────

fn bind_listener(port: u16) -> Result<TcpListener, Error> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = TcpListener::bind(addr).map_err(|e| Error::Config {
        path: addr.to_string(),
        message: format!("could not bind OAuth redirect listener: {e}"),
    })?;
    listener
        .set_nonblocking(false)
        .map_err(|e| Error::Internal {
            context: "set_nonblocking(false)".into(),
            source: anyhow::Error::new(e),
        })?;
    Ok(listener)
}

fn wait_for_redirect(
    listener: &TcpListener,
    timeout: StdDuration,
) -> Result<RedirectResult, Error> {
    listener
        .set_nonblocking(false)
        .map_err(|e| Error::Internal {
            context: "set_nonblocking".into(),
            source: anyhow::Error::new(e),
        })?;
    let (mut stream, _) = listener.accept().map_err(|e| Error::Internal {
        context: "accept redirect".into(),
        source: anyhow::Error::new(e),
    })?;
    stream.set_read_timeout(Some(timeout)).map_err(Error::Io)?;

    let request_line = read_request_line(&stream)?;
    let path = extract_request_path(&request_line)?;

    match parse_redirect_params(&path) {
        Ok(r) => {
            write_http_response(&mut stream, 200, "OK", "text/html", SUCCESS_HTML);
            Ok(r)
        }
        Err(e) => {
            let body = format!("{ERROR_HTML_PREFIX}{e}</pre></body></html>");
            write_http_response(&mut stream, 400, "Bad Request", "text/html", &body);
            Err(e)
        }
    }
}

/// Maximum number of bytes accepted for the HTTP request line.
///
/// A legitimate OAuth redirect is `GET /?code=<64 chars>&state=<43 chars> HTTP/1.1\r\n`
/// — well under 256 bytes. 8 KiB gives headroom for unusually long codes while
/// bounding worst-case allocation from a malicious local connection.
const REQUEST_LINE_LIMIT: u64 = 8 * 1024;

/// Read the first line of an HTTP request from `reader`, capped at
/// [`REQUEST_LINE_LIMIT`] bytes. Returns `Error::Parse` if the line
/// exceeds the limit (no `\n` found within the budget).
fn read_request_line(reader: impl Read) -> Result<String, Error> {
    let mut limited = BufReader::new(reader.take(REQUEST_LINE_LIMIT));
    let mut line = String::new();
    let n = limited.read_line(&mut line).map_err(Error::Io)?;
    // If we consumed the full budget without a newline, the request is oversized.
    if n as u64 == REQUEST_LINE_LIMIT && !line.contains('\n') {
        return Err(Error::Parse {
            context: "OAuth redirect request line".into(),
            source: serde::de::Error::custom(
                "request line exceeds 8 KiB limit — possible malformed or malicious request",
            ),
        });
    }
    Ok(line)
}

fn extract_request_path(request_line: &str) -> Result<String, Error> {
    // Request line: "GET /path?query HTTP/1.1\r\n"
    let mut parts = request_line.split_whitespace();
    parts.next(); // method
    parts.next().map(str::to_owned).ok_or_else(|| Error::Parse {
        context: "OAuth redirect request line".into(),
        source: serde::de::Error::custom("missing request path"),
    })
}

fn parse_redirect_params(path: &str) -> Result<RedirectResult, Error> {
    // Build a full URL so we can use `url`'s query parser; the host doesn't matter.
    let url = Url::parse(&format!("http://127.0.0.1{path}")).map_err(|e| Error::Parse {
        context: "OAuth redirect URL".into(),
        source: serde::de::Error::custom(e.to_string()),
    })?;
    let mut code = None;
    let mut state = None;
    let mut oauth_error = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => oauth_error = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(e) = oauth_error {
        return Err(Error::AuthRequired {
            account: "(pre-auth)".into(),
            reason: format!("Google returned error in redirect: {e}"),
        });
    }
    match (code, state) {
        (Some(code), Some(state)) => Ok(RedirectResult { code, state }),
        _ => Err(Error::Parse {
            context: "OAuth redirect".into(),
            source: serde::de::Error::custom("missing `code` or `state` query param"),
        }),
    }
}

fn write_http_response(stream: &mut TcpStream, status: u16, reason: &str, ctype: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

// ── Token exchange ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenExchangeResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    #[allow(dead_code)]
    scope: Option<String>,
}

fn exchange_code(
    creds: &Credentials,
    code: &str,
    redirect_uri: &str,
    verifier: &PkceCodeVerifier,
    scopes: &[String],
) -> Result<TokenState, Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", &creds.client_id)
        .append_pair("client_secret", &creds.client_secret)
        .append_pair("code_verifier", verifier.secret())
        .finish();

    let client = blocking_reqwest_client()?;
    let resp = client
        .post(&creds.token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(Error::Network)?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(Error::Network)?;
    if !(200..300).contains(&status) {
        return Err(Error::upstream("google-oauth", status, text));
    }
    let parsed: TokenExchangeResponse = serde_json::from_str(&text).map_err(|e| Error::Parse {
        context: "OAuth token exchange response".into(),
        source: e,
    })?;
    let refresh = parsed.refresh_token.ok_or_else(|| Error::AuthRequired {
        account: "(pre-auth)".into(),
        reason: "Google did not return a refresh_token — re-run `auth add` and ensure \
                 you accept the consent screen (access_type=offline + prompt=consent)"
            .into(),
    })?;
    Ok(TokenState {
        access_token: parsed.access_token,
        refresh_token: refresh,
        expires_at: Utc::now() + Duration::seconds(parsed.expires_in),
        scopes: scopes.to_vec(),
        client_id: creds.client_id.clone(),
        client_secret: creds.client_secret.clone(),
        failed_until: None,
        consecutive_failures: 0,
    })
}

// ── Userinfo (email lookup) ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct UserInfo {
    email: String,
}

fn fetch_userinfo_email(access_token: &str) -> Result<String, Error> {
    let client = blocking_reqwest_client()?;
    let resp = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access_token)
        .send()
        .map_err(Error::Network)?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(Error::Network)?;
    if !(200..300).contains(&status) {
        return Err(Error::upstream("google-userinfo", status, text));
    }
    let parsed: UserInfo = serde_json::from_str(&text).map_err(|e| Error::Parse {
        context: "userinfo response".into(),
        source: e,
    })?;
    Ok(parsed.email)
}

fn blocking_reqwest_client() -> Result<reqwest::blocking::Client, Error> {
    reqwest::blocking::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .build()
        .map_err(Error::Network)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use base64::Engine;

    fn sample_creds() -> Credentials {
        Credentials {
            client_id: "cid".into(),
            client_secret: "csec".into(),
            auth_uri: "https://accounts.google.com/o/oauth2/auth".into(),
            token_uri: "https://oauth2.googleapis.com/token".into(),
        }
    }

    // ── PKCE: verifier and challenge shape (RFC 7636) ────────────────────────

    #[test]
    fn pkce_verifier_is_within_rfc7636_bounds() {
        let (_challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let len = verifier.secret().len();
        assert!(
            (43..=128).contains(&len),
            "verifier length {len} outside RFC 7636 [43, 128]"
        );
        // Verifier must use only the unreserved character set.
        for c in verifier.secret().chars() {
            assert!(
                c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'),
                "verifier contains illegal char {c:?}"
            );
        }
    }

    #[test]
    fn pkce_challenge_is_base64url_no_padding() {
        let (challenge, _verifier) = PkceCodeChallenge::new_random_sha256();
        let c = challenge.as_str();
        // S256 challenge is base64url-no-pad of a 32-byte digest → 43 chars.
        assert_eq!(
            c.len(),
            43,
            "S256 challenge should be 43 chars (got {})",
            c.len()
        );
        for ch in c.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'),
                "challenge contains non-base64url char {ch:?}"
            );
        }
        // And it must decode cleanly via base64url-no-pad to exactly 32 bytes.
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(c)
            .expect("challenge should decode");
        assert_eq!(decoded.len(), 32, "SHA-256 digest is 32 bytes");
    }

    #[test]
    fn pkce_two_calls_produce_distinct_values() {
        let (c1, v1) = PkceCodeChallenge::new_random_sha256();
        let (c2, v2) = PkceCodeChallenge::new_random_sha256();
        assert_ne!(c1.as_str(), c2.as_str());
        assert_ne!(v1.secret(), v2.secret());
    }

    // ── Auth URL ─────────────────────────────────────────────────────────────

    #[test]
    fn auth_url_contains_required_params() {
        let url = build_auth_url(
            "https://accounts.google.com/o/oauth2/auth",
            "cid",
            "http://127.0.0.1:8080",
            &["scope.a".into(), "scope.b".into()],
            "CHALLENGE",
            "STATE",
        )
        .expect("url builds");
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("client_id").map(String::as_str), Some("cid"));
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            pairs.get("scope").map(String::as_str),
            Some("scope.a scope.b")
        );
        assert_eq!(pairs.get("state").map(String::as_str), Some("STATE"));
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some("CHALLENGE")
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            pairs.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(pairs.get("prompt").map(String::as_str), Some("consent"));
    }

    // ── Redirect parsing ─────────────────────────────────────────────────────

    #[test]
    fn parses_redirect_with_code_and_state() {
        let r = parse_redirect_params("/?code=AUTHCODE&state=ST").expect("ok");
        assert_eq!(r.code, "AUTHCODE");
        assert_eq!(r.state, "ST");
    }

    #[test]
    fn parses_redirect_with_extra_params_ignored() {
        let r = parse_redirect_params("/?scope=x&code=AC&state=ST&authuser=0").expect("ok");
        assert_eq!(r.code, "AC");
        assert_eq!(r.state, "ST");
    }

    #[test]
    fn missing_code_is_parse_error() {
        let err = parse_redirect_params("/?state=ST").expect_err("must fail");
        assert!(matches!(err, Error::Parse { .. }));
    }

    #[test]
    fn google_error_in_redirect_is_auth_required() {
        let err = parse_redirect_params("/?error=access_denied&state=ST").expect_err("must fail");
        assert!(
            matches!(err, Error::AuthRequired { ref reason, .. } if reason.contains("access_denied")),
            "got: {err:?}"
        );
    }

    // ── Request-line parsing ─────────────────────────────────────────────────

    #[test]
    fn extracts_path_from_request_line() {
        let p = extract_request_path("GET /?code=x&state=y HTTP/1.1\r\n").expect("ok");
        assert_eq!(p, "/?code=x&state=y");
    }

    #[test]
    fn malformed_request_line_is_parse_error() {
        let err = extract_request_path("\r\n").expect_err("must fail");
        assert!(matches!(err, Error::Parse { .. }));
    }

    // ── TokenState round-trip (AC) ───────────────────────────────────────────

    #[test]
    fn token_state_round_trips_via_json() {
        let original = TokenState {
            access_token: "AAAA".into(),
            refresh_token: "RRRR".into(),
            expires_at: Utc::now(),
            scopes: vec!["s1".into(), "s2".into()],
            client_id: sample_creds().client_id,
            client_secret: sample_creds().client_secret,
            failed_until: None,
            consecutive_failures: 0,
        };
        let json = serde_json::to_string(&original).expect("ser");
        let decoded: TokenState = serde_json::from_str(&json).expect("de");
        assert_eq!(decoded.access_token, original.access_token);
        assert_eq!(decoded.refresh_token, original.refresh_token);
        assert_eq!(decoded.scopes, original.scopes);
        assert_eq!(decoded.client_id, original.client_id);
        assert_eq!(decoded.client_secret, original.client_secret);
    }

    // ── request-line size bound (Layer 1) ────────────────────────────────────

    #[test]
    fn read_request_line_accepts_normal_redirect() {
        let input = "GET /?code=abc&state=xyz HTTP/1.1\r\n";
        let result = read_request_line(input.as_bytes());
        assert!(result.is_ok(), "unexpected error: {result:?}");
        assert_eq!(
            result.unwrap().trim_end(),
            "GET /?code=abc&state=xyz HTTP/1.1"
        );
    }

    #[test]
    fn read_request_line_rejects_oversized_input() {
        // 10 KiB of 'A' with no newline — exceeds the 8 KiB limit.
        let oversized = vec![b'A'; 10 * 1024];
        let result = read_request_line(oversized.as_slice());
        assert!(
            matches!(result, Err(Error::Parse { .. })),
            "expected Parse error for oversized line, got {result:?}"
        );
    }

    #[test]
    fn read_request_line_accepts_line_just_under_limit() {
        // 8 KiB - 1 byte of content + newline — should succeed (within budget).
        let mut input = vec![b'X'; REQUEST_LINE_LIMIT as usize - 1];
        input.push(b'\n');
        let result = read_request_line(input.as_slice());
        assert!(
            result.is_ok(),
            "expected Ok for line just under limit: {result:?}"
        );
    }
}
