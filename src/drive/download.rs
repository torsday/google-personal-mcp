//! Drive download/export — `download_file` (`files.get?alt=media`) +
//! `export_file` (`files.export`) per
//! [ADR-0025](../../docs/adr/0025-drive-service-surface.md).
//!
//! Stub: the tool implementations land in #215. When they do, the safe
//! filesystem write reuses the ADR-0021 safe-write policy (path-traversal +
//! extension-blocklist + size-cap per
//! [ADR-0021](../../docs/adr/0021-attachment-download-policy.md)) — today
//! implemented inline as `write_to_disk` in
//! [`crate::tools::download_attachment`] (#63), to be lifted into a shared
//! helper so Gmail attachments and Drive downloads enforce one policy from two
//! callers. The Google-native-doc path raises
//! [`crate::error::Error::ExportRequired`]; an unsupported export target raises
//! [`crate::error::Error::UnsupportedExportType`].
//!
//! This module exists now so the scaffold's module tree matches the final shape
//! and follow-up tickets only add code here rather than restructuring
//! `src/drive/`.
