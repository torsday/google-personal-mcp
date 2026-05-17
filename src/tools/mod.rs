//! Cross-cutting infrastructure consumed by every tool implementation —
//! starting with `dry_run` + send-deduplication (ADR-0012). Tool modules
//! themselves live under `gmail::tools` / `calendar::tools` per service.

pub(crate) mod destructive;
