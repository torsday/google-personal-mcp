//! `list_accounts` tool — returns all accounts registered in `accounts.toml`.
//!
//! No `account` parameter: this tool doesn't touch Google, it reads
//! local config per [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md).

use serde::Serialize;

use crate::config::AccountEntry;

// ── Response types ────────────────────────────────────────────────────────────

/// A single account entry in the `list_accounts` response.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct AccountItem {
    pub alias: String,
    pub email: String,
    /// `true` for every account present in `accounts.toml`. An account
    /// that has been explicitly disabled by the operator (future feature)
    /// would have `enabled: false`. For v0.2 all registered accounts are
    /// considered enabled.
    pub enabled: bool,
}

/// Response envelope per ADR-0016 §`list_accounts`.
#[derive(Debug, Serialize)]
pub(crate) struct ListAccountsOutput {
    pub items: Vec<AccountItem>,
}

// ── Pure logic ────────────────────────────────────────────────────────────────

/// Build the `list_accounts` response from a slice of config entries.
/// Pure — no I/O, no network.
pub(crate) fn list_accounts(entries: &[AccountEntry]) -> ListAccountsOutput {
    let items = entries
        .iter()
        .map(|e| AccountItem {
            alias: e.alias.clone(),
            email: e.email.clone(),
            enabled: true, // v0.2: all registered accounts are enabled
        })
        .collect();
    ListAccountsOutput { items }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::AccountEntry;

    fn entry(alias: &str, email: &str) -> AccountEntry {
        AccountEntry {
            alias: alias.into(),
            email: email.into(),
            default: false,
        }
    }

    #[test]
    fn empty_entries_returns_empty_list() {
        let out = list_accounts(&[]);
        assert!(out.items.is_empty());
    }

    #[test]
    fn single_entry_maps_correctly() {
        let out = list_accounts(&[entry("personal", "alice@gmail.com")]);
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].alias, "personal");
        assert_eq!(out.items[0].email, "alice@gmail.com");
        assert!(out.items[0].enabled);
    }

    #[test]
    fn multiple_entries_preserve_order() {
        let entries = vec![
            entry("personal", "alice@gmail.com"),
            entry("work", "alice@company.com"),
        ];
        let out = list_accounts(&entries);
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.items[0].alias, "personal");
        assert_eq!(out.items[1].alias, "work");
    }

    #[test]
    fn all_items_have_enabled_true() {
        let entries = vec![entry("a", "a@example.com"), entry("b", "b@example.com")];
        let out = list_accounts(&entries);
        assert!(out.items.iter().all(|i| i.enabled));
    }

    #[test]
    fn output_serialises_to_expected_shape() {
        let out = list_accounts(&[entry("p", "p@g.com")]);
        let json = serde_json::to_value(&out).expect("serialise");
        assert_eq!(json["items"][0]["alias"], "p");
        assert_eq!(json["items"][0]["email"], "p@g.com");
        assert_eq!(json["items"][0]["enabled"], true);
    }
}
