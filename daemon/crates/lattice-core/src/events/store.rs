use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, CachedStatement, Connection, OptionalExtension};
use thiserror::Error;
use tracing::{debug, info, warn};

use super::migrations::run_migrations;

const EVENT_DB_BUSY_TIMEOUT_SECS: u64 = 5;
const EVENT_DB_AUTO_CHECKPOINT_PAGES: u32 = 100;
const EVENT_DB_JOURNAL_SIZE_LIMIT_BYTES: u32 = 1_048_576;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_SPILLED_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_QUERY_LIMIT: i64 = 10_000;
const INSERT_ENVELOPE_SQL: &str = "INSERT INTO events
    (event_uuid, workspace_id, branch, session_id, task_id, actor_kind, actor_detail,
     kind, ts_unix_micros, payload_hash, summary, payload_inline, payload_spill_id,
     references_json, schema_version)
 VALUES
    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)";

#[derive(Debug, Error)]
pub enum EventStoreError {
    #[error("event store I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("event store SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("event store migration {migration_id} failed: {reason}")]
    MigrationFailed {
        migration_id: &'static str,
        reason: String,
    },
    #[error("event payload is too large: {bytes_len} bytes exceeds {max_bytes} bytes")]
    PayloadTooLarge { bytes_len: usize, max_bytes: usize },
    #[error("event envelope row is invalid: {reason}")]
    EnvelopeInvalid { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertEnvelopeRow {
    pub event_uuid: String,
    pub workspace_id: String,
    pub branch: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub actor_kind: String,
    pub actor_detail: Option<String>,
    pub kind: String,
    pub ts_unix_micros: i64,
    pub payload_hash: Vec<u8>,
    pub summary: String,
    pub payload_inline: Option<Vec<u8>>,
    pub payload_spill_id: Option<i64>,
    pub references_json: String,
    pub schema_version: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEnvelopeRow {
    pub event_id: i64,
    pub event_uuid: String,
    pub workspace_id: String,
    pub branch: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub actor_kind: String,
    pub actor_detail: Option<String>,
    pub kind: String,
    pub ts_unix_micros: i64,
    pub payload_hash: Vec<u8>,
    pub summary: String,
    pub payload_inline: Option<Vec<u8>>,
    pub payload_spill_id: Option<i64>,
    pub references_json: String,
    pub schema_version: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventPayloadRow {
    pub row_id: i64,
    pub payload_hash: Vec<u8>,
    pub bytes: Vec<u8>,
    pub bytes_len: i64,
    pub created_ts_unix_micros: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventCursor {
    pub row_id: i64,
    pub event_uuid: String,
    pub workspace_id: String,
    pub branch: String,
}

pub struct EventStore {
    conn: Mutex<Connection>,
}

impl EventStore {
    pub fn open(path: &Path) -> Result<Self, EventStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut conn = Connection::open(path)?;
        configure_connection(&conn, true)?;
        run_migrations(&mut conn)?;
        info!(
            event_type = "recovery",
            db_path = %path.display(),
            "event store opened with SQLite recovery enabled"
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, EventStoreError> {
        let mut conn = Connection::open_in_memory()?;
        configure_connection(&conn, false)?;
        run_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert_or_get_payload(
        &self,
        payload_hash: &[u8],
        bytes: &[u8],
        created_ts_unix_micros: i64,
    ) -> Result<i64, EventStoreError> {
        validate_payload(payload_hash, bytes, created_ts_unix_micros)?;
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO event_payloads
                (payload_hash, bytes, bytes_len, created_ts_unix_micros)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                payload_hash,
                bytes,
                checked_len(bytes)?,
                created_ts_unix_micros
            ],
        )?;
        let row_id = conn.query_row(
            "SELECT row_id FROM event_payloads WHERE payload_hash = ?1",
            params![payload_hash],
            |row| row.get(0),
        )?;
        Ok(row_id)
    }

    pub fn insert_envelope_row(&self, row: &InsertEnvelopeRow) -> Result<i64, EventStoreError> {
        validate_envelope_row(row)?;
        let conn = self.lock_conn()?;
        let inserted = execute_insert(&conn, row, INSERT_ENVELOPE_SQL);

        match inserted {
            Ok(_) => Ok(conn.last_insert_rowid()),
            Err(error) => {
                warn!(
                    event_uuid = row.event_uuid.as_str(),
                    sqlite_error = %error,
                    "failed to insert event envelope row"
                );
                Err(EventStoreError::Sqlite(error))
            }
        }
    }

    pub fn insert_envelope_row_cached(
        &self,
        row: &InsertEnvelopeRow,
    ) -> Result<i64, EventStoreError> {
        validate_envelope_row(row)?;
        let conn = self.lock_conn()?;
        let mut statement = conn.prepare_cached(INSERT_ENVELOPE_SQL)?;
        execute_insert_cached(&mut statement, row).map_err(|error| {
            warn!(
                event_uuid = row.event_uuid.as_str(),
                sqlite_error = %error,
                "failed to insert cached event envelope row"
            );
            EventStoreError::Sqlite(error)
        })?;
        Ok(conn.last_insert_rowid())
    }

    pub fn query_events_by_task(
        &self,
        task_id: &str,
        limit: i64,
    ) -> Result<Vec<EventEnvelopeRow>, EventStoreError> {
        validate_scope_value("task_id", task_id)?;
        self.query_events(
            "SELECT event_id, event_uuid, workspace_id, branch, session_id, task_id, actor_kind,
                    actor_detail, kind, ts_unix_micros, payload_hash, summary, payload_inline,
                    payload_spill_id, references_json, schema_version
             FROM events
             WHERE task_id = ?1
             ORDER BY ts_unix_micros ASC, event_id ASC
             LIMIT ?2",
            params![task_id, checked_limit(limit)?],
        )
    }

    pub fn query_events_by_session(
        &self,
        session_id: &str,
        limit: i64,
    ) -> Result<Vec<EventEnvelopeRow>, EventStoreError> {
        validate_scope_value("session_id", session_id)?;
        self.query_events(
            "SELECT event_id, event_uuid, workspace_id, branch, session_id, task_id, actor_kind,
                    actor_detail, kind, ts_unix_micros, payload_hash, summary, payload_inline,
                    payload_spill_id, references_json, schema_version
             FROM events
             WHERE session_id = ?1
             ORDER BY ts_unix_micros ASC, event_id ASC
             LIMIT ?2",
            params![session_id, checked_limit(limit)?],
        )
    }

    pub fn query_events_by_workspace_branch(
        &self,
        workspace_id: &str,
        branch: &str,
        limit: i64,
    ) -> Result<Vec<EventEnvelopeRow>, EventStoreError> {
        validate_scope_value("workspace_id", workspace_id)?;
        validate_scope_value("branch", branch)?;
        self.query_events(
            "SELECT event_id, event_uuid, workspace_id, branch, session_id, task_id, actor_kind,
                    actor_detail, kind, ts_unix_micros, payload_hash, summary, payload_inline,
                    payload_spill_id, references_json, schema_version
             FROM events
             WHERE workspace_id = ?1 AND branch = ?2
             ORDER BY ts_unix_micros ASC, event_id ASC
             LIMIT ?3",
            params![workspace_id, branch, checked_limit(limit)?],
        )
    }

    pub fn get_payload_row(&self, row_id: i64) -> Result<Option<EventPayloadRow>, EventStoreError> {
        let conn = self.lock_conn()?;
        let mut statement = conn.prepare(
            "SELECT row_id, payload_hash, bytes, bytes_len, created_ts_unix_micros
             FROM event_payloads
             WHERE row_id = ?1",
        )?;
        let mut rows = statement.query(params![row_id])?;
        let payload = match rows.next()? {
            Some(row) => Some(EventPayloadRow {
                row_id: row.get(0)?,
                payload_hash: row.get(1)?,
                bytes: row.get(2)?,
                bytes_len: row.get(3)?,
                created_ts_unix_micros: row.get(4)?,
            }),
            None => None,
        };
        Ok(payload)
    }

    pub fn latest_cursor(&self) -> Result<Option<EventCursor>, EventStoreError> {
        let conn = self.lock_conn()?;
        let mut statement = conn.prepare(
            "SELECT event_id, event_uuid, workspace_id, branch
             FROM events
             ORDER BY event_id DESC
             LIMIT 1",
        )?;
        let mut rows = statement.query([])?;
        let cursor = match rows.next()? {
            Some(row) => Some(EventCursor {
                row_id: row.get(0)?,
                event_uuid: row.get(1)?,
                workspace_id: row.get(2)?,
                branch: row.get(3)?,
            }),
            None => None,
        };
        Ok(cursor)
    }

    pub fn event_count_after(&self, row_id: i64) -> Result<u64, EventStoreError> {
        let conn = self.lock_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_id > ?1",
            params![row_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    pub fn compactable_event_count_after(&self, row_id: i64) -> Result<u64, EventStoreError> {
        let conn = self.lock_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE event_id > ?1
               AND NOT (kind = 'memory_consolidated'
                    AND summary LIKE 'event log snapshot:%')",
            params![row_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    pub fn event_count_through(&self, row_id: i64) -> Result<u64, EventStoreError> {
        let conn = self.lock_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_id <= ?1",
            params![row_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    pub fn query_events_after_row_id(
        &self,
        row_id: i64,
        limit: i64,
    ) -> Result<Vec<EventEnvelopeRow>, EventStoreError> {
        self.query_events(
            "SELECT event_id, event_uuid, workspace_id, branch, session_id, task_id, actor_kind,
                    actor_detail, kind, ts_unix_micros, payload_hash, summary, payload_inline,
                    payload_spill_id, references_json, schema_version
             FROM events
             WHERE event_id > ?1
             ORDER BY event_id ASC
             LIMIT ?2",
            params![row_id, checked_limit(limit)?],
        )
    }

    pub fn row_id_for_event_uuid(&self, event_uuid: &str) -> Result<Option<i64>, EventStoreError> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT event_id FROM events WHERE event_uuid = ?1",
            params![event_uuid],
            |row| row.get(0),
        )
        .optional()
        .map_err(EventStoreError::from)
    }

    pub fn truncate_through(&self, row_id: i64) -> Result<u64, EventStoreError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE event_compaction_control SET allow_delete = 1 WHERE id = 1",
            [],
        )?;
        let deleted = tx.execute("DELETE FROM events WHERE event_id <= ?1", params![row_id]);
        tx.execute(
            "UPDATE event_compaction_control SET allow_delete = 0 WHERE id = 1",
            [],
        )?;
        let deleted = deleted?;
        tx.commit()?;
        Ok(deleted as u64)
    }

    fn query_events<P>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<EventEnvelopeRow>, EventStoreError>
    where
        P: rusqlite::Params,
    {
        let conn = self.lock_conn()?;
        let mut statement = conn.prepare(sql)?;
        let rows = statement.query_map(params, read_event_row)?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    pub(crate) fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>, EventStoreError> {
        self.conn
            .lock()
            .map_err(|_| EventStoreError::EnvelopeInvalid {
                reason: "event store connection lock was poisoned".to_string(),
            })
    }

    #[cfg(test)]
    pub(crate) fn with_connection<R>(&self, inspect: impl FnOnce(&Connection) -> R) -> R {
        let guard = self.conn.lock().expect("connection lock succeeds");
        inspect(&guard)
    }

    pub fn checkpoint_wal(&self) -> Result<(), EventStoreError> {
        let conn = self.lock_conn()?;
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        Ok(())
    }
}

fn execute_insert(
    conn: &Connection,
    row: &InsertEnvelopeRow,
    sql: &str,
) -> Result<usize, rusqlite::Error> {
    conn.execute(
        sql,
        params![
            row.event_uuid,
            row.workspace_id,
            row.branch,
            row.session_id,
            row.task_id,
            row.actor_kind,
            row.actor_detail,
            row.kind,
            row.ts_unix_micros,
            row.payload_hash,
            row.summary,
            row.payload_inline,
            row.payload_spill_id,
            row.references_json,
            row.schema_version,
        ],
    )
}

fn execute_insert_cached(
    statement: &mut CachedStatement<'_>,
    row: &InsertEnvelopeRow,
) -> Result<usize, rusqlite::Error> {
    statement.execute(params![
        row.event_uuid,
        row.workspace_id,
        row.branch,
        row.session_id,
        row.task_id,
        row.actor_kind,
        row.actor_detail,
        row.kind,
        row.ts_unix_micros,
        row.payload_hash,
        row.summary,
        row.payload_inline,
        row.payload_spill_id,
        row.references_json,
        row.schema_version,
    ])
}

fn configure_connection(conn: &Connection, enable_wal: bool) -> Result<(), EventStoreError> {
    conn.busy_timeout(Duration::from_secs(EVENT_DB_BUSY_TIMEOUT_SECS))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    if enable_wal {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "wal_autocheckpoint", EVENT_DB_AUTO_CHECKPOINT_PAGES)?;
        conn.pragma_update(
            None,
            "journal_size_limit",
            EVENT_DB_JOURNAL_SIZE_LIMIT_BYTES,
        )?;
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
    }
    debug!(
        wal_enabled = enable_wal,
        "configured event store SQLite connection"
    );
    Ok(())
}

fn read_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventEnvelopeRow> {
    Ok(EventEnvelopeRow {
        event_id: row.get(0)?,
        event_uuid: row.get(1)?,
        workspace_id: row.get(2)?,
        branch: row.get(3)?,
        session_id: row.get(4)?,
        task_id: row.get(5)?,
        actor_kind: row.get(6)?,
        actor_detail: row.get(7)?,
        kind: row.get(8)?,
        ts_unix_micros: row.get(9)?,
        payload_hash: row.get(10)?,
        summary: row.get(11)?,
        payload_inline: row.get(12)?,
        payload_spill_id: row.get(13)?,
        references_json: row.get(14)?,
        schema_version: row.get(15)?,
    })
}

fn validate_payload(
    payload_hash: &[u8],
    bytes: &[u8],
    created_ts_unix_micros: i64,
) -> Result<(), EventStoreError> {
    if payload_hash.is_empty() {
        return invalid("payload_hash must not be empty");
    }
    if bytes.len() > MAX_SPILLED_PAYLOAD_BYTES {
        return Err(EventStoreError::PayloadTooLarge {
            bytes_len: bytes.len(),
            max_bytes: MAX_SPILLED_PAYLOAD_BYTES,
        });
    }
    if created_ts_unix_micros < 0 {
        return invalid("created_ts_unix_micros must be non-negative");
    }
    Ok(())
}

fn validate_envelope_row(row: &InsertEnvelopeRow) -> Result<(), EventStoreError> {
    validate_scope_value("event_uuid", &row.event_uuid)?;
    validate_scope_value("workspace_id", &row.workspace_id)?;
    validate_scope_value("branch", &row.branch)?;
    validate_scope_value("session_id", &row.session_id)?;
    validate_scope_value("actor_kind", &row.actor_kind)?;
    validate_scope_value("kind", &row.kind)?;
    validate_payload_hash(&row.payload_hash)?;
    validate_summary(&row.summary)?;
    validate_payload_location(row)?;
    validate_references_json(&row.references_json)?;
    if row.ts_unix_micros < 0 {
        return invalid("ts_unix_micros must be non-negative");
    }
    if row.schema_version <= 0 {
        return invalid("schema_version must be positive");
    }
    Ok(())
}

fn validate_payload_hash(payload_hash: &[u8]) -> Result<(), EventStoreError> {
    if payload_hash.is_empty() {
        return invalid("payload_hash must not be empty");
    }
    Ok(())
}

fn validate_summary(summary: &str) -> Result<(), EventStoreError> {
    if summary.as_bytes().len() > 512 {
        return invalid("summary exceeds 512 bytes");
    }
    Ok(())
}

fn validate_payload_location(row: &InsertEnvelopeRow) -> Result<(), EventStoreError> {
    match (&row.payload_inline, row.payload_spill_id) {
        (Some(_), None) | (None, Some(_)) => Ok(()),
        _ => invalid("exactly one payload location must be set"),
    }
}

fn validate_references_json(references_json: &str) -> Result<(), EventStoreError> {
    let parsed: serde_json::Value = serde_json::from_str(references_json).map_err(|error| {
        EventStoreError::EnvelopeInvalid {
            reason: format!("references_json is not valid JSON: {error}"),
        }
    })?;
    if parsed.is_array() {
        return Ok(());
    }
    invalid("references_json must be a JSON array")
}

fn validate_scope_value(field: &'static str, value: &str) -> Result<(), EventStoreError> {
    if value.is_empty() {
        return Err(EventStoreError::EnvelopeInvalid {
            reason: format!("{field} must not be empty"),
        });
    }
    Ok(())
}

fn checked_len(bytes: &[u8]) -> Result<i64, EventStoreError> {
    i64::try_from(bytes.len()).map_err(|_| EventStoreError::PayloadTooLarge {
        bytes_len: bytes.len(),
        max_bytes: i64::MAX as usize,
    })
}

fn checked_limit(limit: i64) -> Result<i64, EventStoreError> {
    if (1..=MAX_QUERY_LIMIT).contains(&limit) {
        return Ok(limit);
    }
    invalid("query limit must be between 1 and 10000")
}

fn invalid<T>(reason: impl Into<String>) -> Result<T, EventStoreError> {
    Err(EventStoreError::EnvelopeInvalid {
        reason: reason.into(),
    })
}
