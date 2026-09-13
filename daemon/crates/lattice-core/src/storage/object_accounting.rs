//! Indexed physical-byte receipts shared by immutable object stores.
use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct ObjectAccounting {
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub complete: bool,
}

/// Existing payloads stay NULL until a bounded maintenance page measures them.
/// Partial-index lookup avoids rescanning the already accounted prefix.
pub(crate) fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let has_column: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('objects') WHERE name='allocated_bytes')", [], |row| row.get(0),
        )?;
        let has_totals: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='object_accounting_totals')", [], |row| row.get(0))?;
        if has_column && !has_totals {
            bail!("object allocation receipts exist without their totals authority; bounded accounting repair is required");
        }
        if !has_column {
            connection.execute_batch("ALTER TABLE objects ADD COLUMN allocated_bytes INTEGER CHECK(allocated_bytes IS NULL OR allocated_bytes>=0)")?;
        }
        let has_error:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('objects') WHERE name='accounting_error')",[],|r|r.get(0))?;
        if !has_error {
            connection.execute_batch("ALTER TABLE objects ADD COLUMN accounting_error TEXT")?;
        }
        connection.execute_batch("DROP INDEX IF EXISTS objects_missing_accounting; CREATE INDEX objects_missing_accounting ON objects(key) WHERE allocated_bytes IS NULL AND accounting_error IS NULL;
            CREATE TABLE IF NOT EXISTS object_accounting_totals(id INTEGER PRIMARY KEY CHECK(id=1),logical_bytes INTEGER NOT NULL,allocated_bytes INTEGER NOT NULL);
            INSERT OR IGNORE INTO object_accounting_totals VALUES(1,0,0);
            CREATE TRIGGER IF NOT EXISTS accounting_object_insert AFTER INSERT ON objects WHEN NEW.allocated_bytes IS NOT NULL BEGIN
              UPDATE object_accounting_totals SET logical_bytes=logical_bytes+NEW.bytes,allocated_bytes=allocated_bytes+NEW.allocated_bytes WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS accounting_object_update AFTER UPDATE OF bytes,allocated_bytes ON objects BEGIN
              UPDATE object_accounting_totals SET logical_bytes=logical_bytes+CASE WHEN NEW.allocated_bytes IS NULL THEN 0 ELSE NEW.bytes END-CASE WHEN OLD.allocated_bytes IS NULL THEN 0 ELSE OLD.bytes END,
                allocated_bytes=allocated_bytes+COALESCE(NEW.allocated_bytes,0)-COALESCE(OLD.allocated_bytes,0) WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS accounting_object_delete AFTER DELETE ON objects WHEN OLD.allocated_bytes IS NOT NULL BEGIN
              UPDATE object_accounting_totals SET logical_bytes=logical_bytes-OLD.bytes,allocated_bytes=allocated_bytes-OLD.allocated_bytes WHERE id=1;
            END;")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            connection.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

pub(crate) fn record(
    connection: &Connection,
    key: &str,
    logical: u64,
    allocated: u64,
) -> Result<()> {
    if logical > i64::MAX as u64 || allocated > i64::MAX as u64 {
        bail!("object accounting size exceeds SQLite integer range");
    }
    let changed = connection.execute(
        "UPDATE objects SET bytes=?2,allocated_bytes=?3,accounting_error=NULL WHERE key=?1",
        params![key, logical, allocated],
    )?;
    if changed != 1 {
        bail!("object disappeared before its allocation receipt was committed");
    }
    Ok(())
}

pub(crate) fn pending(connection: &Connection, limit: usize) -> Result<Vec<String>> {
    if limit == 0 || limit > 4096 {
        bail!("object accounting page must contain 1..=4096 objects");
    }
    let mut statement = connection
        .prepare("SELECT key FROM objects WHERE allocated_bytes IS NULL AND accounting_error IS NULL ORDER BY key LIMIT ?1")?;
    let keys = statement
        .query_map([limit], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(keys)
}

pub(crate) fn read(connection: &Connection) -> Result<ObjectAccounting> {
    Ok(connection.query_row("SELECT logical_bytes,allocated_bytes,NOT EXISTS(SELECT 1 FROM objects WHERE allocated_bytes IS NULL OR accounting_error IS NOT NULL LIMIT 1) FROM object_accounting_totals WHERE id=1", [], |row| Ok(ObjectAccounting { logical_bytes: row.get(0)?, allocated_bytes: row.get(1)?, complete: row.get(2)? }))?)
}
pub(crate) fn mark_error(connection: &Connection, key: &str, error: &str) -> Result<()> {
    let changed = connection.execute(
        "UPDATE objects SET allocated_bytes=NULL,accounting_error=?2 WHERE key=?1",
        params![key, error],
    )?;
    if changed != 1 {
        bail!("object disappeared before accounting failure was recorded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_schema_without_totals_fails_closed() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE objects(key TEXT PRIMARY KEY,bytes INTEGER NOT NULL,allocated_bytes INTEGER)").unwrap();
        assert!(initialize(&db)
            .unwrap_err()
            .to_string()
            .contains("without their totals"));
    }
    #[test]
    fn errored_prefix_does_not_block_later_pending_rows() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE objects(key TEXT PRIMARY KEY,bytes INTEGER NOT NULL)")
            .unwrap();
        initialize(&db).unwrap();
        db.execute("INSERT INTO objects(key,bytes) VALUES('a',1),('b',2)", [])
            .unwrap();
        mark_error(&db, "a", "missing").unwrap();
        assert_eq!(pending(&db, 8).unwrap(), vec!["b"]);
        record(&db, "b", 2, 4096).unwrap();
        let totals = read(&db).unwrap();
        assert!(!totals.complete);
        assert_eq!(totals.allocated_bytes, 4096);
    }
}
