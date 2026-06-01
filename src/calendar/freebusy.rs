//! `freebusy` tool group — `query_freebusy` per
//! [ADR-0023 §Tool inventory](../../docs/adr/0023-calendar-service-surface.md).
//!
//! Calls Calendar v3 `freebusy.query` (`POST /freeBusy`). Requires the
//! `calendar.readonly` OAuth scope. Returns each requested calendar's busy
//! intervals — **no event details** (summary/attendees/etc.), so nothing here
//! is attacker-controllable and no `_untrusted` wrapping is needed
//! ([ADR-0018](../../docs/adr/0018-email-content-trust.md)). Calendar ids are
//! caller-supplied and echoed back; busy `start`/`end` and any per-calendar
//! `errors` are Google-generated and structural — all trusted.
//!
//! Two guards run before the network call:
//! - **`calendarExpansionMax: 50`** — Google rejects more than 50 calendars per
//!   query; we refuse it client-side with a typed error rather than paying the
//!   round-trip.
//! - **Time-window cap** — a configurable maximum span
//!   (`[services.calendar].freebusy_max_window_days`, default 31) bounds payload
//!   and quota.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::calendar::client::QUERY_COST;
use crate::calendar::service::CalendarService;
use crate::error::Error;

/// Google's hard ceiling on calendars per `freebusy.query` (`calendarExpansionMax`).
pub(crate) const CALENDAR_EXPANSION_MAX: usize = 50;

// ── Input ─────────────────────────────────────────────────────────────────────

/// Owned input for `query_freebusy`.
#[derive(Debug)]
pub(crate) struct QueryFreebusyInput {
    pub account: String,
    pub calendar_ids: Vec<String>,
    /// RFC 3339 lower bound (inclusive).
    pub time_min: String,
    /// RFC 3339 upper bound (exclusive).
    pub time_max: String,
    /// MCP-side window cap in days (from
    /// `[services.calendar].freebusy_max_window_days`).
    pub max_window_days: u32,
}

// ── Calendar API request/response shapes ──────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FreeBusyRequest<'a> {
    time_min: &'a str,
    time_max: &'a str,
    items: Vec<FreeBusyRequestItem<'a>>,
}

#[derive(Debug, Serialize)]
struct FreeBusyRequestItem<'a> {
    id: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FreeBusyResponse {
    #[serde(default)]
    calendars: HashMap<String, RawCalendarBusy>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawCalendarBusy {
    busy: Vec<RawBusyInterval>,
    errors: Vec<RawFreeBusyError>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawBusyInterval {
    start: Option<String>,
    end: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawFreeBusyError {
    domain: Option<String>,
    reason: Option<String>,
}

// ── Response types ──────────────────────────────────────────────────────────────

/// A busy time span on a calendar. Google-formatted RFC 3339 timestamps — trusted.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct BusyInterval {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// A per-calendar lookup error (e.g. `notFound` for a calendar the account
/// can't see). Google-generated, trusted.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct FreeBusyError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Busy intervals (and any errors) for one requested calendar.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CalendarBusy {
    pub calendar_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub busy: Vec<BusyInterval>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FreeBusyError>,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryFreebusyOutput {
    pub calendars: Vec<CalendarBusy>,
}

// ── Validation ──────────────────────────────────────────────────────────────────

fn require_non_empty(field: &'static str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::InvalidArgument {
            field: field.into(),
            detail: format!("{field} must not be empty"),
        });
    }
    Ok(())
}

/// Parse an RFC 3339 timestamp, mapping a parse failure to a typed
/// `InvalidArgument` naming the offending field.
fn parse_rfc3339(
    field: &'static str,
    value: &str,
) -> Result<chrono::DateTime<chrono::FixedOffset>, Error> {
    chrono::DateTime::parse_from_rfc3339(value).map_err(|e| Error::InvalidArgument {
        field: field.into(),
        detail: format!("{field} must be an RFC 3339 timestamp: {e}"),
    })
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// Query busy intervals for up to 50 calendars over `[time_min, time_max)`.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "query_freebusy",
        tool.account = %input.account,
        tool.calendar_count = input.calendar_ids.len(),
    ),
)]
pub(crate) async fn query_freebusy<T: RefreshTransport>(
    calendar: &CalendarService<T>,
    input: QueryFreebusyInput,
) -> Result<QueryFreebusyOutput, Error> {
    require_non_empty("account", &input.account)?;
    require_non_empty("time_min", &input.time_min)?;
    require_non_empty("time_max", &input.time_max)?;

    if input.calendar_ids.is_empty() {
        return Err(Error::InvalidArgument {
            field: "calendar_ids".into(),
            detail: "at least one calendar id is required".into(),
        });
    }
    // calendarExpansionMax: refuse client-side rather than pay the round-trip.
    if input.calendar_ids.len() > CALENDAR_EXPANSION_MAX {
        return Err(Error::InvalidArgument {
            field: "calendar_ids".into(),
            detail: format!(
                "at most {CALENDAR_EXPANSION_MAX} calendars per query \
                 (calendarExpansionMax), got {}",
                input.calendar_ids.len()
            ),
        });
    }
    if input.calendar_ids.iter().any(String::is_empty) {
        return Err(Error::InvalidArgument {
            field: "calendar_ids".into(),
            detail: "calendar ids must not be empty".into(),
        });
    }

    // Window guard: bound the span before calling Google.
    let min_dt = parse_rfc3339("time_min", &input.time_min)?;
    let max_dt = parse_rfc3339("time_max", &input.time_max)?;
    if max_dt <= min_dt {
        return Err(Error::InvalidArgument {
            field: "time_max".into(),
            detail: "time_max must be after time_min".into(),
        });
    }
    let max_window = chrono::Duration::days(i64::from(input.max_window_days));
    if max_dt - min_dt > max_window {
        return Err(Error::InvalidArgument {
            field: "time_window".into(),
            detail: format!(
                "freebusy window exceeds the configured maximum of {} days \
                 (freebusy_max_window_days)",
                input.max_window_days
            ),
        });
    }

    let body = FreeBusyRequest {
        time_min: &input.time_min,
        time_max: &input.time_max,
        items: input
            .calendar_ids
            .iter()
            .map(|id| FreeBusyRequestItem { id })
            .collect(),
    };
    let resp: FreeBusyResponse = calendar
        .client()
        .authed_post(&input.account, "/freeBusy", QUERY_COST, &body)
        .await?;

    // Emit one row per *requested* calendar, in request order, so the caller
    // gets a stable, complete result even if Google omits a calendar from the
    // map. `remove` transfers ownership out of the map without cloning.
    let mut by_id = resp.calendars;
    let calendars = input
        .calendar_ids
        .iter()
        .map(|id| {
            let raw = by_id.remove(id).unwrap_or_default();
            CalendarBusy {
                calendar_id: id.clone(),
                busy: raw
                    .busy
                    .into_iter()
                    .map(|b| BusyInterval {
                        start: b.start,
                        end: b.end,
                    })
                    .collect(),
                errors: raw
                    .errors
                    .into_iter()
                    .map(|e| FreeBusyError {
                        domain: e.domain,
                        reason: e.reason,
                    })
                    .collect(),
            }
        })
        .collect();

    Ok(QueryFreebusyOutput { calendars })
}

// ── Test harness (shared by Layer 1 + Layer 2) ───────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod test_support {
    use super::*;
    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::calendar::client::CalendarClient;
    use std::collections::HashMap;
    use std::sync::Arc;

    pub(super) struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _u: &str, _b: String) -> Result<(u16, String), Error> {
            Ok((200, r#"{"access_token":"T","expires_in":3600}"#.into()))
        }
    }

    pub(super) fn service(base_url: &str) -> CalendarService<NoRefresh> {
        let state = TokenState {
            access_token: "T".into(),
            refresh_token: "R".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(3600),
            scopes: vec![],
            client_id: "c".into(),
            client_secret: "s".into(),
            failed_until: None,
            consecutive_failures: 0,
            last_refresh_at: None,
        };
        let dir = std::env::temp_dir().join(format!("gpm-fb-{}", std::process::id()));
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
}

// ── Layer 1 unit tests (validation guards return before any network call) ─────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn base_input() -> QueryFreebusyInput {
        QueryFreebusyInput {
            account: "work".into(),
            calendar_ids: vec!["primary".into()],
            time_min: "2026-06-01T00:00:00Z".into(),
            time_max: "2026-06-08T00:00:00Z".into(),
            max_window_days: 31,
        }
    }

    async fn call(input: QueryFreebusyInput) -> Result<QueryFreebusyOutput, Error> {
        let svc = test_support::service("https://unused.example");
        query_freebusy(&svc, input).await
    }

    #[tokio::test]
    async fn rejects_empty_calendar_ids() {
        let mut input = base_input();
        input.calendar_ids = vec![];
        let err = call(input).await.unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "calendar_ids"));
    }

    #[tokio::test]
    async fn rejects_more_than_fifty_calendars() {
        let mut input = base_input();
        input.calendar_ids = (0..51).map(|i| format!("c{i}")).collect();
        let err = call(input).await.unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { ref field, ref detail }
                if field == "calendar_ids" && detail.contains("calendarExpansionMax"))
        );
    }

    #[tokio::test]
    async fn rejects_window_exceeding_cap() {
        let mut input = base_input();
        input.time_max = "2026-08-01T00:00:00Z".into(); // ~61 days > 31
        let err = call(input).await.unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "time_window"),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_inverted_window() {
        let mut input = base_input();
        input.time_min = "2026-06-08T00:00:00Z".into();
        input.time_max = "2026-06-01T00:00:00Z".into();
        let err = call(input).await.unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "time_max"));
    }

    #[tokio::test]
    async fn rejects_non_rfc3339_time() {
        let mut input = base_input();
        input.time_min = "not-a-date".into();
        let err = call(input).await.unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "time_min"));
    }
}

// ── Layer 2 wiremock tests (end-to-end through CalendarService) ───────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod wiremock_tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn queries_freebusy_and_maps_busy_intervals_in_request_order() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/freeBusy"))
            .and(body_partial_json(serde_json::json!({
                "timeMin": "2026-06-01T00:00:00Z",
                "timeMax": "2026-06-08T00:00:00Z",
                "items": [{"id": "primary"}, {"id": "team@x.com"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "calendars": {
                    "primary": {"busy": [{"start": "2026-06-02T09:00:00Z", "end": "2026-06-02T10:00:00Z"}]},
                    "team@x.com": {"errors": [{"domain": "global", "reason": "notFound"}]}
                }
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let out = query_freebusy(
            &svc,
            QueryFreebusyInput {
                account: "work".into(),
                calendar_ids: vec!["primary".into(), "team@x.com".into()],
                time_min: "2026-06-01T00:00:00Z".into(),
                time_max: "2026-06-08T00:00:00Z".into(),
                max_window_days: 31,
            },
        )
        .await
        .expect("ok");

        assert_eq!(out.calendars.len(), 2);
        // Request order preserved.
        assert_eq!(out.calendars[0].calendar_id, "primary");
        assert_eq!(out.calendars[0].busy.len(), 1);
        assert_eq!(
            out.calendars[0].busy[0].start.as_deref(),
            Some("2026-06-02T09:00:00Z")
        );
        assert_eq!(out.calendars[1].calendar_id, "team@x.com");
        assert_eq!(out.calendars[1].errors.len(), 1);
        assert_eq!(
            out.calendars[1].errors[0].reason.as_deref(),
            Some("notFound")
        );
        // No event details ⇒ no untrusted wrapping anywhere.
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            !json.contains("UNTRUSTED"),
            "freebusy must not wrap: {json}"
        );
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/freeBusy"))
            .respond_with(ResponseTemplate::new(403).set_body_string("insufficient scope"))
            .mount(&server)
            .await;
        let svc = test_support::service(&server.uri());
        let err = query_freebusy(
            &svc,
            QueryFreebusyInput {
                account: "work".into(),
                calendar_ids: vec!["primary".into()],
                time_min: "2026-06-01T00:00:00Z".into(),
                time_max: "2026-06-08T00:00:00Z".into(),
                max_window_days: 31,
            },
        )
        .await
        .expect_err("403 must error");
        assert!(
            matches!(err, Error::Upstream { status: 403, .. }),
            "got: {err:?}"
        );
    }
}
