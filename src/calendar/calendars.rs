//! `list_calendars` tool — enumerate the calendars visible to an account, per
//! [ADR-0023 §Tool inventory](../../docs/adr/0023-calendar-service-surface.md).
//!
//! Calls Calendar v3 `calendarList.list` (`GET /users/me/calendarList`),
//! following `nextPageToken` until exhausted so the response is the account's
//! complete calendar set. Requires the `calendar.readonly` OAuth scope (the
//! token must carry it; an absent scope surfaces as a Google `403`).
//!
//! **Untrusted content** ([ADR-0018](../../docs/adr/0018-email-content-trust.md)):
//! a calendar shared by another party carries an attacker-controllable
//! `summary` / `description`, so both are wrapped `_untrusted`. The calendar
//! `id` is a structural identifier (used as the key for `list_events`), so —
//! like `thread_id` / `label_id` — it is not wrapped.

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::calendar::client::QUERY_COST;
use crate::calendar::service::CalendarService;
use crate::error::Error;
use crate::gmail::untrusted::UntrustedString;
use crate::http::percent_encode_path_segment;

// ── Calendar API response shapes ──────────────────────────────────────────────

/// Raw response from `calendarList.list`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarListResponse {
    #[serde(default)]
    items: Vec<RawCalendarListEntry>,
    next_page_token: Option<String>,
}

/// A single `calendarListEntry` resource (subset of fields we surface).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCalendarListEntry {
    id: String,
    #[serde(default)]
    summary: String,
    description: Option<String>,
    #[serde(default)]
    primary: bool,
    /// `"owner"` / `"writer"` / `"reader"` / `"freeBusyReader"` — enumerated by
    /// Google, not attacker-controllable.
    access_role: Option<String>,
    time_zone: Option<String>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// One calendar in the `list_calendars` response.
#[derive(Debug, Serialize)]
pub(crate) struct CalendarItem {
    /// Calendar identifier (often an email address). Structural key for
    /// `list_events`; trusted per ADR-0018 (same class as `thread_id`).
    pub calendar_id: String,
    pub summary_untrusted: UntrustedString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_untrusted: Option<UntrustedString>,
    /// `true` for the account's primary calendar.
    pub is_primary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

/// Listing envelope per ADR-0016 §"Response convention".
#[derive(Debug, Serialize)]
pub(crate) struct ListCalendarsOutput {
    pub items: Vec<CalendarItem>,
}

// ── Core logic ────────────────────────────────────────────────────────────────

fn map_entry(r: RawCalendarListEntry) -> CalendarItem {
    CalendarItem {
        calendar_id: r.id,
        summary_untrusted: UntrustedString::new("calendar-summary", r.summary),
        description_untrusted: r
            .description
            .map(|d| UntrustedString::new("calendar-description", d)),
        is_primary: r.primary,
        access_role: r.access_role,
        time_zone: r.time_zone,
    }
}

/// Fetch every calendar visible to `account`, paginating until exhausted.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "list_calendars", tool.account = %account),
)]
pub(crate) async fn list_calendars<T: RefreshTransport>(
    calendar: &CalendarService<T>,
    account: &str,
) -> Result<ListCalendarsOutput, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }

    let mut items = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let mut qs = String::from("maxResults=250");
        if let Some(tok) = page_token.as_deref().filter(|t| !t.is_empty()) {
            qs.push_str("&pageToken=");
            qs.push_str(&percent_encode_path_segment(tok));
        }
        let path = format!("/users/me/calendarList?{qs}");
        let resp: CalendarListResponse = calendar
            .client()
            .authed_get(account, &path, QUERY_COST)
            .await?;
        items.extend(resp.items.into_iter().map(map_entry));
        match resp.next_page_token.filter(|t| !t.is_empty()) {
            Some(tok) => page_token = Some(tok),
            None => break,
        }
    }

    Ok(ListCalendarsOutput { items })
}

// ── Layer 1 unit tests (pure mapping / serialization) ─────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn raw(id: &str, summary: &str, primary: bool) -> RawCalendarListEntry {
        RawCalendarListEntry {
            id: id.into(),
            summary: summary.into(),
            description: None,
            primary,
            access_role: Some("owner".into()),
            time_zone: Some("America/Chicago".into()),
        }
    }

    #[test]
    fn maps_primary_flag_and_identifier() {
        let item = map_entry(raw("me@example.com", "Personal", true));
        assert_eq!(item.calendar_id, "me@example.com");
        assert!(item.is_primary);
        assert_eq!(item.access_role.as_deref(), Some("owner"));
        assert_eq!(item.time_zone.as_deref(), Some("America/Chicago"));
    }

    #[test]
    fn summary_serializes_wrapped_untrusted() {
        let out = ListCalendarsOutput {
            items: vec![map_entry(raw("c1", "Team Calendar", false))],
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("<<<UNTRUSTED:calendar-summary"),
            "got: {json}"
        );
        // Structural id is not wrapped.
        assert!(json.contains("\"calendar_id\":\"c1\""), "got: {json}");
        assert!(json.contains("\"is_primary\":false"), "got: {json}");
    }

    #[test]
    fn description_wrapped_when_present_omitted_when_absent() {
        let mut r = raw("c1", "Cal", false);
        r.description = Some("shared notes".into());
        let with = serde_json::to_value(map_entry(r)).unwrap();
        assert!(with["description_untrusted"]
            .as_str()
            .unwrap()
            .contains("<<<UNTRUSTED:calendar-description"));

        let without = serde_json::to_value(map_entry(raw("c2", "Cal", false))).unwrap();
        assert!(without.get("description_untrusted").is_none());
    }

    #[test]
    fn deserializes_calendar_list_response() {
        let body = serde_json::json!({
            "items": [
                {"id": "primary", "summary": "Me", "primary": true, "accessRole": "owner"},
                {"id": "team@x.com", "summary": "Team", "accessRole": "reader"}
            ],
            "nextPageToken": "PAGE2"
        });
        let resp: CalendarListResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.items.len(), 2);
        assert!(resp.items[0].primary);
        assert!(!resp.items[1].primary);
        assert_eq!(resp.next_page_token.as_deref(), Some("PAGE2"));
    }
}

// ── Layer 2 wiremock tests (end-to-end through CalendarService) ───────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod wiremock_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::calendar::client::CalendarClient;

    struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _u: &str, _b: String) -> Result<(u16, String), Error> {
            Ok((200, r#"{"access_token":"T","expires_in":3600}"#.into()))
        }
    }

    fn make_service(base_url: &str) -> CalendarService<NoRefresh> {
        let state = TokenState {
            access_token: "T".into(),
            refresh_token: "R".into(),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            scopes: vec![],
            client_id: "c".into(),
            client_secret: "s".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        };
        let dir = std::env::temp_dir().join(format!("gpm-cal-l2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            dir,
        ));
        let client = Arc::new(CalendarClient::new(
            base_url,
            tokens,
            reqwest::Client::new(),
        ));
        CalendarService::new(client)
    }

    #[tokio::test]
    async fn lists_single_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me/calendarList"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [
                    {"id": "primary", "summary": "Me", "primary": true, "accessRole": "owner"},
                    {"id": "team@x.com", "summary": "Team", "accessRole": "reader"}
                ]
            })))
            .mount(&server)
            .await;

        let svc = make_service(&server.uri());
        let out = list_calendars(&svc, "work").await.expect("ok");
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.items[0].calendar_id, "primary");
        assert!(out.items[0].is_primary);
        assert!(!out.items[1].is_primary);
    }

    #[tokio::test]
    async fn follows_next_page_token_until_exhausted() {
        let server = MockServer::start().await;
        // Page 1 (no pageToken) → returns nextPageToken=P2.
        Mock::given(method("GET"))
            .and(path("/users/me/calendarList"))
            .and(query_param("maxResults", "250"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{"id": "c1", "summary": "One"}],
                "nextPageToken": "P2"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Page 2 (pageToken=P2) → no further token.
        Mock::given(method("GET"))
            .and(path("/users/me/calendarList"))
            .and(query_param("pageToken", "P2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{"id": "c2", "summary": "Two"}]
            })))
            .mount(&server)
            .await;

        let svc = make_service(&server.uri());
        let out = list_calendars(&svc, "work").await.expect("ok");
        let ids: Vec<&str> = out.items.iter().map(|c| c.calendar_id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c2"], "both pages accumulated in order");
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me/calendarList"))
            .respond_with(ResponseTemplate::new(403).set_body_string("insufficient scope"))
            .mount(&server)
            .await;

        let svc = make_service(&server.uri());
        let err = list_calendars(&svc, "work")
            .await
            .expect_err("403 must error");
        assert!(
            matches!(err, Error::Upstream { status: 403, .. }),
            "got: {err:?}"
        );
    }
}
