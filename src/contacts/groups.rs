//! `groups` tool group — `list_contact_groups` / `get_contact_group`, the
//! `ContactGroup` read tools per
//! [ADR-0024 §Tool inventory](../../docs/adr/0024-contacts-service-surface.md).
//!
//! Both require the `contacts.readonly` OAuth scope. `ContactGroup`s are the
//! label-analog for organizing contacts (ADR-0024): system groups (`myContacts`,
//! `starred`) coexist with user-created groups, flagged via [`GroupType`].
//!
//! **Trust posture** ([ADR-0018](../../docs/adr/0018-email-content-trust.md)):
//! unlike Person fields (which are attacker-influenceable and therefore wrapped
//! `_untrusted` in [`super::people`]), a `ContactGroup`'s `name` / `formatted_name`
//! is **operator-owned or Google-assigned** — there is no path for an external
//! party to create a group in a personal account. They are trusted, mirroring
//! the identical disposition on Gmail's `list_labels`
//! ([`crate::tools::list_labels::LabelItem::name`]). `resource_name`, `etag`,
//! `group_type`, `member_count`, and member `resource_names` are Google-side
//! identifiers, also trusted. This tool group therefore emits no `_untrusted`
//! fields.
//!
//! `modify_contact_group_membership` (write) lands separately in #211.

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::contacts::client::QUERY_COST;
use crate::contacts::etag::person_fields_mask;
use crate::contacts::service::ContactsService;
use crate::error::Error;
use crate::http::percent_encode_path_segment;

/// Default `maxMembers` for `get_contact_group` when the caller omits it. The
/// People API returns **no** member resource names unless `maxMembers > 0`, and
/// the ADR contract is that `get_contact_group` "includes member resource
/// names" — so we request a bounded page by default. A group larger than this
/// is truncated; callers detect that by comparing `member_count` to the length
/// of `member_resource_names`, and can raise `max_members` to fetch more.
const DEFAULT_MAX_MEMBERS: u32 = 100;

// ── Inputs ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct ListContactGroupsInput {
    pub account: String,
    /// People API `groupFields` mask (e.g. `["name", "groupType"]`). Optional —
    /// when empty the parameter is omitted and the API returns its default set.
    pub group_fields: Vec<String>,
    pub page_token: Option<String>,
}

#[derive(Debug)]
pub(crate) struct GetContactGroupInput {
    pub account: String,
    /// Opaque group identifier, e.g. `contactGroups/myContacts`.
    pub resource_name: String,
    pub group_fields: Vec<String>,
    /// Cap on member `resource_names` returned. Defaults to
    /// [`DEFAULT_MAX_MEMBERS`]; `0` returns none.
    pub max_members: Option<u32>,
}

// ── People API response shapes ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupsListResponse {
    #[serde(default)]
    contact_groups: Vec<RawGroup>,
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawGroup {
    resource_name: String,
    etag: Option<String>,
    group_type: Option<GroupType>,
    /// User-set name for user groups; the system key (e.g. `myContacts`) for
    /// system groups.
    name: Option<String>,
    formatted_name: Option<String>,
    member_count: Option<u32>,
    member_resource_names: Vec<String>,
}

/// People API `ContactGroup.groupType`. Mirrors [`super::etag::SourceType`]'s
/// unknown-tolerant shape so a future / unmodeled value never fails the parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum GroupType {
    /// A Google-defined system group (`myContacts`, `starred`, …).
    SystemContactGroup,
    /// A group the account owner created.
    UserContactGroup,
    /// `GROUP_TYPE_UNSPECIFIED` or any unmodeled value.
    #[serde(other)]
    Unknown,
}

// ── Response types ──────────────────────────────────────────────────────────────

/// One contact group in a read response. Every field is trusted — see the
/// module-level trust note.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ContactGroupItem {
    pub resource_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_type: Option<GroupType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatted_name: Option<String>,
    /// Total members in the group, per Google — may exceed
    /// `member_resource_names.len()` when the listing was capped by
    /// `max_members`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_count: Option<u32>,
    /// Member `people/<id>` resource names. Empty on `list_contact_groups`
    /// (Google only returns members on `get`); populated up to `max_members` on
    /// `get_contact_group`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub member_resource_names: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListContactGroupsOutput {
    pub items: Vec<ContactGroupItem>,
    pub next_page_token: Option<String>,
}

// ── Mapping ────────────────────────────────────────────────────────────────────

fn map_group(g: RawGroup) -> ContactGroupItem {
    ContactGroupItem {
        resource_name: g.resource_name,
        etag: g.etag,
        group_type: g.group_type,
        name: g.name,
        formatted_name: g.formatted_name,
        member_count: g.member_count,
        member_resource_names: g.member_resource_names,
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

/// Append a `groupFields` query parameter when the caller supplied a mask. The
/// People API treats an absent mask as "return the default field set", so an
/// empty mask is simply omitted rather than sent as an empty string.
fn push_group_fields(qs: &mut String, group_fields: &[String]) {
    if group_fields.is_empty() {
        return;
    }
    let mask = person_fields_mask(group_fields);
    qs.push_str("&groupFields=");
    qs.push_str(&percent_encode_path_segment(&mask));
}

// ── Core logic ────────────────────────────────────────────────────────────────

/// List the account owner's contact groups (`contactGroups.list`), one page per
/// call. Returns both system groups (`myContacts`, `starred`, …) and
/// user-created groups, distinguished by [`GroupType`].
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "list_contact_groups", tool.account = %input.account),
)]
pub(crate) async fn list_contact_groups<T: RefreshTransport>(
    contacts: &ContactsService<T>,
    input: ListContactGroupsInput,
) -> Result<ListContactGroupsOutput, Error> {
    require_non_empty("account", &input.account)?;

    // `contactGroups.list` paginates via pageToken; start the query string with
    // a stable key so `push_group_fields`'s `&`-prefixed append is always valid.
    let mut qs = String::from("pageSize=100");
    push_group_fields(&mut qs, &input.group_fields);
    if let Some(tok) = input.page_token.as_deref().filter(|t| !t.is_empty()) {
        qs.push_str("&pageToken=");
        qs.push_str(&percent_encode_path_segment(tok));
    }
    let path = format!("/contactGroups?{qs}");
    let resp: GroupsListResponse = contacts
        .client()
        .authed_get(&input.account, &path, QUERY_COST)
        .await?;

    Ok(ListContactGroupsOutput {
        items: resp.contact_groups.into_iter().map(map_group).collect(),
        next_page_token: resp.next_page_token.filter(|t| !t.is_empty()),
    })
}

/// Fetch a single contact group by `resource_name` (`contactGroups.get`),
/// including up to `max_members` member resource names.
#[tracing::instrument(
    skip_all,
    err(Display),
    fields(tool.name = "get_contact_group", tool.account = %input.account),
)]
pub(crate) async fn get_contact_group<T: RefreshTransport>(
    contacts: &ContactsService<T>,
    input: GetContactGroupInput,
) -> Result<ContactGroupItem, Error> {
    require_non_empty("account", &input.account)?;

    // `resource_name` is caller-supplied. Validate the `contactGroups/<id>`
    // shape and encode only the id segment so a crafted value can't smuggle
    // extra path segments or query parameters.
    let id = input
        .resource_name
        .strip_prefix("contactGroups/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .ok_or_else(|| Error::InvalidArgument {
            field: "resource_name".into(),
            detail: "resource_name must look like `contactGroups/<id>`".into(),
        })?;

    let max_members = input.max_members.unwrap_or(DEFAULT_MAX_MEMBERS);
    let mut qs = format!("maxMembers={max_members}");
    push_group_fields(&mut qs, &input.group_fields);
    let path = format!(
        "/contactGroups/{id}?{qs}",
        id = percent_encode_path_segment(id),
    );
    let group: RawGroup = contacts
        .client()
        .authed_get(&input.account, &path, QUERY_COST)
        .await?;

    Ok(map_group(group))
}

// ── Layer 1 unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_fields_unwrapped() {
        let raw: RawGroup = serde_json::from_value(serde_json::json!({
            "resourceName": "contactGroups/abc",
            "etag": "ETAG1",
            "groupType": "USER_CONTACT_GROUP",
            "name": "Family",
            "formattedName": "Family",
            "memberCount": 2,
            "memberResourceNames": ["people/c1", "people/c2"]
        }))
        .unwrap();
        let item = map_group(raw);

        assert_eq!(item.resource_name, "contactGroups/abc");
        assert_eq!(item.etag.as_deref(), Some("ETAG1"));
        assert_eq!(item.group_type, Some(GroupType::UserContactGroup));
        assert_eq!(item.member_count, Some(2));
        assert_eq!(item.member_resource_names.len(), 2);

        // Group fields are trusted: no UNTRUSTED wrapping anywhere in the output.
        let json = serde_json::to_string(&item).unwrap();
        assert!(
            !json.contains("UNTRUSTED"),
            "group output must not wrap: {json}"
        );
        assert!(json.contains("\"name\":\"Family\""));
        assert!(json.contains("\"resource_name\":\"contactGroups/abc\""));
    }

    #[test]
    fn empty_group_omits_optional_fields() {
        let raw: RawGroup =
            serde_json::from_value(serde_json::json!({"resourceName": "contactGroups/x"})).unwrap();
        let json = serde_json::to_value(map_group(raw)).unwrap();
        assert_eq!(json["resource_name"], "contactGroups/x");
        for f in [
            "etag",
            "group_type",
            "name",
            "formatted_name",
            "member_count",
            "member_resource_names",
        ] {
            assert!(json.get(f).is_none(), "{f} should be omitted");
        }
    }

    #[test]
    fn group_type_tolerates_unknown_values() {
        let raw: RawGroup = serde_json::from_value(serde_json::json!({
            "resourceName": "contactGroups/s",
            "groupType": "GROUP_TYPE_UNSPECIFIED"
        }))
        .unwrap();
        assert_eq!(map_group(raw).group_type, Some(GroupType::Unknown));
    }

    #[test]
    fn push_group_fields_omits_when_empty_and_joins_when_present() {
        let mut empty = String::from("pageSize=100");
        push_group_fields(&mut empty, &[]);
        assert_eq!(empty, "pageSize=100");

        let mut present = String::from("pageSize=100");
        push_group_fields(&mut present, &["name".into(), "groupType".into()]);
        assert_eq!(present, "pageSize=100&groupFields=name%2CgroupType");
    }

    #[tokio::test]
    async fn list_requires_non_empty_account() {
        let svc = test_support::service("https://unused.example");
        let err = list_contact_groups(
            &svc,
            ListContactGroupsInput {
                account: String::new(),
                group_fields: vec![],
                page_token: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { ref field, .. } if field == "account"));
    }

    #[tokio::test]
    async fn get_rejects_malformed_resource_name() {
        let svc = test_support::service("https://unused.example");
        for bad in ["abc", "contactGroups/", "contactGroups/a/b", "people/c1"] {
            let err = get_contact_group(
                &svc,
                GetContactGroupInput {
                    account: "work".into(),
                    resource_name: bad.into(),
                    group_fields: vec![],
                    max_members: None,
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

// Shared test harness for the Layer 1 (`tests`) and Layer 2 (`wiremock_tests`)
// modules — mirrors `super::people`'s harness.
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
        let dir = std::env::temp_dir().join(format!("gpm-groups-{}", std::process::id()));
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
    use wiremock::matchers::{method, path, path_regex, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn list_groups_omits_group_fields_and_returns_page_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/contactGroups"))
            .and(query_param("pageSize", "100"))
            .and(query_param_is_missing("groupFields"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "contactGroups": [
                    {"resourceName": "contactGroups/myContacts", "groupType": "SYSTEM_CONTACT_GROUP", "name": "myContacts"},
                    {"resourceName": "contactGroups/u1", "groupType": "USER_CONTACT_GROUP", "name": "Friends"}
                ],
                "nextPageToken": "P2"
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let out = list_contact_groups(
            &svc,
            ListContactGroupsInput {
                account: "work".into(),
                group_fields: vec![],
                page_token: None,
            },
        )
        .await
        .expect("ok");
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.items[0].group_type, Some(GroupType::SystemContactGroup));
        assert_eq!(out.items[1].name.as_deref(), Some("Friends"));
        assert_eq!(out.next_page_token.as_deref(), Some("P2"));
    }

    #[tokio::test]
    async fn list_groups_forwards_group_fields_mask() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/contactGroups"))
            .and(query_param("groupFields", "name,groupType"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "contactGroups": [{"resourceName": "contactGroups/u1", "name": "Friends"}]
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let out = list_contact_groups(
            &svc,
            ListContactGroupsInput {
                account: "work".into(),
                group_fields: vec!["name".into(), "groupType".into()],
                page_token: None,
            },
        )
        .await
        .expect("ok");
        assert_eq!(out.items.len(), 1);
        assert!(out.next_page_token.is_none());
    }

    #[tokio::test]
    async fn get_group_encodes_id_defaults_max_members_and_returns_members() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/contactGroups/u1$"))
            .and(query_param("maxMembers", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resourceName": "contactGroups/u1",
                "etag": "ETAGG",
                "groupType": "USER_CONTACT_GROUP",
                "name": "Friends",
                "formattedName": "Friends",
                "memberCount": 2,
                "memberResourceNames": ["people/c1", "people/c2"]
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let item = get_contact_group(
            &svc,
            GetContactGroupInput {
                account: "work".into(),
                resource_name: "contactGroups/u1".into(),
                group_fields: vec![],
                max_members: None,
            },
        )
        .await
        .expect("ok");
        assert_eq!(item.resource_name, "contactGroups/u1");
        assert_eq!(item.etag.as_deref(), Some("ETAGG"));
        assert_eq!(item.member_count, Some(2));
        assert_eq!(item.member_resource_names.len(), 2);
    }

    #[tokio::test]
    async fn get_group_honors_explicit_max_members_zero() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/contactGroups/u1$"))
            .and(query_param("maxMembers", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resourceName": "contactGroups/u1",
                "memberCount": 5
            })))
            .mount(&server)
            .await;

        let svc = test_support::service(&server.uri());
        let item = get_contact_group(
            &svc,
            GetContactGroupInput {
                account: "work".into(),
                resource_name: "contactGroups/u1".into(),
                group_fields: vec![],
                max_members: Some(0),
            },
        )
        .await
        .expect("ok");
        assert_eq!(item.member_count, Some(5));
        assert!(item.member_resource_names.is_empty());
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/contactGroups"))
            .respond_with(ResponseTemplate::new(403).set_body_string("insufficient scope"))
            .mount(&server)
            .await;
        let svc = test_support::service(&server.uri());
        let err = list_contact_groups(
            &svc,
            ListContactGroupsInput {
                account: "work".into(),
                group_fields: vec![],
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
