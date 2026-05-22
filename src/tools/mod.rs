//! Cross-cutting infrastructure consumed by every tool implementation —
//! starting with `dry_run` + send-deduplication (ADR-0012). Tool modules
//! themselves live under `gmail::tools` / `calendar::tools` per service.

pub(crate) mod archive;
pub(crate) mod batch;
pub(crate) mod destructive;
pub(crate) mod get_thread;
pub(crate) mod list_accounts;
pub(crate) mod list_labels;
pub(crate) mod mcp_status;
pub(crate) mod modify_labels;
pub(crate) mod search_threads;
pub(crate) mod trash;
