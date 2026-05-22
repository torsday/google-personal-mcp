-- ADR-0009 §Schema (v1) — per-account cache.
-- One file per account at ~/.config/google-personal-mcp/cache/<account>.db.
-- WAL mode is enabled by the connection opener, not by this script.

CREATE TABLE messages (
    id               TEXT PRIMARY KEY,
    thread_id        TEXT NOT NULL,
    internal_date    INTEGER NOT NULL,        -- Gmail internalDate (ms epoch)
    headers_json     TEXT NOT NULL,           -- full headers as JSON
    body_text        TEXT,                    -- best-effort plain-text per ADR-0010
    body_html        TEXT,                    -- raw HTML if present
    snippet          TEXT,
    has_attachments  INTEGER NOT NULL,        -- bool 0/1
    attachments_json TEXT,                    -- attachment metadata (no content) per ADR-0010
    raw_size         INTEGER,
    fetched_at       INTEGER NOT NULL,        -- ms epoch; diagnostics only
    deleted_at       INTEGER                  -- nullable; set on Gmail-side delete
);

CREATE INDEX idx_messages_thread ON messages (thread_id);
CREATE INDEX idx_messages_date ON messages (internal_date DESC);

-- Per-message label state. Mutates via history.list deltas.
CREATE TABLE message_labels (
    message_id TEXT NOT NULL,
    label_id   TEXT NOT NULL,
    PRIMARY KEY (message_id, label_id),
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE TABLE threads (
    id          TEXT PRIMARY KEY,
    snippet     TEXT,
    history_id  INTEGER,                      -- latest history event we know
    fetched_at  INTEGER NOT NULL
);

CREATE TABLE labels (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT,                         -- "system" | "user"
    fetched_at  INTEGER NOT NULL
);

-- Single-row sync state per account.
CREATE TABLE account_state (
    rowid              INTEGER PRIMARY KEY CHECK (rowid = 1),
    last_history_id    INTEGER,
    last_full_sync_at  INTEGER,
    schema_version     INTEGER NOT NULL DEFAULT 1
);

-- Search-result memoization. TTL'd; History API invalidation in a later migration.
CREATE TABLE query_cache (
    query_hash      TEXT PRIMARY KEY,         -- sha256(query || max_results)
    query           TEXT NOT NULL,
    max_results     INTEGER NOT NULL,
    page_token      TEXT,
    result_ids_json TEXT NOT NULL,
    cached_at       INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL
);

-- Seed the single-row account_state at version 1. Migrations beyond this set
-- schema_version on the existing row; the migration framework refuses to start
-- if the on-disk value exceeds the highest known migration target.
INSERT INTO account_state (rowid, schema_version) VALUES (1, 1);
