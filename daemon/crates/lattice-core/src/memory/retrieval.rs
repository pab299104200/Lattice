//! Indexed, authority-bound recall support.
//!
//! The membership tables are derived only from durable memory fields. They are
//! acceleration indexes, never a second source of memory authority.

use crate::error::LatticeError;
use rusqlite::{params_from_iter, Connection};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallOptions {
    /// Retention-stale records are available only to an explicit inspection
    /// request. Automatic briefing and ordinary recall keep them out.
    pub include_retention_stale: bool,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            include_retention_stale: false,
        }
    }
}

/// Initialize derived membership indexes. JSON extraction happens at write
/// time, so recall can join normalized paths and symbols without scanning a
/// JSON document for every candidate.
pub fn initialize(conn: &Connection) -> Result<(), LatticeError> {
    let initialize = || -> rusqlite::Result<()> {
        conn.execute_batch("SAVEPOINT memory_retrieval_initialize")?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_retrieval_paths (
            memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            path TEXT NOT NULL,
            PRIMARY KEY(memory_id, path)
          );
          CREATE INDEX IF NOT EXISTS idx_memory_retrieval_paths_path
            ON memory_retrieval_paths(path, memory_id);
          CREATE TABLE IF NOT EXISTS memory_retrieval_symbols (
            memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            symbol TEXT NOT NULL,
            PRIMARY KEY(memory_id, symbol)
          );
          CREATE INDEX IF NOT EXISTS idx_memory_retrieval_symbols_symbol
            ON memory_retrieval_symbols(symbol, memory_id);
          CREATE TABLE IF NOT EXISTS memory_retrieval_docs (
            memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            doc TEXT NOT NULL,
            PRIMARY KEY(memory_id, doc)
          );
          CREATE INDEX IF NOT EXISTS idx_memory_retrieval_docs_doc
            ON memory_retrieval_docs(doc, memory_id);
          CREATE INDEX IF NOT EXISTS idx_memories_supersedes
            ON memories(supersedes_memory_id);
          CREATE TABLE IF NOT EXISTS memory_retrieval_failures (
            memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            failure TEXT NOT NULL,
            PRIMARY KEY(memory_id, failure)
          );
          CREATE INDEX IF NOT EXISTS idx_memory_retrieval_failures_failure
            ON memory_retrieval_failures(failure, memory_id);
          CREATE TRIGGER IF NOT EXISTS memory_retrieval_membership_insert
          AFTER INSERT ON memories BEGIN
            INSERT OR IGNORE INTO memory_retrieval_paths(memory_id,path)
              SELECT NEW.id, lower(replace(trim(value), '\\', '/')) FROM json_each(NEW.linked_files)
              WHERE trim(value) <> '';
            INSERT OR IGNORE INTO memory_retrieval_symbols(memory_id,symbol)
              SELECT NEW.id, lower(trim(value)) FROM json_each(NEW.linked_symbols) WHERE trim(value) <> '';
            INSERT OR IGNORE INTO memory_retrieval_failures(memory_id,failure)
              SELECT NEW.id, lower(trim(json_extract(value, '$.reference')))
              FROM json_each(NEW.evidence_json) WHERE json_extract(value, '$.kind') = 'error' AND trim(json_extract(value, '$.reference')) <> '';
          END;
          CREATE TRIGGER IF NOT EXISTS memory_retrieval_membership_update
          AFTER UPDATE OF linked_files, linked_symbols, evidence_json ON memories BEGIN
            DELETE FROM memory_retrieval_paths WHERE memory_id = NEW.id;
            DELETE FROM memory_retrieval_symbols WHERE memory_id = NEW.id;
            DELETE FROM memory_retrieval_failures WHERE memory_id = NEW.id;
            INSERT OR IGNORE INTO memory_retrieval_paths(memory_id,path)
              SELECT NEW.id, lower(replace(trim(value), '\\', '/')) FROM json_each(NEW.linked_files)
              WHERE trim(value) <> '';
            INSERT OR IGNORE INTO memory_retrieval_symbols(memory_id,symbol)
              SELECT NEW.id, lower(trim(value)) FROM json_each(NEW.linked_symbols) WHERE trim(value) <> '';
            INSERT OR IGNORE INTO memory_retrieval_failures(memory_id,failure)
              SELECT NEW.id, lower(trim(json_extract(value, '$.reference')))
              FROM json_each(NEW.evidence_json) WHERE json_extract(value, '$.kind') = 'error' AND trim(json_extract(value, '$.reference')) <> '';
          END;
          CREATE TRIGGER IF NOT EXISTS memory_retrieval_docs_insert
          AFTER INSERT ON memories BEGIN
            INSERT OR IGNORE INTO memory_retrieval_docs(memory_id,doc)
              SELECT NEW.id, lower(replace(trim(value), '\\', '/')) FROM json_each(NEW.linked_docs_json)
              WHERE trim(value) <> '';
          END;
          CREATE TRIGGER IF NOT EXISTS memory_retrieval_docs_update
          AFTER UPDATE OF linked_docs_json ON memories BEGIN
            DELETE FROM memory_retrieval_docs WHERE memory_id = NEW.id;
            INSERT OR IGNORE INTO memory_retrieval_docs(memory_id,doc)
              SELECT NEW.id, lower(replace(trim(value), '\\', '/')) FROM json_each(NEW.linked_docs_json)
              WHERE trim(value) <> '';
          END;
CREATE TABLE IF NOT EXISTS memory_retrieval_metadata(key TEXT PRIMARY KEY,value INTEGER NOT NULL);")?;
        let migrated: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM memory_retrieval_metadata WHERE key='membership-v1' AND value=1)",[],|row|row.get(0))?;
        if !migrated {
            conn.execute_batch("          INSERT OR IGNORE INTO memory_retrieval_paths(memory_id,path)
            SELECT memories.id, lower(replace(trim(value), '\\', '/')) FROM memories, json_each(memories.linked_files) WHERE trim(value) <> '';
          INSERT OR IGNORE INTO memory_retrieval_symbols(memory_id,symbol)
            SELECT memories.id, lower(trim(value)) FROM memories, json_each(memories.linked_symbols) WHERE trim(value) <> '';
          INSERT OR IGNORE INTO memory_retrieval_failures(memory_id,failure)
            SELECT memories.id, lower(trim(json_extract(value, '$.reference'))) FROM memories, json_each(memories.evidence_json) WHERE json_extract(value, '$.kind') = 'error' AND trim(json_extract(value, '$.reference')) <> '';")?;
            conn.execute(
                "INSERT OR REPLACE INTO memory_retrieval_metadata VALUES('membership-v1',1)",
                [],
            )?;
        }
        advance_doc_membership_backfill(conn)?;
        conn.execute_batch("RELEASE memory_retrieval_initialize")?;
        Ok(())
    };
    if let Err(error) = initialize() {
        let _ = conn.execute_batch(
            "ROLLBACK TO memory_retrieval_initialize; RELEASE memory_retrieval_initialize",
        );
        return Err(LatticeError::Storage(format!(
            "failed to initialize memory retrieval indexes: {error}"
        )));
    }
    Ok(())
}

/// Advance the historical linked-doc membership migration by one durable page.
/// New writes are indexed immediately by triggers while old stores converge
/// without turning startup or a single inspection into a full-ledger scan.
pub(crate) fn advance_doc_membership_backfill(conn: &Connection) -> Result<bool, rusqlite::Error> {
    conn.execute_batch("SAVEPOINT memory_retrieval_docs_page")?;
    let advance = || -> rusqlite::Result<bool> {
        let docs_migrated: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_retrieval_metadata WHERE key='membership-docs-v1' AND value=1)",
            [],
            |row| row.get(0),
        )?;
        if !docs_migrated {
            const DOC_BACKFILL_PAGE: i64 = 256;
            let cursor: i64 = conn.query_row(
                "SELECT coalesce((SELECT value FROM memory_retrieval_metadata WHERE key='membership-docs-cursor-v1'),0)",
                [],
                |row| row.get(0),
            )?;
            let last_rowid: Option<i64> = conn.query_row(
                "SELECT max(rowid) FROM (SELECT rowid FROM memories WHERE rowid>?1 ORDER BY rowid LIMIT ?2)",
                (cursor, DOC_BACKFILL_PAGE),
                |row| row.get(0),
            )?;
            if let Some(last_rowid) = last_rowid {
                conn.execute(
                    "INSERT OR IGNORE INTO memory_retrieval_docs(memory_id,doc)
                     SELECT page.id,lower(replace(trim(value),'\\','/'))
                     FROM (SELECT id,linked_docs_json FROM memories WHERE rowid>?1 AND rowid<=?2) page,
                          json_each(page.linked_docs_json)
                     WHERE trim(value)<>''",
                    (cursor, last_rowid),
                )?;
                conn.execute(
                    "INSERT OR REPLACE INTO memory_retrieval_metadata VALUES('membership-docs-cursor-v1',?1)",
                    [last_rowid],
                )?;
                let more: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE rowid>?1)",
                    [last_rowid],
                    |row| row.get(0),
                )?;
                if !more {
                    conn.execute("INSERT OR REPLACE INTO memory_retrieval_metadata VALUES('membership-docs-v1',1)", [])?;
                }
            } else {
                conn.execute("INSERT OR REPLACE INTO memory_retrieval_metadata VALUES('membership-docs-v1',1)", [])?;
            }
        }
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_retrieval_metadata WHERE key='membership-docs-v1' AND value=1)",
            [],
            |row| row.get(0),
        )
    };
    match advance() {
        Ok(complete) => {
            conn.execute_batch("RELEASE memory_retrieval_docs_page")?;
            Ok(complete)
        }
        Err(error) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO memory_retrieval_docs_page; RELEASE memory_retrieval_docs_page",
            );
            Err(error)
        }
    }
}

pub fn retention_stale_by_id(
    conn: &Connection,
    ids: impl IntoIterator<Item = String>,
) -> Result<BTreeMap<String, bool>, LatticeError> {
    let ids = ids.into_iter().collect::<BTreeSet<_>>();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders = std::iter::repeat("?")
        .take(ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = conn
        .prepare(&format!(
            "SELECT id, retention_stale FROM memories WHERE id IN ({placeholders})"
        ))
        .map_err(|error| {
            LatticeError::Storage(format!("failed to prepare retention recall state: {error}"))
        })?;
    let rows = statement
        .query_map(params_from_iter(ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
        })
        .map_err(|error| {
            LatticeError::Storage(format!("failed to query retention recall state: {error}"))
        })?;
    let mut result = BTreeMap::new();
    for row in rows {
        let (id, stale) = row.map_err(|error| {
            LatticeError::Storage(format!("failed to read retention recall state: {error}"))
        })?;
        result.insert(id, stale);
    }
    Ok(result)
}
