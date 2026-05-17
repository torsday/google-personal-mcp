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

    /// Tool received an argument it cannot use.
    #[error("invalid argument `{field}`: {detail}")]
    InvalidArgument { field: String, detail: String },

    /// Header injection attempt detected (e.g. CR/LF in email subject).
    /// This is a security event — log loud, refuse the operation.
    #[error("header injection attempt in field `{field}`")]
    HeaderInjection { field: String },

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
    pub(crate) fn upstream(service: impl Into<String>, status: u16, body: String) -> Self {
        const MAX_BODY: usize = 4 * 1024;
        let message = if body.len() > MAX_BODY {
            format!("{}… (truncated)", &body[..MAX_BODY])
        } else {
            body
        };
        Self::Upstream {
            service: service.into(),
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
}

/// Map an `Error` to an `rmcp::ErrorData` for return at the MCP tool boundary.
pub(crate) fn to_mcp_error(e: &Error) -> rmcp::ErrorData {
    let msg = e.to_string();
    match e {
        // Model-actionable: bad input or recoverable user state.
        Error::AccountNotFound { .. }
        | Error::NotFound { .. }
        | Error::InvalidArgument { .. }
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
        Error::Parse { .. } | Error::Io(_) | Error::Internal { .. } => {
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
}
