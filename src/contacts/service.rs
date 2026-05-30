//! `ContactsService` — the seam above [`PeopleClient`], mirroring
//! [`crate::gmail::service::GmailService`] / [`crate::calendar::service::CalendarService`].
//!
//! Contacts ships without a cache for now, so the scaffold simply owns the
//! client and hands it to tools via [`ContactsService::client`] /
//! [`ContactsService::client_arc`]. A cache slot can be added later without
//! changing tool call sites.
//!
//! Scaffold module: tools that consume this land in follow-up tickets (#206+).
#![allow(dead_code)]

use std::sync::Arc;

use crate::auth::tokens::RefreshTransport;

use super::client::PeopleClient;

pub(crate) struct ContactsService<T: RefreshTransport> {
    client: Arc<PeopleClient<T>>,
}

impl<T: RefreshTransport> std::fmt::Debug for ContactsService<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContactsService").finish_non_exhaustive()
    }
}

impl<T: RefreshTransport> ContactsService<T> {
    /// Build a service wrapping `client`. Mirrors `GmailService::new`; a cache
    /// slot is omitted until Contacts caching is designed.
    pub(crate) const fn new(client: Arc<PeopleClient<T>>) -> Self {
        Self { client }
    }

    /// Borrow the underlying HTTP client — the path tools take for People API
    /// endpoints (all of them, for now).
    pub(crate) fn client(&self) -> &PeopleClient<T> {
        &self.client
    }

    /// `Arc<PeopleClient<T>>` clone for tools that move the client into a
    /// `'static` future. Mirrors `GmailService::client_arc`.
    pub(crate) fn client_arc(&self) -> Arc<PeopleClient<T>> {
        Arc::clone(&self.client)
    }
}
