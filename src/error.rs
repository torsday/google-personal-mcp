use std::time::Duration;
use thiserror::Error;

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("authentication required for account `{account}`: {reason}")]
    AuthRequired { account: String, reason: String },

    #[error("account `{account}` not found; run `google-personal-mcp auth list` to see available accounts")]
    AccountNotFound { account: String },

    #[error("not found: {what}")]
    NotFound { what: String },

    #[error("invalid argument `{field}`: {detail}")]
    InvalidArgument { field: String, detail: String },

    #[error("header injection attempt in field `{field}`")]
    HeaderInjection { field: String },

    #[error("rate limited on account `{account}`; retry after {retry_after:?}")]
    RateLimited {
        account: String,
        retry_after: Duration,
    },

    #[error("upstream {service} returned {status}: {message}")]
    Upstream {
        service: String,
        status: u16,
        message: String,
    },

    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    #[error("parse error in {context}: {source}")]
    Parse {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error in {context}: {source}")]
    Internal {
        context: String,
        #[source]
        source: anyhow::Error,
    },
}
