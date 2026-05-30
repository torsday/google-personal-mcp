//! Contacts concurrency + field-mask helpers per
//! [ADR-0024](../../docs/adr/0024-contacts-service-surface.md).
//!
//! Scaffold surface consumed by the Contacts tool tickets (#206+).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Serialize a People API field mask (`personFields` / `updatePersonFields`).
/// The API wants a single comma-delimited string, e.g.
/// `"names,emailAddresses,phoneNumbers"`.
pub(crate) fn person_fields_mask(fields: &[String]) -> String {
    fields.join(",")
}

/// Optimistic-concurrency guard for People API mutations. The API requires the
/// client to echo the `etag` it last read; a mismatch means the resource
/// changed underneath us, so the write must not proceed. Returns
/// [`Error::ConcurrencyConflict`] with the actionable "re-fetch and re-apply"
/// hint (ADR-0024 / [ADR-0005](../../docs/adr/0005-error-model.md)).
pub(crate) fn ensure_etag_matches(
    resource: &str,
    expected_etag: &str,
    actual_etag: &str,
) -> Result<(), Error> {
    if expected_etag == actual_etag {
        Ok(())
    } else {
        Err(Error::ConcurrencyConflict {
            resource: resource.to_owned(),
            hint: format!("re-fetch `{resource}` and re-apply"),
        })
    }
}

/// People API `metadata.sources[].type` population, surfaced on every read so
/// callers can distinguish primary contacts from auto-saved "other contacts"
/// and Workspace directory entries — three different trust postures inside the
/// `read` aspect (ADR-0024 §read populations; the basis for the ADR-0022
/// per-tool override carve-out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum SourceType {
    /// A user-created contact (`people.connections`).
    Contact,
    /// Auto-saved from Gmail interactions (`otherContacts`).
    OtherContact,
    /// A Workspace directory entry (`listDirectoryPeople`).
    Directory,
    /// Any other People API source type (`ACCOUNT`, `PROFILE`, `DOMAIN_*`).
    /// Captured so deserializing a real response never fails on an unmodeled
    /// value.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn person_fields_mask_joins_with_commas() {
        assert_eq!(
            person_fields_mask(&["names".into(), "emailAddresses".into()]),
            "names,emailAddresses"
        );
    }

    #[test]
    fn person_fields_mask_single_and_empty() {
        assert_eq!(person_fields_mask(&["names".into()]), "names");
        assert_eq!(person_fields_mask(&[]), "");
    }

    #[test]
    fn ensure_etag_matches_ok_when_equal() {
        assert!(ensure_etag_matches("people/c1", "abc", "abc").is_ok());
    }

    #[test]
    fn ensure_etag_mismatch_is_concurrency_conflict_with_hint() {
        let err = ensure_etag_matches("people/c1", "abc", "xyz").expect_err("must conflict");
        match err {
            Error::ConcurrencyConflict { resource, hint } => {
                assert_eq!(resource, "people/c1");
                assert!(
                    hint.contains("re-fetch") && hint.contains("people/c1"),
                    "got: {hint}"
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn source_type_deserializes_api_values() {
        assert_eq!(
            serde_json::from_str::<SourceType>("\"CONTACT\"").unwrap(),
            SourceType::Contact
        );
        assert_eq!(
            serde_json::from_str::<SourceType>("\"OTHER_CONTACT\"").unwrap(),
            SourceType::OtherContact
        );
        assert_eq!(
            serde_json::from_str::<SourceType>("\"DIRECTORY\"").unwrap(),
            SourceType::Directory
        );
        // Unmodeled values fall through to Unknown rather than failing.
        assert_eq!(
            serde_json::from_str::<SourceType>("\"DOMAIN_PROFILE\"").unwrap(),
            SourceType::Unknown
        );
    }

    #[test]
    fn source_type_serializes_to_screaming_snake() {
        assert_eq!(
            serde_json::to_string(&SourceType::OtherContact).unwrap(),
            "\"OTHER_CONTACT\""
        );
    }
}
