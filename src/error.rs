use std::time::Duration;

use thiserror::Error;

/// Typed error enum for all fallible operations in google-personal-mcp.
///
/// Use `Result<T, Error>` directly at call sites — no type aliases.
#[derive(Debug, Error)]
pub(crate) enum Error {
    /// Re-authentication needed. Carries the account so the user knows which.
    #[error("authentication required for account `{account}`: {reason}")]
    AuthRequired { account: String, reason: String },

    /// User specified an account alias that doesn't exist.
    #[error(
        "account `{account}` not found; run `google-personal-mcp auth list` to see available accounts"
    )]
    AccountNotFound { account: String },

    /// Resource not found (thread, message, label, event, etc.).
    #[error("not found: {what}")]
    NotFound { what: String },

    /// Calendar recurring-event instance not found: the event series resolves
    /// but the requested individual occurrence does not exist (e.g. a deleted
    /// or out-of-range instance). Calendar-specific per
    /// [ADR-0023](../docs/adr/0023-calendar-service-surface.md); distinct from
    /// the generic [`Error::NotFound`] so callers can special-case recurrence
    /// expansion.
    #[error("calendar recurrence instance not found: {what}")]
    RecurrenceInstanceNotFound { what: String },

    /// Tool received an argument it cannot use.
    #[error("invalid argument `{field}`: {detail}")]
    InvalidArgument { field: String, detail: String },

    /// Optimistic-concurrency failure: a resource (e.g. a People API contact)
    /// changed between read and write, so the etag the client sent no longer
    /// matches. Per [ADR-0024](../docs/adr/0024-contacts-service-surface.md);
    /// the `hint` is the actionable remediation ("re-fetch `<resource>` and
    /// re-apply") and carries no secrets.
    #[error("concurrency conflict on `{resource}`: {hint}")]
    ConcurrencyConflict { resource: String, hint: String },

    /// Header injection attempt detected (e.g. CR/LF in email subject).
    /// This is a security event — log loud, refuse the operation.
    #[error("header injection attempt in field `{field}`")]
    HeaderInjection { field: String },

    /// A tool was handed an attachment whose MIME type it cannot process.
    /// Raised by `parse_forwarded_attachment` per
    /// [ADR-0026](../docs/adr/0026-gmail-tool-surface-phase-2.md) when the
    /// referenced attachment is not `message/rfc822`. Model-actionable: the
    /// caller passed the wrong `attachment_id`. `found`/`expected` are
    /// server-derived MIME tokens, never attacker free-text bodies.
    #[error("unsupported MIME type `{found}`: expected `{expected}`")]
    UnsupportedMimeType {
        found: String,
        expected: &'static str,
    },

    /// A Drive file has no directly downloadable binary content because it is a
    /// Google-native document (Docs/Sheets/Slides). `download_file` raises this
    /// per [ADR-0025](../docs/adr/0025-drive-service-surface.md); the caller
    /// should retry via `export_file` with one of `supported_export_types`.
    /// Both fields are server-derived MIME tokens — never secrets or
    /// attacker-free-text, so `Debug` needs no redaction.
    #[error(
        "file requires export: source type `{mime_type}` has no direct download; \
         use export_file with one of {supported_export_types:?}"
    )]
    ExportRequired {
        mime_type: String,
        supported_export_types: Vec<String>,
    },

    /// `export_file` was asked to export a Google-native document to a target
    /// MIME type its source type does not support. Per ADR-0025; both fields are
    /// server-derived MIME tokens, never secrets. The source-type field is named
    /// `source_type` (not `source`) because `thiserror` reserves a field named
    /// `source` for the error-chain source, which a plain `String` cannot be.
    #[error("unsupported export: source `{source_type}` cannot export to `{requested}`")]
    UnsupportedExportType {
        source_type: String,
        requested: String,
    },

    /// Upstream rate-limited us. Surfaced only AFTER the HTTP layer's bounded retries are exhausted.
    #[error("rate limited on account `{account}`; retry after {retry_after:?}")]
    RateLimited {
        account: String,
        retry_after: Duration,
    },

    /// Upstream returned a non-success status; carries body for diagnosis.
    /// 4xx (other than the specific variants above) and 5xx (after retries) end up here.
    /// Body is truncated to 4 KiB on construction so it doesn't flood logs.
    #[error("upstream {service} returned {status}: {message}")]
    Upstream {
        service: String,
        status: u16,
        message: String,
    },

    /// Network-level failure (connection reset, DNS, TLS, timeout).
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    /// Response parsing failed — usually means upstream changed schema, or a bug.
    #[error("parse error in {context}: {source}")]
    Parse {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    /// Local IO failure (token file unreadable, config dir missing, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration could not be loaded or validated.
    #[error("config error in `{path}`: {message}")]
    Config { path: String, message: String },

    /// File or directory has wider-than-required permissions, or is a rejected
    /// symlink. Refuses startup per ADR-0017. The `message` is a complete
    /// remediation hint (mode, expected mode, and the `chmod` command).
    #[error("insecure permissions on `{path}`: {message}")]
    InsecurePermissions { path: String, message: String },

    /// Catch-all for anything not yet categorized. Treated as a bug to triage.
    #[error("internal error in {context}: {source}")]
    Internal {
        context: String,
        #[source]
        source: anyhow::Error,
    },
}

impl Error {
    /// Construct an `Upstream` error, truncating the body to 4 KiB.
    ///
    /// The truncation point is rounded *down* to the nearest UTF-8 character
    /// boundary so multi-byte characters straddling the byte limit don't cause
    /// a panic (#98). 4 KiB is per ADR-0005.
    ///
    /// **OAuth-specific redaction (#103):** when `service == "google-oauth"`,
    /// the body is scrubbed of `access_token`, `refresh_token`, and `id_token`
    /// values *before* truncation, so a token sitting at byte 3000 of a 5 KiB
    /// response can't leak via the truncated prefix.
    /// [ADR-0017](../docs/adr/0017-secrets-at-rest.md) §"Logging hygiene".
    pub(crate) fn upstream(service: impl Into<String>, status: u16, body: String) -> Self {
        const MAX_BODY: usize = 4 * 1024;
        let service = service.into();
        let body = if service == "google-oauth" {
            redact_oauth_token_fields(&body).into_owned()
        } else {
            body
        };
        let message = if body.len() > MAX_BODY {
            format!(
                "{}… (truncated)",
                truncate_at_char_boundary(&body, MAX_BODY)
            )
        } else {
            body
        };
        Self::Upstream {
            service,
            status,
            message,
        }
    }

    /// Check `value` for CR or LF characters that could inject headers.
    /// Returns `Err(HeaderInjection)` if any are found.
    pub(crate) fn check_header_field(field: &str, value: &str) -> Result<(), Self> {
        if value.contains('\r') || value.contains('\n') {
            return Err(Self::HeaderInjection {
                field: field.to_owned(),
            });
        }
        Ok(())
    }

    /// Short, stable identifier for this error variant — suitable for
    /// the `error.kind` tracing field per ADR-0005 / ADR-0008. Never
    /// includes user-controlled bytes; safe to log unconditionally.
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::AuthRequired { .. } => "auth_required",
            Self::AccountNotFound { .. } => "account_not_found",
            Self::NotFound { .. } => "not_found",
            Self::RecurrenceInstanceNotFound { .. } => "recurrence_instance_not_found",
            Self::ConcurrencyConflict { .. } => "concurrency_conflict",
            Self::InvalidArgument { .. } => "invalid_argument",
            Self::HeaderInjection { .. } => "header_injection",
            Self::UnsupportedMimeType { .. } => "unsupported_mime_type",
            Self::ExportRequired { .. } => "export_required",
            Self::UnsupportedExportType { .. } => "unsupported_export_type",
            Self::RateLimited { .. } => "rate_limited",
            Self::Upstream { .. } => "upstream",
            Self::Network(_) => "network",
            Self::Parse { .. } => "parse",
            Self::Io(_) => "io",
            Self::Config { .. } => "config",
            Self::InsecurePermissions { .. } => "insecure_permissions",
            Self::Internal { .. } => "internal",
        }
    }
}

/// Scrub OAuth-token-shaped JSON fields from `body`.
///
/// Replaces the string value of any `"access_token"`, `"refresh_token"`,
/// or `"id_token"` JSON key with `"<redacted>"`. Tries strict JSON
/// parsing first — any structure (objects, arrays, deeply nested) is
/// walked. If `body` isn't valid JSON (truncated, surrounded by HTML,
/// etc.) we fall back to a substring scrubber that looks for the same
/// keys and rewrites just the string literal that follows.
///
/// Returned as `Cow` so the common "no tokens present" path doesn't
/// allocate. Per [#103](https://github.com/torsday/google-personal-mcp/issues/103) /
/// [ADR-0017](../docs/adr/0017-secrets-at-rest.md) §"Logging hygiene":
/// Google's `invalid_grant` body may contain a fresh `access_token` on
/// partial-refresh paths.
pub(crate) fn redact_oauth_token_fields(body: &str) -> std::borrow::Cow<'_, str> {
    const SENSITIVE: &[&str] = &["access_token", "refresh_token", "id_token"];
    const REDACTED: &str = "<redacted>";

    // Fast path: if none of the keys appear, nothing to do.
    if !SENSITIVE.iter().any(|k| body.contains(k)) {
        return std::borrow::Cow::Borrowed(body);
    }

    // Preferred path: strict JSON walk. Handles nested objects, arrays,
    // mixed quoting, and the partial-refresh "the body is valid JSON
    // and the token sits at /access_token" case the ADR calls out.
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) {
        redact_json_in_place(&mut value, SENSITIVE, REDACTED);
        if let Ok(s) = serde_json::to_string(&value) {
            return std::borrow::Cow::Owned(s);
        }
    }

    // Fallback: scanner-based scrub for non-JSON or truncated bodies.
    // Walks the input once, looking for `"<key>"`-then-`:`-then-string,
    // overwriting the string literal value with REDACTED. Conservative
    // — better to leave a literal-looking byte that doesn't match the
    // pattern in place than risk corrupting unrelated data.
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    'outer: while !rest.is_empty() {
        // Find the next sensitive key occurrence (any of the three).
        let next_key = SENSITIVE
            .iter()
            .filter_map(|k| {
                let needle = format!("\"{k}\"");
                rest.find(&needle).map(|i| (i, needle))
            })
            .min_by_key(|(i, _)| *i);
        let Some((idx, needle)) = next_key else {
            out.push_str(rest);
            break;
        };
        // Copy through the key + `"key"` text.
        let key_end = idx + needle.len();
        out.push_str(&rest[..key_end]);
        // Skip whitespace + colon + whitespace.
        let mut tail = rest[key_end..].chars();
        let mut consumed = 0;
        let mut saw_colon = false;
        for c in tail.by_ref() {
            consumed += c.len_utf8();
            if c == ':' {
                saw_colon = true;
                break;
            }
            if !c.is_whitespace() {
                // Not a `"key": value` shape — bail on rewriting, just
                // emit what we consumed so far and continue scanning.
                out.push_str(&rest[key_end..key_end + consumed]);
                rest = &rest[key_end + consumed..];
                continue 'outer;
            }
        }
        if !saw_colon {
            out.push_str(&rest[key_end..]);
            break;
        }
        // Skip post-colon whitespace.
        let value_start_rel = consumed;
        let post_colon = &rest[key_end + value_start_rel..];
        let ws_skip = post_colon
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(post_colon.len());
        out.push_str(&rest[key_end..key_end + value_start_rel + ws_skip]);
        let value_pos = key_end + value_start_rel + ws_skip;
        let value_slice = &rest[value_pos..];
        // Expect an opening quote.
        if !value_slice.starts_with('"') {
            rest = value_slice;
            continue;
        }
        // Find closing quote, skipping `\\"` escapes.
        let mut end = 1;
        let bytes = value_slice.as_bytes();
        while end < bytes.len() {
            let b = bytes[end];
            if b == b'\\' && end + 1 < bytes.len() {
                end += 2;
                continue;
            }
            if b == b'"' {
                break;
            }
            end += 1;
        }
        if end >= bytes.len() {
            // Unterminated string — emit a redacted stub for the rest.
            out.push('"');
            out.push_str(REDACTED);
            out.push('"');
            break;
        }
        out.push('"');
        out.push_str(REDACTED);
        out.push('"');
        rest = &value_slice[end + 1..];
    }
    std::borrow::Cow::Owned(out)
}

fn redact_json_in_place(value: &mut serde_json::Value, keys: &[&str], placeholder: &str) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if keys.contains(&k.as_str()) {
                    *v = serde_json::Value::String(placeholder.to_owned());
                } else {
                    redact_json_in_place(v, keys, placeholder);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_json_in_place(v, keys, placeholder);
            }
        }
        _ => {}
    }
}

/// Return the longest prefix of `s` that fits within `max_bytes` and ends on a
/// UTF-8 character boundary. Walks at most 3 bytes (UTF-8's longest scalar is 4
/// bytes, so the nearest boundary is at most 3 bytes back).
///
/// Used by `Error::upstream` to avoid the `&s[..max_bytes]` panic when
/// `max_bytes` lands inside a multi-byte codepoint. See #98.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Map an `Error` to an `rmcp::ErrorData` for return at the MCP tool boundary.
pub(crate) fn to_mcp_error(e: &Error) -> rmcp::ErrorData {
    let msg = e.to_string();
    match e {
        // Model-actionable: bad input or recoverable user state.
        Error::AccountNotFound { .. }
        | Error::NotFound { .. }
        | Error::RecurrenceInstanceNotFound { .. }
        | Error::ConcurrencyConflict { .. }
        | Error::InvalidArgument { .. }
        | Error::UnsupportedMimeType { .. }
        | Error::ExportRequired { .. }
        | Error::UnsupportedExportType { .. }
        | Error::AuthRequired { .. } => rmcp::ErrorData::invalid_params(msg, None),

        // Security event: refuse and log loud.
        Error::HeaderInjection { field } => {
            tracing::warn!(field = %field, "header injection blocked");
            rmcp::ErrorData::invalid_params(msg, None)
        }

        // Transient conditions that exhausted retries — internal_error at this point.
        Error::RateLimited { .. } | Error::Network(_) | Error::Upstream { .. } => {
            rmcp::ErrorData::internal_error(msg, None)
        }

        // Bugs and unexpected failures — log full chain at ERROR.
        Error::Parse { .. }
        | Error::Io(_)
        | Error::Config { .. }
        | Error::InsecurePermissions { .. }
        | Error::Internal { .. } => {
            tracing::error!(error = ?e, "internal error in tool dispatch");
            rmcp::ErrorData::internal_error(msg, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Display formatting ────────────────────────────────────────────────────

    #[test]
    fn auth_required_display() {
        let e = Error::AuthRequired {
            account: "work".into(),
            reason: "token expired".into(),
        };
        assert_eq!(
            e.to_string(),
            "authentication required for account `work`: token expired"
        );
    }

    #[test]
    fn account_not_found_display() {
        let e = Error::AccountNotFound {
            account: "personal".into(),
        };
        let s = e.to_string();
        assert!(s.contains("`personal`"), "got: {s}");
        assert!(s.contains("auth list"), "got: {s}");
    }

    #[test]
    fn not_found_display() {
        let e = Error::NotFound {
            what: "thread:abc123".into(),
        };
        assert_eq!(e.to_string(), "not found: thread:abc123");
    }

    #[test]
    fn invalid_argument_display() {
        let e = Error::InvalidArgument {
            field: "max_results".into(),
            detail: "must be 1-500".into(),
        };
        assert_eq!(
            e.to_string(),
            "invalid argument `max_results`: must be 1-500"
        );
    }

    #[test]
    fn header_injection_display() {
        let e = Error::HeaderInjection {
            field: "subject".into(),
        };
        assert_eq!(e.to_string(), "header injection attempt in field `subject`");
    }

    #[test]
    fn unsupported_mime_type_display_and_kind() {
        let e = Error::UnsupportedMimeType {
            found: "application/pdf".into(),
            expected: "message/rfc822",
        };
        assert_eq!(
            e.to_string(),
            "unsupported MIME type `application/pdf`: expected `message/rfc822`"
        );
        assert_eq!(e.kind(), "unsupported_mime_type");
    }

    #[test]
    fn mcp_mapping_unsupported_mime_type_is_invalid_params() {
        let e = Error::UnsupportedMimeType {
            found: "image/png".into(),
            expected: "message/rfc822",
        };
        // Model-actionable bad input → invalid_params, not internal_error.
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn rate_limited_display() {
        let e = Error::RateLimited {
            account: "work".into(),
            retry_after: Duration::from_secs(10),
        };
        let s = e.to_string();
        assert!(s.contains("work"), "got: {s}");
        assert!(s.contains("10s"), "got: {s}");
    }

    #[test]
    fn upstream_display() {
        let e = Error::upstream("gmail", 429, "quota exceeded".into());
        assert_eq!(e.to_string(), "upstream gmail returned 429: quota exceeded");
    }

    #[test]
    fn upstream_truncates_long_body() {
        let big = "x".repeat(8 * 1024);
        let e = Error::upstream("gmail", 500, big);
        let msg = e.to_string();
        assert!(msg.contains("truncated"), "got: {msg}");
        assert!(
            msg.len() < 5 * 1024,
            "message too long: {} bytes",
            msg.len()
        );
    }

    // ── #98 regressions: multi-byte UTF-8 at the truncation boundary ─────────

    #[test]
    fn upstream_handles_3_byte_char_at_truncation_boundary() {
        // '€' is 3 bytes (E2 82 AC). With 4095 ASCII chars before it, byte 4096
        // lands inside the codepoint — the bug manifests as a panic in
        // `&body[..4096]` before the fix.
        let mut body = "x".repeat(4095);
        body.push('€');
        body.push_str(&"y".repeat(100));

        // Must not panic.
        let e = Error::upstream("gmail", 500, body);
        let msg = e.to_string();
        assert!(msg.contains("truncated"));
        // The euro sign should be dropped; only the 4095 ASCII bytes survive.
        assert!(msg.contains(&"x".repeat(100)));
        assert!(
            !msg.contains('€'),
            "partial euro byte must not survive truncation"
        );
    }

    #[test]
    fn upstream_handles_4_byte_emoji_at_truncation_boundary() {
        // 🎉 (U+1F389) is 4 bytes. Placed at 4093..4097, byte 4096 is its last
        // byte — `&body[..4096]` would slice in the middle of the codepoint.
        let mut body = "x".repeat(4093);
        body.push('🎉');
        body.push_str(&"y".repeat(100));

        let e = Error::upstream("gmail", 500, body);
        let msg = e.to_string();
        assert!(msg.contains("truncated"));
        assert!(
            !msg.contains('🎉'),
            "partial emoji must not survive truncation"
        );
    }

    #[test]
    fn upstream_handles_2_byte_char_at_truncation_boundary() {
        // 'é' is 2 bytes (C3 A9). Placed at 4095..4097, byte 4096 is its last
        // byte.
        let mut body = "x".repeat(4095);
        body.push('é');
        body.push_str(&"y".repeat(100));

        let e = Error::upstream("gmail", 500, body);
        let msg = e.to_string();
        assert!(msg.contains("truncated"));
        assert!(!msg.contains('é'));
    }

    #[test]
    fn truncate_at_char_boundary_is_a_noop_on_short_input() {
        // Path that never enters the boundary walk.
        assert_eq!(truncate_at_char_boundary("hello", 100), "hello");
    }

    #[test]
    fn truncate_at_char_boundary_handles_all_multi_byte_widths() {
        // ASCII (1) + Latin-1 (2) + BMP (3) + supplementary (4) — 10 bytes total
        // by construction, then truncate at various positions to confirm we
        // never panic and always return a valid str slice.
        let s = "aébo€uo🎉"; // 1 + 2 + 1 + 1 + 3 + 1 + 1 + 4 = 14 bytes
        for n in 0..=s.len() + 1 {
            // The function must always return a valid &str; calling .len() / .chars()
            // would panic on an invalid slice.
            let out = truncate_at_char_boundary(s, n);
            // Sanity: returned slice is at most `n` bytes (or full s if shorter).
            assert!(out.len() <= n || s.len() <= n);
            assert!(out.chars().count() <= s.chars().count());
        }
    }

    #[test]
    fn internal_display() {
        let e = Error::Internal {
            context: "config::load".into(),
            source: anyhow::anyhow!("bad toml"),
        };
        assert!(e.to_string().contains("config::load"));
    }

    // ── HeaderInjection constructor ───────────────────────────────────────────

    #[test]
    fn check_header_field_clean() {
        assert!(Error::check_header_field("subject", "Hello World").is_ok());
    }

    #[test]
    fn check_header_field_rejects_lf() {
        let result = Error::check_header_field("subject", "Hello\nWorld");
        assert!(
            matches!(result, Err(Error::HeaderInjection { ref field }) if field == "subject"),
            "expected HeaderInjection, got: {result:?}"
        );
    }

    #[test]
    fn check_header_field_rejects_cr() {
        let result = Error::check_header_field("subject", "Hello\rWorld");
        assert!(matches!(result, Err(Error::HeaderInjection { .. })));
    }

    #[test]
    fn check_header_field_rejects_crlf() {
        let result =
            Error::check_header_field("to", "victim@example.com\r\nBcc: attacker@evil.com");
        assert!(matches!(result, Err(Error::HeaderInjection { .. })));
    }

    // ── From impls ────────────────────────────────────────────────────────────

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let e: Error = io_err.into();
        assert!(matches!(e, Error::Io(_)));
        assert!(e.to_string().contains("io error"), "got: {e}");
    }

    // ── MCP error mapping ─────────────────────────────────────────────────────

    #[test]
    fn mcp_mapping_account_not_found_is_invalid_params() {
        let e = Error::AccountNotFound {
            account: "x".into(),
        };
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn mcp_mapping_not_found_is_invalid_params() {
        let e = Error::NotFound {
            what: "thread:xyz".into(),
        };
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn mcp_mapping_auth_required_is_invalid_params() {
        let e = Error::AuthRequired {
            account: "work".into(),
            reason: "expired".into(),
        };
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn mcp_mapping_header_injection_is_invalid_params() {
        let e = Error::HeaderInjection {
            field: "subject".into(),
        };
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn mcp_mapping_rate_limited_is_internal_error() {
        let e = Error::RateLimited {
            account: "work".into(),
            retry_after: Duration::from_secs(5),
        };
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn mcp_mapping_upstream_is_internal_error() {
        let e = Error::upstream("gmail", 500, "server error".into());
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn mcp_mapping_internal_is_internal_error() {
        let e = Error::Internal {
            context: "test".into(),
            source: anyhow::anyhow!("oops"),
        };
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn mcp_mapping_io_error_is_internal_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let e: Error = io_err.into();
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn mcp_mapping_invalid_argument_is_invalid_params() {
        let e = Error::InvalidArgument {
            field: "account".into(),
            detail: "required".into(),
        };
        let mcp = to_mcp_error(&e);
        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn mcp_message_contains_error_text() {
        let e = Error::NotFound {
            what: "thread:abc".into(),
        };
        let mcp = to_mcp_error(&e);
        assert!(mcp.message.contains("thread:abc"), "got: {}", mcp.message);
    }

    // ── OAuth token-leak redaction (#103, ADR-0017) ───────────────────────────

    #[test]
    fn redact_oauth_token_fields_strict_json() {
        let body = r#"{"access_token":"FRESH-AT","refresh_token":"R-T","id_token":"ID","error":"invalid_grant"}"#;
        let out = redact_oauth_token_fields(body);
        assert!(!out.contains("FRESH-AT"), "got: {out}");
        assert!(!out.contains("R-T\""), "got: {out}");
        assert!(!out.contains("\"ID\""), "got: {out}");
        assert!(out.contains("<redacted>"), "got: {out}");
        assert!(out.contains("invalid_grant"), "got: {out}");
    }

    #[test]
    fn redact_oauth_token_fields_nested_json() {
        let body =
            r#"{"wrapper":{"access_token":"NESTED-AT"},"list":[{"refresh_token":"NESTED-RT"}]}"#;
        let out = redact_oauth_token_fields(body);
        assert!(!out.contains("NESTED-AT"), "got: {out}");
        assert!(!out.contains("NESTED-RT"), "got: {out}");
    }

    #[test]
    fn redact_oauth_token_fields_non_json_fallback() {
        // Truncated JSON (no closing brace) — fallback scanner must
        // still scrub the token literal.
        let body = r#"some preamble {"access_token":"BARE-AT","#;
        let out = redact_oauth_token_fields(body);
        assert!(!out.contains("BARE-AT"), "got: {out}");
        assert!(out.contains("<redacted>"), "got: {out}");
    }

    #[test]
    fn redact_oauth_token_fields_passthrough_when_clean() {
        // No sensitive keys — should return Borrowed (no allocation).
        let body = r#"{"error":"invalid_grant","error_description":"Token revoked"}"#;
        let out = redact_oauth_token_fields(body);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, body);
    }

    #[test]
    fn upstream_google_oauth_redacts_in_display() {
        let body = r#"{"access_token":"FRESH-AT","error":"invalid_grant"}"#;
        let e = Error::upstream("google-oauth", 400, body.to_owned());
        let display = e.to_string();
        let debug = format!("{e:?}");
        assert!(!display.contains("FRESH-AT"), "display: {display}");
        assert!(!debug.contains("FRESH-AT"), "debug: {debug}");
    }

    #[test]
    fn upstream_non_oauth_service_does_not_redact() {
        // Defense-in-depth: only google-oauth is scrubbed so we don't
        // accidentally hide structured Gmail error bodies that
        // operators need to debug.
        let body = r#"{"access_token":"OTHER-AT","error":"x"}"#;
        let e = Error::upstream("gmail", 400, body.to_owned());
        assert!(e.to_string().contains("OTHER-AT"));
    }

    #[test]
    fn auth_required_constructed_with_clean_reason_never_leaks_body() {
        // Mirrors the new tokens.rs path: we build AuthRequired with a
        // stable reason, never the raw body. This test guards against
        // anyone re-introducing the splice.
        let body = r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked.","access_token":"FRESH-AT"}"#;
        let detail: Option<String> = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error_description")
                    .and_then(|d| d.as_str().map(str::to_owned))
            });
        let reason = detail.map_or_else(
            || "refresh_token rejected (invalid_grant)".to_owned(),
            |d| format!("refresh_token rejected (invalid_grant): {d}"),
        );
        let e = Error::AuthRequired {
            account: "work".into(),
            reason,
        };
        assert!(!e.to_string().contains("FRESH-AT"));
        assert!(!format!("{e:?}").contains("FRESH-AT"));
        assert!(e.to_string().contains("Token has been expired"));
    }
}
