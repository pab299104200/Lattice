CREATE TABLE IF NOT EXISTS event_schema_version (
    version INTEGER PRIMARY KEY,
    applied_ts_unix_micros INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS event_payloads (
    row_id INTEGER PRIMARY KEY AUTOINCREMENT,
    payload_hash BLOB NOT NULL UNIQUE,
    bytes BLOB NOT NULL,
    bytes_len INTEGER NOT NULL,
    created_ts_unix_micros INTEGER NOT NULL,
    CHECK (length(payload_hash) > 0),
    CHECK (bytes_len = length(bytes)),
    CHECK (created_ts_unix_micros >= 0)
);

CREATE TABLE IF NOT EXISTS events (
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_uuid TEXT NOT NULL UNIQUE,
    workspace_id TEXT NOT NULL,
    branch TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NULL,
    actor_kind TEXT NOT NULL,
    actor_detail TEXT NULL,
    kind TEXT NOT NULL,
    ts_unix_micros INTEGER NOT NULL,
    payload_hash BLOB NOT NULL,
    summary TEXT NOT NULL,
    payload_inline BLOB NULL,
    payload_spill_id INTEGER NULL,
    references_json TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    FOREIGN KEY (payload_spill_id) REFERENCES event_payloads(row_id),
    CHECK (event_uuid <> ''),
    CHECK (workspace_id <> ''),
    CHECK (branch <> ''),
    CHECK (session_id <> ''),
    CHECK (actor_kind <> ''),
    CHECK (kind <> ''),
    CHECK (ts_unix_micros >= 0),
    CHECK (length(payload_hash) > 0),
    CHECK (length(CAST(summary AS BLOB)) <= 512),
    CHECK (references_json <> ''),
    CHECK (schema_version > 0),
    CHECK (
        (payload_inline IS NOT NULL AND payload_spill_id IS NULL)
        OR (payload_inline IS NULL AND payload_spill_id IS NOT NULL)
    )
);

CREATE TABLE IF NOT EXISTS event_compaction_control (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    allow_delete INTEGER NOT NULL DEFAULT 0 CHECK (allow_delete IN (0, 1))
);

INSERT OR IGNORE INTO event_compaction_control (id, allow_delete)
VALUES (1, 0);

CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events table is append-only');
END;

CREATE TRIGGER IF NOT EXISTS events_no_delete
BEFORE DELETE ON events
WHEN (SELECT allow_delete FROM event_compaction_control WHERE id = 1) != 1
BEGIN
    SELECT RAISE(ABORT, 'events table is append-only');
END;

CREATE INDEX IF NOT EXISTS idx_events_task
    ON events(task_id, ts_unix_micros);
CREATE INDEX IF NOT EXISTS idx_events_session
    ON events(session_id, ts_unix_micros);
CREATE INDEX IF NOT EXISTS idx_events_workspace_branch
    ON events(workspace_id, branch, ts_unix_micros);
CREATE INDEX IF NOT EXISTS idx_events_kind_ts
    ON events(kind, ts_unix_micros);

CREATE INDEX IF NOT EXISTS idx_events_payload_spill
    ON events(payload_spill_id) WHERE payload_spill_id IS NOT NULL;

CREATE TRIGGER IF NOT EXISTS events_reclaim_payload
AFTER DELETE ON events
WHEN OLD.payload_spill_id IS NOT NULL
BEGIN
    DELETE FROM event_payloads
    WHERE row_id = OLD.payload_spill_id
      AND NOT EXISTS (SELECT 1 FROM events WHERE payload_spill_id = OLD.payload_spill_id);
END;
