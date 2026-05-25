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
    /// Also bumps `gmcp_hot_reload_total{outcome}` per ADR-0008 — when
    /// the reload subsystem lands, callers should pass a more specific
    /// outcome via [`Self::set_last_reload_outcome`].
    #[allow(dead_code)]
    pub(crate) fn set_last_reload_succeeded(&self, ok: bool) {
        self.last_reload_succeeded.store(ok, Ordering::Relaxed);
        let outcome = if ok { "success" } else { "validation_error" };
        metrics::counter!(
            crate::observability::metrics::names::HOT_RELOAD_TOTAL,
            "outcome" => outcome,
        )
        .increment(1);
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

    // ── Route ────────────────────────────────────────────────────────────────
    // Single match because both routes need to ship a body and the
    // metrics rendering is more than a const lookup. The owned-String
    // branch carries through the response writer below.
    let (status_line, content_type, body): (&str, &str, std::borrow::Cow<'_, str>) =
        if method == "GET" && path == "/healthz" {
            let (s, b) = state.evaluate();
            (
                s,
                "text/plain; charset=utf-8",
                std::borrow::Cow::Borrowed(b),
            )
        } else if method == "GET" && path == "/metrics" {
            // Per Prometheus exposition format: text/plain; version=0.0.4.
            // Returns 404 when no recorder was installed — the operator
            // did not enable `[metrics]` in config.
            crate::observability::metrics::handle().map_or(
                (
                    "HTTP/1.1 404 Not Found",
                    "text/plain; charset=utf-8",
                    std::borrow::Cow::Borrowed("metrics recorder not installed"),
                ),
                |h| {
                    (
                        "HTTP/1.1 200 OK",
                        "text/plain; version=0.0.4; charset=utf-8",
                        std::borrow::Cow::Owned(h.render()),
                    )
                },
            )
        } else {
            (
                "HTTP/1.1 404 Not Found",
                "text/plain; charset=utf-8",
                std::borrow::Cow::Borrowed("not found"),
            )
        };

    let body_bytes = body.as_bytes();
    let header = format!(
        "{status_line}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body_bytes.len(),
    );
    sock.write_all(header.as_bytes()).await?;
    sock.write_all(body_bytes).await?;
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
        let (status, body) = fetch(addr, "/banana").await;
        assert!(status.contains("404"), "status was: {status}");
        assert_eq!(body, "not found");
    }

    /// `/metrics` returns 404 with a distinct body when no recorder has
    /// been installed. The exporter installation is process-global and
    /// one-shot; we don't install it from this unit test (other tests
    /// in `observability::metrics` exercise the install path).
    #[tokio::test]
    async fn metrics_returns_404_when_recorder_not_installed() {
        let state = Arc::new(HealthState::new(1));
        let (addr, _h) = spawn_server(state).await;
        let (status, body) = fetch(addr, "/metrics").await;
        if crate::observability::metrics::handle().is_some() {
            // A previous test in this process installed the recorder —
            // /metrics now serves 200. Skip the negative assertion.
            assert!(status.contains("200"), "status was: {status}");
            return;
        }
        assert!(status.contains("404"), "status was: {status}");
        assert_eq!(body, "metrics recorder not installed");
    }

    /// Layer 2 integration test per the issue: install the recorder,
    /// scrape `/metrics`, and assert at least `gmcp_tool_calls_total`
    /// and `gmcp_build_info` appear. We also exercise a counter bump so
    /// `gmcp_tool_calls_total` shows a sample series (not just the
    /// `# TYPE` line from `describe_counter!`).
    ///
    /// The Prometheus exporter installs a process-global recorder; if a
    /// concurrent test already installed it, our `install` call returns
    /// the existing handle without re-registering — the assertions
    /// below still hold.
    #[tokio::test]
    async fn metrics_endpoint_serves_inventory() {
        crate::observability::metrics::install(
            2,
            crate::observability::metrics::BuildInfoLabels::from_env(),
        )
        .expect("install");
        // Bump the counter so the series materializes in the scrape
        // output, not just the type-description preamble.
        metrics::counter!(
            crate::observability::metrics::names::TOOL_CALLS_TOTAL,
            "tool" => "list_accounts",
            "outcome" => "success",
        )
        .increment(1);

        let state = Arc::new(HealthState::new(2));
        let (addr, _h) = spawn_server(state).await;
        let (status, body) = fetch(addr, "/metrics").await;
        assert!(status.contains("200"), "status was: {status}");
        assert!(
            body.contains("gmcp_tool_calls_total"),
            "scrape missing gmcp_tool_calls_total: {body}",
        );
        assert!(
            body.contains("gmcp_build_info"),
            "scrape missing gmcp_build_info: {body}",
        );
        // The counter bump above must show up as a sample line.
        assert!(
            body.lines().any(|l| {
                l.starts_with("gmcp_tool_calls_total")
                    && l.contains("tool=\"list_accounts\"")
                    && l.contains("outcome=\"success\"")
            }),
            "scrape missing the bumped counter line: {body}",
        );
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
