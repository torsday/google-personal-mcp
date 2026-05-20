//! `list_labels` tool — returns all Gmail labels for an account.
//!
//! Calls `users.labels.list` (1 quota unit) and maps the response
//! per [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md)
//! §`list_labels`.

use serde::{Deserialize, Serialize};

use crate::auth::tokens::RefreshTransport;
use crate::error::Error;
use crate::gmail::client::GmailClient;
use crate::gmail::quota::GmailMethod;

// ── Gmail API response shapes ─────────────────────────────────────────────────

/// Raw response from `users.labels.list`.
#[derive(Debug, Deserialize)]
struct LabelsListResponse {
    #[serde(default)]
    labels: Vec<RawLabel>,
}

/// A single label as returned by the Gmail API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLabel {
    id: String,
    name: String,
    /// `"system"` or `"user"` per the Gmail API.
    #[serde(rename = "type")]
    label_type: String,
    messages_total: Option<u32>,
    messages_unread: Option<u32>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// A single label item in the `list_labels` response.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct LabelItem {
    pub label_id: String,
    /// User-assigned name or well-known system name (e.g. `INBOX`, `STARRED`).
    /// Operator-owned or Google-assigned — treated as trusted per ADR-0018.
    pub name: String,
    /// `"system"` or `"user"`.
    pub kind: String,
    pub messages_total: Option<u32>,
    pub messages_unread: Option<u32>,
}

/// Response envelope per ADR-0016 §`list_labels`.
#[derive(Debug, Serialize)]
pub(crate) struct ListLabelsOutput {
    pub items: Vec<LabelItem>,
}

// ── Core logic ────────────────────────────────────────────────────────────────

fn map_raw_label(r: RawLabel) -> LabelItem {
    LabelItem {
        label_id: r.id,
        name: r.name,
        kind: r.label_type,
        messages_total: r.messages_total,
        messages_unread: r.messages_unread,
    }
}

/// Fetch all labels for `account` and return the listing envelope.
pub(crate) async fn list_labels<T: RefreshTransport>(
    client: &GmailClient<T>,
    account: &str,
) -> Result<ListLabelsOutput, Error> {
    if account.is_empty() {
        return Err(Error::InvalidArgument {
            field: "account".into(),
            detail: "account alias must not be empty".into(),
        });
    }

    let path = format!("/users/{account}/labels");
    let resp: LabelsListResponse = client
        .authed_get(account, &path, GmailMethod::LabelsList.cost())
        .await?;

    let items = resp.labels.into_iter().map(map_raw_label).collect();
    Ok(ListLabelsOutput { items })
}

// ── Pure-logic unit tests (Layer 1 — no I/O) ─────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn raw(id: &str, name: &str, label_type: &str) -> RawLabel {
        RawLabel {
            id: id.into(),
            name: name.into(),
            label_type: label_type.into(),
            messages_total: None,
            messages_unread: None,
        }
    }

    #[test]
    fn map_raw_label_system() {
        let item = map_raw_label(raw("INBOX", "INBOX", "system"));
        assert_eq!(item.label_id, "INBOX");
        assert_eq!(item.name, "INBOX");
        assert_eq!(item.kind, "system");
        assert!(item.messages_total.is_none());
        assert!(item.messages_unread.is_none());
    }

    #[test]
    fn map_raw_label_user_with_counts() {
        let r = RawLabel {
            id: "Label_1".into(),
            name: "Work".into(),
            label_type: "user".into(),
            messages_total: Some(42),
            messages_unread: Some(3),
        };
        let item = map_raw_label(r);
        assert_eq!(item.kind, "user");
        assert_eq!(item.messages_total, Some(42));
        assert_eq!(item.messages_unread, Some(3));
    }

    #[test]
    fn output_serialises_to_expected_shape() {
        let out = ListLabelsOutput {
            items: vec![LabelItem {
                label_id: "INBOX".into(),
                name: "INBOX".into(),
                kind: "system".into(),
                messages_total: Some(10),
                messages_unread: Some(2),
            }],
        };
        let json = serde_json::to_value(&out).expect("serialise");
        assert_eq!(json["items"][0]["label_id"], "INBOX");
        assert_eq!(json["items"][0]["name"], "INBOX");
        assert_eq!(json["items"][0]["kind"], "system");
        assert_eq!(json["items"][0]["messages_total"], 10);
        assert_eq!(json["items"][0]["messages_unread"], 2);
    }
}
