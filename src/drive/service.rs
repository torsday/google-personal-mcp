//! `DriveService` — the seam above [`DriveClient`], mirroring
//! [`crate::gmail::service::GmailService`] /
//! [`crate::calendar::service::CalendarService`] /
//! [`crate::contacts::service::ContactsService`].
//!
//! Drive ships without a cache for now, so the scaffold simply owns the client
//! and hands it to tools via [`DriveService::client`] /
//! [`DriveService::client_arc`]. A cache slot can be added later without
//! changing tool call sites — the same reason the Gmail seam exists.
//!
//! Scaffold module: tools that consume this land in follow-up tickets
//! (#213/#215/#217/#220).
#![allow(dead_code)]

use std::sync::Arc;

use crate::auth::tokens::RefreshTransport;

use super::client::DriveClient;

pub(crate) struct DriveService<T: RefreshTransport> {
    client: Arc<DriveClient<T>>,
}

impl<T: RefreshTransport> std::fmt::Debug for DriveService<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveService").finish_non_exhaustive()
    }
}

impl<T: RefreshTransport> DriveService<T> {
    /// Build a service wrapping `client`. Mirrors `GmailService::new`; a cache
    /// slot is omitted until Drive caching is designed.
    pub(crate) const fn new(client: Arc<DriveClient<T>>) -> Self {
        Self { client }
    }

    /// Borrow the underlying HTTP client — the path tools take for Drive API
    /// endpoints (all of them, for now).
    pub(crate) fn client(&self) -> &DriveClient<T> {
        &self.client
    }

    /// `Arc<DriveClient<T>>` clone for tools that move the client into a
    /// `'static` future. Mirrors `GmailService::client_arc`.
    pub(crate) fn client_arc(&self) -> Arc<DriveClient<T>> {
        Arc::clone(&self.client)
    }
}
