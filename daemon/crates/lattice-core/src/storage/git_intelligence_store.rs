//! SQLite persistence for published Git-intelligence generations.
//!
//! The store is deliberately separate from history extraction. A caller mines a
//! complete candidate first, then publishes it with one short `BEGIN IMMEDIATE`
//! transaction. Readers therefore observe either the previous generation or the
//! complete replacement, never a mixture of the two.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::LatticeError;
use crate::git_intelligence::{
    canonical_repository_path, CoChangeSignal, FileHistorySignal, GitIntelligenceSnapshot,
    GitMiningReport, SymbolHistorySignal, MAX_CO_CHANGE_PAIRS, MAX_CO_CHANGE_WIDTH,
    MAX_HISTORY_LIMIT, MAX_PATHS_PER_COMMIT, MAX_SYMBOLS_PER_COMMIT,
};

const CREATE_GIT_INTELLIGENCE_TABLES: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS git_intelligence_repositories (
    repository_id TEXT PRIMARY KEY,
    active_generation INTEGER,
    prior_generation INTEGER,
    next_generation INTEGER NOT NULL DEFAULT 1 CHECK (next_generation > 0)
);

CREATE TABLE IF NOT EXISTS git_intelligence_generations (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    head_commit_id TEXT,
    refreshed_at INTEGER NOT NULL,
    report_json TEXT NOT NULL,
    snapshot_digest BLOB NOT NULL CHECK (length(snapshot_digest) = 32),
    PRIMARY KEY (repository_id, generation),
    FOREIGN KEY (repository_id) REFERENCES git_intelligence_repositories(repository_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS git_intelligence_commits (
    repository_id TEXT NOT NULL,
    commit_id TEXT NOT NULL,
    PRIMARY KEY (repository_id, commit_id),
    FOREIGN KEY (repository_id) REFERENCES git_intelligence_repositories(repository_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS git_intelligence_window_commits (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    commit_id TEXT NOT NULL,
    PRIMARY KEY (repository_id, generation, ordinal),
    UNIQUE (repository_id, generation, commit_id),
    FOREIGN KEY (repository_id, generation)
        REFERENCES git_intelligence_generations(repository_id, generation) ON DELETE CASCADE,
    FOREIGN KEY (repository_id, commit_id)
        REFERENCES git_intelligence_commits(repository_id, commit_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS git_intelligence_files (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    path TEXT NOT NULL,
    hotspot_score INTEGER NOT NULL CHECK (hotspot_score >= 0),
    bug_fix_commits INTEGER NOT NULL CHECK (bug_fix_commits >= 0),
    bug_fix_density_per_mille INTEGER NOT NULL
        CHECK (bug_fix_density_per_mille BETWEEN 0 AND 1000),
    author_count INTEGER NOT NULL CHECK (author_count >= 0),
    top_author_share_per_mille INTEGER
        CHECK (top_author_share_per_mille BETWEEN 0 AND 1000),
    bus_factor INTEGER CHECK (bus_factor >= 0),
    -- H2.5: per-file line churn (diff stats only), aggregated over the same
    -- window/generation as hotspot_score. See docs/plans/2026-08-13-health-engine.md.
    lines_added INTEGER NOT NULL DEFAULT 0 CHECK (lines_added >= 0),
    lines_deleted INTEGER NOT NULL DEFAULT 0 CHECK (lines_deleted >= 0),
    line_churn INTEGER NOT NULL DEFAULT 0 CHECK (line_churn >= 0),
    PRIMARY KEY (repository_id, generation, path),
    FOREIGN KEY (repository_id, generation)
        REFERENCES git_intelligence_generations(repository_id, generation) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS git_intelligence_symbols (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    symbol TEXT NOT NULL,
    hotspot_score INTEGER NOT NULL CHECK (hotspot_score >= 0),
    bug_fix_commits INTEGER NOT NULL CHECK (bug_fix_commits >= 0),
    author_count INTEGER NOT NULL CHECK (author_count >= 0),
    PRIMARY KEY (repository_id, generation, symbol),
    FOREIGN KEY (repository_id, generation)
        REFERENCES git_intelligence_generations(repository_id, generation) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS git_intelligence_co_changes (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    left_path TEXT NOT NULL,
    right_path TEXT NOT NULL,
    commit_count INTEGER NOT NULL CHECK (commit_count > 0),
    CHECK (left_path < right_path),
    PRIMARY KEY (repository_id, generation, left_path, right_path),
    FOREIGN KEY (repository_id, generation)
        REFERENCES git_intelligence_generations(repository_id, generation) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_git_intelligence_active_generation
    ON git_intelligence_generations(repository_id, generation);
CREATE INDEX IF NOT EXISTS idx_git_intelligence_file_generation
    ON git_intelligence_files(repository_id, generation);
CREATE INDEX IF NOT EXISTS idx_git_intelligence_symbol_generation
    ON git_intelligence_symbols(repository_id, generation);
CREATE INDEX IF NOT EXISTS idx_git_intelligence_co_change_generation
    ON git_intelligence_co_changes(repository_id, generation);
"#;

/// One immutable snapshot generation selected by a repository's active pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGitIntelligenceSnapshot {
    pub repository_id: String,
    pub generation: i64,
    pub head_commit_id: Option<String>,
    pub refreshed_at: i64,
    pub snapshot: GitIntelligenceSnapshot,
}

/// Whether a recovering load had to discard corrupt Git-derived rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitIntelligenceRecovery {
    None,
    RebuiltCorruptRows,
}

/// Result of a load which is allowed to remove corrupt rows for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIntelligenceLoad {
    pub stored: Option<StoredGitIntelligenceSnapshot>,
    pub recovery: GitIntelligenceRecovery,
}

/// A graph-database adapter for atomic Git-intelligence snapshots.
pub struct GitIntelligenceStore {
    conn: Connection,
    path: Option<PathBuf>,
}

impl GitIntelligenceStore {
    /// Opens the workspace graph database and adds the Git-intelligence schema.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|error| storage_error("open Git-intelligence database", error))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| storage_error("enable WAL for Git intelligence", error))?;
        let store = Self {
            conn,
            path: Some(path.to_path_buf()),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Opens an isolated store for unit tests and embedders.
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory()
            .map_err(|error| storage_error("open in-memory Git-intelligence database", error))?;
        let store = Self { conn, path: None };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(CREATE_GIT_INTELLIGENCE_TABLES)
            .map_err(|error| storage_error("initialize Git-intelligence schema", error))
    }

    /// Publishes `snapshot` and atomically selects it as the active generation.
    ///
    /// Replaying an identical candidate for the same repository is idempotent:
    /// it returns the existing generation and does not duplicate commit rows.
    pub fn publish(
        &self,
        repository_id: &str,
        refreshed_at: i64,
        snapshot: &GitIntelligenceSnapshot,
    ) -> Result<StoredGitIntelligenceSnapshot, LatticeError> {
        validate_repository_id(repository_id)?;
        validate_snapshot(snapshot)?;
        let report_json = serde_json::to_string(&snapshot.report).map_err(|error| {
            LatticeError::Storage(format!("Failed to serialize Git mining report: {error}"))
        })?;
        let digest = snapshot_digest(snapshot)?;
        let head_commit_id = snapshot.head_commit_id().map(str::to_owned);

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin Git-intelligence publication", error))?;
        tx.execute(
            "INSERT INTO git_intelligence_repositories (repository_id) VALUES (?1) \
             ON CONFLICT(repository_id) DO NOTHING",
            [repository_id],
        )
        .map_err(|error| storage_error("initialize Git repository state", error))?;

        if let Some(active) = active_identity(&tx, repository_id)? {
            if active.head_commit_id == head_commit_id && active.digest == digest {
                tx.execute(
                    "UPDATE git_intelligence_generations SET refreshed_at = ?3 \
                     WHERE repository_id = ?1 AND generation = ?2",
                    params![repository_id, active.generation, refreshed_at],
                )
                .map_err(|error| {
                    storage_error("update replayed Git-intelligence refresh time", error)
                })?;
                let stored = load_generation(&tx, repository_id, active.generation, self.path())?;
                tx.commit().map_err(|error| {
                    storage_error("finish idempotent Git-intelligence publication", error)
                })?;
                return Ok(stored);
            }
        }

        let (active_generation, next_generation): (Option<i64>, i64) = tx
            .query_row(
                "SELECT active_generation, next_generation \
                 FROM git_intelligence_repositories WHERE repository_id = ?1",
                [repository_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| storage_error("read Git repository generation", error))?;
        let following_generation = next_generation.checked_add(1).ok_or_else(|| {
            LatticeError::Storage(format!(
                "Git-intelligence generation exhausted for repository {repository_id}"
            ))
        })?;

        tx.execute(
            "INSERT INTO git_intelligence_generations \
             (repository_id, generation, head_commit_id, refreshed_at, report_json, snapshot_digest) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                repository_id,
                next_generation,
                head_commit_id,
                refreshed_at,
                report_json,
                digest.as_slice()
            ],
        )
        .map_err(|error| storage_error("insert Git-intelligence generation", error))?;

        insert_snapshot_rows(&tx, repository_id, next_generation, snapshot)?;

        tx.execute(
            "UPDATE git_intelligence_repositories \
             SET prior_generation = ?2, active_generation = ?3, next_generation = ?4 \
             WHERE repository_id = ?1",
            params![
                repository_id,
                active_generation,
                next_generation,
                following_generation
            ],
        )
        .map_err(|error| storage_error("activate Git-intelligence generation", error))?;

        // Publication is already durable within this transaction before cleanup.
        // Keep the active generation and exactly one predecessor for diagnostics.
        tx.execute(
            "DELETE FROM git_intelligence_generations \
             WHERE repository_id = ?1 AND generation <> ?2 \
             AND (?3 IS NULL OR generation <> ?3)",
            params![repository_id, next_generation, active_generation],
        )
        .map_err(|error| storage_error("retire Git-intelligence generations", error))?;
        tx.execute(
            "DELETE FROM git_intelligence_commits AS commits \
             WHERE commits.repository_id = ?1 AND NOT EXISTS ( \
               SELECT 1 FROM git_intelligence_window_commits AS window \
               WHERE window.repository_id = commits.repository_id \
                 AND window.commit_id = commits.commit_id \
             )",
            [repository_id],
        )
        .map_err(|error| storage_error("remove orphan Git commit keys", error))?;

        tx.commit()
            .map_err(|error| storage_error("commit Git-intelligence publication", error))?;
        Ok(StoredGitIntelligenceSnapshot {
            repository_id: repository_id.to_owned(),
            generation: next_generation,
            head_commit_id,
            refreshed_at,
            snapshot: snapshot.clone(),
        })
    }

    /// Loads a transactionally consistent active generation.
    pub fn load_active(
        &self,
        repository_id: &str,
    ) -> Result<Option<StoredGitIntelligenceSnapshot>, LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|error| storage_error("begin Git-intelligence read", error))?;
        let generation: Option<i64> = tx
            .query_row(
                "SELECT active_generation FROM git_intelligence_repositories \
                 WHERE repository_id = ?1",
                [repository_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| storage_error("read active Git-intelligence generation", error))?
            .flatten();
        let stored = generation
            .map(|generation| load_generation(&tx, repository_id, generation, self.path()))
            .transpose()?;
        tx.commit()
            .map_err(|error| storage_error("finish Git-intelligence read", error))?;
        Ok(stored)
    }

    /// Loads the active generation, removing only this repository's Git-derived
    /// rows if their cross-table invariants are corrupt.
    pub fn load_active_recovering(
        &self,
        repository_id: &str,
    ) -> Result<GitIntelligenceLoad, LatticeError> {
        match self.load_active(repository_id) {
            Ok(stored) => Ok(GitIntelligenceLoad {
                stored,
                recovery: GitIntelligenceRecovery::None,
            }),
            Err(LatticeError::CorruptStorage { .. }) => {
                self.recover_repository(repository_id)?;
                Ok(GitIntelligenceLoad {
                    stored: None,
                    recovery: GitIntelligenceRecovery::RebuiltCorruptRows,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Discards all derived Git intelligence for one repository. Static graph
    /// tables and other repositories are not touched.
    pub fn recover_repository(&self, repository_id: &str) -> Result<(), LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin Git-intelligence recovery", error))?;
        tx.execute(
            "DELETE FROM git_intelligence_repositories WHERE repository_id = ?1",
            [repository_id],
        )
        .map_err(|error| storage_error("clear corrupt Git-intelligence rows", error))?;
        tx.commit()
            .map_err(|error| storage_error("commit Git-intelligence recovery", error))
    }

    fn path(&self) -> &str {
        self.path
            .as_deref()
            .and_then(Path::to_str)
            .unwrap_or("<in-memory graph.db>")
    }
}

#[derive(Debug)]
struct ActiveIdentity {
    generation: i64,
    head_commit_id: Option<String>,
    digest: [u8; 32],
}

fn active_identity(
    tx: &Transaction<'_>,
    repository_id: &str,
) -> Result<Option<ActiveIdentity>, LatticeError> {
    let row: Option<(i64, Option<String>, Vec<u8>)> = tx
        .query_row(
            "SELECT generation, head_commit_id, snapshot_digest \
             FROM git_intelligence_generations \
             WHERE repository_id = ?1 AND generation = ( \
               SELECT active_generation FROM git_intelligence_repositories \
               WHERE repository_id = ?1 \
             )",
            [repository_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| storage_error("read active Git-intelligence identity", error))?;
    row.map(|(generation, head_commit_id, digest)| {
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| LatticeError::CorruptStorage {
                path: "git_intelligence_generations".to_owned(),
                message: format!(
                "repository {repository_id} generation {generation} has an invalid snapshot digest"
            ),
            })?;
        Ok(ActiveIdentity {
            generation,
            head_commit_id,
            digest,
        })
    })
    .transpose()
}

fn insert_snapshot_rows(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    snapshot: &GitIntelligenceSnapshot,
) -> Result<(), LatticeError> {
    {
        let mut insert_commit = tx
            .prepare(
                "INSERT INTO git_intelligence_commits (repository_id, commit_id) \
                 VALUES (?1, ?2) ON CONFLICT(repository_id, commit_id) DO NOTHING",
            )
            .map_err(|error| storage_error("prepare Git commit key insert", error))?;
        let mut insert_window = tx
            .prepare(
                "INSERT INTO git_intelligence_window_commits \
                 (repository_id, generation, ordinal, commit_id) VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(|error| storage_error("prepare Git window insert", error))?;
        for (ordinal, commit_id) in snapshot.processed_commits.iter().enumerate() {
            insert_commit
                .execute(params![repository_id, commit_id])
                .map_err(|error| storage_error("insert Git commit key", error))?;
            let ordinal = i64::try_from(ordinal).map_err(|_| {
                LatticeError::Storage("Git commit ordinal exceeded SQLite range".to_owned())
            })?;
            insert_window
                .execute(params![repository_id, generation, ordinal, commit_id])
                .map_err(|error| storage_error("insert Git window membership", error))?;
        }
    }
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO git_intelligence_files \
                 (repository_id, generation, path, hotspot_score, bug_fix_commits, \
                  bug_fix_density_per_mille, author_count, top_author_share_per_mille, bus_factor, \
                  lines_added, lines_deleted, line_churn) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .map_err(|error| storage_error("prepare Git file signal insert", error))?;
        for file in &snapshot.files {
            insert
                .execute(params![
                    repository_id,
                    generation,
                    file.path,
                    i64::from(file.hotspot_score),
                    i64::from(file.bug_fix_commits),
                    i64::from(file.bug_fix_density_per_mille),
                    i64::from(file.author_count),
                    file.top_author_share_per_mille.map(i64::from),
                    file.bus_factor.map(i64::from),
                    i64::try_from(file.lines_added).map_err(|_| {
                        LatticeError::Storage("Git lines_added exceeded SQLite range".to_owned())
                    })?,
                    i64::try_from(file.lines_deleted).map_err(|_| {
                        LatticeError::Storage("Git lines_deleted exceeded SQLite range".to_owned())
                    })?,
                    i64::try_from(file.line_churn).map_err(|_| {
                        LatticeError::Storage("Git line_churn exceeded SQLite range".to_owned())
                    })?,
                ])
                .map_err(|error| storage_error("insert Git file signal", error))?;
        }
    }
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO git_intelligence_symbols \
                 (repository_id, generation, symbol, hotspot_score, bug_fix_commits, author_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|error| storage_error("prepare Git symbol signal insert", error))?;
        for symbol in &snapshot.symbols {
            insert
                .execute(params![
                    repository_id,
                    generation,
                    symbol.symbol,
                    i64::from(symbol.hotspot_score),
                    i64::from(symbol.bug_fix_commits),
                    i64::from(symbol.author_count),
                ])
                .map_err(|error| storage_error("insert Git symbol signal", error))?;
        }
    }
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO git_intelligence_co_changes \
                 (repository_id, generation, left_path, right_path, commit_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|error| storage_error("prepare Git co-change insert", error))?;
        for pair in &snapshot.co_changes {
            insert
                .execute(params![
                    repository_id,
                    generation,
                    pair.left_path,
                    pair.right_path,
                    i64::from(pair.commit_count),
                ])
                .map_err(|error| storage_error("insert Git co-change signal", error))?;
        }
    }
    Ok(())
}

fn load_generation(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<StoredGitIntelligenceSnapshot, LatticeError> {
    let (head_commit_id, refreshed_at, report_json, persisted_digest): (
        Option<String>,
        i64,
        String,
        Vec<u8>,
    ) = tx
        .query_row(
            "SELECT head_commit_id, refreshed_at, report_json, snapshot_digest \
             FROM git_intelligence_generations \
             WHERE repository_id = ?1 AND generation = ?2",
            params![repository_id, generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let report: GitMiningReport = serde_json::from_str(&report_json).map_err(|error| {
        corrupt_message(
            storage_path,
            repository_id,
            generation,
            format!("invalid mining report: {error}"),
        )
    })?;

    let processed_commits = query_strings(
        tx,
        "SELECT commit_id FROM git_intelligence_window_commits \
         WHERE repository_id = ?1 AND generation = ?2 ORDER BY ordinal",
        repository_id,
        generation,
        storage_path,
    )?;
    let files = load_files(tx, repository_id, generation, storage_path)?;
    let symbols = load_symbols(tx, repository_id, generation, storage_path)?;
    let co_changes = load_co_changes(tx, repository_id, generation, storage_path)?;
    let snapshot = GitIntelligenceSnapshot {
        processed_commits,
        files,
        symbols,
        co_changes,
        report,
    };
    validate_snapshot(&snapshot).map_err(|error| {
        corrupt_message(
            storage_path,
            repository_id,
            generation,
            format!("invalid snapshot invariants: {error}"),
        )
    })?;
    if snapshot.head_commit_id() != head_commit_id.as_deref() {
        return Err(corrupt_message(
            storage_path,
            repository_id,
            generation,
            "head commit does not match generation membership",
        ));
    }
    let computed_digest = snapshot_digest(&snapshot)?;
    if persisted_digest.as_slice() != computed_digest {
        return Err(corrupt_message(
            storage_path,
            repository_id,
            generation,
            "snapshot digest does not match normalized rows",
        ));
    }
    Ok(StoredGitIntelligenceSnapshot {
        repository_id: repository_id.to_owned(),
        generation,
        head_commit_id,
        refreshed_at,
        snapshot,
    })
}

fn query_strings(
    tx: &Transaction<'_>,
    sql: &str,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<Vec<String>, LatticeError> {
    let mut stmt = tx
        .prepare(sql)
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| row.get(0))
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))
}

fn load_files(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<Vec<FileHistorySignal>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT path, hotspot_score, bug_fix_commits, bug_fix_density_per_mille, \
             author_count, top_author_share_per_mille, bus_factor, \
             lines_added, lines_deleted, line_churn \
             FROM git_intelligence_files WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY path",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            let lines_added: i64 = row.get(7)?;
            let lines_deleted: i64 = row.get(8)?;
            let line_churn: i64 = row.get(9)?;
            Ok(FileHistorySignal {
                path: row.get(0)?,
                hotspot_score: row.get(1)?,
                bug_fix_commits: row.get(2)?,
                bug_fix_density_per_mille: row.get(3)?,
                author_count: row.get(4)?,
                top_author_share_per_mille: row.get(5)?,
                bus_factor: row.get(6)?,
                lines_added: u64::try_from(lines_added)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(7, lines_added))?,
                lines_deleted: u64::try_from(lines_deleted)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(8, lines_deleted))?,
                line_churn: u64::try_from(line_churn)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(9, line_churn))?,
            })
        })
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))
}

fn load_symbols(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<Vec<SymbolHistorySignal>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT symbol, hotspot_score, bug_fix_commits, author_count \
             FROM git_intelligence_symbols WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY symbol",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            Ok(SymbolHistorySignal {
                symbol: row.get(0)?,
                hotspot_score: row.get(1)?,
                bug_fix_commits: row.get(2)?,
                author_count: row.get(3)?,
            })
        })
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))
}

fn load_co_changes(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<Vec<CoChangeSignal>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT left_path, right_path, commit_count \
             FROM git_intelligence_co_changes \
             WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY left_path, right_path",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            Ok(CoChangeSignal {
                left_path: row.get(0)?,
                right_path: row.get(1)?,
                commit_count: row.get(2)?,
            })
        })
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))
}

fn validate_repository_id(repository_id: &str) -> Result<(), LatticeError> {
    if repository_id.is_empty()
        || repository_id.trim() != repository_id
        || repository_id.contains('\0')
    {
        return Err(LatticeError::Storage(
            "Git repository identity must be non-empty, trimmed, and contain no NUL".to_owned(),
        ));
    }
    Ok(())
}

fn validate_snapshot(snapshot: &GitIntelligenceSnapshot) -> Result<(), LatticeError> {
    validate_strictly_ordered(
        &snapshot.processed_commits,
        "commit id",
        |value| !value.is_empty() && value.trim() == value && !value.contains('\0'),
        false,
    )?;
    if usize::try_from(snapshot.report.sampled_commits).ok()
        != Some(snapshot.processed_commits.len())
    {
        return Err(LatticeError::Storage(
            "Git snapshot sampled commit count does not match its commit window".to_owned(),
        ));
    }
    let report = &snapshot.report;
    let limits = report.limits;
    if report.aggregation_version == 0
        || limits.history_limit > MAX_HISTORY_LIMIT
        || limits.paths_per_commit > MAX_PATHS_PER_COMMIT
        || limits.symbols_per_commit > MAX_SYMBOLS_PER_COMMIT
        || limits.co_change_width > MAX_CO_CHANGE_WIDTH
        || limits.co_change_pairs > MAX_CO_CHANGE_PAIRS
        || snapshot.processed_commits.len() > limits.history_limit
        || u64::from(report.sampled_commits)
            .saturating_add(report.duplicate_commits)
            .saturating_add(report.invalid_commit_ids)
            != report.samples_seen
        || report
            .included_commits
            .saturating_add(report.path_overflow_commits)
            != report.sampled_commits
        || report.symbol_overflow_commits > report.included_commits
        || report.co_change_width_exclusions > report.included_commits
        || (!report.co_changes_complete && !snapshot.co_changes.is_empty())
        || snapshot.co_changes.len() > limits.co_change_pairs
    {
        return Err(LatticeError::Storage(
            "Git snapshot mining report is inconsistent with its bounded generation".to_owned(),
        ));
    }
    let mut previous = None;
    for file in &snapshot.files {
        validate_canonical_path(&file.path)?;
        if previous.is_some_and(|path: &str| path >= file.path.as_str()) {
            return Err(LatticeError::Storage(
                "Git file signals must be uniquely ordered by canonical path".to_owned(),
            ));
        }
        previous = Some(file.path.as_str());
        if file.hotspot_score == 0
            || file.bug_fix_commits > file.hotspot_score
            || file.hotspot_score > report.included_commits
            || file.bug_fix_density_per_mille
                != ratio_per_mille(file.bug_fix_commits, file.hotspot_score)
            || file.bug_fix_density_per_mille > 1000
            || file.author_count > file.hotspot_score
            || file
                .top_author_share_per_mille
                .is_some_and(|value| value > 1000)
            || (file.author_count == 0) != file.top_author_share_per_mille.is_none()
            || file.bus_factor.is_some_and(|value| value == 0)
            || file
                .bus_factor
                .is_some_and(|value| value > file.author_count)
            || file.line_churn != file.lines_added.saturating_add(file.lines_deleted)
            // Each included commit contributes at most `u32::MAX` per line
            // direction (the adapter's per-commit, per-path bound); a churn
            // total exceeding that ceiling times the file's commit count is
            // not reachable by the miner and can only be corrupt storage.
            || file.lines_added > u64::from(u32::MAX).saturating_mul(u64::from(file.hotspot_score))
            || file.lines_deleted > u64::from(u32::MAX).saturating_mul(u64::from(file.hotspot_score))
        {
            return Err(LatticeError::Storage(format!(
                "Git file signal {} has impossible aggregate values",
                file.path
            )));
        }
    }
    let symbols: Vec<String> = snapshot
        .symbols
        .iter()
        .map(|item| item.symbol.clone())
        .collect();
    validate_strictly_ordered(
        &symbols,
        "symbol",
        |value| !value.is_empty() && value.trim() == value && !value.contains('\0'),
        true,
    )?;
    for symbol in &snapshot.symbols {
        if symbol.hotspot_score == 0
            || symbol.hotspot_score > report.included_commits
            || symbol.bug_fix_commits > symbol.hotspot_score
            || symbol.author_count > symbol.hotspot_score
        {
            return Err(LatticeError::Storage(format!(
                "Git symbol signal {} has impossible aggregate values",
                symbol.symbol
            )));
        }
    }
    let mut previous_pair: Option<(&str, &str)> = None;
    for pair in &snapshot.co_changes {
        validate_canonical_path(&pair.left_path)?;
        validate_canonical_path(&pair.right_path)?;
        if pair.left_path >= pair.right_path
            || pair.commit_count == 0
            || pair.commit_count > report.included_commits
        {
            return Err(LatticeError::Storage(
                "Git co-change rows require an ordered distinct path pair and nonzero count"
                    .to_owned(),
            ));
        }
        let current = (pair.left_path.as_str(), pair.right_path.as_str());
        if previous_pair.is_some_and(|previous| previous >= current) {
            return Err(LatticeError::Storage(
                "Git co-change rows must be uniquely ordered by path pair".to_owned(),
            ));
        }
        previous_pair = Some(current);
    }
    Ok(())
}

fn ratio_per_mille(numerator: u32, denominator: u32) -> u16 {
    if denominator == 0 {
        return 0;
    }
    ((u64::from(numerator) * 1_000) / u64::from(denominator)) as u16
}

fn validate_strictly_ordered<F>(
    values: &[String],
    label: &str,
    valid: F,
    require_lexical_order: bool,
) -> Result<(), LatticeError>
where
    F: Fn(&str) -> bool,
{
    let mut seen = std::collections::HashSet::with_capacity(values.len());
    let mut previous: Option<&str> = None;
    for value in values {
        if !valid(value) || !seen.insert(value) {
            return Err(LatticeError::Storage(format!(
                "Git snapshot contains an invalid or duplicate {label}"
            )));
        }
        if require_lexical_order && previous.is_some_and(|previous| previous >= value.as_str()) {
            return Err(LatticeError::Storage(format!(
                "Git snapshot {label} rows are not in lexical order"
            )));
        }
        previous = Some(value);
    }
    Ok(())
}

fn validate_canonical_path(path: &str) -> Result<(), LatticeError> {
    if canonical_repository_path(path).as_deref() != Some(path) {
        return Err(LatticeError::Storage(format!(
            "Git snapshot path is not canonical and repository-relative: {path:?}"
        )));
    }
    Ok(())
}

fn snapshot_digest(snapshot: &GitIntelligenceSnapshot) -> Result<[u8; 32], LatticeError> {
    #[derive(Serialize)]
    struct DigestEnvelope<'a> {
        format: u8,
        snapshot: &'a GitIntelligenceSnapshot,
    }
    let bytes = serde_json::to_vec(&DigestEnvelope {
        format: 1,
        snapshot,
    })
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to encode Git snapshot digest input: {error}"
        ))
    })?;
    Ok(Sha256::digest(bytes).into())
}

fn storage_error(operation: &str, error: rusqlite::Error) -> LatticeError {
    LatticeError::Storage(format!("Failed to {operation}: {error}"))
}

fn corrupt_rows(
    storage_path: &str,
    repository_id: &str,
    generation: i64,
    error: rusqlite::Error,
) -> LatticeError {
    corrupt_message(
        storage_path,
        repository_id,
        generation,
        format!("cannot read normalized rows: {error}"),
    )
}

fn corrupt_message(
    storage_path: &str,
    repository_id: &str,
    generation: i64,
    message: impl Into<String>,
) -> LatticeError {
    LatticeError::CorruptStorage {
        path: storage_path.to_owned(),
        message: format!(
            "Git-intelligence repository {repository_id} generation {generation}: {}",
            message.into()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_intelligence::{CommitSample, GitHistoryMiner, PathChange};

    fn snapshot(head: &str, second: &str) -> GitIntelligenceSnapshot {
        GitHistoryMiner::default().mine([
            CommitSample {
                id: head.to_owned(),
                author: Some("alice".to_owned()),
                subject: "fix parser regression".to_owned(),
                changes: vec![
                    PathChange {
                        path: "src/a.rs".to_owned(),
                        symbols: vec!["rust::src/a.rs::parse".to_owned()],
                        lines_added: 4,
                        lines_deleted: 1,
                    },
                    PathChange {
                        path: "src/b.rs".to_owned(),
                        symbols: Vec::new(),
                        lines_added: 2,
                        lines_deleted: 0,
                    },
                ],
            },
            CommitSample {
                id: second.to_owned(),
                author: Some("bob".to_owned()),
                subject: "add parser".to_owned(),
                changes: vec![PathChange {
                    path: "src/a.rs".to_owned(),
                    symbols: vec!["rust::src/a.rs::parse".to_owned()],
                    lines_added: 10,
                    lines_deleted: 0,
                }],
            },
        ])
    }

    #[test]
    fn publish_round_trips_normalized_snapshot_and_empty_generation() {
        let store = GitIntelligenceStore::open_in_memory().unwrap();
        let expected = snapshot("c2", "c1");
        let published = store.publish("repo-a", 42, &expected).unwrap();
        assert_eq!(published.generation, 1);
        assert_eq!(published.head_commit_id.as_deref(), Some("c2"));
        assert_eq!(store.load_active("repo-a").unwrap(), Some(published));

        let empty = GitIntelligenceSnapshot::empty();
        let published_empty = store.publish("repo-empty", 43, &empty).unwrap();
        assert_eq!(published_empty.head_commit_id, None);
        assert_eq!(
            store.load_active("repo-empty").unwrap(),
            Some(published_empty)
        );
    }

    #[test]
    fn identical_replay_reuses_generation_and_commit_keys() {
        let store = GitIntelligenceStore::open_in_memory().unwrap();
        let expected = snapshot("c2", "c1");
        let first = store.publish("repo", 1, &expected).unwrap();
        let replay = store.publish("repo", 99, &expected).unwrap();
        assert_eq!(first.generation, replay.generation);
        assert_eq!(replay.refreshed_at, 99);
        assert_eq!(replay.snapshot, first.snapshot);
        let generations: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM git_intelligence_generations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let commits: i64 = store
            .conn
            .query_row("SELECT count(*) FROM git_intelligence_commits", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(generations, 1);
        assert_eq!(commits, 2);
    }

    #[test]
    fn generations_and_repositories_are_isolated_and_only_one_prior_is_retained() {
        let store = GitIntelligenceStore::open_in_memory().unwrap();
        store.publish("left", 1, &snapshot("l1", "base")).unwrap();
        store.publish("right", 2, &snapshot("r1", "base")).unwrap();
        store.publish("left", 3, &snapshot("l2", "l1")).unwrap();
        let current = store.publish("left", 4, &snapshot("l3", "l2")).unwrap();

        assert_eq!(current.generation, 3);
        assert_eq!(
            store
                .load_active("right")
                .unwrap()
                .unwrap()
                .head_commit_id
                .as_deref(),
            Some("r1")
        );
        let left_generations: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM git_intelligence_generations WHERE repository_id = 'left'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left_generations, 2);
    }

    #[test]
    fn failed_candidate_rolls_back_and_preserves_active_pointer() {
        let store = GitIntelligenceStore::open_in_memory().unwrap();
        let first = store.publish("repo", 1, &snapshot("c2", "c1")).unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_candidate BEFORE INSERT ON git_intelligence_files \
                 WHEN NEW.path = 'src/a.rs' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        let error = store.publish("repo", 2, &snapshot("c3", "c2")).unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(store.load_active("repo").unwrap(), Some(first));
        let generations: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM git_intelligence_generations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(generations, 1);
    }

    #[test]
    fn recovering_load_removes_only_corrupt_repository_git_rows() {
        let store = GitIntelligenceStore::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TABLE nodes_for_recovery_proof (value TEXT NOT NULL); \
                 INSERT INTO nodes_for_recovery_proof VALUES ('preserved');",
            )
            .unwrap();
        store.publish("broken", 1, &snapshot("b2", "b1")).unwrap();
        let healthy = store.publish("healthy", 1, &snapshot("h2", "h1")).unwrap();
        store
            .conn
            .execute(
                "UPDATE git_intelligence_generations SET report_json = '{' \
                 WHERE repository_id = 'broken'",
                [],
            )
            .unwrap();

        let recovered = store.load_active_recovering("broken").unwrap();
        assert_eq!(recovered.stored, None);
        assert_eq!(
            recovered.recovery,
            GitIntelligenceRecovery::RebuiltCorruptRows
        );
        assert_eq!(store.load_active("healthy").unwrap(), Some(healthy));
        let proof: String = store
            .conn
            .query_row("SELECT value FROM nodes_for_recovery_proof", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(proof, "preserved");
    }

    #[test]
    fn rejects_noncanonical_and_internally_inconsistent_candidates() {
        let store = GitIntelligenceStore::open_in_memory().unwrap();
        let mut invalid = snapshot("c2", "c1");
        invalid.files[0].path = "../outside.rs".to_owned();
        assert!(store.publish("repo", 1, &invalid).is_err());
        assert!(store.load_active("repo").unwrap().is_none());

        let mut duplicate_commit = snapshot("c2", "c1");
        duplicate_commit.processed_commits[1] = "c2".to_owned();
        assert!(store.publish("repo", 1, &duplicate_commit).is_err());
    }

    #[test]
    fn file_store_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.db");
        let expected = {
            let store = GitIntelligenceStore::open(&path).unwrap();
            store.publish("repo", 17, &snapshot("c2", "c1")).unwrap()
        };
        let reopened = GitIntelligenceStore::open(&path).unwrap();
        assert_eq!(reopened.load_active("repo").unwrap(), Some(expected));
    }

    #[test]
    fn line_churn_round_trips_and_is_isolated_per_generation() {
        let store = GitIntelligenceStore::open_in_memory().unwrap();
        let first = store.publish("repo", 1, &snapshot("c2", "c1")).unwrap();

        let a = first
            .snapshot
            .file("src/a.rs")
            .expect("src/a.rs signal present");
        assert_eq!(a.lines_added, 14);
        assert_eq!(a.lines_deleted, 1);
        assert_eq!(a.line_churn, 15);
        let b = first
            .snapshot
            .file("src/b.rs")
            .expect("src/b.rs signal present");
        assert_eq!(b.lines_added, 2);
        assert_eq!(b.lines_deleted, 0);
        assert_eq!(b.line_churn, 2);

        let loaded = store.load_active("repo").unwrap().unwrap();
        assert_eq!(loaded, first, "reload must reproduce persisted line churn");

        // Publishing a new generation must not leak line-churn rows from the
        // retired generation, even though both generations share `src/a.rs`.
        let second_candidate = GitHistoryMiner::default().mine([CommitSample {
            id: "c3".to_owned(),
            author: Some("carol".to_owned()),
            subject: "feature".to_owned(),
            changes: vec![PathChange {
                path: "src/a.rs".to_owned(),
                symbols: Vec::new(),
                lines_added: 999,
                lines_deleted: 500,
            }],
        }]);
        let second = store.publish("repo", 2, &second_candidate).unwrap();
        let second_a = second
            .snapshot
            .file("src/a.rs")
            .expect("second generation src/a.rs signal");
        assert_eq!(second_a.lines_added, 999);
        assert_eq!(second_a.lines_deleted, 500);
        assert_ne!(
            second_a.lines_added, a.lines_added,
            "second generation must recompute its own churn, not inherit the first's"
        );

        // Only the active generation plus one immediate predecessor survive;
        // publishing a third generation must purge the first's line-churn rows.
        let third_candidate = GitHistoryMiner::default().mine([CommitSample {
            id: "c4".to_owned(),
            author: Some("dave".to_owned()),
            subject: "chore".to_owned(),
            changes: vec![PathChange {
                path: "src/a.rs".to_owned(),
                symbols: Vec::new(),
                lines_added: 1,
                lines_deleted: 1,
            }],
        }]);
        store.publish("repo", 3, &third_candidate).unwrap();
        let generation_rows: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM git_intelligence_files \
                 WHERE repository_id = 'repo' AND generation = ?1",
                [first.generation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            generation_rows, 0,
            "the first generation's file rows (including line churn) must be purged \
             once it is no longer the active generation or its immediate predecessor"
        );
    }
}
