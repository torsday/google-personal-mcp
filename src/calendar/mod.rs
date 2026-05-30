//! Google Calendar service module per
//! [ADR-0023](../../docs/adr/0023-calendar-service-surface.md).
//!
//! Scaffold only — this ticket (#199) lands the module tree, the
//! [`client::CalendarClient`] HTTP wrapper, and the [`service::CalendarService`]
//! seam, mirroring the `gmail/` module shape ([ADR-0001](../../docs/adr/0001-monolithic-google-personal-mcp-architecture.md)).
//! No tools are registered yet; each tool group (`calendars`, `events`,
//! `freebusy`) lands in a follow-up ticket.

pub(crate) mod calendars;
pub(crate) mod client;
pub(crate) mod events;
pub(crate) mod freebusy;
pub(crate) mod service;
