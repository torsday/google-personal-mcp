//! Destructive-operation safety net per
//! [ADR-0012](../../docs/adr/0012-idempotency-and-dry-run.md).
//!
//! Two protections, separate concerns:
//!
//! - **`dry_run`** — short-circuit destructive ops before any Gmail API call.
//!   Tools propagate `dry_run: bool` from their input params into
//!   [`DestructiveContext::should_apply`] (non-send ops) or
//!   [`DestructiveContext::should_send`] (`send_email`).
//!
//! - **Send dedup** — `send_email` retries within a 60-second window hash to
//!   the same key and are returned as `Deduped { prior_message_id }` instead
//!   of sending twice. v0.2 keeps state in memory only — daemon restart resets
//!   the window (acceptable per ADR-0012 §"v0.2 scope").

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

/// Default dedup window when no config override is supplied. Matches the
/// ADR-0012 §"Send deduplication" default and what
/// `[idempotency] send_dedup_window_seconds` defaults to in config.
pub(crate) const DEFAULT_WINDOW_SECONDS: u64 = 60;

/// Outcome of a check on a non-send destructive op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Caller should proceed with the Gmail mutation.
    Apply,
    /// Caller should return a structured preview without mutating.
    DryRun,
}

/// Outcome of a check on a `send_email` op. Adds the `Deduped` variant
/// missing from [`Decision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendDecision {
    Apply,
    DryRun,
    /// An identical message was sent inside the window. The recorded
    /// `message_id` / `thread_id` are returned so the caller can surface them
    /// as if the second call had succeeded — agent retries that the network
    /// duplicated through us see the original successful response.
    Deduped {
        message_id: String,
        thread_id: Option<String>,
        sent_at: DateTime<Utc>,
    },
}

/// Identifying inputs to a `send_email` call. Borrows everywhere — the
/// caller builds this once per send and discards it. Recipient slices may be
/// in any order; the hash function sorts them.
#[derive(Debug)]
pub(crate) struct SendDedupKey<'a> {
    pub account: &'a str,
    pub to: &'a [String],
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body_text: &'a str,
    /// `in_reply_to_thread_id` distinguishes "same body, different thread"
    /// (legitimate two-sends) from "same body, same thread" (a real dup).
    pub in_reply_to_thread_id: Option<&'a str>,
}

/// Persisted record of a prior successful send. Returned in
/// `SendDecision::Deduped`.
#[derive(Debug, Clone)]
struct SendRecord {
    message_id: String,
    thread_id: Option<String>,
    sent_at: DateTime<Utc>,
}

/// Shared destructive-op safety net. Cloneable handle around an `Arc<RwLock<_>>`.
#[derive(Clone)]
pub(crate) struct DestructiveContext {
    cache: Arc<RwLock<HashMap<String, SendRecord>>>,
    window: Duration,
}

impl Default for DestructiveContext {
    fn default() -> Self {
        Self::with_window(Duration::from_secs(DEFAULT_WINDOW_SECONDS))
    }
}

impl DestructiveContext {
    pub(crate) fn with_window(window: Duration) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            window,
        }
    }

    /// Non-send destructive ops (`archive`, `trash`, `modify_thread_labels`).
    /// Just routes the `dry_run` bool through the [`Decision`] enum so call
    /// sites have a single uniform shape.
    pub(crate) const fn should_apply(dry_run: bool) -> Decision {
        if dry_run {
            Decision::DryRun
        } else {
            Decision::Apply
        }
    }

    /// Check whether a `send_email` should proceed. Returns:
    /// - `DryRun` when `dry_run` is true (no cache lookup).
    /// - `Deduped` when the key was recorded within the window.
    /// - `Apply` otherwise. Pruning of expired entries happens on read.
    pub(crate) async fn should_send(&self, dry_run: bool, key: &SendDedupKey<'_>) -> SendDecision {
        if dry_run {
            return SendDecision::DryRun;
        }
        let hash = dedup_hash(key);
        let now = Utc::now();

        // Fast path: read lock, return the cached record if still in-window.
        // The lock-then-recheck pattern matches `TokenManager::access_token`.
        {
            let cache = self.cache.read().await;
            if let Some(rec) = cache.get(&hash) {
                if is_within(rec.sent_at, now, self.window) {
                    return SendDecision::Deduped {
                        message_id: rec.message_id.clone(),
                        thread_id: rec.thread_id.clone(),
                        sent_at: rec.sent_at,
                    };
                }
            }
        }
        SendDecision::Apply
    }

    /// Record a successful send so subsequent identical requests within the
    /// window short-circuit to `Deduped`. Also prunes any entries older than
    /// the window — the cache stays bounded to roughly `send_rate × window`.
    pub(crate) async fn record_send(
        &self,
        key: &SendDedupKey<'_>,
        message_id: String,
        thread_id: Option<String>,
    ) {
        let hash = dedup_hash(key);
        let now = Utc::now();
        let mut cache = self.cache.write().await;
        cache.retain(|_, rec| is_within(rec.sent_at, now, self.window));
        cache.insert(
            hash,
            SendRecord {
                message_id,
                thread_id,
                sent_at: now,
            },
        );
    }
}

/// SHA-256 over `account || sorted to || sorted cc || sorted bcc || subject
/// || body || in_reply_to_thread_id`. Returned as lowercase hex. Stable
/// across permutations of recipient lists.
pub(crate) fn dedup_hash(key: &SendDedupKey<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"account=");
    hasher.update(key.account.as_bytes());
    hasher.update(b"\x00");
    write_sorted(&mut hasher, "to=", key.to);
    write_sorted(&mut hasher, "cc=", key.cc);
    write_sorted(&mut hasher, "bcc=", key.bcc);
    hasher.update(b"subject=");
    hasher.update(key.subject.as_bytes());
    hasher.update(b"\x00");
    hasher.update(b"body=");
    hasher.update(key.body_text.as_bytes());
    hasher.update(b"\x00");
    hasher.update(b"in_reply_to=");
    hasher.update(key.in_reply_to_thread_id.unwrap_or("").as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

fn write_sorted(hasher: &mut Sha256, prefix: &str, items: &[String]) {
    hasher.update(prefix.as_bytes());
    let mut sorted: Vec<&str> = items.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    for s in sorted {
        hasher.update(s.as_bytes());
        // `\x01` separator distinguishes addresses from each other, and a
        // trailing `\x00` distinguishes the list from the next field. Without
        // these a list `["a", "bc"]` would hash identically to `["ab", "c"]`.
        hasher.update(b"\x01");
    }
    hasher.update(b"\x00");
}

fn is_within(sent_at: DateTime<Utc>, now: DateTime<Utc>, window: Duration) -> bool {
    let elapsed = now.signed_duration_since(sent_at);
    elapsed >= chrono::Duration::zero()
        && elapsed
            < chrono::Duration::from_std(window).unwrap_or_else(|_| chrono::Duration::seconds(60))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn key<'a>(
        account: &'a str,
        to: &'a [String],
        subject: &'a str,
        body: &'a str,
    ) -> SendDedupKey<'a> {
        SendDedupKey {
            account,
            to,
            cc: &[],
            bcc: &[],
            subject,
            body_text: body,
            in_reply_to_thread_id: None,
        }
    }

    // ── Decision routing (AC) ────────────────────────────────────────────────

    #[test]
    fn should_apply_routes_dry_run() {
        assert_eq!(DestructiveContext::should_apply(false), Decision::Apply);
        assert_eq!(DestructiveContext::should_apply(true), Decision::DryRun);
    }

    #[tokio::test]
    async fn should_send_short_circuits_on_dry_run() {
        let ctx = DestructiveContext::default();
        let to = vec!["a@example.com".to_owned()];
        let k = key("work", &to, "hi", "body");
        assert_eq!(ctx.should_send(true, &k).await, SendDecision::DryRun);
        // dry_run does NOT consume cache space.
        ctx.record_send(&k, "msg-1".into(), Some("thr-1".into()))
            .await;
        assert!(matches!(
            ctx.should_send(false, &k).await,
            SendDecision::Deduped { .. }
        ));
    }

    // ── Hash stability (AC) ──────────────────────────────────────────────────

    #[test]
    fn hash_is_stable_across_address_ordering() {
        let to_a = vec!["a@x".to_owned(), "b@y".to_owned()];
        let to_b = vec!["b@y".to_owned(), "a@x".to_owned()];
        let k_a = key("work", &to_a, "hi", "body");
        let k_b = key("work", &to_b, "hi", "body");
        assert_eq!(dedup_hash(&k_a), dedup_hash(&k_b));
    }

    #[test]
    fn hash_changes_with_account() {
        let to = vec!["a@x".to_owned()];
        let k_work = key("work", &to, "hi", "body");
        let k_personal = key("personal", &to, "hi", "body");
        assert_ne!(dedup_hash(&k_work), dedup_hash(&k_personal));
    }

    #[test]
    fn hash_changes_with_subject_body_or_thread() {
        let to = vec!["a@x".to_owned()];
        let base = dedup_hash(&key("work", &to, "hi", "body"));
        assert_ne!(base, dedup_hash(&key("work", &to, "hi2", "body")));
        assert_ne!(base, dedup_hash(&key("work", &to, "hi", "body2")));
        let mut k = key("work", &to, "hi", "body");
        k.in_reply_to_thread_id = Some("thr-1");
        assert_ne!(base, dedup_hash(&k));
    }

    #[test]
    fn hash_distinguishes_concatenation_collisions() {
        // Without separators, [a, bc] would hash the same as [ab, c].
        let to1 = vec!["a".to_owned(), "bc".to_owned()];
        let to2 = vec!["ab".to_owned(), "c".to_owned()];
        let k1 = key("work", &to1, "", "");
        let k2 = key("work", &to2, "", "");
        assert_ne!(dedup_hash(&k1), dedup_hash(&k2));
    }

    #[test]
    fn hash_is_64_lowercase_hex() {
        let to = vec!["a@x".to_owned()];
        let h = dedup_hash(&key("work", &to, "hi", "body"));
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    // ── Dedup window behavior (AC) ───────────────────────────────────────────

    #[tokio::test]
    async fn second_identical_send_within_window_is_deduped() {
        let ctx = DestructiveContext::default();
        let to = vec!["a@x".to_owned()];
        let k = key("work", &to, "hi", "body");

        assert_eq!(ctx.should_send(false, &k).await, SendDecision::Apply);
        ctx.record_send(&k, "msg-1".into(), Some("thr-1".into()))
            .await;

        match ctx.should_send(false, &k).await {
            SendDecision::Deduped {
                message_id,
                thread_id,
                ..
            } => {
                assert_eq!(message_id, "msg-1");
                assert_eq!(thread_id.as_deref(), Some("thr-1"));
            }
            other => panic!("expected Deduped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn different_subject_is_not_deduped() {
        let ctx = DestructiveContext::default();
        let to = vec!["a@x".to_owned()];
        let k1 = key("work", &to, "hi", "body");
        let k2 = key("work", &to, "hello", "body");
        ctx.record_send(&k1, "msg-1".into(), None).await;
        assert_eq!(ctx.should_send(false, &k2).await, SendDecision::Apply);
    }

    #[tokio::test]
    async fn expired_entries_are_pruned_on_record() {
        // Use a 1-second window and manually backdate the inserted record.
        let ctx = DestructiveContext::with_window(Duration::from_secs(1));
        let to = vec!["a@x".to_owned()];
        let k = key("work", &to, "hi", "body");

        // Insert an entry directly with an old sent_at to simulate expiry.
        {
            let mut cache = ctx.cache.write().await;
            cache.insert(
                dedup_hash(&k),
                SendRecord {
                    message_id: "old-msg".into(),
                    thread_id: None,
                    sent_at: Utc::now() - chrono::Duration::seconds(10),
                },
            );
        }

        // Expired record → should not dedup.
        assert_eq!(ctx.should_send(false, &k).await, SendDecision::Apply);

        // Recording a different key should also evict the expired one.
        let to_b = vec!["b@y".to_owned()];
        ctx.record_send(&key("work", &to_b, "hi", "body"), "msg-new".into(), None)
            .await;
        assert!(
            !ctx.cache.read().await.contains_key(&dedup_hash(&k)),
            "expired entry not pruned"
        );
    }

    // ── is_within boundary ──────────────────────────────────────────────────

    #[test]
    fn within_window_boundary() {
        let now = Utc::now();
        let win = Duration::from_mins(1);
        // 30s ago — within
        assert!(is_within(now - chrono::Duration::seconds(30), now, win));
        // Exact boundary — sent_at = now − 60s — NOT within (strict <).
        assert!(!is_within(now - chrono::Duration::seconds(60), now, win));
        // Slightly over — not within
        assert!(!is_within(now - chrono::Duration::seconds(61), now, win));
        // Future sent_at (clock skew) — not within
        assert!(!is_within(now + chrono::Duration::seconds(1), now, win));
    }
}
