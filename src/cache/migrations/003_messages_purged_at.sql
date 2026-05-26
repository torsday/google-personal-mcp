-- Migration 003 — ADR-0019 §Cache body age cap (#169).
--
-- Adds nullable `purged_at` (ms epoch) to `messages`. Set by the
-- body-purge phase of the eviction task when a row's body columns
-- (body_text, body_html, snippet, attachments_json) get nulled because
-- the row exceeded `body_max_age_days` or was soft-deleted > 7 days
-- ago. The row itself stays (id, thread_id, internal_date,
-- headers_json, has_attachments survive) so lookup_thread_metadata
-- still resolves; lookup_thread treats `purged_at IS NOT NULL`
-- identically to `body_text IS NULL` — miss + refetch.
--
-- No backfill. Existing rows keep `purged_at = NULL` (the row was
-- never purged; its body is whatever insert_thread / sync wrote).

ALTER TABLE messages
    ADD COLUMN purged_at INTEGER;
