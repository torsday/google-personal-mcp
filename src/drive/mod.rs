//! Google Drive service module per
//! [ADR-0025](../../docs/adr/0025-drive-service-surface.md).
//!
//! Scaffold only — this ticket (#212) lands the module tree, the
//! [`client::DriveClient`] HTTP wrapper, the [`service::DriveService`] seam, the
//! Drive OAuth [`scopes`] vocabulary (the first service with a per-account scope
//! override), and the two Drive-specific typed errors
//! ([`crate::error::Error::ExportRequired`] /
//! [`crate::error::Error::UnsupportedExportType`]), mirroring the `gmail/`,
//! `calendar/`, and `contacts/` module shapes
//! ([ADR-0001](../../docs/adr/0001-monolithic-google-personal-mcp-architecture.md)).
//! No tools are registered yet; each tool group lands in a follow-up ticket:
//! `files` (#213), `download` (#215), file metadata (#217), `permissions`
//! (#220). Shared drives (Team Drives) are out of scope for v1.1 per ADR-0025.

pub(crate) mod client;
pub(crate) mod download;
pub(crate) mod files;
pub(crate) mod permissions;
pub(crate) mod scopes;
pub(crate) mod service;
