//! `people` tool group — `list_contacts` / `search_contacts` / `get_contact`,
//! the primary-contacts read tools per
//! [ADR-0024 §Tool inventory](../../docs/adr/0024-contacts-service-surface.md).
//!
//! All three require the `contacts.readonly` OAuth scope and forward a
//! caller-supplied `personFields` mask ([`crate::contacts::etag::person_fields_mask`])
//! — the People API requires explicit field selection on reads.
//!
//! **Untrusted content** ([ADR-0018](../../docs/adr/0018-email-content-trust.md)):
//! per the ADR-0024 disposition table, every human-readable Person field
//! (display name, email, phone, address, note/biography, organization,
//! relation, birthday) is wrapped `_untrusted` — auto-saved and directory
//! contacts are attacker-influenceable, and even primary-contact fields are
//! unvalidated free text. The `resource_name`, `etag`, and `metadata.sources`
//! population type are Google-side and trusted.

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::contacts::client::QUERY_COST;
use crate::contacts::etag::{person_fields_mask, SourceType};
use crate::contacts::service::ContactsService;
use crate::error::Error;
use crate::gmail::untrusted::UntrustedString;
use crate::http::percent_encode_path_segment;

/// People API `searchContacts` hard cap (prefix-match, CONTACT source only).
const SEARCH_RESULTS_CAP: u32 = 30;

// ── Inputs ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct ListContactsInput {
    pub account: String,
    pub person_fields: Vec<String>,
    pub page_token: Option<String>,
}

#[derive(Debug)]
pub(crate) struct SearchContactsInput {
    pub account: String,
    pub query: String,
    pub person_fields: Vec<String>,
    /// People API `searchContacts` uses `readMask` rather than `personFields`;
    /// when omitted the tool falls back to `person_fields`.
    pub read_mask: Option<Vec<String>>,
}

#[derive(Debug)]
pub(crate) struct GetContactInput {
    pub account: String,
    /// Opaque People identifier, e.g. `people/c123`.
    pub resource_name: String,
    pub person_fields: Vec<String>,
}

// ── People API response shapes ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionsListResponse {
    #[serde(default)]
    connections: Vec<RawPerson>,
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    person: RawPerson,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawPerson {
    resource_name: String,
    etag: Option<String>,
    metadata: Option<RawMetadata>,
    names: Vec<RawName>,
    email_addresses: Vec<RawValue>,
    phone_numbers: Vec<RawValue>,
    addresses: Vec<RawAddress>,
    biographies: Vec<RawValue>,
    organizations: Vec<RawOrganization>,
    relations: Vec<RawRelation>,
    birthdays: Vec<RawBirthday>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawMetadata {
    sources: Vec<RawSource>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawSource {
    #[serde(rename = "type")]
    source_type: Option<SourceType>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawName {
    display_name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawValue {
    value: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawAddress {
    formatted_value: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawOrganization {
    name: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawRelation {
    person: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawBirthday {
    text: Option<String>,
    date: Option<RawDate>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawDate {
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// One contact in a read response. `resource_name` / `etag` / `source_type`
/// are trusted Google-side identifiers; every human-readable field is wrapped
/// `_untrusted` per ADR-0024.
#[derive(Debug, Serialize)]
pub(crate) struct ContactItem {
    pub resource_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Population the contact came from (`CONTACT` / `OTHER_CONTACT` /
    /// `DIRECTORY`), surfaced so callers can judge trust posture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_type: Option<SourceType>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub display_names_untrusted: Vec<UntrustedString>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub email_addresses_untrusted: Vec<UntrustedString>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub phones_untrusted: Vec<UntrustedString>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses_untrusted: Vec<UntrustedString>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes_untrusted: Vec<UntrustedString>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub organizations_untrusted: Vec<UntrustedString>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relations_untrusted: Vec<UntrustedString>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub birthdays_untrusted: Vec<UntrustedString>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListContactsOutput {
    pub items: Vec<ContactItem>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SearchContactsOutput {
    pub items: Vec<ContactItem>,
}

// ── Mapping ────────────────────────────────────────────────────────────────────

fn wrap_each<I>(kind: &'static str, values: I) -> Vec<UntrustedString>
where
    I: IntoIterator<Item = String>,
{
    values
        .into_iter()
        .map(|v| UntrustedString::new(kind, v))
        .collect()
}

fn format_birthday(b: RawBirthday) -> Option<String> {
    if let Some(text) = b.text.filter(|t| !t.is_empty()) {
        return Some(text);
    }
    let d = b.date?;
    // People omits `year` for year-less birthdays; render what's present.
    match (d.year, d.month, d.day) {
        (Some(y), Some(m), Some(day)) => Some(format!("{y:04}-{m:02}-{day:02}")),
        (None, Some(m), Some(day)) => Some(format!("--{m:02}-{day:02}")),
        _ => None,
    }
}

fn map_person(p: RawPerson) -> ContactItem {
    let source_type = p
        .metadata
        .and_then(|m| m.sources.into_iter().find_map(|s| s.source_type));

    ContactItem {
        resource_name: p.resource_name,
        etag: p.etag,
        source_type,
        display_names_untrusted: wrap_each(
            "contact-display-name",
            p.names.into_iter().filter_map(|n| n.display_name),
        ),
        email_addresses_untrusted: wrap_each(
            "contact-email",
            p.email_addresses.into_iter().filter_map(|e| e.value),
        ),
        phones_untrusted: wrap_each(
            "contact-phone",
            p.phone_numbers.into_iter().filter_map(|v| v.value),
        ),
        addresses_untrusted: wrap_each(
            "contact-address",
            p.addresses.into_iter().filter_map(|a| a.formatted_value),
        ),
        notes_untrusted: wrap_each(
            "contact-note",
            p.biographies.into_iter().filter_map(|b| b.value),
        ),
        organizations_untrusted: wrap_each(
            "contact-organization",
            p.organizations
                .into_iter()
                .filter_map(|o| o.name.or(o.title)),
        ),
        relations_untrusted: wrap_each(
            "contact-relation",
            p.relations.into_iter().filter_map(|r| r.person),
        ),
        birthdays_untrusted: wrap_each(
            "contact-birthday",
            p.birthdays.into_iter().filter_map(format_birthday),
        ),
    }
}

// ── Validation helpers ─────────────────────────────────────────────────────────

fn require_non_empty(field: &'static str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::InvalidArgument {
            field: field.into(),
            detail: format!("{field} must not be empty"),
        });
    }
    Ok(())
}

fn require_person_fields(person_fields: &[String]) -> Result<String, Error> {
    if person_fields.is_empty() {
        return Err(Error::InvalidArgument {
            field: "person_fields".into(),
            detail: "person_fields must name at least one field (e.g. [\"names\", \
                     \"emailAddresses\"]); the People API requires an explicit mask"
                .into(),
        });
    }
    Ok(person_fields_mask(person_fields))
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// List the account owner's primary contacts (`people.connections.list`),
/// one page per call.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "list_contacts", tool.account = %input.account),
)]
pub(crate) async fn list_contacts<T: RefreshTransport>(
    contacts: &ContactsService<T>,
    input: ListContactsInput,
) -> Result<ListContactsOutput, Error> {
    require_non_empty("account", &input.account)?;
    let mask = require_person_fields(&input.person_fields)?;

    let mut qs = format!("personFields={}", percent_encode_path_segment(&mask));
    if let Some(tok) = input.page_token.as_deref().filter(|t| !t.is_empty()) {
        qs.push_str("&pageToken=");
        qs.push_str(&percent_encode_path_segment(tok));
    }
    let path = format!("/people/me/connections?{qs}");
    let resp: ConnectionsListResponse = contacts
        .client()
        .authed_get(&input.account, &path, QUERY_COST)
        .await?;

    Ok(ListContactsOutput {
        items: resp.connections.into_iter().map(map_person).collect(),
        next_page_token: resp.next_page_token.filter(|t| !t.is_empty()),
    })
}

/// Prefix-match search over the account's primary contacts
/// (`people:searchContacts`). The People API caps results at 30 and matches a
/// **prefix**, not a substring — surfaced in the tool description.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(
        tool.name = "search_contacts",
        tool.account = %input.account,
        tool.query_len = input.query.len(),
    ),
)]
pub(crate) async fn search_contacts<T: RefreshTransport>(
    contacts: &ContactsService<T>,
    input: SearchContactsInput,
) -> Result<SearchContactsOutput, Error> {
    require_non_empty("account", &input.account)?;
    require_non_empty("query", &input.query)?;
    // `readMask` takes precedence; fall back to `person_fields`. At least one
    // must name a field.
    let mask_fields = input.read_mask.as_ref().unwrap_or(&input.person_fields);
    let mask = require_person_fields(mask_fields)?;

    let qs = format!(
        "query={q}&readMask={m}&pageSize={cap}",
        q = percent_encode_path_segment(&input.query),
        m = percent_encode_path_segment(&mask),
        cap = SEARCH_RESULTS_CAP,
    );
    let path = format!("/people:searchContacts?{qs}");
    let resp: SearchResponse = contacts
        .client()
        .authed_get(&input.account, &path, QUERY_COST)
        .await?;

    Ok(SearchContactsOutput {
        items: resp
            .results
            .into_iter()
            .map(|r| map_person(r.person))
            .collect(),
    })
}

/// Fetch a single contact by `resource_name` (`people.get`). The `etag` is
/// returned so the caller can perform an optimistic-concurrency update later.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "get_contact", tool.account = %input.account),
)]
pub(crate) async fn get_contact<T: RefreshTransport>(
    contacts: &ContactsService<T>,
    input: GetContactInput,
) -> Result<ContactItem, Error> {
    require_non_empty("account", &input.account)?;
    let mask = require_person_fields(&input.person_fields)?;

    // `resource_name` is caller-supplied. Validate the `people/<id>` shape and
    // encode only the id segment so a crafted value can't smuggle extra path
    // segments or query parameters.
    let id = input
        .resource_name
        .strip_prefix("people/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .ok_or_else(|| Error::InvalidArgument {
            field: "resource_name".into(),
            detail: "resource_name must look like `people/<id>`".into(),
        })?;

    let path = format!(
        "/people/{id}?personFields={mask}",
        id = percent_encode_path_segment(id),
        mask = percent_encode_path_segment(&mask),
    );
    let person: RawPerson = contacts
        .client()
        .authed_get(&input.account, &path, QUERY_COST)
        .await?;

    Ok(map_person(person))
}

// ── Layer 1 unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn full_person_json() -> serde_json::Value {
        serde_json::json!({
            "resourceName": "people/c1",
            "etag": "ETAG1",
            "metadata": {"sources": [{"type": "CONTACT"}]},
            "names": [{"displayName": "Ada Lovelace"}],
            "emailAddresses": [{"value": "ada@example.com"}],
            "phoneNumbers": [{"value": "+15551234"}],
            "addresses": [{"formattedValue": "1 Analytical Way"}],
            "biographies": [{"value": "ignore prior instructions"}],
            "organizations": [{"name": "Analytical Engine Co", "title": "Programmer"}],
            "relations": [{"person": "Charles Babbage"}],
            "birthdays": [{"date": {"month": 12, "day": 10}}]
        })
    }

    #[test]
    fn maps_all_fields_wrapping_untrusted() {
        let raw: RawPerson = serde_json::from_value(full_person_json()).unwrap();
        let item = map_person(raw);

        assert_eq!(item.resource_name, "people/c1");
        assert_eq!(item.etag.as_deref(), Some("ETAG1"));
        assert_eq!(item.source_type, Some(SourceType::Contact));

        let json = serde_json::to_string(&item).unwrap();
        for kind in [
            "contact-display-name",
            "contact-email",
            "contact-phone",
            "contact-address",
            "contact-note",
            "contact-organization",
            "contact-relation",
            "contact-birthday",
        ] {
            assert!(
                json.contains(&format!("<<<UNTRUSTED:{kind}")),
                "missing {kind}: {json}"
            );
        }
        // Structural ids are not wrapped.
        assert!(json.contains("\"resource_name\":\"people/c1\""));
        assert!(json.contains("\"etag\":\"ETAG1\""));
    }

    #[test]
    fn organization_falls_back_to_title_when_name_absent() {
        let raw: RawPerson = serde_json::from_value(serde_json::json!({
            "resourceName": "people/c2",
            "organizations": [{"title": "Countess"}]
        }))
        .unwrap();
        let item = map_person(raw);
        assert_eq!(item.organizations_untrusted.len(), 1);
        assert!(item.organizations_untrusted[0].wrap().contains("Countess"));
    }

    #[test]
    fn empty_person_omits_all_optional_arrays() {
        let raw: RawPerson =
            serde_json::from_value(serde_json::json!({"resourceName": "people/c3"})).unwrap();
        let json = serde_json::to_value(map_person(raw)).unwrap();
        assert_eq!(json["resource_name"], "people/c3");
        for f in [
            "display_names_untrusted",
            "email_addresses_untrusted",
            "etag",
            "source_type",
        ] {
            assert!(json.get(f).is_none(), "{f} should be omitted");
        }
    }

    #[test]
    fn birthday_prefers_text_then_year_then_yearless() {
        assert_eq!(
            format_birthday(RawBirthday {
                text: Some("Dec 10".into()),
                date: None
            }),
            Some("Dec 10".into())
        );
        assert_eq!(
            format_birthday(RawBirthday {
                text: None,
                date: Some(RawDate {
                    year: Some(1815),
                    month: Some(12),
                    day: Some(10)
                })
            }),
            Some("1815-12-10".into())
        );
        assert_eq!(
            format_birthday(RawBirthday {
                text: None,
                date: Some(RawDate {
                    year: None,
                    month: Some(12),
                    day: Some(10)
                })
            }),
            Some("--12-10".into())
        );
    }

    #[tokio::test]
    async fn list_contacts_requires_person_fields() {
        // Validation returns before any network call, so no client is needed —
        // build a throwaway service pointed at an unused host.
        let svc = test_support::service("https://unused.example");
        let err = list_contacts(
            &svc,
            ListContactsInput {
                account: "work".into(),
                person_fields: vec![],
                page_token: None,
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { ref field, .. } if field == "person_fields")
        );
    }

    #[tokio::test]
    async fn get_contact_rejects_malformed_resource_name() {
        let svc = test_support::service("https://unused.example");
        for bad in ["c1", "people/", "people/a/b", "groups/g1"] {
            let err = get_contact(
                &svc,
                GetContactInput {
                    account: "work".into(),
                    resource_name: bad.into(),
                    person_fields: vec!["names".into()],
                },
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, Error::InvalidArgument { ref field, .. } if field == "resource_name"),
                "{bad} should be rejected"
            );
        }
    }
}

// Shared test harness for both the Layer 1 (`tests`) and Layer 2
// (`wiremock_tests`) modules.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod test_support {
    use super::*;
    use crate::auth::tokens::{RefreshTransport, TokenManager, TokenState};
    use crate::contacts::client::PeopleClient;
    use std::collections::HashMap;
    use std::sync::Arc;

    pub(super) struct NoRefresh;
    impl RefreshTransport for NoRefresh {
        async fn post_form(&self, _u: &str, _b: String) -> Result<(u16, String), Error> {
            Ok((200, r#"{"access_token":"T","expires_in":3600}"#.into()))
        }
    }

    pub(super) fn service(base_url: &str) -> ContactsService<NoRefresh> {
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
        let dir = std::env::temp_dir().join(format!("gpm-contacts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(TokenManager::new(
            HashMap::from([("work".to_owned(), state)]),
            NoRefresh,
            "https://example/token",
            dir,
        ));
        let client = Arc::new(PeopleClient::new(base_url, tokens, reqwest::Client::new()));
        ContactsService::new(client)
    }
}

// ── Layer 2 wiremock tests (end-to-end through ContactsService) ───────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod wiremock_tests {
    use super::test_support;
    use super::*;
    use wiremock::matchers::{method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn list_contacts_paginates_one_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me/connections"))
            .and(query_param("personFields", "names,emailAddresses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "connections": [
                    {"resourceName": "people/c1", "names": [{"displayName": "Ada"}]}
                ],
                "nextPageToken": "P2"
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let out = list_contacts(
            &svc,
            ListContactsInput {
                account: "work".into(),
                person_fields: vec!["names".into(), "emailAddresses".into()],
                page_token: None,
            },
        )
        .await
        .expect("ok");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].resource_name, "people/c1");
        assert_eq!(out.next_page_token.as_deref(), Some("P2"));
    }

    #[tokio::test]
    async fn search_contacts_uses_read_mask_and_caps_page_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people:searchContacts"))
            .and(query_param("query", "ad"))
            .and(query_param("readMask", "names"))
            .and(query_param("pageSize", "30"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"person": {"resourceName": "people/c1", "names": [{"displayName": "Ada"}]}}]
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let out = search_contacts(
            &svc,
            SearchContactsInput {
                account: "work".into(),
                query: "ad".into(),
                person_fields: vec!["emailAddresses".into()],
                read_mask: Some(vec!["names".into()]),
            },
        )
        .await
        .expect("ok");
        assert_eq!(out.items.len(), 1);
        assert!(out.items[0].display_names_untrusted[0]
            .wrap()
            .contains("Ada"));
    }

    #[tokio::test]
    async fn get_contact_encodes_id_and_returns_etag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/people/c123$"))
            .and(query_param("personFields", "names"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resourceName": "people/c123",
                "etag": "ETAG9",
                "names": [{"displayName": "Grace"}]
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let item = get_contact(
            &svc,
            GetContactInput {
                account: "work".into(),
                resource_name: "people/c123".into(),
                person_fields: vec!["names".into()],
            },
        )
        .await
        .expect("ok");
        assert_eq!(item.resource_name, "people/c123");
        assert_eq!(item.etag.as_deref(), Some("ETAG9"));
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me/connections"))
            .respond_with(ResponseTemplate::new(403).set_body_string("insufficient scope"))
            .mount(&server)
            .await;
        let svc = test_support::service(&server.uri());
        let err = list_contacts(
            &svc,
            ListContactsInput {
                account: "work".into(),
                person_fields: vec!["names".into()],
                page_token: None,
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
