//! `CalendarService` — the seam above [`CalendarClient`], mirroring
//! [`crate::gmail::service::GmailService`].
//!
//! For Gmail this seam is where cache lookup happens; Calendar ships without a
//! cache for now, so the scaffold version simply owns the client and hands it
//! to tools via [`CalendarService::client`] / [`CalendarService::client_arc`].
//! A cache slot can be added here later without changing tool call sites — the
//! same reason the Gmail seam exists.
//!
//! Scaffold module: tools that consume this land in follow-up tickets (#200+).
#![allow(dead_code)]

use std::sync::Arc;

use crate::auth::tokens::RefreshTransport;

use super::client::CalendarClient;

pub(crate) struct CalendarService<T: RefreshTransport> {
    client: Arc<CalendarClient<T>>,
}

impl<T: RefreshTransport> std::fmt::Debug for CalendarService<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalendarService").finish_non_exhaustive()
    }
}

impl<T: RefreshTransport> CalendarService<T> {
    /// Build a service wrapping `client`. Mirrors
    /// [`crate::gmail::service::GmailService::new`]; a cache slot is omitted
    /// until Calendar caching is designed.
    pub(crate) const fn new(client: Arc<CalendarClient<T>>) -> Self {
        Self { client }
    }

    /// Borrow the underlying HTTP client — the path tools take for endpoints
    /// with no cacheable shape (all of them, for now).
    pub(crate) fn client(&self) -> &CalendarClient<T> {
        &self.client
    }

    /// `Arc<CalendarClient<T>>` clone for tools that move the client into a
    /// `'static` fan-out future. Mirrors `GmailService::client_arc`.
    pub(crate) fn client_arc(&self) -> Arc<CalendarClient<T>> {
        Arc::clone(&self.client)
    }
}
