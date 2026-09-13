//! Repository-shared immutable Git-history snapshots.
//!
//! Checkout graph databases select and materialize a snapshot locally, while
//! this store owns the expensive, path-free mined facts.  A key includes the
//! complete bounded commit window and every mining limit, so branches and
//! configuration changes cannot accidentally reuse each other's facts.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::LatticeError;
use crate::git_intelligence::{GitIntelligenceSnapshot, GitMiningLimits, AGGREGATION_VERSION};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS history_object_cache (
    cache_key TEXT PRIMARY KEY CHECK (length(cache_key) = 64),
    commit_ids_json TEXT NOT NULL,
    limits_json TEXT NOT NULL,
    aggregation_version INTEGER NOT NULL,
    payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    accessed_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS idx_history_object_cache_accessed
ON history_object_cache(accessed_at);
"#;

/// Content-addressed cache for complete Git-miner outputs.  It is deliberately
/// independent from checkout graph generations.
pub struct HistoryObjectCache {
    connection: Mutex<Connection>,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryCacheLookup {
    Hit,
    Miss,
    Invalid,
}

impl HistoryObjectCache {
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to create history cache directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        if std::fs::symlink_metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(LatticeError::Storage(format!(
                "Refusing to open history cache through symlink: {}",
                path.display()
            )));
        }
        Ok(Self {
            connection: Mutex::new(open_connection(path)?),
            path: Some(path.to_path_buf()),
        })
    }

    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let connection = Connection::open_in_memory().map_err(|error| {
            LatticeError::Storage(format!("Failed to open in-memory history cache: {error}"))
        })?;
        configure(&connection)?;
        connection.execute_batch(SCHEMA).map_err(|error| {
            LatticeError::Storage(format!("Failed to initialize history cache: {error}"))
        })?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn key(commit_ids: &[String], limits: GitMiningLimits) -> Result<String, LatticeError> {
        let commit_ids = serde_json::to_string(commit_ids).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to serialize history commit window: {error}"
            ))
        })?;
        let limits = serde_json::to_string(&limits).map_err(|error| {
            LatticeError::Storage(format!("Failed to serialize history limits: {error}"))
        })?;
        Ok(sha256_hex(
            format!("{commit_ids}\0{limits}\0{AGGREGATION_VERSION}").as_bytes(),
        ))
    }

    pub fn get(
        &self,
        commit_ids: &[String],
        limits: GitMiningLimits,
    ) -> Result<(HistoryCacheLookup, Option<GitIntelligenceSnapshot>), LatticeError> {
        let key = Self::key(commit_ids, limits)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("History cache lock was poisoned".to_string()))?;
        let row = connection
            .query_row(
                "SELECT payload_sha256, payload FROM history_object_cache WHERE cache_key = ?1",
                [key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to query history cache: {error}"))
            })?;
        let Some((checksum, payload)) = row else {
            return Ok((HistoryCacheLookup::Miss, None));
        };
        if sha256_hex(payload.as_bytes()) != checksum {
            return Ok((HistoryCacheLookup::Invalid, None));
        }
        let snapshot: GitIntelligenceSnapshot = match serde_json::from_str(&payload) {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok((HistoryCacheLookup::Invalid, None)),
        };
        if snapshot.processed_commits != commit_ids
            || snapshot.report.limits != limits
            || snapshot.report.aggregation_version != AGGREGATION_VERSION
        {
            return Ok((HistoryCacheLookup::Invalid, None));
        }
        connection
            .execute(
                "UPDATE history_object_cache SET accessed_at = unixepoch() WHERE cache_key = ?1",
                [Self::key(commit_ids, limits)?],
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to touch history cache row: {error}"))
            })?;
        Ok((HistoryCacheLookup::Hit, Some(snapshot)))
    }

    pub fn put(&self, snapshot: &GitIntelligenceSnapshot) -> Result<(), LatticeError> {
        let key = Self::key(&snapshot.processed_commits, snapshot.report.limits)?;
        let commits = serde_json::to_string(&snapshot.processed_commits).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to serialize history cache commits: {error}"
            ))
        })?;
        let limits = serde_json::to_string(&snapshot.report.limits).map_err(|error| {
            LatticeError::Storage(format!("Failed to serialize history cache limits: {error}"))
        })?;
        let payload = serde_json::to_string(snapshot).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to serialize history cache snapshot: {error}"
            ))
        })?;
        let checksum = sha256_hex(payload.as_bytes());
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("History cache lock was poisoned".to_string()))?;
        let transaction = connection.transaction().map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to begin history cache publication: {error}"
            ))
        })?;
        transaction.execute("INSERT INTO history_object_cache (cache_key, commit_ids_json, limits_json, aggregation_version, payload_sha256, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(cache_key) DO UPDATE SET payload_sha256 = excluded.payload_sha256, payload = excluded.payload, accessed_at = unixepoch()", params![key, commits, limits, AGGREGATION_VERSION, checksum, payload])
            .map_err(|error| LatticeError::Storage(format!("Failed to publish history cache row: {error}")))?;
        transaction.commit().map_err(|error| {
            LatticeError::Storage(format!("Failed to commit history cache row: {error}"))
        })
    }

    pub fn gc_before(&self, oldest_accessed_at: i64, limit: usize) -> Result<usize, LatticeError> {
        if limit == 0 {
            return Ok(0);
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("History cache lock was poisoned".to_string()))?;
        connection.execute("DELETE FROM history_object_cache WHERE cache_key IN (SELECT cache_key FROM history_object_cache WHERE accessed_at < ?1 ORDER BY accessed_at ASC LIMIT ?2)", params![oldest_accessed_at, limit as i64]).map_err(|error| LatticeError::Storage(format!("Failed to GC history cache: {error}")))
    }
}

fn open_connection(path: &Path) -> Result<Connection, LatticeError> {
    let connection = Connection::open(path).map_err(|error| map_open_error(path, error))?;
    let integrity: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| map_open_error(path, error))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(LatticeError::CorruptStorage {
            path: path.display().to_string(),
            message: integrity,
        });
    }
    configure(&connection)?;
    connection.execute_batch(SCHEMA).map_err(|error| {
        LatticeError::Storage(format!("Failed to initialize history cache: {error}"))
    })?;
    Ok(connection)
}
fn configure(connection: &Connection) -> Result<(), LatticeError> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| {
            LatticeError::Storage(format!("Failed to set history cache timeout: {error}"))
        })?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| {
            LatticeError::Storage(format!("Failed to enable history cache WAL: {error}"))
        })?;
    Ok(())
}
fn map_open_error(path: &Path, error: rusqlite::Error) -> LatticeError {
    use rusqlite::ErrorCode;
    if matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    ) {
        LatticeError::CorruptStorage {
            path: path.display().to_string(),
            message: error.to_string(),
        }
    } else {
        LatticeError::Storage(format!(
            "Failed to open history cache {}: {error}",
            path.display()
        ))
    }
}
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_intelligence::GitIntelligenceSnapshot;
    #[test]
    fn commit_window_and_config_are_exact_cache_identity() {
        let cache = HistoryObjectCache::open_in_memory().unwrap();
        let mut snapshot = GitIntelligenceSnapshot::empty();
        snapshot.processed_commits = vec!["a".repeat(40), "b".repeat(40)];
        cache.put(&snapshot).unwrap();
        assert_eq!(
            cache
                .get(&snapshot.processed_commits, snapshot.report.limits)
                .unwrap()
                .0,
            HistoryCacheLookup::Hit
        );
        assert_eq!(
            cache
                .get(&vec!["b".repeat(40)], snapshot.report.limits)
                .unwrap()
                .0,
            HistoryCacheLookup::Miss
        );
        let changed = GitMiningLimits {
            history_limit: 1,
            ..snapshot.report.limits
        };
        assert_eq!(
            cache.get(&snapshot.processed_commits, changed).unwrap().0,
            HistoryCacheLookup::Miss
        );
    }
}
