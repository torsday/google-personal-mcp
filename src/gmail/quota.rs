//! Gmail API quota cost table. Sourced from Google's documented per-method
//! costs and validated against the cheat-sheet in `CLAUDE.md` (last verified
//! 2026-05-16).
//!
//! Keep this as data, not constants scattered across call sites — if Google
//! updates a method's cost, one number changes here. The constants below are
//! the v1 quota units **per single invocation** as charged against the
//! per-user-per-minute bucket (see [`crate::rate_limit::GMAIL_CAPACITY`]).

/// Quota cost of one Gmail API method invocation, in per-user-per-minute units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GmailMethod {
    ThreadsList,
    ThreadsGet,
    ThreadsModify,
    ThreadsTrash,
    MessagesGet,
    MessagesSend,
    MessagesBatchModify,
    HistoryList,
    GetProfile,
    LabelsList,
}

impl GmailMethod {
    /// Returns the quota cost (units) charged per single call.
    #[allow(clippy::match_same_arms)] // Same cost ≠ same meaning; one arm per method.
    pub(crate) const fn cost(self) -> u32 {
        match self {
            Self::ThreadsList => 10,
            Self::ThreadsGet => 40,
            Self::ThreadsModify => 10,
            Self::ThreadsTrash => 20,
            Self::MessagesGet => 20,
            Self::MessagesSend => 100,
            Self::MessagesBatchModify => 50,
            Self::HistoryList => 2,
            Self::GetProfile => 1,
            Self::LabelsList => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_match_claude_md_cheatsheet() {
        // Validated 2026-05-16. If Google changes any of these, the cheat
        // sheet in CLAUDE.md and this table must both update.
        assert_eq!(GmailMethod::ThreadsList.cost(), 10);
        assert_eq!(GmailMethod::ThreadsGet.cost(), 40);
        assert_eq!(GmailMethod::ThreadsModify.cost(), 10);
        assert_eq!(GmailMethod::ThreadsTrash.cost(), 20);
        assert_eq!(GmailMethod::MessagesGet.cost(), 20);
        assert_eq!(GmailMethod::MessagesSend.cost(), 100);
        assert_eq!(GmailMethod::MessagesBatchModify.cost(), 50);
        assert_eq!(GmailMethod::HistoryList.cost(), 2);
        assert_eq!(GmailMethod::GetProfile.cost(), 1);
        assert_eq!(GmailMethod::LabelsList.cost(), 1);
    }
}
