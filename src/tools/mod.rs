//! Cross-cutting infrastructure consumed by every tool implementation.
//!
//! Starts with `dry_run` + send-deduplication (ADR-0012). Tool modules
//! themselves live under `gmail::tools` / `calendar::tools` per service.

pub(crate) mod archive;
pub(crate) mod aspect;
pub(crate) mod audit_summary;
pub(crate) mod batch;
pub(crate) mod cache_invalidate;
pub(crate) mod cache_status;
pub(crate) mod destructive;
pub(crate) mod download_attachment;
pub(crate) mod fanout;
pub(crate) mod get_thread;
pub(crate) mod list_accounts;
pub(crate) mod list_attachments;
pub(crate) mod list_labels;
pub(crate) mod mcp_status;
pub(crate) mod metadata;
pub(crate) mod modify_labels;
pub(crate) mod purge_account;
pub(crate) mod search_threads;
pub(crate) mod trash;
