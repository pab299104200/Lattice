use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use tracing::{debug, warn};

use super::store::EventStoreError;

pub const EVENT_SCHEMA_MIGRATION_ID: &str = "p2_001_events";
pub const EVENT_SCHEMA_TARGET_VERSION: i64 = 2;

const EVENT_SCHEMA_SQL: &str = include_str!("schema.sql");

pub fn run_migrations(conn: &mut Connection) -> Result<(), EventStoreError> {
    debug!(
        migration_id = EVENT_SCHEMA_MIGRATION_ID,
        target_version = EVENT_SCHEMA_TARGET_VERSION,
        "checking event store migrations"
    );

    create_version_table(conn)?;
    let current_version = current_schema_version(conn)?;
    if current_version > EVENT_SCHEMA_TARGET_VERSION {
        warn!(
            migration_id = EVENT_SCHEMA_MIGRATION_ID,
            current_version,
            target_version = EVENT_SCHEMA_TARGET_VERSION,
            "event store schema is newer than this binary"
        );
        return Err(EventStoreError::MigrationFailed {
            migration_id: EVENT_SCHEMA_MIGRATION_ID,
            reason: "database schema is newer than this binary".to_string(),
        });
    }
    if current_version == EVENT_SCHEMA_TARGET_VERSION {
        debug!(
            migration_id = EVENT_SCHEMA_MIGRATION_ID,
            current_version, "event store schema is current"
        );
        return Ok(());
    }

    if current_version == 0 {
        return apply_schema(conn);
    }

    if current_version < 2 {
        apply_compaction_control_migration(conn)?;
    }
    Ok(())
}

fn create_version_table(conn: &Connection) -> Result<(), EventStoreError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS event_schema_version (
            version INTEGER PRIMARY KEY,
            applied_ts_unix_micros INTEGER NOT NULL
        )",
        params![],
    )?;
    Ok(())
}

fn current_schema_version(conn: &Connection) -> Result<i64, EventStoreError> {
    let version = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM event_schema_version",
        params![],
        |row| row.get(0),
    )?;
    Ok(version)
}

fn apply_schema(conn: &mut Connection) -> Result<(), EventStoreError> {
    let tx = conn.transaction()?;
    tx.execute_batch(EVENT_SCHEMA_SQL)
        .map_err(|error| EventStoreError::MigrationFailed {
            migration_id: EVENT_SCHEMA_MIGRATION_ID,
            reason: error.to_string(),
        })?;
    tx.execute(
        "INSERT OR IGNORE INTO event_schema_version (version, applied_ts_unix_micros)
         VALUES (?1, ?2)",
        params![EVENT_SCHEMA_TARGET_VERSION, now_unix_micros()],
    )?;
    tx.commit()?;
    debug!(
        migration_id = EVENT_SCHEMA_MIGRATION_ID,
        target_version = EVENT_SCHEMA_TARGET_VERSION,
        "applied event store schema"
    );
    Ok(())
}

fn apply_compaction_control_migration(conn: &mut Connection) -> Result<(), EventStoreError> {
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_compaction_control (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            allow_delete INTEGER NOT NULL DEFAULT 0 CHECK (allow_delete IN (0, 1))
        );
        INSERT OR IGNORE INTO event_compaction_control (id, allow_delete)
        VALUES (1, 0);
        DROP TRIGGER IF EXISTS events_no_delete;
        CREATE TRIGGER events_no_delete
        BEFORE DELETE ON events
        WHEN (SELECT allow_delete FROM event_compaction_control WHERE id = 1) != 1
        BEGIN
            SELECT RAISE(ABORT, 'events table is append-only');
        END;",
    )
    .map_err(|error| EventStoreError::MigrationFailed {
        migration_id: EVENT_SCHEMA_MIGRATION_ID,
        reason: error.to_string(),
    })?;
    tx.execute(
        "INSERT OR IGNORE INTO event_schema_version (version, applied_ts_unix_micros)
         VALUES (?1, ?2)",
        params![2, now_unix_micros()],
    )?;
    tx.commit()?;
    debug!(
        migration_id = EVENT_SCHEMA_MIGRATION_ID,
        target_version = 2,
        "applied event compaction control schema"
    );
    Ok(())
}

fn now_unix_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(i64::MAX as u128) as i64
}
