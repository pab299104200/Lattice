//! SQLite persistence for published graph-derived health-fact generations.
//!
//! The store is deliberately separate from fact production. A caller produces a
//! complete candidate from the built graph first, then publishes it with one
//! short `BEGIN IMMEDIATE` transaction. Readers therefore observe either the
//! previous generation or the complete replacement, never a mixture.
//!
//! The schema conventions here follow
//! [`crate::storage::git_intelligence_store`] exactly — repository pointer row
//! with `active`/`prior`/`next` generations, an immutable generation row
//! carrying the completeness report and a snapshot digest, normalized fact
//! tables keyed by `(repository_id, generation, stable key)`, and canonical-path
//! validation on the way in and on the way out. Sibling health fact families
//! (complexity, dead symbols, test proximity, line churn) are expected to add
//! their own stores in the same shape.
//!
//! See `docs/plans/2026-08-13-health-engine.md`, section "Phase H2".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::LatticeError;
use crate::git_intelligence::canonical_repository_path;
use crate::graph::EdgeKind;
use crate::health::graph_facts::{
    instability_per_mille, symbol_fact_key, FileFactDelta, FileGraphFacts, GraphFactsReport,
    GraphFactsSnapshot, SymbolGraphFacts, UnstableDependencySignal, MAX_FILE_FACTS,
    MAX_SYMBOL_FACTS, MAX_UNSTABLE_DEPENDENCIES,
};

const CREATE_HEALTH_GRAPH_FACTS_TABLES: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS health_graph_facts_repositories (
    repository_id TEXT PRIMARY KEY,
    active_generation INTEGER,
    prior_generation INTEGER,
    next_generation INTEGER NOT NULL DEFAULT 1 CHECK (next_generation > 0)
);

CREATE TABLE IF NOT EXISTS health_graph_facts_generations (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    graph_revision TEXT,
    refreshed_at INTEGER NOT NULL,
    report_json TEXT NOT NULL,
    snapshot_digest BLOB NOT NULL CHECK (length(snapshot_digest) = 32),
    PRIMARY KEY (repository_id, generation),
    FOREIGN KEY (repository_id) REFERENCES health_graph_facts_repositories(repository_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS health_graph_facts_files (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    path TEXT NOT NULL,
    fan_in INTEGER NOT NULL CHECK (fan_in >= 0),
    fan_out INTEGER NOT NULL CHECK (fan_out >= 0),
    -- The component's lexically smallest member path, not a dense index.
    scc_id TEXT NOT NULL,
    scc_size INTEGER NOT NULL CHECK (scc_size >= 1),
    cycle_member INTEGER NOT NULL CHECK (cycle_member IN (0, 1)),
    -- Nullable by design: an isolated file has no defined instability, and a
    -- stored zero would claim maximum stability for an unknown.
    instability_per_mille INTEGER
        CHECK (instability_per_mille BETWEEN 0 AND 1000),
    CHECK ((cycle_member = 1) = (scc_size > 1)),
    PRIMARY KEY (repository_id, generation, path),
    FOREIGN KEY (repository_id, generation)
        REFERENCES health_graph_facts_generations(repository_id, generation) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS health_graph_facts_symbols (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    symbol_key TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    fan_in INTEGER NOT NULL CHECK (fan_in >= 0),
    fan_out INTEGER NOT NULL CHECK (fan_out >= 0),
    definition_count INTEGER NOT NULL CHECK (definition_count > 0),
    PRIMARY KEY (repository_id, generation, symbol_key),
    FOREIGN KEY (repository_id, generation)
        REFERENCES health_graph_facts_generations(repository_id, generation) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS health_graph_facts_unstable_dependencies (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    from_path TEXT NOT NULL,
    to_path TEXT NOT NULL,
    from_instability_per_mille INTEGER NOT NULL
        CHECK (from_instability_per_mille BETWEEN 0 AND 1000),
    to_instability_per_mille INTEGER NOT NULL
        CHECK (to_instability_per_mille BETWEEN 0 AND 1000),
    instability_gap_per_mille INTEGER NOT NULL
        CHECK (instability_gap_per_mille > 0),
    edge_kind TEXT NOT NULL,
    from_symbol TEXT NOT NULL,
    to_symbol TEXT NOT NULL,
    source_line INTEGER NOT NULL CHECK (source_line >= 0),
    source_end_line INTEGER NOT NULL CHECK (source_end_line >= source_line),
    CHECK (from_path <> to_path),
    CHECK (instability_gap_per_mille
        = to_instability_per_mille - from_instability_per_mille),
    PRIMARY KEY (repository_id, generation, from_path, to_path),
    FOREIGN KEY (repository_id, generation)
        REFERENCES health_graph_facts_generations(repository_id, generation) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_health_graph_facts_file_generation
    ON health_graph_facts_files(repository_id, generation);
CREATE INDEX IF NOT EXISTS idx_health_graph_facts_symbol_generation
    ON health_graph_facts_symbols(repository_id, generation);
CREATE INDEX IF NOT EXISTS idx_health_graph_facts_unstable_generation
    ON health_graph_facts_unstable_dependencies(repository_id, generation);
CREATE INDEX IF NOT EXISTS idx_health_graph_facts_cycle_members
    ON health_graph_facts_files(repository_id, generation, cycle_member);
"#;

/// One immutable fact generation selected by a repository's active pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGraphFacts {
    pub repository_id: String,
    pub generation: i64,
    /// Caller-supplied identity of the graph the facts were produced from.
    pub graph_revision: Option<String>,
    pub refreshed_at: i64,
    pub snapshot: GraphFactsSnapshot,
}

/// Whether a recovering load had to discard corrupt graph-fact rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphFactsRecovery {
    None,
    RebuiltCorruptRows,
}

/// Result of a load which is allowed to remove corrupt rows for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFactsLoad {
    pub stored: Option<StoredGraphFacts>,
    pub recovery: GraphFactsRecovery,
}

/// A graph-database adapter for atomic graph-fact generations.
pub struct HealthGraphFactsStore {
    conn: Connection,
    path: Option<PathBuf>,
}

impl HealthGraphFactsStore {
    /// Opens the workspace graph database and adds the graph-fact schema.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|error| storage_error("open health graph-fact database", error))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| storage_error("enable WAL for health graph facts", error))?;
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
            .map_err(|error| storage_error("open in-memory health graph-fact database", error))?;
        let store = Self { conn, path: None };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(CREATE_HEALTH_GRAPH_FACTS_TABLES)
            .map_err(|error| storage_error("initialize health graph-fact schema", error))
    }

    /// Publishes `snapshot` and atomically selects it as the active generation.
    ///
    /// `graph_revision` identifies the graph the candidate was produced from —
    /// typically a module or index digest. Replaying an identical candidate for
    /// the same repository and revision is idempotent: it refreshes the
    /// timestamp and returns the existing generation.
    pub fn publish(
        &self,
        repository_id: &str,
        graph_revision: Option<&str>,
        refreshed_at: i64,
        snapshot: &GraphFactsSnapshot,
    ) -> Result<StoredGraphFacts, LatticeError> {
        validate_repository_id(repository_id)?;
        let graph_revision = validate_graph_revision(graph_revision)?;
        validate_snapshot(snapshot)?;
        let report_json = serde_json::to_string(&snapshot.report).map_err(|error| {
            LatticeError::Storage(format!("Failed to serialize graph-fact report: {error}"))
        })?;
        let digest = snapshot_digest(snapshot)?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin graph-fact publication", error))?;
        tx.execute(
            "INSERT INTO health_graph_facts_repositories (repository_id) VALUES (?1) \
             ON CONFLICT(repository_id) DO NOTHING",
            [repository_id],
        )
        .map_err(|error| storage_error("initialize graph-fact repository state", error))?;

        if let Some(active) = active_identity(&tx, repository_id)? {
            if active.graph_revision.as_deref() == graph_revision && active.digest == digest {
                tx.execute(
                    "UPDATE health_graph_facts_generations SET refreshed_at = ?3 \
                     WHERE repository_id = ?1 AND generation = ?2",
                    params![repository_id, active.generation, refreshed_at],
                )
                .map_err(|error| storage_error("update replayed graph-fact refresh time", error))?;
                let stored = load_generation(&tx, repository_id, active.generation, self.path())?;
                tx.commit().map_err(|error| {
                    storage_error("finish idempotent graph-fact publication", error)
                })?;
                return Ok(stored);
            }
        }

        let (active_generation, next_generation): (Option<i64>, i64) = tx
            .query_row(
                "SELECT active_generation, next_generation \
                 FROM health_graph_facts_repositories WHERE repository_id = ?1",
                [repository_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| storage_error("read graph-fact repository generation", error))?;
        let following_generation = next_generation.checked_add(1).ok_or_else(|| {
            LatticeError::Storage(format!(
                "Graph-fact generation exhausted for repository {repository_id}"
            ))
        })?;

        tx.execute(
            "INSERT INTO health_graph_facts_generations \
             (repository_id, generation, graph_revision, refreshed_at, report_json, snapshot_digest) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                repository_id,
                next_generation,
                graph_revision,
                refreshed_at,
                report_json,
                digest.as_slice()
            ],
        )
        .map_err(|error| storage_error("insert graph-fact generation", error))?;

        insert_snapshot_rows(&tx, repository_id, next_generation, snapshot)?;

        tx.execute(
            "UPDATE health_graph_facts_repositories \
             SET prior_generation = ?2, active_generation = ?3, next_generation = ?4 \
             WHERE repository_id = ?1",
            params![
                repository_id,
                active_generation,
                next_generation,
                following_generation
            ],
        )
        .map_err(|error| storage_error("activate graph-fact generation", error))?;

        // Publication is already durable within this transaction before cleanup.
        // Keep the active generation and exactly one predecessor, both for
        // diagnostics and so `file_delta_since_prior` has something to compare.
        tx.execute(
            "DELETE FROM health_graph_facts_generations \
             WHERE repository_id = ?1 AND generation <> ?2 \
             AND (?3 IS NULL OR generation <> ?3)",
            params![repository_id, next_generation, active_generation],
        )
        .map_err(|error| storage_error("retire graph-fact generations", error))?;

        tx.commit()
            .map_err(|error| storage_error("commit graph-fact publication", error))?;
        Ok(StoredGraphFacts {
            repository_id: repository_id.to_owned(),
            generation: next_generation,
            graph_revision: graph_revision.map(str::to_owned),
            refreshed_at,
            snapshot: snapshot.clone(),
        })
    }

    /// Loads a transactionally consistent active generation.
    pub fn load_active(
        &self,
        repository_id: &str,
    ) -> Result<Option<StoredGraphFacts>, LatticeError> {
        self.load_pointer(repository_id, "active_generation")
    }

    /// Loads the single retained predecessor of the active generation.
    pub fn load_prior(
        &self,
        repository_id: &str,
    ) -> Result<Option<StoredGraphFacts>, LatticeError> {
        self.load_pointer(repository_id, "prior_generation")
    }

    fn load_pointer(
        &self,
        repository_id: &str,
        column: &str,
    ) -> Result<Option<StoredGraphFacts>, LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|error| storage_error("begin graph-fact read", error))?;
        // `column` is never caller-supplied; it is one of two literals below.
        let sql = format!(
            "SELECT {column} FROM health_graph_facts_repositories WHERE repository_id = ?1"
        );
        let generation: Option<i64> = tx
            .query_row(&sql, [repository_id], |row| row.get(0))
            .optional()
            .map_err(|error| storage_error("read graph-fact generation pointer", error))?
            .flatten();
        let stored = generation
            .map(|generation| load_generation(&tx, repository_id, generation, self.path()))
            .transpose()?;
        tx.commit()
            .map_err(|error| storage_error("finish graph-fact read", error))?;
        Ok(stored)
    }

    /// Which files' facts moved between the retained prior generation and the
    /// active one.
    ///
    /// This is the incremental-refresh hook: after republishing facts following
    /// a one-file reindex, a consumer reads this delta and invalidates only the
    /// affected files instead of every file in the repository. `None` means no
    /// predecessor is retained, so nothing may be assumed unchanged.
    pub fn file_delta_since_prior(
        &self,
        repository_id: &str,
    ) -> Result<Option<FileFactDelta>, LatticeError> {
        let Some(active) = self.load_active(repository_id)? else {
            return Ok(None);
        };
        let Some(prior) = self.load_prior(repository_id)? else {
            return Ok(None);
        };
        Ok(Some(active.snapshot.file_delta(&prior.snapshot)))
    }

    /// Loads the active generation, removing only this repository's graph-fact
    /// rows if their cross-table invariants are corrupt.
    pub fn load_active_recovering(
        &self,
        repository_id: &str,
    ) -> Result<GraphFactsLoad, LatticeError> {
        match self.load_active(repository_id) {
            Ok(stored) => Ok(GraphFactsLoad {
                stored,
                recovery: GraphFactsRecovery::None,
            }),
            Err(LatticeError::CorruptStorage { .. }) => {
                self.recover_repository(repository_id)?;
                Ok(GraphFactsLoad {
                    stored: None,
                    recovery: GraphFactsRecovery::RebuiltCorruptRows,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Discards all graph facts for one repository. Static graph tables, Git
    /// intelligence, and other repositories are not touched.
    pub fn recover_repository(&self, repository_id: &str) -> Result<(), LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin graph-fact recovery", error))?;
        tx.execute(
            "DELETE FROM health_graph_facts_repositories WHERE repository_id = ?1",
            [repository_id],
        )
        .map_err(|error| storage_error("clear corrupt graph-fact rows", error))?;
        tx.commit()
            .map_err(|error| storage_error("commit graph-fact recovery", error))
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
    graph_revision: Option<String>,
    digest: [u8; 32],
}

fn active_identity(
    tx: &Transaction<'_>,
    repository_id: &str,
) -> Result<Option<ActiveIdentity>, LatticeError> {
    let row: Option<(i64, Option<String>, Vec<u8>)> = tx
        .query_row(
            "SELECT generation, graph_revision, snapshot_digest \
             FROM health_graph_facts_generations \
             WHERE repository_id = ?1 AND generation = ( \
               SELECT active_generation FROM health_graph_facts_repositories \
               WHERE repository_id = ?1 \
             )",
            [repository_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| storage_error("read active graph-fact identity", error))?;
    row.map(|(generation, graph_revision, digest)| {
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| LatticeError::CorruptStorage {
                path: "health_graph_facts_generations".to_owned(),
                message: format!(
                "repository {repository_id} generation {generation} has an invalid snapshot digest"
            ),
            })?;
        Ok(ActiveIdentity {
            generation,
            graph_revision,
            digest,
        })
    })
    .transpose()
}

fn insert_snapshot_rows(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    snapshot: &GraphFactsSnapshot,
) -> Result<(), LatticeError> {
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO health_graph_facts_files \
                 (repository_id, generation, path, fan_in, fan_out, scc_id, scc_size, \
                  cycle_member, instability_per_mille) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(|error| storage_error("prepare graph-fact file insert", error))?;
        for file in &snapshot.files {
            insert
                .execute(params![
                    repository_id,
                    generation,
                    file.path,
                    i64::from(file.fan_in),
                    i64::from(file.fan_out),
                    file.scc_id,
                    i64::from(file.scc_size),
                    i64::from(file.cycle_member),
                    file.instability_per_mille.map(i64::from),
                ])
                .map_err(|error| storage_error("insert graph-fact file row", error))?;
        }
    }
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO health_graph_facts_symbols \
                 (repository_id, generation, symbol_key, path, name, fan_in, fan_out, \
                  definition_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .map_err(|error| storage_error("prepare graph-fact symbol insert", error))?;
        for symbol in &snapshot.symbols {
            insert
                .execute(params![
                    repository_id,
                    generation,
                    symbol.key,
                    symbol.path,
                    symbol.name,
                    i64::from(symbol.fan_in),
                    i64::from(symbol.fan_out),
                    i64::from(symbol.definition_count),
                ])
                .map_err(|error| storage_error("insert graph-fact symbol row", error))?;
        }
    }
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO health_graph_facts_unstable_dependencies \
                 (repository_id, generation, from_path, to_path, from_instability_per_mille, \
                  to_instability_per_mille, instability_gap_per_mille, edge_kind, from_symbol, \
                  to_symbol, source_line, source_end_line) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .map_err(|error| storage_error("prepare graph-fact unstable insert", error))?;
        for signal in &snapshot.unstable_dependencies {
            insert
                .execute(params![
                    repository_id,
                    generation,
                    signal.from_path,
                    signal.to_path,
                    i64::from(signal.from_instability_per_mille),
                    i64::from(signal.to_instability_per_mille),
                    i64::from(signal.instability_gap_per_mille),
                    signal.edge_kind.short_code(),
                    signal.from_symbol,
                    signal.to_symbol,
                    i64::from(signal.source_line),
                    i64::from(signal.source_end_line),
                ])
                .map_err(|error| storage_error("insert graph-fact unstable row", error))?;
        }
    }
    Ok(())
}

fn load_generation(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<StoredGraphFacts, LatticeError> {
    let (graph_revision, refreshed_at, report_json, persisted_digest): (
        Option<String>,
        i64,
        String,
        Vec<u8>,
    ) = tx
        .query_row(
            "SELECT graph_revision, refreshed_at, report_json, snapshot_digest \
             FROM health_graph_facts_generations \
             WHERE repository_id = ?1 AND generation = ?2",
            params![repository_id, generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let report: GraphFactsReport = serde_json::from_str(&report_json).map_err(|error| {
        corrupt_message(
            storage_path,
            repository_id,
            generation,
            format!("invalid facts report: {error}"),
        )
    })?;

    let files = load_files(tx, repository_id, generation, storage_path)?;
    let symbols = load_symbols(tx, repository_id, generation, storage_path)?;
    let unstable_dependencies =
        load_unstable_dependencies(tx, repository_id, generation, storage_path)?;
    let snapshot = GraphFactsSnapshot {
        files,
        symbols,
        unstable_dependencies,
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
    let computed_digest = snapshot_digest(&snapshot)?;
    if persisted_digest.as_slice() != computed_digest {
        return Err(corrupt_message(
            storage_path,
            repository_id,
            generation,
            "snapshot digest does not match normalized rows",
        ));
    }
    Ok(StoredGraphFacts {
        repository_id: repository_id.to_owned(),
        generation,
        graph_revision,
        refreshed_at,
        snapshot,
    })
}

fn load_files(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<Vec<FileGraphFacts>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT path, fan_in, fan_out, scc_id, scc_size, cycle_member, instability_per_mille \
             FROM health_graph_facts_files WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY path",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            let cycle_member: i64 = row.get(5)?;
            Ok(FileGraphFacts {
                path: row.get(0)?,
                fan_in: row.get(1)?,
                fan_out: row.get(2)?,
                scc_id: row.get(3)?,
                scc_size: row.get(4)?,
                cycle_member: cycle_member != 0,
                instability_per_mille: row.get(6)?,
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
) -> Result<Vec<SymbolGraphFacts>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT symbol_key, path, name, fan_in, fan_out, definition_count \
             FROM health_graph_facts_symbols WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY symbol_key",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            Ok(SymbolGraphFacts {
                key: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                fan_in: row.get(3)?,
                fan_out: row.get(4)?,
                definition_count: row.get(5)?,
            })
        })
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))
}

fn load_unstable_dependencies(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<Vec<UnstableDependencySignal>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT from_path, to_path, from_instability_per_mille, to_instability_per_mille, \
             instability_gap_per_mille, edge_kind, from_symbol, to_symbol, source_line, \
             source_end_line \
             FROM health_graph_facts_unstable_dependencies \
             WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY instability_gap_per_mille DESC, from_path, to_path",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            let code: String = row.get(5)?;
            let edge_kind = EdgeKind::from_short_code(&code).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown edge kind code {code:?}"),
                    )),
                )
            })?;
            Ok(UnstableDependencySignal {
                from_path: row.get(0)?,
                to_path: row.get(1)?,
                from_instability_per_mille: row.get(2)?,
                to_instability_per_mille: row.get(3)?,
                instability_gap_per_mille: row.get(4)?,
                edge_kind,
                from_symbol: row.get(6)?,
                to_symbol: row.get(7)?,
                source_line: row.get(8)?,
                source_end_line: row.get(9)?,
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
            "Graph-fact repository identity must be non-empty, trimmed, and contain no NUL"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_graph_revision(revision: Option<&str>) -> Result<Option<&str>, LatticeError> {
    match revision {
        None => Ok(None),
        Some(value) if value.is_empty() || value.trim() != value || value.contains('\0') => {
            Err(LatticeError::Storage(
                "Graph revision must be non-empty, trimmed, and contain no NUL".to_owned(),
            ))
        }
        Some(value) => Ok(Some(value)),
    }
}

/// Rejects a candidate whose rows cannot have come from the pure producer.
///
/// The same checks run on the way out, so a hand-edited or partially written
/// database surfaces as `CorruptStorage` rather than as a plausible-looking
/// fact a score would go on to cite as evidence.
fn validate_snapshot(snapshot: &GraphFactsSnapshot) -> Result<(), LatticeError> {
    let report = &snapshot.report;
    let limits = report.limits;
    if report.facts_version == 0
        || limits.max_files > MAX_FILE_FACTS
        || limits.max_symbols > MAX_SYMBOL_FACTS
        || limits.max_unstable_dependencies > MAX_UNSTABLE_DEPENDENCIES
        || report.unstable_dependency_threshold_per_mille > 1_000
        || snapshot.files.len() > limits.max_files
        || snapshot.symbols.len() > limits.max_symbols
        || snapshot.unstable_dependencies.len() > limits.max_unstable_dependencies
        || usize::try_from(report.files_observed).ok() < Some(snapshot.files.len())
        || usize::try_from(report.symbols_observed).ok() < Some(snapshot.symbols.len())
        || report.file_overflow
            != (usize::try_from(report.files_observed).ok() > Some(snapshot.files.len()))
        || report.symbol_overflow
            != (usize::try_from(report.symbols_observed).ok() > Some(snapshot.symbols.len()))
        || (report.availability() == crate::health::graph_facts::FactAvailability::Unavailable
            && !snapshot.files.is_empty())
    {
        return Err(LatticeError::Storage(
            "Graph-fact report is inconsistent with its persisted rows".to_owned(),
        ));
    }

    let mut components: BTreeMap<&str, (u32, usize)> = BTreeMap::new();
    let mut previous: Option<&str> = None;
    for file in &snapshot.files {
        validate_canonical_path(&file.path)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) {
            return Err(LatticeError::Storage(
                "Graph-fact file rows must be uniquely ordered by canonical path".to_owned(),
            ));
        }
        previous = Some(file.path.as_str());
        if file.scc_size == 0
            || file.cycle_member != (file.scc_size > 1)
            || file.instability_per_mille != instability_per_mille(file.fan_in, file.fan_out)
        {
            return Err(LatticeError::Storage(format!(
                "Graph-fact file row {} has impossible aggregate values",
                file.path
            )));
        }
        validate_canonical_path(&file.scc_id)?;
        // The representative is the component's smallest member, so no file may
        // claim a representative that sorts after itself.
        if file.scc_id.as_str() > file.path.as_str() {
            return Err(LatticeError::Storage(format!(
                "Graph-fact file {} names a component representative that sorts after it",
                file.path
            )));
        }
        let entry = components
            .entry(file.scc_id.as_str())
            .or_insert((file.scc_size, 0));
        if entry.0 != file.scc_size {
            return Err(LatticeError::Storage(format!(
                "Graph-fact component {} reports conflicting sizes",
                file.scc_id
            )));
        }
        entry.1 += 1;
    }
    // Membership counts are only complete when no file row was truncated.
    if !report.file_overflow {
        for (id, (size, observed)) in &components {
            if usize::try_from(*size).ok() != Some(*observed) {
                return Err(LatticeError::Storage(format!(
                    "Graph-fact component {id} has {observed} members but reports size {size}"
                )));
            }
        }
    }

    let mut previous_key: Option<&str> = None;
    for symbol in &snapshot.symbols {
        validate_canonical_path(&symbol.path)?;
        if symbol.name.is_empty()
            || symbol.name.trim() != symbol.name
            || symbol.definition_count == 0
            || symbol.key != symbol_fact_key(&symbol.path, &symbol.name)
        {
            return Err(LatticeError::Storage(format!(
                "Graph-fact symbol row {} is not a well-formed fact key",
                symbol.key
            )));
        }
        if previous_key.is_some_and(|key| key >= symbol.key.as_str()) {
            return Err(LatticeError::Storage(
                "Graph-fact symbol rows must be uniquely ordered by fact key".to_owned(),
            ));
        }
        previous_key = Some(symbol.key.as_str());
    }

    let mut previous_signal: Option<(u16, &str, &str)> = None;
    for signal in &snapshot.unstable_dependencies {
        validate_canonical_path(&signal.from_path)?;
        validate_canonical_path(&signal.to_path)?;
        if signal.from_path == signal.to_path
            || signal.from_instability_per_mille > 1_000
            || signal.to_instability_per_mille > 1_000
            || signal.source_end_line < signal.source_line
            || signal.from_symbol.is_empty()
            || signal.to_symbol.is_empty()
            || signal
                .to_instability_per_mille
                .checked_sub(signal.from_instability_per_mille)
                != Some(signal.instability_gap_per_mille)
            || signal.instability_gap_per_mille <= report.unstable_dependency_threshold_per_mille
        {
            return Err(LatticeError::Storage(format!(
                "Graph-fact unstable dependency {} -> {} contradicts its own threshold",
                signal.from_path, signal.to_path
            )));
        }
        // Ordering is descending by gap, then ascending by path pair.
        let current = (
            signal.instability_gap_per_mille,
            signal.from_path.as_str(),
            signal.to_path.as_str(),
        );
        if let Some(previous) = previous_signal {
            let ordered = previous.0 > current.0
                || (previous.0 == current.0 && (previous.1, previous.2) < (current.1, current.2));
            if !ordered {
                return Err(LatticeError::Storage(
                    "Graph-fact unstable dependencies must be ordered by gap then path pair"
                        .to_owned(),
                ));
            }
        }
        previous_signal = Some(current);

        if !report.file_overflow {
            for path in [&signal.from_path, &signal.to_path] {
                if snapshot
                    .files
                    .binary_search_by(|candidate| candidate.path.cmp(path))
                    .is_err()
                {
                    return Err(LatticeError::Storage(format!(
                        "Graph-fact unstable dependency references unknown file {path}"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_canonical_path(path: &str) -> Result<(), LatticeError> {
    if canonical_repository_path(path).as_deref() != Some(path) {
        return Err(LatticeError::Storage(format!(
            "Graph-fact path is not canonical and repository-relative: {path:?}"
        )));
    }
    Ok(())
}

fn snapshot_digest(snapshot: &GraphFactsSnapshot) -> Result<[u8; 32], LatticeError> {
    #[derive(Serialize)]
    struct DigestEnvelope<'a> {
        format: u8,
        snapshot: &'a GraphFactsSnapshot,
    }
    let bytes = serde_json::to_vec(&DigestEnvelope {
        format: 1,
        snapshot,
    })
    .map_err(|error| {
        LatticeError::Storage(format!("Failed to encode graph-fact digest input: {error}"))
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
            "Graph-fact repository {repository_id} generation {generation}: {}",
            message.into()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CodeGraph;
    use crate::health::graph_facts::fixtures::{add_edge, add_symbol, symbol};
    use crate::health::graph_facts::GraphFactProducer;

    /// `src/core.rs` (Ca 2, Ce 1) depends on `src/volatile.rs` (Ca 1, Ce 3),
    /// which yields one unstable-dependency row alongside the file and symbol
    /// rows, so a publication exercises every table.
    fn snapshot(extra_leaf: Option<&str>) -> GraphFactsSnapshot {
        let mut graph = CodeGraph::new();
        let core = symbol("src/core.rs", "core_entry");
        let one = symbol("src/one.rs", "one");
        let two = symbol("src/two.rs", "two");
        let volatile = symbol("src/volatile.rs", "volatile_helper");
        let sinks = [
            symbol("src/sink_a.rs", "sink_a"),
            symbol("src/sink_b.rs", "sink_b"),
            symbol("src/sink_c.rs", "sink_c"),
        ];
        for id in [&core, &one, &two, &volatile] {
            add_symbol(&mut graph, id, true, 1);
        }
        for sink in &sinks {
            add_symbol(&mut graph, sink, true, 1);
        }
        add_edge(&mut graph, &one, &core, EdgeKind::Imports);
        add_edge(&mut graph, &two, &core, EdgeKind::Imports);
        add_edge(&mut graph, &core, &volatile, EdgeKind::Calls);
        for sink in &sinks {
            add_edge(&mut graph, &volatile, sink, EdgeKind::Calls);
        }
        if let Some(path) = extra_leaf {
            let leaf = symbol(path, "leaf");
            add_symbol(&mut graph, &leaf, true, 1);
            add_edge(&mut graph, &one, &leaf, EdgeKind::Calls);
        }
        GraphFactProducer::default().produce(&graph, true)
    }

    fn count(store: &HealthGraphFactsStore, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn publish_round_trips_every_normalized_table() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let expected = snapshot(None);
        assert_eq!(expected.unstable_dependencies.len(), 1);

        let published = store
            .publish("repo-a", Some("rev-1"), 42, &expected)
            .unwrap();
        assert_eq!(published.generation, 1);
        assert_eq!(published.graph_revision.as_deref(), Some("rev-1"));

        let loaded = store.load_active("repo-a").unwrap().unwrap();
        assert_eq!(loaded, published);
        assert_eq!(loaded.snapshot, expected);
        // The nullable instability column survives a round trip as None.
        assert_eq!(
            loaded.snapshot.file("src/sink_a.rs").unwrap().fan_out,
            0,
            "leaf files keep their zero fan-out"
        );
        assert_eq!(
            loaded.snapshot.unstable_dependencies[0].edge_kind,
            EdgeKind::Calls
        );
    }

    #[test]
    fn empty_snapshot_publishes_and_stays_unavailable() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let published = store
            .publish("repo-empty", None, 7, &GraphFactsSnapshot::empty())
            .unwrap();
        assert_eq!(published.graph_revision, None);
        let loaded = store.load_active("repo-empty").unwrap().unwrap();
        assert_eq!(loaded.snapshot, GraphFactsSnapshot::empty());
        assert_eq!(
            loaded.snapshot.availability(),
            crate::health::graph_facts::FactAvailability::Unavailable
        );
    }

    #[test]
    fn identical_replay_reuses_the_active_generation() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let expected = snapshot(None);
        let first = store.publish("repo", Some("rev-1"), 1, &expected).unwrap();
        let replay = store.publish("repo", Some("rev-1"), 99, &expected).unwrap();
        assert_eq!(first.generation, replay.generation);
        assert_eq!(replay.refreshed_at, 99);
        assert_eq!(replay.snapshot, first.snapshot);
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_graph_facts_generations"
            ),
            1
        );
    }

    #[test]
    fn a_new_revision_over_identical_facts_still_advances_the_generation() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let expected = snapshot(None);
        store.publish("repo", Some("rev-1"), 1, &expected).unwrap();
        let second = store.publish("repo", Some("rev-2"), 2, &expected).unwrap();
        assert_eq!(second.generation, 2);
        assert_eq!(
            store.load_active("repo").unwrap().unwrap().graph_revision,
            Some("rev-2".to_owned())
        );
    }

    #[test]
    fn generations_are_isolated_and_only_one_prior_is_retained() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let first = store
            .publish("repo", Some("r1"), 1, &snapshot(None))
            .unwrap();
        let second = store
            .publish("repo", Some("r2"), 2, &snapshot(Some("src/leaf_two.rs")))
            .unwrap();
        let third = store
            .publish("repo", Some("r3"), 3, &snapshot(Some("src/leaf_three.rs")))
            .unwrap();

        assert_eq!(
            (first.generation, second.generation, third.generation),
            (1, 2, 3)
        );
        assert_eq!(store.load_active("repo").unwrap(), Some(third));
        assert_eq!(store.load_prior("repo").unwrap(), Some(second));
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_graph_facts_generations"
            ),
            2,
            "generation 1 is retired once it is no longer the prior pointer"
        );
        // Retiring a generation cascades into every normalized fact table.
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_graph_facts_files WHERE generation = 1"
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_graph_facts_unstable_dependencies WHERE generation = 1"
            ),
            0
        );
    }

    #[test]
    fn repositories_do_not_observe_each_others_generations() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let left = store
            .publish("left", Some("l1"), 1, &snapshot(None))
            .unwrap();
        let right = store
            .publish("right", Some("r1"), 2, &snapshot(Some("src/right_leaf.rs")))
            .unwrap();
        store
            .publish("left", Some("l2"), 3, &snapshot(Some("src/left_leaf.rs")))
            .unwrap();

        assert_eq!(store.load_active("right").unwrap(), Some(right));
        assert_ne!(
            store.load_active("left").unwrap().unwrap().snapshot,
            left.snapshot
        );
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_graph_facts_generations WHERE repository_id = 'right'"
            ),
            1
        );
    }

    #[test]
    fn failed_candidate_rolls_back_and_preserves_the_active_pointer() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let first = store
            .publish("repo", Some("r1"), 1, &snapshot(None))
            .unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_candidate BEFORE INSERT ON \
                 health_graph_facts_unstable_dependencies \
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();

        let error = store
            .publish("repo", Some("r2"), 2, &snapshot(Some("src/leaf.rs")))
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(store.load_active("repo").unwrap(), Some(first));
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_graph_facts_generations"
            ),
            1
        );
        assert_eq!(
            count(&store, "SELECT count(*) FROM health_graph_facts_files"),
            i64::try_from(snapshot(None).files.len()).unwrap(),
            "no partial file rows from the aborted candidate survive"
        );
    }

    #[test]
    fn file_delta_since_prior_confines_an_incremental_refresh() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        store
            .publish("repo", Some("r1"), 1, &snapshot(None))
            .unwrap();
        assert_eq!(
            store.file_delta_since_prior("repo").unwrap(),
            None,
            "the first generation has no predecessor to compare against"
        );

        // One file is reindexed: src/one.rs gains a call into a new leaf file.
        store
            .publish("repo", Some("r2"), 2, &snapshot(Some("src/leaf.rs")))
            .unwrap();
        let delta = store.file_delta_since_prior("repo").unwrap().unwrap();
        assert_eq!(delta.added, ["src/leaf.rs"]);
        assert_eq!(delta.updated, ["src/one.rs"]);
        assert!(delta.removed.is_empty());
        assert_eq!(delta.touched_paths(), ["src/leaf.rs", "src/one.rs"]);
    }

    #[test]
    fn recovering_load_removes_only_the_corrupt_repository() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TABLE nodes_for_recovery_proof (value TEXT NOT NULL); \
                 INSERT INTO nodes_for_recovery_proof VALUES ('preserved');",
            )
            .unwrap();
        store
            .publish("broken", Some("b1"), 1, &snapshot(None))
            .unwrap();
        let healthy = store
            .publish("healthy", Some("h1"), 1, &snapshot(Some("src/healthy.rs")))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE health_graph_facts_generations SET report_json = '{' \
                 WHERE repository_id = 'broken'",
                [],
            )
            .unwrap();

        let recovered = store.load_active_recovering("broken").unwrap();
        assert_eq!(recovered.stored, None);
        assert_eq!(recovered.recovery, GraphFactsRecovery::RebuiltCorruptRows);
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
    fn tampered_fact_rows_are_detected_by_the_snapshot_digest() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        store
            .publish("repo", Some("r1"), 1, &snapshot(None))
            .unwrap();
        // Raise a fan-in without touching instability: the row still satisfies
        // every column CHECK, so only the recomputed invariants catch it.
        store
            .conn
            .execute(
                "UPDATE health_graph_facts_files SET fan_in = fan_in + 5 \
                 WHERE path = 'src/core.rs'",
                [],
            )
            .unwrap();
        let error = store.load_active("repo").unwrap_err();
        assert!(matches!(error, LatticeError::CorruptStorage { .. }));
        assert!(error.to_string().contains("impossible aggregate values"));
    }

    #[test]
    fn rejects_noncanonical_and_internally_inconsistent_candidates() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();

        let mut escaped = snapshot(None);
        escaped.files[0].path = "../outside.rs".to_owned();
        assert!(store.publish("repo", Some("r1"), 1, &escaped).is_err());

        let mut fabricated_zero = snapshot(None);
        let sink = fabricated_zero
            .files
            .iter_mut()
            .find(|file| file.path == "src/sink_a.rs")
            .unwrap();
        sink.fan_in = 0;
        sink.fan_out = 0;
        // Instability now claims a value where the producer would report None.
        assert!(store
            .publish("repo", Some("r2"), 2, &fabricated_zero)
            .is_err());

        let mut mismatched_cycle = snapshot(None);
        mismatched_cycle.files[0].cycle_member = true;
        assert!(store
            .publish("repo", Some("r3"), 3, &mismatched_cycle)
            .is_err());

        let mut below_threshold = snapshot(None);
        below_threshold
            .report
            .unstable_dependency_threshold_per_mille = 1_000;
        assert!(store
            .publish("repo", Some("r4"), 4, &below_threshold)
            .is_err());

        let mut misordered = snapshot(Some("src/zzz.rs"));
        misordered.files.swap(0, 1);
        assert!(store.publish("repo", Some("r5"), 5, &misordered).is_err());

        assert!(
            store.load_active("repo").unwrap().is_none(),
            "no rejected candidate ever became active"
        );
    }

    #[test]
    fn rejects_invalid_repository_and_revision_identities() {
        let store = HealthGraphFactsStore::open_in_memory().unwrap();
        let facts = snapshot(None);
        assert!(store.publish("", Some("r1"), 1, &facts).is_err());
        assert!(store.publish(" padded ", Some("r1"), 1, &facts).is_err());
        assert!(store.publish("repo", Some(""), 1, &facts).is_err());
        assert!(store.publish("repo", Some(" r1 "), 1, &facts).is_err());
    }

    #[test]
    fn store_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.db");
        let expected = {
            let store = HealthGraphFactsStore::open(&path).unwrap();
            store
                .publish("repo", Some("r1"), 17, &snapshot(None))
                .unwrap()
        };
        let reopened = HealthGraphFactsStore::open(&path).unwrap();
        assert_eq!(reopened.load_active("repo").unwrap(), Some(expected));
    }

    #[test]
    fn coexists_with_the_git_intelligence_schema_in_one_database() {
        use crate::git_intelligence::GitIntelligenceSnapshot;
        use crate::storage::GitIntelligenceStore;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.db");
        let git = GitIntelligenceStore::open(&path).unwrap();
        git.publish("repo", 1, &GitIntelligenceSnapshot::empty())
            .unwrap();

        let facts = HealthGraphFactsStore::open(&path).unwrap();
        let published = facts
            .publish("repo", Some("r1"), 2, &snapshot(None))
            .unwrap();

        assert_eq!(facts.load_active("repo").unwrap(), Some(published));
        assert!(git.load_active("repo").unwrap().is_some());
    }
}
