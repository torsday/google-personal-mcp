//! Google Contacts service module (People API) per
//! [ADR-0024](../../docs/adr/0024-contacts-service-surface.md).
//!
//! Scaffold only — this ticket (#205) lands the module tree, the
//! [`client::PeopleClient`] HTTP wrapper, the [`service::ContactsService`] seam,
//! and the [`etag`] concurrency + field-mask helpers, mirroring the `gmail/`
//! module shape. No tools are registered yet; each tool group (`people`,
//! `groups`) lands in a follow-up ticket.

pub(crate) mod client;
pub(crate) mod etag;
pub(crate) mod groups;
pub(crate) mod people;
pub(crate) mod service;
