//! Liveness `/healthz` HTTP endpoint per
//! [ADR-0008 §Health endpoint](../docs/adr/0008-observability-and-deployment.md)
//! (issue #70).
//!
//! Bound to the same internal listener that will host `/metrics` once
//! [#75](https://github.com/torsday/google-personal-mcp/issues/75) ships.
//! v0.x scope: only `/healthz` — Prometheus exporter is v1.0. We
//! intentionally hand-roll the request reader instead of pulling in
//! `axum`/`hyper`: one route, ≤ 16-byte requests, no need for routing
//! infrastructure or extra dep weight on a daemon that already
//! exclusively serves stdio.
//!
//! ## Response contract (mirrors ADR-0008 lines 142-147)
//!
//! - `GET /healthz` → `200 OK` body `ok` when every invariant holds
//! - `GET /healthz` → `503 Service Unavailable` body `<reason>` when an
//!   invariant fails (currently: zero accounts in the registry)
//! - Anything else → `404 Not Found` body `not found`
//!
//! Hot-reload state ("last reload succeeded") isn't wired yet — there's
//! no reload subsystem on the daemon. Once one lands, flip
//! [`HealthState::last_reload_succeeded`] from `AtomicBool`'s
//! initial-true to the actual outcome.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Per-process liveness snapshot. Cheap to clone (`Arc` wrapped) and
/// safe to mutate concurrently — every field is atomic. Constructed once
/// at startup; readers (`/healthz` connection handlers) and writers
/// (future hot-reload, future account-state observers) share it.
#[derive(Debug)]
pub(crate) struct HealthState {
    /// Number of accounts present in `TokenManager`'s registry at
    /// startup. ADR-0008 line 145 makes "≥ 1 account configured" a hard
    /// liveness gate.
    accounts_configured: usize,
    /// Outcome of the most recent config/account hot-reload, if any.
    /// `true` initially because at startup the only reload-equivalent
    /// (cold load) is the path that brought us here — by the time we
    /// answer `/healthz`, startup succeeded.
    last_reload_succeeded: AtomicBool,
}

impl HealthState {
    pub(crate) const fn new(accounts_configured: usize) -> Self {
        Self {
            accounts_configured,
            last_reload_succeeded: AtomicBool::new(true),
        }
    }

    /// Reload outcomes flow through here. Unused today; wired so the
    /// reload work (no ticket yet) doesn't need to plumb a new path.
    #[allow(dead_code)]
    pub(crate) fn set_last_reload_succeeded(&self, ok: bool) {
        self.last_reload_succeeded.store(ok, Ordering::Relaxed);
    }

    /// Compute the response. Returns `(status_line, body)` so the
    /// connection handler doesn't have to know the policy.
    fn evaluate(&self) -> (&'static str, &'static str) {
        if self.accounts_configured == 0 {
            return ("HTTP/1.1 503 Service Unavailable", "no accounts configured");
        }
        if !self.last_reload_succeeded.load(Ordering::Relaxed) {
            return ("HTTP/1.1 503 Service Unavailable", "last reload failed");
        }
        ("HTTP/1.1 200 OK", "ok")
    }
}

/// Drive the listener: accept connections, dispatch each to a
/// fire-and-forget task. Returns only when accept errors fatally
/// (e.g. file-descriptor exhaustion or the socket is closed). The
/// caller decides whether that's terminal or recoverable.
pub(crate) async fn run(listener: TcpListener, state: Arc<HealthState>) -> io::Result<()> {
    tracing::info!(
        addr = ?listener.local_addr().ok(),
        "healthz listener accepting"
    );
    loop {
        let (sock, peer) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle(sock, &state).await {
                tracing::debug!(?peer, error = %err, "healthz connection error");
            }
        });
    }
}

/// Per-connection handler. Reads up to the end of the request line
/// (whatever appears first within `MAX_REQUEST_BYTES`), inspects the
/// method+path, writes one fixed response. We don't keep-alive — every
/// connection is one request, one response, then close. That keeps the
/// state machine trivial and avoids the slowloris-resistance dance.
async fn handle(mut sock: TcpStream, state: &HealthState) -> io::Result<()> {
    const MAX_REQUEST_BYTES: usize = 4096;

    let mut buf = [0u8; MAX_REQUEST_BYTES];
    let mut filled = 0usize;
    // Read until we see "\r\n" or hit the byte cap. The bound matters:
    // an attacker streaming a `GET ...` without ever sending CRLF must
    // not pin a worker forever.
    while filled < MAX_REQUEST_BYTES {
        let n = sock.read(&mut buf[filled..]).await?;
        if n == 0 {
            break; // peer closed
        }
        filled += n;
        if buf[..filled].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }

    let line_end = buf[..filled]
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(filled);
    let request_line = std::str::from_utf8(&buf[..line_end]).unwrap_or("");
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let (status, body) = if method == "GET" && path == "/healthz" {
        state.evaluate()
    } else {
        ("HTTP/1.1 404 Not Found", "not found")
    };

    let response = format!(
        "{status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    sock.write_all(response.as_bytes()).await?;
    sock.shutdown().await?;
    Ok(())
}

// ── Layer 2 integration tests — bind ephemeral port, hit endpoint ───────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    /// Bind 127.0.0.1:0, hand the resulting addr back, spawn the
    /// server task on the current runtime, and return its `JoinHandle`
    /// alongside the addr so callers can drop the handle (server task
    /// dies with the runtime).
    async fn spawn_server(state: Arc<HealthState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            // `run` only returns on a fatal accept error; in tests we
            // just drop the handle when the runtime tears down.
            let _ = run(listener, state).await;
        });
        (addr, handle)
    }

    /// Tiny ad-hoc HTTP/1.0-ish client. Hyper is overkill for one
    /// request that we already control on both sides.
    async fn fetch(addr: SocketAddr, path: &str) -> (String, String) {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        sock.shutdown().await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), sock.read_to_end(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let raw = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status_line = head.lines().next().unwrap_or("").to_owned();
        (status_line, body.to_owned())
    }

    #[tokio::test]
    async fn healthz_returns_ok_when_invariants_hold() {
        let state = Arc::new(HealthState::new(2));
        let (addr, _h) = spawn_server(state).await;
        let (status, body) = fetch(addr, "/healthz").await;
        assert!(status.contains("200"), "status was: {status}");
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn healthz_returns_503_when_no_accounts() {
        let state = Arc::new(HealthState::new(0));
        let (addr, _h) = spawn_server(state).await;
        let (status, body) = fetch(addr, "/healthz").await;
        assert!(status.contains("503"), "status was: {status}");
        assert_eq!(body, "no accounts configured");
    }

    #[tokio::test]
    async fn healthz_returns_503_when_last_reload_failed() {
        let state = Arc::new(HealthState::new(3));
        state.set_last_reload_succeeded(false);
        let (addr, _h) = spawn_server(state).await;
        let (status, body) = fetch(addr, "/healthz").await;
        assert!(status.contains("503"), "status was: {status}");
        assert_eq!(body, "last reload failed");
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let state = Arc::new(HealthState::new(1));
        let (addr, _h) = spawn_server(state).await;
        let (status, body) = fetch(addr, "/metrics").await;
        assert!(status.contains("404"), "status was: {status}");
        assert_eq!(body, "not found");
    }

    #[tokio::test]
    async fn non_get_method_returns_404() {
        // ADR-0008 doesn't speak to method semantics; we treat
        // non-GET as unknown route rather than 405, matching the
        // "anything else → not found" line in this file's header.
        let state = Arc::new(HealthState::new(1));
        let (addr, _h) = spawn_server(state).await;
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"POST /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        sock.shutdown().await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let raw = String::from_utf8_lossy(&buf);
        assert!(raw.contains("404"), "raw was: {raw}");
    }
}
