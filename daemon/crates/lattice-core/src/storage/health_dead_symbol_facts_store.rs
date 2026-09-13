//! SQLite persistence for published dead-symbol health-fact generations.
//!
//! The store is deliberately separate from fact production. A caller produces
//! a complete candidate snapshot from the built graph first, then publishes
//! it with one short `BEGIN IMMEDIATE` transaction. Readers therefore observe
//! either the previous generation or the complete replacement, never a
//! mixture.
//!
//! The schema conventions here follow
//! [`crate::storage::health_graph_facts_store`] exactly — repository pointer
//! row with `active`/`prior`/`next` generations, an immutable generation row
//! carrying the completeness report and a snapshot digest, one normalized
//! fact table keyed by `(repository_id, generation, stable key)`, and
//! canonical-path validation on the way in and on the way out.
//!
//! See `docs/plans/2026-08-13-health-engine.md`, section "H2.3 Dead-symbol
//! facts", and "Phase H2" for the generational persistence contract every
//! health fact-store follows.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::LatticeError;
use crate::git_intelligence::canonical_repository_path;
use crate::health::dead_symbol_facts::{
    DeadSymbolCandidate, DeadSymbolExclusionReason, DeadSymbolFactsReport, DeadSymbolFactsSnapshot,
    MAX_CANDIDATES,
};
use crate::symbols::SymbolKind;

const CREATE_HEALTH_DEAD_SYMBOL_FACTS_TABLES: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS health_dead_symbol_facts_repositories (
    repository_id TEXT PRIMARY KEY,
    active_generation INTEGER,
    prior_generation INTEGER,
    next_generation INTEGER NOT NULL DEFAULT 1 CHECK (next_generation > 0)
);

CREATE TABLE IF NOT EXISTS health_dead_symbol_facts_generations (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    graph_revision TEXT,
    refreshed_at INTEGER NOT NULL,
    report_json TEXT NOT NULL,
    snapshot_digest BLOB NOT NULL CHECK (length(snapshot_digest) = 32),
    PRIMARY KEY (repository_id, generation),
    FOREIGN KEY (repository_id) REFERENCES health_dead_symbol_facts_repositories(repository_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS health_dead_symbol_facts_candidates (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    symbol_key TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    kind_code TEXT NOT NULL,
    definition_count INTEGER NOT NULL CHECK (definition_count > 0),
    source_line INTEGER NOT NULL CHECK (source_line >= 0),
    source_end_line INTEGER NOT NULL CHECK (source_end_line >= source_line),
    -- Comma-joined DeadSymbolExclusionReason codes in declared order; empty
    -- string means the candidate is actually flagged (no exclusion applies).
    exclusion_reasons TEXT NOT NULL,
    PRIMARY KEY (repository_id, generation, symbol_key),
    FOREIGN KEY (repository_id, generation)
        REFERENCES health_dead_symbol_facts_generations(repository_id, generation)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_health_dead_symbol_facts_candidate_generation
    ON health_dead_symbol_facts_candidates(repository_id, generation);
CREATE INDEX IF NOT EXISTS idx_health_dead_symbol_facts_flagged
    ON health_dead_symbol_facts_candidates(repository_id, generation, exclusion_reasons);
"#;

/// One immutable fact generation selected by a repository's active pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDeadSymbolFacts {
    pub repository_id: String,
    pub generation: i64,
    /// Caller-supplied identity of the graph the candidate was produced from.
    pub graph_revision: Option<String>,
    pub refreshed_at: i64,
    pub snapshot: DeadSymbolFactsSnapshot,
}

/// Whether a recovering load had to discard corrupt dead-symbol-fact rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadSymbolFactsRecovery {
    None,
    RebuiltCorruptRows,
}

/// Result of a load which is allowed to remove corrupt rows for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadSymbolFactsLoad {
    pub stored: Option<StoredDeadSymbolFacts>,
    pub recovery: DeadSymbolFactsRecovery,
}

/// A graph-database adapter for atomic dead-symbol-fact generations.
pub struct HealthDeadSymbolFactsStore {
    conn: Connection,
    path: Option<PathBuf>,
}

impl HealthDeadSymbolFactsStore {
    /// Opens the workspace graph database and adds the dead-symbol-fact schema.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|error| storage_error("open health dead-symbol-fact database", error))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| storage_error("enable WAL for health dead-symbol facts", error))?;
        let store = Self {
            conn,
            path: Some(path.to_path_buf()),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Opens an isolated store for unit tests and embedders.
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory().map_err(|error| {
            storage_error("open in-memory health dead-symbol-fact database", error)
        })?;
        let store = Self { conn, path: None };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(CREATE_HEALTH_DEAD_SYMBOL_FACTS_TABLES)
            .map_err(|error| storage_error("initialize health dead-symbol-fact schema", error))
    }

    /// Publishes `snapshot` and atomically selects it as the active generation.
    ///
    /// `graph_revision` identifies the graph the candidate was produced from —
    /// typically a module or index digest. Replaying an identical candidate
    /// for the same repository and revision is idempotent: it refreshes the
    /// timestamp and returns the existing generation.
    pub fn publish(
        &self,
        repository_id: &str,
        graph_revision: Option<&str>,
        refreshed_at: i64,
        snapshot: &DeadSymbolFactsSnapshot,
    ) -> Result<StoredDeadSymbolFacts, LatticeError> {
        validate_repository_id(repository_id)?;
        let graph_revision = validate_graph_revision(graph_revision)?;
        validate_snapshot(snapshot)?;
        let report_json = serde_json::to_string(&snapshot.report).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to serialize dead-symbol-fact report: {error}"
            ))
        })?;
        let digest = snapshot_digest(snapshot)?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin dead-symbol-fact publication", error))?;
        tx.execute(
            "INSERT INTO health_dead_symbol_facts_repositories (repository_id) VALUES (?1) \
             ON CONFLICT(repository_id) DO NOTHING",
            [repository_id],
        )
        .map_err(|error| storage_error("initialize dead-symbol-fact repository state", error))?;

        if let Some(active) = active_identity(&tx, repository_id)? {
            if active.graph_revision.as_deref() == graph_revision && active.digest == digest {
                tx.execute(
                    "UPDATE health_dead_symbol_facts_generations SET refreshed_at = ?3 \
                     WHERE repository_id = ?1 AND generation = ?2",
                    params![repository_id, active.generation, refreshed_at],
                )
                .map_err(|error| {
                    storage_error("update replayed dead-symbol-fact refresh time", error)
                })?;
                let stored = load_generation(&tx, repository_id, active.generation, self.path())?;
                tx.commit().map_err(|error| {
                    storage_error("finish idempotent dead-symbol-fact publication", error)
                })?;
                return Ok(stored);
            }
        }

        let (active_generation, next_generation): (Option<i64>, i64) = tx
            .query_row(
                "SELECT active_generation, next_generation \
                 FROM health_dead_symbol_facts_repositories WHERE repository_id = ?1",
                [repository_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| storage_error("read dead-symbol-fact repository generation", error))?;
        let following_generation = next_generation.checked_add(1).ok_or_else(|| {
            LatticeError::Storage(format!(
                "Dead-symbol-fact generation exhausted for repository {repository_id}"
            ))
        })?;

        tx.execute(
            "INSERT INTO health_dead_symbol_facts_generations \
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
        .map_err(|error| storage_error("insert dead-symbol-fact generation", error))?;

        insert_snapshot_rows(&tx, repository_id, next_generation, snapshot)?;

        tx.execute(
            "UPDATE health_dead_symbol_facts_repositories \
             SET prior_generation = ?2, active_generation = ?3, next_generation = ?4 \
             WHERE repository_id = ?1",
            params![
                repository_id,
                active_generation,
                next_generation,
                following_generation
            ],
        )
        .map_err(|error| storage_error("activate dead-symbol-fact generation", error))?;

        // Publication is already durable within this transaction before
        // cleanup. Keep the active generation and exactly one predecessor,
        // both for diagnostics and so `candidate_delta_since_prior` has
        // something to compare.
        tx.execute(
            "DELETE FROM health_dead_symbol_facts_generations \
             WHERE repository_id = ?1 AND generation <> ?2 \
             AND (?3 IS NULL OR generation <> ?3)",
            params![repository_id, next_generation, active_generation],
        )
        .map_err(|error| storage_error("retire dead-symbol-fact generations", error))?;

        tx.commit()
            .map_err(|error| storage_error("commit dead-symbol-fact publication", error))?;
        Ok(StoredDeadSymbolFacts {
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
    ) -> Result<Option<StoredDeadSymbolFacts>, LatticeError> {
        self.load_pointer(repository_id, "active_generation")
    }

    /// Loads the single retained predecessor of the active generation.
    pub fn load_prior(
        &self,
        repository_id: &str,
    ) -> Result<Option<StoredDeadSymbolFacts>, LatticeError> {
        self.load_pointer(repository_id, "prior_generation")
    }

    fn load_pointer(
        &self,
        repository_id: &str,
        column: &str,
    ) -> Result<Option<StoredDeadSymbolFacts>, LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|error| storage_error("begin dead-symbol-fact read", error))?;
        // `column` is never caller-supplied; it is one of two literals below.
        let sql = format!(
            "SELECT {column} FROM health_dead_symbol_facts_repositories WHERE repository_id = ?1"
        );
        let generation: Option<i64> = tx
            .query_row(&sql, [repository_id], |row| row.get(0))
            .optional()
            .map_err(|error| storage_error("read dead-symbol-fact generation pointer", error))?
            .flatten();
        let stored = generation
            .map(|generation| load_generation(&tx, repository_id, generation, self.path()))
            .transpose()?;
        tx.commit()
            .map_err(|error| storage_error("finish dead-symbol-fact read", error))?;
        Ok(stored)
    }

    /// Which candidate keys moved between the retained prior generation and
    /// the active one.
    ///
    /// This is the incremental-refresh hook: after republishing facts
    /// following a one-file reindex, a consumer reads this delta and
    /// invalidates only the affected symbols instead of every symbol in the
    /// repository. `None` means no predecessor is retained, so nothing may be
    /// assumed unchanged.
    pub fn candidate_delta_since_prior(
        &self,
        repository_id: &str,
    ) -> Result<Option<crate::health::dead_symbol_facts::CandidateFactDelta>, LatticeError> {
        let Some(active) = self.load_active(repository_id)? else {
            return Ok(None);
        };
        let Some(prior) = self.load_prior(repository_id)? else {
            return Ok(None);
        };
        Ok(Some(active.snapshot.candidate_delta(&prior.snapshot)))
    }

    /// Loads the active generation, removing only this repository's
    /// dead-symbol-fact rows if their cross-table invariants are corrupt.
    pub fn load_active_recovering(
        &self,
        repository_id: &str,
    ) -> Result<DeadSymbolFactsLoad, LatticeError> {
        match self.load_active(repository_id) {
            Ok(stored) => Ok(DeadSymbolFactsLoad {
                stored,
                recovery: DeadSymbolFactsRecovery::None,
            }),
            Err(LatticeError::CorruptStorage { .. }) => {
                self.recover_repository(repository_id)?;
                Ok(DeadSymbolFactsLoad {
                    stored: None,
                    recovery: DeadSymbolFactsRecovery::RebuiltCorruptRows,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Discards all dead-symbol facts for one repository. Static graph
    /// tables, other health fact families, and other repositories are not
    /// touched.
    pub fn recover_repository(&self, repository_id: &str) -> Result<(), LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin dead-symbol-fact recovery", error))?;
        tx.execute(
            "DELETE FROM health_dead_symbol_facts_repositories WHERE repository_id = ?1",
            [repository_id],
        )
        .map_err(|error| storage_error("clear corrupt dead-symbol-fact rows", error))?;
        tx.commit()
            .map_err(|error| storage_error("commit dead-symbol-fact recovery", error))
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
             FROM health_dead_symbol_facts_generations \
             WHERE repository_id = ?1 AND generation = ( \
               SELECT active_generation FROM health_dead_symbol_facts_repositories \
               WHERE repository_id = ?1 \
             )",
            [repository_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| storage_error("read active dead-symbol-fact identity", error))?;
    row.map(|(generation, graph_revision, digest)| {
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| LatticeError::CorruptStorage {
                path: "health_dead_symbol_facts_generations".to_owned(),
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
    snapshot: &DeadSymbolFactsSnapshot,
) -> Result<(), LatticeError> {
    let mut insert = tx
        .prepare(
            "INSERT INTO health_dead_symbol_facts_candidates \
             (repository_id, generation, symbol_key, path, name, kind_code, definition_count, \
              source_line, source_end_line, exclusion_reasons) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .map_err(|error| storage_error("prepare dead-symbol-fact candidate insert", error))?;
    for candidate in &snapshot.candidates {
        insert
            .execute(params![
                repository_id,
                generation,
                candidate.key,
                candidate.path,
                candidate.name,
                candidate.kind.short_code(),
                i64::from(candidate.definition_count),
                i64::from(candidate.source_line),
                i64::from(candidate.source_end_line),
                encode_exclusion_reasons(&candidate.exclusion_reasons),
            ])
            .map_err(|error| storage_error("insert dead-symbol-fact candidate row", error))?;
    }
    Ok(())
}

fn load_generation(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<StoredDeadSymbolFacts, LatticeError> {
    let (graph_revision, refreshed_at, report_json, persisted_digest): (
        Option<String>,
        i64,
        String,
        Vec<u8>,
    ) = tx
        .query_row(
            "SELECT graph_revision, refreshed_at, report_json, snapshot_digest \
             FROM health_dead_symbol_facts_generations \
             WHERE repository_id = ?1 AND generation = ?2",
            params![repository_id, generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let report: DeadSymbolFactsReport = serde_json::from_str(&report_json).map_err(|error| {
        corrupt_message(
            storage_path,
            repository_id,
            generation,
            format!("invalid facts report: {error}"),
        )
    })?;

    let candidates = load_candidates(tx, repository_id, generation, storage_path)?;
    let snapshot = DeadSymbolFactsSnapshot { candidates, report };
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
    Ok(StoredDeadSymbolFacts {
        repository_id: repository_id.to_owned(),
        generation,
        graph_revision,
        refreshed_at,
        snapshot,
    })
}

fn load_candidates(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<Vec<DeadSymbolCandidate>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT symbol_key, path, name, kind_code, definition_count, source_line, \
             source_end_line, exclusion_reasons \
             FROM health_dead_symbol_facts_candidates \
             WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY symbol_key",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            let kind_code: String = row.get(3)?;
            let kind = SymbolKind::from_short_code(&kind_code).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown symbol kind code {kind_code:?}"),
                    )),
                )
            })?;
            let reasons_code: String = row.get(7)?;
            let exclusion_reasons = decode_exclusion_reasons(&reasons_code).map_err(|message| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        message,
                    )),
                )
            })?;
            Ok(DeadSymbolCandidate {
                key: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                kind,
                definition_count: row.get(4)?,
                source_line: row.get(5)?,
                source_end_line: row.get(6)?,
                exclusion_reasons,
            })
        })
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))
}

/// Comma-joins exclusion-reason codes in their declared (already
/// deterministic) order; the empty vector encodes as the empty string.
fn encode_exclusion_reasons(reasons: &[DeadSymbolExclusionReason]) -> String {
    reasons
        .iter()
        .map(DeadSymbolExclusionReason::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

fn decode_exclusion_reasons(value: &str) -> Result<Vec<DeadSymbolExclusionReason>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|code| {
            DeadSymbolExclusionReason::from_code(code)
                .ok_or_else(|| format!("unknown exclusion reason code {code:?}"))
        })
        .collect()
}

fn validate_repository_id(repository_id: &str) -> Result<(), LatticeError> {
    if repository_id.is_empty()
        || repository_id.trim() != repository_id
        || repository_id.contains('\0')
    {
        return Err(LatticeError::Storage(
            "Dead-symbol-fact repository identity must be non-empty, trimmed, and contain no NUL"
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
fn validate_snapshot(snapshot: &DeadSymbolFactsSnapshot) -> Result<(), LatticeError> {
    let report = &snapshot.report;
    let limits = report.limits;
    if report.facts_version == 0
        || limits.max_candidates > MAX_CANDIDATES
        || snapshot.candidates.len() > limits.max_candidates
        || usize::try_from(report.candidates_observed).ok() < Some(snapshot.candidates.len())
        || report.candidate_overflow
            != (usize::try_from(report.candidates_observed).ok() > Some(snapshot.candidates.len()))
        || (report.availability()
            == crate::health::dead_symbol_facts::FactAvailability::Unavailable
            && !snapshot.candidates.is_empty())
    {
        return Err(LatticeError::Storage(
            "Dead-symbol-fact report is inconsistent with its persisted rows".to_owned(),
        ));
    }

    let mut previous_key: Option<&str> = None;
    for candidate in &snapshot.candidates {
        validate_canonical_path(&candidate.path)?;
        if candidate.name.is_empty()
            || candidate.definition_count == 0
            || candidate.source_end_line < candidate.source_line
            || candidate.key
                != crate::health::graph_facts::symbol_fact_key(&candidate.path, &candidate.name)
        {
            return Err(LatticeError::Storage(format!(
                "Dead-symbol-fact candidate row {} is not a well-formed fact key",
                candidate.key
            )));
        }
        if previous_key.is_some_and(|key| key >= candidate.key.as_str()) {
            return Err(LatticeError::Storage(
                "Dead-symbol-fact candidate rows must be uniquely ordered by fact key".to_owned(),
            ));
        }
        previous_key = Some(candidate.key.as_str());

        let mut seen: Vec<DeadSymbolExclusionReason> = Vec::new();
        for reason in &candidate.exclusion_reasons {
            if seen.contains(reason) {
                return Err(LatticeError::Storage(format!(
                    "Dead-symbol-fact candidate {} lists exclusion reason {:?} more than once",
                    candidate.key, reason
                )));
            }
            seen.push(*reason);
        }
    }
    Ok(())
}

fn validate_canonical_path(path: &str) -> Result<(), LatticeError> {
    if canonical_repository_path(path).as_deref() != Some(path) {
        return Err(LatticeError::Storage(format!(
            "Dead-symbol-fact path is not canonical and repository-relative: {path:?}"
        )));
    }
    Ok(())
}

fn snapshot_digest(snapshot: &DeadSymbolFactsSnapshot) -> Result<[u8; 32], LatticeError> {
    #[derive(Serialize)]
    struct DigestEnvelope<'a> {
        format: u8,
        snapshot: &'a DeadSymbolFactsSnapshot,
    }
    let bytes = serde_json::to_vec(&DigestEnvelope {
        format: 1,
        snapshot,
    })
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to encode dead-symbol-fact digest input: {error}"
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
            "Dead-symbol-fact repository {repository_id} generation {generation}: {}",
            message.into()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CodeGraph;
    use crate::health::dead_symbol_facts::fixtures::{add_symbol, add_symbol_kind, symbol};
    use crate::health::dead_symbol_facts::{
        DeadSymbolExclusionInputs, DeadSymbolFactProducer, FactAvailability,
    };
    use crate::symbols::SymbolKind;

    /// One truly dead symbol, one excluded (test file), and one live symbol,
    /// so a publication exercises every branch of `exclusion_reasons`.
    fn snapshot(extra_dead: Option<&str>) -> DeadSymbolFactsSnapshot {
        let mut graph = CodeGraph::new();
        let dead = symbol("src/lib.rs", "unused_fn");
        let excluded = symbol("tests/support.rs", "make_fixture");
        let live = symbol("src/core.rs", "core_entry");
        let caller = symbol("src/caller.rs", "caller_fn");
        add_symbol(&mut graph, &dead, true, 1);
        add_symbol(&mut graph, &excluded, true, 1);
        add_symbol(&mut graph, &live, true, 1);
        add_symbol(&mut graph, &caller, true, 1);
        graph.add_edge(&caller, &live, crate::graph::EdgeKind::Calls);
        if let Some(path) = extra_dead {
            let leaf = symbol(path, "leaf_fn");
            add_symbol(&mut graph, &leaf, true, 1);
        }
        DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        )
    }

    fn count(store: &HealthDeadSymbolFactsStore, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn publish_round_trips_every_candidate_row() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let expected = snapshot(None);
        // dead + caller_fn (itself uncalled) are flagged; make_fixture is
        // excluded as a test symbol; live (called by caller_fn) is not a
        // candidate at all.
        assert_eq!(expected.candidates.len(), 3);
        assert_eq!(expected.flagged().count(), 2);

        let published = store
            .publish("repo-a", Some("rev-1"), 42, &expected)
            .unwrap();
        assert_eq!(published.generation, 1);
        assert_eq!(published.graph_revision.as_deref(), Some("rev-1"));

        let loaded = store.load_active("repo-a").unwrap().unwrap();
        assert_eq!(loaded.snapshot, expected);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM health_dead_symbol_facts_candidates"
            ),
            3
        );
    }

    #[test]
    fn republishing_an_identical_candidate_is_idempotent() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let expected = snapshot(None);
        let first = store
            .publish("repo-a", Some("rev-1"), 1, &expected)
            .unwrap();
        let second = store
            .publish("repo-a", Some("rev-1"), 2, &expected)
            .unwrap();

        assert_eq!(first.generation, second.generation);
        let loaded = store.load_active("repo-a").unwrap().unwrap();
        assert_eq!(loaded.refreshed_at, 2);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(DISTINCT generation) FROM health_dead_symbol_facts_generations \
                 WHERE repository_id = 'repo-a'"
            ),
            1
        );
    }

    #[test]
    fn publishing_a_changed_snapshot_advances_the_generation_and_retains_one_predecessor() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let first = snapshot(None);
        let second = snapshot(Some("src/extra.rs"));
        assert_ne!(first, second);

        store.publish("repo-a", Some("rev-1"), 1, &first).unwrap();
        let published_second = store.publish("repo-a", Some("rev-2"), 2, &second).unwrap();
        assert_eq!(published_second.generation, 2);

        let active = store.load_active("repo-a").unwrap().unwrap();
        assert_eq!(active.snapshot, second);
        let prior = store.load_prior("repo-a").unwrap().unwrap();
        assert_eq!(prior.snapshot, first);

        // Exactly two generations retained: active and one predecessor.
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM health_dead_symbol_facts_generations \
                 WHERE repository_id = 'repo-a'"
            ),
            2
        );
    }

    #[test]
    fn candidate_delta_since_prior_reflects_the_new_candidate() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let first = snapshot(None);
        let second = snapshot(Some("src/extra.rs"));
        store.publish("repo-a", Some("rev-1"), 1, &first).unwrap();
        store.publish("repo-a", Some("rev-2"), 2, &second).unwrap();

        let delta = store
            .candidate_delta_since_prior("repo-a")
            .unwrap()
            .unwrap();
        assert_eq!(delta.added, vec!["src/extra.rs::leaf_fn"]);
        assert!(delta.removed.is_empty());
    }

    #[test]
    fn separate_repositories_do_not_share_generations() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let repo_a = snapshot(None);
        let repo_b = snapshot(Some("src/extra.rs"));
        store.publish("repo-a", None, 1, &repo_a).unwrap();
        store.publish("repo-b", None, 1, &repo_b).unwrap();

        let loaded_a = store.load_active("repo-a").unwrap().unwrap();
        let loaded_b = store.load_active("repo-b").unwrap().unwrap();
        assert_eq!(loaded_a.snapshot, repo_a);
        assert_eq!(loaded_b.snapshot, repo_b);
        assert_ne!(loaded_a.snapshot, loaded_b.snapshot);
    }

    #[test]
    fn recover_repository_clears_only_that_repository() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let repo_a = snapshot(None);
        let repo_b = snapshot(Some("src/extra.rs"));
        store.publish("repo-a", None, 1, &repo_a).unwrap();
        store.publish("repo-b", None, 1, &repo_b).unwrap();

        store.recover_repository("repo-a").unwrap();

        assert!(store.load_active("repo-a").unwrap().is_none());
        assert!(store.load_active("repo-b").unwrap().is_some());
    }

    #[test]
    fn load_active_recovering_rebuilds_on_corrupt_digest() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let expected = snapshot(None);
        store
            .publish("repo-a", Some("rev-1"), 1, &expected)
            .unwrap();

        store
            .conn
            .execute(
                "UPDATE health_dead_symbol_facts_generations SET snapshot_digest = randomblob(32) \
                 WHERE repository_id = 'repo-a'",
                [],
            )
            .unwrap();

        let load = store.load_active_recovering("repo-a").unwrap();
        assert_eq!(load.recovery, DeadSymbolFactsRecovery::RebuiltCorruptRows);
        assert!(load.stored.is_none());
        assert!(store.load_active("repo-a").unwrap().is_none());
    }

    #[test]
    fn no_generation_published_yet_loads_as_none() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        assert!(store.load_active("unknown-repo").unwrap().is_none());
        assert!(store.load_prior("unknown-repo").unwrap().is_none());
        assert!(store
            .candidate_delta_since_prior("unknown-repo")
            .unwrap()
            .is_none());
    }

    #[test]
    fn empty_snapshot_publishes_and_round_trips() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let empty = DeadSymbolFactsSnapshot::empty();
        store.publish("repo-a", None, 1, &empty).unwrap();
        let loaded = store.load_active("repo-a").unwrap().unwrap();
        assert!(loaded.snapshot.candidates.is_empty());
        assert_eq!(
            loaded.snapshot.availability(),
            FactAvailability::Unavailable
        );
    }

    #[test]
    fn method_kind_and_multiple_exclusion_reasons_round_trip() {
        let store = HealthDeadSymbolFactsStore::open_in_memory().unwrap();
        let mut graph = CodeGraph::new();
        // Test-file trait-impl method: exercises both a non-Function
        // SymbolKind round trip and a multi-reason exclusion round trip.
        let odd = symbol("tests/support.rs", "Fixture.fmt");
        add_symbol_kind(&mut graph, &odd, SymbolKind::Method, true, 1);
        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        store.publish("repo-a", None, 1, &snapshot).unwrap();
        let loaded = store.load_active("repo-a").unwrap().unwrap();
        let candidate = loaded
            .snapshot
            .candidate("tests/support.rs::Fixture.fmt")
            .unwrap();
        assert_eq!(candidate.kind, SymbolKind::Method);
        assert_eq!(
            candidate.exclusion_reasons,
            vec![
                DeadSymbolExclusionReason::TestSymbol,
                DeadSymbolExclusionReason::TraitImplMethod,
            ]
        );
    }
}
