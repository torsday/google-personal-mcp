use std::fmt::Write as _;
use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::gmail::GmailClient;

#[derive(Clone)]
pub(crate) struct GmailServer {
    client: Arc<GmailClient>,
}

impl GmailServer {
    pub(crate) fn new(client: GmailClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }
}

// ── Tool parameter types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SearchThreadsParams {
    #[schemars(
        description = "Gmail search query (same syntax as the Gmail search bar, e.g. 'from:boss@example.com is:unread')"
    )]
    pub query: String,
    #[schemars(description = "Maximum number of threads to return (default: 20, max: 50)")]
    pub max_results: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetThreadParams {
    #[schemars(description = "Thread ID to retrieve")]
    pub thread_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ArchiveThreadParams {
    #[schemars(description = "Thread ID to archive")]
    pub thread_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct BatchArchiveParams {
    #[schemars(description = "List of thread IDs to archive")]
    pub thread_ids: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ModifyLabelsParams {
    #[schemars(description = "Thread ID to modify")]
    pub thread_id: String,
    #[schemars(description = "Label IDs to add (e.g. ['STARRED', 'Label_123'])")]
    pub add_labels: Option<Vec<String>>,
    #[schemars(description = "Label IDs to remove (e.g. ['INBOX', 'UNREAD'])")]
    pub remove_labels: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SendEmailParams {
    #[schemars(description = "Recipient email address")]
    pub to: String,
    #[schemars(description = "Email subject")]
    pub subject: String,
    #[schemars(description = "Email body (plain text)")]
    pub body: String,
    #[schemars(description = "CC recipients (comma-separated, optional)")]
    pub cc: Option<String>,
    #[schemars(description = "Thread ID to reply into (optional — omit for a new thread)")]
    pub reply_to_thread_id: Option<String>,
    #[schemars(description = "Message-ID value for the In-Reply-To header (optional)")]
    pub in_reply_to: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct TrashThreadParams {
    #[schemars(description = "Thread ID to move to trash")]
    pub thread_id: String,
}

// ── Tool implementations ──────────────────────────────────────────────────────

// We intentionally take by value; the only thing we do is call `to_string()`,
// which would clone anyway if we took by reference. The lint's "consider taking
// a reference" advice is a false positive for ToString-only consumers.
#[allow(clippy::needless_pass_by_value)]
fn internal_err(e: impl ToString) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(e.to_string(), None)
}

#[tool_router(server_handler)]
impl GmailServer {
    #[tool(
        description = "Search Gmail threads using Gmail query syntax. Returns thread IDs and snippets."
    )]
    async fn search_threads(
        &self,
        Parameters(params): Parameters<SearchThreadsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let max = params.max_results.unwrap_or(20).min(50);
        let threads = self
            .client
            .search_threads(&params.query, max)
            .await
            .map_err(internal_err)?;

        let text = serde_json::to_string_pretty(&threads).map_err(internal_err)?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get the full content of a Gmail thread including all messages, headers, and body text."
    )]
    async fn get_thread(
        &self,
        Parameters(params): Parameters<GetThreadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let thread = self
            .client
            .get_thread(&params.thread_id)
            .await
            .map_err(internal_err)?;

        let mut output = String::new();
        let _ = writeln!(output, "Thread ID: {}", thread.id);
        if let Some(messages) = &thread.messages {
            let _ = writeln!(output, "Messages: {}\n", messages.len());
            for (i, msg) in messages.iter().enumerate() {
                let _ = writeln!(output, "--- Message {} ---", i + 1);
                if let Some(from) = msg.from() {
                    let _ = writeln!(output, "From: {from}");
                }
                if let Some(date) = msg.date() {
                    let _ = writeln!(output, "Date: {date}");
                }
                if let Some(subject) = msg.subject() {
                    let _ = writeln!(output, "Subject: {subject}");
                }
                if let Some(labels) = &msg.label_ids {
                    let _ = writeln!(output, "Labels: {}", labels.join(", "));
                }
                if let Some(body) = msg.body_text() {
                    let preview: String = body.chars().take(500).collect();
                    let _ = writeln!(output, "\n{preview}");
                    if body.len() > 500 {
                        output.push_str("[... truncated]\n");
                    }
                } else if let Some(snippet) = &msg.snippet {
                    let _ = writeln!(output, "\nSnippet: {snippet}");
                }
                output.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Archive a Gmail thread (removes it from inbox). The thread is preserved and searchable."
    )]
    async fn archive_thread(
        &self,
        Parameters(params): Parameters<ArchiveThreadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.client
            .archive_thread(&params.thread_id)
            .await
            .map_err(internal_err)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Thread {} archived successfully.",
            params.thread_id
        ))]))
    }

    #[tool(
        description = "Archive multiple Gmail threads at once. Returns a summary of successes and failures."
    )]
    async fn batch_archive(
        &self,
        Parameters(params): Parameters<BatchArchiveParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .client
            .batch_archive(&params.thread_ids)
            .await
            .map_err(internal_err)?;

        let mut text = format!(
            "Archived: {} threads\nFailed: {} threads",
            result.succeeded.len(),
            result.failed.len()
        );
        if !result.failed.is_empty() {
            text.push('\n');
            for f in &result.failed {
                let _ = writeln!(text, "  ✗ {}: {}", f.id, f.error);
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Add or remove labels on a Gmail thread. Use label IDs such as 'INBOX', 'STARRED', 'UNREAD', or custom label IDs from list_labels."
    )]
    async fn modify_thread_labels(
        &self,
        Parameters(params): Parameters<ModifyLabelsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let add: Vec<&str> = params
            .add_labels
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect();
        let remove: Vec<&str> = params
            .remove_labels
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect();

        self.client
            .modify_thread_labels(&params.thread_id, &add, &remove)
            .await
            .map_err(internal_err)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Labels updated on thread {}.",
            params.thread_id
        ))]))
    }

    #[tool(
        description = "List all Gmail labels — both system labels (INBOX, STARRED, etc.) and user-created labels."
    )]
    async fn list_labels(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let labels = self.client.list_labels().await.map_err(internal_err)?;

        let text = labels
            .iter()
            .map(|l| format!("{} ({})", l.name, l.id))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Send an email or reply to an existing thread.")]
    async fn send_email(
        &self,
        Parameters(params): Parameters<SendEmailParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let raw = compose_rfc2822(
            &params.to,
            &params.subject,
            params.cc.as_deref(),
            params.in_reply_to.as_deref(),
            &params.body,
        )
        .map_err(|e| rmcp::ErrorData::invalid_params(e, None))?;

        let sent = self
            .client
            .send_message(&raw, params.reply_to_thread_id.as_deref())
            .await
            .map_err(internal_err)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Email sent. Message ID: {}, Thread ID: {}",
            sent.id,
            sent.thread_id.as_deref().unwrap_or("unknown")
        ))]))
    }

    #[tool(description = "Move a Gmail thread to trash. Recoverable within 30 days.")]
    async fn trash_thread(
        &self,
        Parameters(params): Parameters<TrashThreadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.client
            .trash_thread(&params.thread_id)
            .await
            .map_err(internal_err)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Thread {} moved to trash.",
            params.thread_id
        ))]))
    }
}

/// Compose an RFC 2822 message body. Rejects header-injection attempts (CR/LF
/// in any header field) per ADR-0005's `HeaderInjection` rule.
fn compose_rfc2822(
    to: &str,
    subject: &str,
    cc: Option<&str>,
    in_reply_to: Option<&str>,
    body: &str,
) -> Result<String, String> {
    reject_header_injection("to", to)?;
    reject_header_injection("subject", subject)?;
    if let Some(cc) = cc {
        reject_header_injection("cc", cc)?;
    }
    if let Some(irt) = in_reply_to {
        reject_header_injection("in_reply_to", irt)?;
    }

    let mut raw = String::new();
    let _ = writeln!(raw, "To: {to}\r");
    let _ = writeln!(raw, "Subject: {subject}\r");
    raw.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    if let Some(cc) = cc {
        let _ = writeln!(raw, "Cc: {cc}\r");
    }
    if let Some(irt) = in_reply_to {
        let _ = writeln!(raw, "In-Reply-To: {irt}\r");
        let _ = writeln!(raw, "References: {irt}\r");
    }
    raw.push_str("\r\n");
    raw.push_str(body);
    Ok(raw)
}

fn reject_header_injection(field: &str, value: &str) -> Result<(), String> {
    if value.contains('\r') || value.contains('\n') {
        return Err(format!(
            "header injection blocked: field `{field}` contains CR or LF"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn compose_rfc2822_rejects_lf_in_subject() {
        let err = compose_rfc2822(
            "to@example.com",
            "ok\nBcc: attacker@evil.com",
            None,
            None,
            "body",
        )
        .unwrap_err();
        assert!(err.contains("header injection"));
    }

    #[test]
    fn compose_rfc2822_rejects_cr_in_to() {
        let err = compose_rfc2822(
            "ok@example.com\rBcc: attacker@evil.com",
            "subject",
            None,
            None,
            "body",
        )
        .unwrap_err();
        assert!(err.contains("header injection"));
    }

    #[test]
    fn compose_rfc2822_writes_basic_message() {
        let raw = compose_rfc2822("to@example.com", "Hello", None, None, "Hi there").unwrap();
        assert!(raw.contains("To: to@example.com\r\n"));
        assert!(raw.contains("Subject: Hello\r\n"));
        assert!(raw.ends_with("Hi there"));
    }

    #[test]
    fn compose_rfc2822_includes_in_reply_to_and_references() {
        let raw = compose_rfc2822(
            "to@example.com",
            "Re: foo",
            Some("cc@example.com"),
            Some("<msg-id@example.com>"),
            "body",
        )
        .unwrap();
        assert!(raw.contains("Cc: cc@example.com\r\n"));
        assert!(raw.contains("In-Reply-To: <msg-id@example.com>\r\n"));
        assert!(raw.contains("References: <msg-id@example.com>\r\n"));
    }
}
