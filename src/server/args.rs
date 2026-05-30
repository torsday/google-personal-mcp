//! Tool-argument extraction helpers + `ok_result`.
//!
//! These functions normalize the boilerplate of pulling typed values out of a
//! [`CallToolRequestParams`] envelope. Required-arg extractors return
//! `rmcp::ErrorData` (so dispatch arms can `?` them); optional-arg extractors
//! return `Option<T>` and never fail.

use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::Value;

use crate::error::{self, Error};

/// Serialize a successful tool output into a `CallToolResult`.
pub(super) fn ok_result(
    context: &'static str,
    v: &impl serde::Serialize,
) -> Result<CallToolResult, rmcp::ErrorData> {
    serde_json::to_value(v)
        .map(CallToolResult::structured)
        .map_err(|e| {
            error::to_mcp_error(&Error::Internal {
                context: context.into(),
                source: anyhow::Error::new(e),
            })
        })
}

/// Extract an optional boolean parameter; returns `false` when absent or not a bool.
pub(super) fn extract_bool_arg(request: &CallToolRequestParams, field: &str) -> bool {
    request
        .arguments
        .as_ref()
        .and_then(|a| a.get(field))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Extract an optional `String` parameter — returns `None` when missing or
/// when present but not a string. Empty strings round-trip as `Some("")`.
pub(super) fn extract_optional_string_arg(
    request: &CallToolRequestParams,
    field: &str,
) -> Option<String> {
    request
        .arguments
        .as_ref()
        .and_then(|a| a.get(field))
        .and_then(Value::as_str)
        .map(String::from)
}

/// Extract an optional `u32` parameter — returns `None` when missing or not
/// a number, or when the value is out of `u32` range.
pub(super) fn extract_optional_u32_arg(
    request: &CallToolRequestParams,
    field: &str,
) -> Option<u32> {
    request
        .arguments
        .as_ref()
        .and_then(|a| a.get(field))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Extract a required `Vec<String>` parameter from a `CallToolRequestParams`.
pub(super) fn extract_string_array_arg(
    request: &CallToolRequestParams,
    field: &str,
) -> Result<Vec<String>, rmcp::ErrorData> {
    let items: Vec<String> = request
        .arguments
        .as_ref()
        .and_then(|a| a.get(field))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        return Err(rmcp::ErrorData::invalid_params(
            format!("missing required argument `{field}`"),
            None,
        ));
    }
    Ok(items)
}

/// Extract a required `String` parameter from a `CallToolRequestParams`.
pub(super) fn extract_string_arg(
    request: &CallToolRequestParams,
    field: &str,
) -> Result<String, rmcp::ErrorData> {
    let args = request.arguments.as_ref().ok_or_else(|| {
        rmcp::ErrorData::invalid_params(format!("missing required argument `{field}`"), None)
    })?;
    match args.get(field) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(rmcp::ErrorData::invalid_params(
            format!("argument `{field}` must be a string"),
            None,
        )),
        None => Err(rmcp::ErrorData::invalid_params(
            format!("missing required argument `{field}`"),
            None,
        )),
    }
}

/// Extract the `account` argument and reject `"*"` unless `tool_name` is a
/// read-aspect tool, per ADR-0013. The cross-account fan-out wildcard is a
/// read-tool affordance only; allowing it on a write or destructive tool would
/// let a single mistaken call mutate every registered account.
///
/// The eligibility check keys off [`is_fanout_eligible`] (aspect == read), not
/// `is_destructive`: ADR-0022 classification separates `write` from
/// `destructive`, and write tools (`archive_thread`, `modify_thread_labels`)
/// must stay non-fan-out-able even though they are no longer "destructive".
///
/// [`is_fanout_eligible`]: crate::tools::metadata::is_fanout_eligible
pub(super) fn extract_account_arg(
    request: &CallToolRequestParams,
    tool_name: &str,
) -> Result<String, rmcp::ErrorData> {
    let account = extract_string_arg(request, "account")?;
    if account == "*" && !crate::tools::metadata::is_fanout_eligible(tool_name) {
        return Err(rmcp::ErrorData::invalid_params(
            "cross-account fan-out is permitted on read-only tools only",
            None,
        ));
    }
    Ok(account)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::server::descriptors::registered_tools;

    // ── extract_string_arg helper ─────────────────────────────────────────────

    #[test]
    fn extract_string_arg_present() {
        let mut params = CallToolRequestParams::new("list_labels");
        let mut args = serde_json::Map::new();
        args.insert("account".into(), Value::String("personal".into()));
        params.arguments = Some(args);
        let result = extract_string_arg(&params, "account").unwrap();
        assert_eq!(result, "personal");
    }

    #[test]
    fn extract_string_arg_missing_args_returns_error() {
        let params = CallToolRequestParams::new("list_labels");
        let err = extract_string_arg(&params, "account").unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn extract_string_arg_wrong_type_returns_error() {
        let mut params = CallToolRequestParams::new("list_labels");
        let mut args = serde_json::Map::new();
        args.insert("account".into(), Value::Number(42.into()));
        params.arguments = Some(args);
        let err = extract_string_arg(&params, "account").unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn extract_string_arg_absent_key_returns_error() {
        let mut params = CallToolRequestParams::new("list_labels");
        let args = serde_json::Map::new();
        params.arguments = Some(args);
        let err = extract_string_arg(&params, "account").unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    // ── extract_account_arg — ADR-0013 fan-out rejection (#85) ───────────────

    fn account_request(tool: &str, account_value: &str) -> CallToolRequestParams {
        // `CallToolRequestParams::new` wants `&'static str`; the tool name
        // here comes from a `for` loop so we round-trip it through the request
        // by setting the `arguments` map only and leaving `name` empty. The
        // dispatch layer reads `name` separately; for these unit tests of
        // `extract_account_arg`, the value of `name` doesn't matter — the
        // helper takes `tool_name` as a separate parameter.
        let _ = tool;
        let mut params = CallToolRequestParams::new("placeholder");
        let mut args = serde_json::Map::new();
        args.insert("account".into(), Value::String(account_value.into()));
        params.arguments = Some(args);
        params
    }

    /// Every mutating tool (write or destructive) rejects `account = "*"` with
    /// `InvalidParams` and the ADR-0013 wording. Exhaustive — one assertion per
    /// tool. `archive_thread` / `modify_thread_labels` are `write` under
    /// ADR-0022 yet must stay non-fan-out-able (the reason the guard keys off
    /// read-eligibility, not `is_destructive`).
    #[test]
    fn extract_account_rejects_wildcard_on_every_mutating_tool() {
        for tool in [
            "archive_thread",
            "batch_archive",
            "trash_thread",
            "batch_trash",
            "modify_thread_labels",
            "batch_modify_thread_labels",
            "send_email",
        ] {
            let params = account_request(tool, "*");
            match extract_account_arg(&params, tool) {
                Err(e) => {
                    assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
                    assert!(
                        e.message
                            .contains("cross-account fan-out is permitted on read-only tools only"),
                        "mutating tool `{tool}` rejected with wrong message: {}",
                        e.message,
                    );
                }
                Ok(acct) => {
                    panic!("mutating tool `{tool}` accepted account=\"*\" — got `{acct}`")
                }
            }
        }
    }

    #[test]
    fn extract_account_rejects_wildcard_archive_thread_explicit_error() {
        let params = account_request("archive_thread", "*");
        let err = extract_account_arg(&params, "archive_thread").unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            err.message
                .contains("cross-account fan-out is permitted on read-only tools only"),
            "got message: {}",
            err.message
        );
    }

    #[test]
    fn extract_account_allows_wildcard_on_read_only_tools() {
        // Read-only tools may legitimately accept `*` once #84 ships fan-out.
        // The guard rejects everything non-read; verify it doesn't over-reach
        // onto the read tools.
        for tool in [
            "list_labels",
            "list_accounts",
            "search_threads",
            "get_thread",
        ] {
            let params = account_request(tool, "*");
            let account = extract_account_arg(&params, tool)
                .unwrap_or_else(|e| panic!("read-only tool `{tool}` over-rejected: {}", e.message));
            assert_eq!(account, "*");
        }
    }

    #[test]
    fn extract_account_passes_normal_aliases_for_destructive_tools() {
        // Normal aliases must round-trip even on destructive tools.
        for tool in [
            "archive_thread",
            "batch_archive",
            "trash_thread",
            "batch_trash",
            "modify_thread_labels",
            "batch_modify_thread_labels",
        ] {
            let params = account_request(tool, "personal");
            let account = extract_account_arg(&params, tool)
                .unwrap_or_else(|e| panic!("alias rejected on `{tool}`: {}", e.message));
            assert_eq!(account, "personal");
        }
    }

    #[test]
    fn extract_account_inherits_missing_account_error() {
        // Reuses extract_string_arg, so the missing-arg path must still surface.
        let params = CallToolRequestParams::new("archive_thread");
        let err = extract_account_arg(&params, "archive_thread").unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("account"), "got: {}", err.message);
    }

    /// Descriptor sanity: no non-read (write/destructive) tool advertises the
    /// fan-out wildcard in its `account` description. Catches "we added fan-out
    /// copy to a mutating tool by mistake" before it ships.
    ///
    /// Keys off the "fan out" advertising phrase rather than a bare `*`: a
    /// non-read tool may legitimately *prohibit* the wildcard (e.g.
    /// `purge_account` says "Must NOT be `*`"), which contains `*` but is the
    /// opposite of advertising it.
    #[test]
    fn non_read_descriptors_do_not_advertise_fanout() {
        for tool in registered_tools() {
            if crate::tools::metadata::is_fanout_eligible(&tool.name) {
                continue;
            }
            let schema = serde_json::to_value(tool.input_schema.as_ref()).expect("schema");
            let acct_desc = schema
                .pointer("/properties/account/description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            assert!(
                !acct_desc.to_lowercase().contains("fan out"),
                "non-read tool `{}` advertises fan-out in account description: {acct_desc}",
                tool.name
            );
        }
    }
}
