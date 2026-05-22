//! Shared HTTP retry helper.
//!
//! Retry policy per [ADR-0005](../docs/adr/0005-error-model.md):
//!
//! - **429**: honor `Retry-After` if present (seconds form); else exponential
//!   backoff like 5xx.
//! - **5xx**: exponential backoff with jitter — `base_delay * 2^attempt` plus
//!   up to `base_delay` of random jitter.
//! - **Other 4xx**: no retry — return `Error::Upstream` immediately.
//! - **Max retries**: 3 by default.
//!
//! Response bodies are captured BEFORE status checks so an error response is
//! preserved for diagnostics (Clippy `disallowed_methods` would forbid
//! `error_for_status` if added).

use std::future::Future;
use std::time::Duration;

use reqwest::{Response, StatusCode};
use tokio::time::sleep;

use crate::error::Error;

/// Percent-encode a single URL path segment per RFC 3986 §2.3 / §3.3. Every
/// byte outside the unreserved set (`A-Z a-z 0-9 - _ . ~`) is replaced with
/// `%XX`. Critically, `/`, `?`, `#`, `%` are all encoded — preventing
/// untrusted segment values (`thread_id`, `account` alias) from reshaping the
/// API path or smuggling a query string.
///
/// Use for every caller-controlled component interpolated into a Gmail URL
/// path. Query-string values use the same encoding rules and may also call
/// this helper.
///
/// Implemented inline (~10 `LoC`) rather than pulling in `percent-encoding` —
/// the unreserved set is small and stable.
pub(crate) fn percent_encode_path_segment(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xF) as usize] as char);
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub(crate) struct RetryPolicy {
    pub(crate) max_retries: u32,
    pub(crate) base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
        }
    }
}

impl RetryPolicy {
    /// Aggressively short delays for tests — keeps Layer 2 wiremock tests fast.
    #[cfg(test)]
    pub(crate) const fn for_tests() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
        }
    }
}

/// Drive `send_request` against `policy`. The closure must produce a fresh
/// in-flight request on every call — `RequestBuilder` is single-use.
///
/// Returns the first 2xx response, or the last `Error::Upstream` / `Error::Network`
/// after retries are exhausted.
pub(crate) async fn execute_with_retry<F, Fut>(
    service: &str,
    policy: &RetryPolicy,
    mut send_request: F,
) -> Result<Response, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Response, reqwest::Error>>,
{
    let mut attempt: u32 = 0;
    loop {
        let result = send_request().await;
        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                if !is_retryable(status) || attempt >= policy.max_retries {
                    let code = status.as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::upstream(service, code, body));
                }
                let delay = retry_delay_for(&resp, attempt, policy);
                sleep(delay).await;
            }
            Err(e) => {
                if attempt >= policy.max_retries {
                    return Err(Error::Network(e));
                }
                let delay = backoff_with_jitter(attempt, policy.base_delay);
                sleep(delay).await;
            }
        }
        attempt = attempt.saturating_add(1);
    }
}

const fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500..=599)
}

fn retry_delay_for(resp: &Response, attempt: u32, policy: &RetryPolicy) -> Duration {
    if resp.status().as_u16() == 429 {
        if let Some(d) = parse_retry_after(resp) {
            return d;
        }
    }
    backoff_with_jitter(attempt, policy.base_delay)
}

fn parse_retry_after(resp: &Response) -> Option<Duration> {
    resp.headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// `base * 2^attempt + jitter ∈ [0, base)`. Jitter is derived from a cheap
/// time-based PRNG — sufficient to break retry storms without pulling in
/// `rand` for runtime use.
fn backoff_with_jitter(attempt: u32, base: Duration) -> Duration {
    let shift = attempt.min(10);
    let factor = 1_u32 << shift;
    let exp = base.saturating_mul(factor);
    let jitter = jitter_under(base);
    exp.saturating_add(jitter)
}

fn jitter_under(bound: Duration) -> Duration {
    let nanos = bound.as_nanos();
    if nanos == 0 {
        return Duration::ZERO;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_u128, |d| d.as_nanos());
    let take = u64::try_from(seed % nanos).unwrap_or(0);
    Duration::from_nanos(take)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn is_retryable_table() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable(StatusCode::BAD_GATEWAY));
        assert!(is_retryable(StatusCode::GATEWAY_TIMEOUT));
        assert!(!is_retryable(StatusCode::OK));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::NOT_FOUND));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn backoff_grows_monotonically() {
        let base = Duration::from_millis(100);
        let zero = backoff_with_jitter(0, base);
        let one = backoff_with_jitter(1, base);
        let two = backoff_with_jitter(2, base);
        // The exponential floor is monotonic; jitter only adds.
        assert!(one >= base * 2);
        assert!(two >= base * 4);
        // And the deltas roughly double — every attempt at least doubles the floor.
        assert!(one > zero / 2);
        assert!(two > one / 2);
    }

    #[test]
    fn jitter_is_under_bound() {
        let bound = Duration::from_millis(100);
        for _ in 0..32 {
            let j = jitter_under(bound);
            assert!(j < bound, "jitter {j:?} >= bound {bound:?}");
        }
    }

    // ── percent_encode_path_segment ──────────────────────────────────────────

    #[test]
    fn percent_encode_passes_unreserved_through() {
        // Per RFC 3986: `A-Z a-z 0-9 - _ . ~` are unreserved.
        let s = "Abc-XYZ_123.tilde~";
        assert_eq!(percent_encode_path_segment(s), s);
    }

    #[test]
    fn percent_encode_escapes_path_separator() {
        // The whole point of the helper: `/` must not survive into a path segment.
        assert_eq!(percent_encode_path_segment("foo/bar"), "foo%2Fbar");
    }

    #[test]
    fn percent_encode_escapes_query_and_fragment_delimiters() {
        // `?` would truncate the URL Gmail sees; `#` would slice off a fragment.
        assert_eq!(percent_encode_path_segment("foo?bar"), "foo%3Fbar");
        assert_eq!(percent_encode_path_segment("foo#bar"), "foo%23bar");
    }

    #[test]
    fn percent_encode_escapes_percent_itself() {
        // Pre-encoded `%2f` must be double-encoded so it can't smuggle a `/`.
        assert_eq!(percent_encode_path_segment("foo%2fbar"), "foo%252fbar");
    }

    #[test]
    fn percent_encode_escapes_whitespace_and_specials() {
        assert_eq!(percent_encode_path_segment("a b"), "a%20b");
        assert_eq!(percent_encode_path_segment("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn percent_encode_handles_multibyte_utf8() {
        // 好 = E5 A5 BD (3 bytes); each must be encoded.
        assert_eq!(percent_encode_path_segment("好"), "%E5%A5%BD");
    }

    #[test]
    fn percent_encode_empty_string_yields_empty() {
        assert_eq!(percent_encode_path_segment(""), "");
    }
}
