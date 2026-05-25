//! Gmail API wrapper for `users.getProfile`.
//!
//! Returns the `historyId` the cache uses to seed `account_state.last_history_id`
//! on first touch and after a 404 `historyNotFound` reseed (per
//! [ADR-0009](../../docs/adr/0009-caching-with-sqlite-and-history-api.md) §"Sync
//! protocol"). Cost: 1 quota unit per call (see `gmail::quota::GmailMethod::GetProfile`).

use serde::Deserialize;

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::quota::GmailMethod;
use crate::http::percent_encode_path_segment;

/// Raw `users.getProfile` response. Gmail returns `historyId` as a decimal
/// string; we keep it as `String` so it round-trips through `RawThread` /
/// `RawListedThread` without lossy conversion.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Profile {
    pub email_address: String,
    pub history_id: String,
    #[serde(default)]
    pub messages_total: u64,
    #[serde(default)]
    pub threads_total: u64,
}

/// Issue `users.getProfile` for `account` and return the parsed body.
///
/// Errors:
/// - [`Error::InvalidArgument`] if `account` is empty.
/// - All HTTP/auth errors surface as-is from [`GmailClient::authed_get`].
pub(crate) async fn get_profile<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
) -> Result<Profile, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }
    let path = format!(
        "/users/{a}/profile",
        a = percent_encode_path_segment(account),
    );
    client
        .authed_get(account, &path, GmailMethod::GetProfile.cost())
        .await
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::tokens::{TokenManager, TokenState};
    use crate::http::RetryPolicy;

    struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _token_uri: &str, _body: String) -> Result<(u16, String), Error> {
            Ok((
                200,
                r#"{"access_token":"NEW","expires_in":3600}"#.to_owned(),
            ))
        }
    }

    fn make_client(base_url: &str) -> Arc<GmailClient<NoRefresh>> {
        let state = TokenState {
            access_token: "TOKEN".into(),
            refresh_token: "R".into(),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            scopes: vec![],
            client_id: "cid".into(),
            client_secret: "csec".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        };
        let dir = std::env::temp_dir().join(format!(
            "gpm-profile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            dir,
        ));
        Arc::new(
            GmailClient::new(base_url, tokens, reqwest::Client::new())
                .with_retry(RetryPolicy::for_tests()),
        )
    }

    #[tokio::test]
    async fn get_profile_round_trips_history_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/work/profile$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "alice@example.com",
                "messagesTotal": 1234,
                "threadsTotal": 567,
                "historyId": "9876543"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let profile = get_profile(&client, "work").await.expect("ok");
        assert_eq!(profile.email_address, "alice@example.com");
        assert_eq!(profile.history_id, "9876543");
        assert_eq!(profile.messages_total, 1234);
        assert_eq!(profile.threads_total, 567);
    }

    #[tokio::test]
    async fn empty_account_returns_invalid_argument() {
        let client = make_client("http://localhost:1");
        let err = get_profile(&client, "").await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "account"));
    }
}
