-- Migration 002 — Phase 4 of docs/cache-implementation-plan.md (#81).
--
-- Adds the `fetched_at_history_id` column to `query_cache` so each cached
-- list result records the account history watermark Gmail was at when
-- the row was fetched. ADR-0009 §"Race-prevention":
--
--   * Write path snapshots `last_history_id` before the upstream API call
--     and discards the resulting INSERT if the watermark advances during
--     the round-trip — conservative, eliminates the published-stale race.
--   * Read path refuses any row whose `fetched_at_history_id` is older
--     than the current `last_history_id` — replaces Phase 3's
--     brute-force "drop every row on any mutation" rule.
--
-- Existing rows get DEFAULT 0. Because every live account's
-- `last_history_id` is > 0 once first-touch sync completes, those rows
-- effectively expire on schema upgrade — exactly the desired conservative
-- behavior (one round-trip of cold cache, then steady state).

ALTER TABLE query_cache
    ADD COLUMN fetched_at_history_id INTEGER NOT NULL DEFAULT 0;
