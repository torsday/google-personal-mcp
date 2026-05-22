//! Transport-layer infrastructure used by the (future) Streamable HTTP
//! server transport per [ADR-0003](../../docs/adr/0003-transport-stdio-and-streamable-http.md).
//!
//! Only [`session`] ships in v0.3 — the actual HTTP serving loop is
//! gated on issue #72 and lands separately. The session module is
//! foundational: when #72 wires the transport in, it will create one
//! [`SessionStore`] at startup and call [`SessionStore::touch`] on every
//! inbound request to refresh the idle timer.

pub(crate) mod session;
