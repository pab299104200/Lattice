//! SQLite persistence for published test-proximity health-fact generations.
//!
//! The store is deliberately separate from fact production. A caller produces
//! a complete candidate from the built graph first, then publishes it with
//! one short `BEGIN IMMEDIATE` transaction. Readers therefore observe either
//! the previous generation or the complete replacement, never a mixture.
//!
//! The schema conventions here follow
//! [`crate::storage::health_graph_facts_store::HealthGraphFactsStore`] exactly
//! — repository pointer row with `active`/`prior`/`next` generations, an
//! immutable generation row carrying the completeness report and a snapshot
//! digest, one normalized fact table keyed by `(repository_id, generation,
//! path)`, and canonical-path validation on the way in and on the way out.
//!
//! See `docs/plans/2026-08-13-health-engine.md`, section "H2.4 Test-proximity
//! facts".

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::LatticeError;
use crate::git_intelligence::canonical_repository_path;
use crate::health::test_proximity_facts::{
    FileFactDelta, FileTestProximityFacts, TestLinkKind, TestProximityReport,
    TestProximitySnapshot, MAX_FILE_FACTS, MAX_LINKED_TEST_EVIDENCE,
};

const CREATE_HEALTH_TEST_PROXIMITY_FACTS_TABLES: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS health_test_proximity_facts_repositories (
    repository_id TEXT PRIMARY KEY,
    active_generation INTEGER,
    prior_generation INTEGER,
    next_generation INTEGER NOT NULL DEFAULT 1 CHECK (next_generation > 0)
);

CREATE TABLE IF NOT EXISTS health_test_proximity_facts_generations (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    graph_revision TEXT,
    refreshed_at INTEGER NOT NULL,
    report_json TEXT NOT NULL,
    snapshot_digest BLOB NOT NULL CHECK (length(snapshot_digest) = 32),
    PRIMARY KEY (repository_id, generation),
    FOREIGN KEY (repository_id) REFERENCES health_test_proximity_facts_repositories(repository_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS health_test_proximity_facts_files (
    repository_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    path TEXT NOT NULL,
    linked_test_count INTEGER NOT NULL CHECK (linked_test_count >= 0),
    -- NULL exactly when linked_test_count = 0; "graph_edge" otherwise. No
    -- other code is ever persisted by this producer (see TestLinkKind).
    strongest_link_kind TEXT,
    untested_change INTEGER NOT NULL CHECK (untested_change IN (0, 1)),
    -- Ordered evidence paths joined with a unit separator; bounded by
    -- MAX_LINKED_TEST_EVIDENCE and never authoritative over linked_test_count.
    linked_test_files TEXT NOT NULL,
    CHECK (untested_change = (linked_test_count = 0)),
    CHECK ((strongest_link_kind IS NULL) = (linked_test_count = 0)),
    PRIMARY KEY (repository_id, generation, path),
    FOREIGN KEY (repository_id, generation)
        REFERENCES health_test_proximity_facts_generations(repository_id, generation)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_health_test_proximity_facts_file_generation
    ON health_test_proximity_facts_files(repository_id, generation);
-- Serves the H4 `impact` query pattern directly: "which diff files in this
-- generation are untested" without a table scan.
CREATE INDEX IF NOT EXISTS idx_health_test_proximity_facts_untested
    ON health_test_proximity_facts_files(repository_id, generation, untested_change);
"#;

/// Separator between evidence paths in the persisted `linked_test_files` column.
///
/// Canonical repository paths are validated to be free of control characters
/// and NUL, and `\u{1f}` (unit separator) is not a legal path byte, so it can
/// never collide with a real path.
const LINKED_TEST_FILES_SEPARATOR: char = '\u{1f}';

/// One immutable fact generation selected by a repository's active pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTestProximityFacts {
    pub repository_id: String,
    pub generation: i64,
    /// Caller-supplied identity of the graph the facts were produced from.
    pub graph_revision: Option<String>,
    pub refreshed_at: i64,
    pub snapshot: TestProximitySnapshot,
}

/// Whether a recovering load had to discard corrupt test-proximity-fact rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestProximityFactsRecovery {
    None,
    RebuiltCorruptRows,
}

/// Result of a load which is allowed to remove corrupt rows for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestProximityFactsLoad {
    pub stored: Option<StoredTestProximityFacts>,
    pub recovery: TestProximityFactsRecovery,
}

/// A graph-database adapter for atomic test-proximity-fact generations.
pub struct HealthTestProximityFactsStore {
    conn: Connection,
    path: Option<PathBuf>,
}

impl HealthTestProximityFactsStore {
    /// Opens the workspace graph database and adds the test-proximity-fact
    /// schema.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|error| storage_error("open health test-proximity-fact database", error))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| storage_error("enable WAL for health test-proximity facts", error))?;
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
            storage_error("open in-memory health test-proximity-fact database", error)
        })?;
        let store = Self { conn, path: None };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(CREATE_HEALTH_TEST_PROXIMITY_FACTS_TABLES)
            .map_err(|error| storage_error("initialize health test-proximity-fact schema", error))
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
        snapshot: &TestProximitySnapshot,
    ) -> Result<StoredTestProximityFacts, LatticeError> {
        validate_repository_id(repository_id)?;
        let graph_revision = validate_graph_revision(graph_revision)?;
        validate_snapshot(snapshot)?;
        let report_json = serde_json::to_string(&snapshot.report).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to serialize test-proximity-fact report: {error}"
            ))
        })?;
        let digest = snapshot_digest(snapshot)?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin test-proximity-fact publication", error))?;
        tx.execute(
            "INSERT INTO health_test_proximity_facts_repositories (repository_id) VALUES (?1) \
             ON CONFLICT(repository_id) DO NOTHING",
            [repository_id],
        )
        .map_err(|error| storage_error("initialize test-proximity-fact repository state", error))?;

        if let Some(active) = active_identity(&tx, repository_id)? {
            if active.graph_revision.as_deref() == graph_revision && active.digest == digest {
                tx.execute(
                    "UPDATE health_test_proximity_facts_generations SET refreshed_at = ?3 \
                     WHERE repository_id = ?1 AND generation = ?2",
                    params![repository_id, active.generation, refreshed_at],
                )
                .map_err(|error| {
                    storage_error("update replayed test-proximity-fact refresh time", error)
                })?;
                let stored = load_generation(&tx, repository_id, active.generation, self.path())?;
                tx.commit().map_err(|error| {
                    storage_error("finish idempotent test-proximity-fact publication", error)
                })?;
                return Ok(stored);
            }
        }

        let (active_generation, next_generation): (Option<i64>, i64) = tx
            .query_row(
                "SELECT active_generation, next_generation \
                 FROM health_test_proximity_facts_repositories WHERE repository_id = ?1",
                [repository_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| {
                storage_error("read test-proximity-fact repository generation", error)
            })?;
        let following_generation = next_generation.checked_add(1).ok_or_else(|| {
            LatticeError::Storage(format!(
                "Test-proximity-fact generation exhausted for repository {repository_id}"
            ))
        })?;

        tx.execute(
            "INSERT INTO health_test_proximity_facts_generations \
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
        .map_err(|error| storage_error("insert test-proximity-fact generation", error))?;

        insert_snapshot_rows(&tx, repository_id, next_generation, snapshot)?;

        tx.execute(
            "UPDATE health_test_proximity_facts_repositories \
             SET prior_generation = ?2, active_generation = ?3, next_generation = ?4 \
             WHERE repository_id = ?1",
            params![
                repository_id,
                active_generation,
                next_generation,
                following_generation
            ],
        )
        .map_err(|error| storage_error("activate test-proximity-fact generation", error))?;

        // Publication is already durable within this transaction before
        // cleanup. Keep the active generation and exactly one predecessor,
        // both for diagnostics and so `file_delta_since_prior` has something
        // to compare.
        tx.execute(
            "DELETE FROM health_test_proximity_facts_generations \
             WHERE repository_id = ?1 AND generation <> ?2 \
             AND (?3 IS NULL OR generation <> ?3)",
            params![repository_id, next_generation, active_generation],
        )
        .map_err(|error| storage_error("retire test-proximity-fact generations", error))?;

        tx.commit()
            .map_err(|error| storage_error("commit test-proximity-fact publication", error))?;
        Ok(StoredTestProximityFacts {
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
    ) -> Result<Option<StoredTestProximityFacts>, LatticeError> {
        self.load_pointer(repository_id, "active_generation")
    }

    /// Loads the single retained predecessor of the active generation.
    pub fn load_prior(
        &self,
        repository_id: &str,
    ) -> Result<Option<StoredTestProximityFacts>, LatticeError> {
        self.load_pointer(repository_id, "prior_generation")
    }

    fn load_pointer(
        &self,
        repository_id: &str,
        column: &str,
    ) -> Result<Option<StoredTestProximityFacts>, LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|error| storage_error("begin test-proximity-fact read", error))?;
        // `column` is never caller-supplied; it is one of two literals below.
        let sql = format!(
            "SELECT {column} FROM health_test_proximity_facts_repositories \
             WHERE repository_id = ?1"
        );
        let generation: Option<i64> = tx
            .query_row(&sql, [repository_id], |row| row.get(0))
            .optional()
            .map_err(|error| storage_error("read test-proximity-fact generation pointer", error))?
            .flatten();
        let stored = generation
            .map(|generation| load_generation(&tx, repository_id, generation, self.path()))
            .transpose()?;
        tx.commit()
            .map_err(|error| storage_error("finish test-proximity-fact read", error))?;
        Ok(stored)
    }

    /// Which files' facts moved between the retained prior generation and the
    /// active one.
    ///
    /// This is the incremental-refresh hook: after republishing facts
    /// following a one-file reindex, a consumer reads this delta and
    /// invalidates only the affected files instead of every file in the
    /// repository. `None` means no predecessor is retained, so nothing may be
    /// assumed unchanged.
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

    /// Loads the active generation, removing only this repository's
    /// test-proximity-fact rows if their cross-table invariants are corrupt.
    pub fn load_active_recovering(
        &self,
        repository_id: &str,
    ) -> Result<TestProximityFactsLoad, LatticeError> {
        match self.load_active(repository_id) {
            Ok(stored) => Ok(TestProximityFactsLoad {
                stored,
                recovery: TestProximityFactsRecovery::None,
            }),
            Err(LatticeError::CorruptStorage { .. }) => {
                self.recover_repository(repository_id)?;
                Ok(TestProximityFactsLoad {
                    stored: None,
                    recovery: TestProximityFactsRecovery::RebuiltCorruptRows,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Discards all test-proximity facts for one repository. Static graph
    /// tables, sibling health-fact families, and other repositories are not
    /// touched.
    pub fn recover_repository(&self, repository_id: &str) -> Result<(), LatticeError> {
        validate_repository_id(repository_id)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin test-proximity-fact recovery", error))?;
        tx.execute(
            "DELETE FROM health_test_proximity_facts_repositories WHERE repository_id = ?1",
            [repository_id],
        )
        .map_err(|error| storage_error("clear corrupt test-proximity-fact rows", error))?;
        tx.commit()
            .map_err(|error| storage_error("commit test-proximity-fact recovery", error))
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
             FROM health_test_proximity_facts_generations \
             WHERE repository_id = ?1 AND generation = ( \
               SELECT active_generation FROM health_test_proximity_facts_repositories \
               WHERE repository_id = ?1 \
             )",
            [repository_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| storage_error("read active test-proximity-fact identity", error))?;
    row.map(|(generation, graph_revision, digest)| {
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| LatticeError::CorruptStorage {
                path: "health_test_proximity_facts_generations".to_owned(),
                message: format!(
                    "repository {repository_id} generation {generation} has an invalid \
                     snapshot digest"
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
    snapshot: &TestProximitySnapshot,
) -> Result<(), LatticeError> {
    let mut insert = tx
        .prepare(
            "INSERT INTO health_test_proximity_facts_files \
             (repository_id, generation, path, linked_test_count, strongest_link_kind, \
              untested_change, linked_test_files) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .map_err(|error| storage_error("prepare test-proximity-fact file insert", error))?;
    for file in &snapshot.files {
        insert
            .execute(params![
                repository_id,
                generation,
                file.path,
                i64::from(file.linked_test_count),
                file.strongest_link_kind.map(|kind| kind.as_str()),
                i64::from(file.untested_change),
                encode_linked_test_files(&file.linked_test_files),
            ])
            .map_err(|error| storage_error("insert test-proximity-fact file row", error))?;
    }
    Ok(())
}

fn load_generation(
    tx: &Transaction<'_>,
    repository_id: &str,
    generation: i64,
    storage_path: &str,
) -> Result<StoredTestProximityFacts, LatticeError> {
    let (graph_revision, refreshed_at, report_json, persisted_digest): (
        Option<String>,
        i64,
        String,
        Vec<u8>,
    ) = tx
        .query_row(
            "SELECT graph_revision, refreshed_at, report_json, snapshot_digest \
             FROM health_test_proximity_facts_generations \
             WHERE repository_id = ?1 AND generation = ?2",
            params![repository_id, generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let report: TestProximityReport = serde_json::from_str(&report_json).map_err(|error| {
        corrupt_message(
            storage_path,
            repository_id,
            generation,
            format!("invalid facts report: {error}"),
        )
    })?;

    let files = load_files(tx, repository_id, generation, storage_path)?;
    let snapshot = TestProximitySnapshot { files, report };
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
    Ok(StoredTestProximityFacts {
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
) -> Result<Vec<FileTestProximityFacts>, LatticeError> {
    let mut stmt = tx
        .prepare(
            "SELECT path, linked_test_count, strongest_link_kind, untested_change, \
             linked_test_files \
             FROM health_test_proximity_facts_files \
             WHERE repository_id = ?1 AND generation = ?2 \
             ORDER BY path",
        )
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
    let rows = stmt
        .query_map(params![repository_id, generation], |row| {
            let kind_code: Option<String> = row.get(2)?;
            let untested_change: i64 = row.get(3)?;
            let linked_test_files_raw: String = row.get(4)?;
            Ok((
                FileTestProximityFacts {
                    path: row.get(0)?,
                    linked_test_count: row.get(1)?,
                    strongest_link_kind: None,
                    untested_change: untested_change != 0,
                    linked_test_files: decode_linked_test_files(&linked_test_files_raw),
                },
                kind_code,
            ))
        })
        .map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;

    let mut files = Vec::new();
    for row in rows {
        let (mut facts, kind_code) =
            row.map_err(|error| corrupt_rows(storage_path, repository_id, generation, error))?;
        facts.strongest_link_kind = match kind_code {
            None => None,
            Some(code) => Some(TestLinkKind::from_code(&code).ok_or_else(|| {
                corrupt_message(
                    storage_path,
                    repository_id,
                    generation,
                    format!("unknown test-link kind code {code:?}"),
                )
            })?),
        };
        files.push(facts);
    }
    Ok(files)
}

fn encode_linked_test_files(paths: &[String]) -> String {
    paths.join(&LINKED_TEST_FILES_SEPARATOR.to_string())
}

fn decode_linked_test_files(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(LINKED_TEST_FILES_SEPARATOR)
        .map(str::to_owned)
        .collect()
}

fn validate_repository_id(repository_id: &str) -> Result<(), LatticeError> {
    if repository_id.is_empty()
        || repository_id.trim() != repository_id
        || repository_id.contains('\0')
    {
        return Err(LatticeError::Storage(
            "Test-proximity-fact repository identity must be non-empty, trimmed, and contain \
             no NUL"
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
/// fact a score or `impact` would go on to cite as evidence.
fn validate_snapshot(snapshot: &TestProximitySnapshot) -> Result<(), LatticeError> {
    let report = &snapshot.report;
    let limits = report.limits;
    if report.facts_version == 0
        || limits.max_files > MAX_FILE_FACTS
        || limits.max_linked_test_evidence > MAX_LINKED_TEST_EVIDENCE
        || snapshot.files.len() > limits.max_files
        || usize::try_from(report.production_files_observed).ok() < Some(snapshot.files.len())
        || report.file_overflow
            != (usize::try_from(report.production_files_observed).ok() > Some(snapshot.files.len()))
        || (report.availability()
            == crate::health::test_proximity_facts::FactAvailability::Unavailable
            && !snapshot.files.is_empty())
    {
        return Err(LatticeError::Storage(
            "Test-proximity-fact report is inconsistent with its persisted rows".to_owned(),
        ));
    }

    let mut previous: Option<&str> = None;
    for file in &snapshot.files {
        validate_canonical_path(&file.path)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) {
            return Err(LatticeError::Storage(
                "Test-proximity-fact file rows must be uniquely ordered by canonical path"
                    .to_owned(),
            ));
        }
        previous = Some(file.path.as_str());

        if file.untested_change != (file.linked_test_count == 0)
            || file.strongest_link_kind.is_some() != (file.linked_test_count > 0)
            || file
                .strongest_link_kind
                .is_some_and(|kind| kind != TestLinkKind::GraphEdge)
            || file.linked_test_files.len() > MAX_LINKED_TEST_EVIDENCE
        {
            return Err(LatticeError::Storage(format!(
                "Test-proximity-fact file row {} has impossible aggregate values",
                file.path
            )));
        }
        for evidence_path in &file.linked_test_files {
            validate_canonical_path(evidence_path)?;
            if evidence_path == &file.path {
                return Err(LatticeError::Storage(format!(
                    "Test-proximity-fact file {} lists itself as its own linking test",
                    file.path
                )));
            }
        }
    }
    Ok(())
}

fn validate_canonical_path(path: &str) -> Result<(), LatticeError> {
    if canonical_repository_path(path).as_deref() != Some(path) {
        return Err(LatticeError::Storage(format!(
            "Test-proximity-fact path is not canonical and repository-relative: {path:?}"
        )));
    }
    Ok(())
}

fn snapshot_digest(snapshot: &TestProximitySnapshot) -> Result<[u8; 32], LatticeError> {
    #[derive(Serialize)]
    struct DigestEnvelope<'a> {
        format: u8,
        snapshot: &'a TestProximitySnapshot,
    }
    let bytes = serde_json::to_vec(&DigestEnvelope {
        format: 1,
        snapshot,
    })
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to encode test-proximity-fact digest input: {error}"
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
            "Test-proximity-fact repository {repository_id} generation {generation}: {}",
            message.into()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{CodeGraph, EdgeKind};
    use crate::health::test_proximity_facts::fixtures::{add_edge, add_symbol, symbol};
    use crate::health::test_proximity_facts::TestProximityFactProducer;

    /// `src/widget.rs` is linked from one test; `src/orphan.rs` has no linking
    /// test at all, so a publication exercises both the linked and untested
    /// branches of the single fact table.
    fn snapshot(extra_test_for_orphan: bool) -> TestProximitySnapshot {
        let mut graph = CodeGraph::new();
        let widget = symbol("src/widget.rs", "make_widget");
        let orphan = symbol("src/orphan.rs", "do_thing");
        let widget_test = symbol("tests/widget_test.rs", "test_make_widget");
        add_symbol(&mut graph, &widget, true, 1);
        add_symbol(&mut graph, &orphan, true, 1);
        add_symbol(&mut graph, &widget_test, true, 1);
        add_edge(&mut graph, &widget_test, &widget, EdgeKind::Calls);
        if extra_test_for_orphan {
            let orphan_test = symbol("tests/orphan_test.rs", "test_do_thing");
            add_symbol(&mut graph, &orphan_test, true, 1);
            add_edge(&mut graph, &orphan_test, &orphan, EdgeKind::Calls);
        }
        TestProximityFactProducer::default().produce(&graph, true)
    }

    fn count(store: &HealthTestProximityFactsStore, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn publish_round_trips_linked_and_untested_rows() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let expected = snapshot(false);

        let published = store
            .publish("repo-a", Some("rev-1"), 42, &expected)
            .unwrap();
        assert_eq!(published.generation, 1);
        assert_eq!(published.graph_revision.as_deref(), Some("rev-1"));

        let loaded = store.load_active("repo-a").unwrap().unwrap();
        assert_eq!(loaded, published);
        assert_eq!(loaded.snapshot, expected);

        let widget = loaded.snapshot.file("src/widget.rs").unwrap();
        assert_eq!(widget.linked_test_count, 1);
        assert_eq!(widget.strongest_link_kind, Some(TestLinkKind::GraphEdge));
        assert!(!widget.untested_change);
        assert_eq!(widget.linked_test_files, ["tests/widget_test.rs"]);

        let orphan = loaded.snapshot.file("src/orphan.rs").unwrap();
        assert_eq!(orphan.linked_test_count, 0);
        assert_eq!(orphan.strongest_link_kind, None);
        assert!(orphan.untested_change);
        assert!(orphan.linked_test_files.is_empty());
    }

    #[test]
    fn empty_snapshot_publishes_and_stays_unavailable() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let published = store
            .publish("repo-empty", None, 7, &TestProximitySnapshot::empty())
            .unwrap();
        assert_eq!(published.graph_revision, None);
        let loaded = store.load_active("repo-empty").unwrap().unwrap();
        assert_eq!(loaded.snapshot, TestProximitySnapshot::empty());
        assert_eq!(
            loaded.snapshot.availability(),
            crate::health::test_proximity_facts::FactAvailability::Unavailable
        );
    }

    #[test]
    fn identical_replay_reuses_the_active_generation() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let expected = snapshot(false);
        let first = store.publish("repo", Some("rev-1"), 1, &expected).unwrap();
        let replay = store.publish("repo", Some("rev-1"), 99, &expected).unwrap();
        assert_eq!(first.generation, replay.generation);
        assert_eq!(replay.refreshed_at, 99);
        assert_eq!(replay.snapshot, first.snapshot);
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_test_proximity_facts_generations"
            ),
            1
        );
    }

    #[test]
    fn a_new_revision_over_identical_facts_still_advances_the_generation() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let expected = snapshot(false);
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
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let first = store
            .publish("repo", Some("r1"), 1, &snapshot(false))
            .unwrap();
        let second = store
            .publish("repo", Some("r2"), 2, &snapshot(true))
            .unwrap();
        let third = store
            .publish("repo", Some("r3"), 3, &snapshot(false))
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
                "SELECT count(*) FROM health_test_proximity_facts_generations"
            ),
            2,
            "generation 1 is retired once it is no longer the prior pointer"
        );
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_test_proximity_facts_files WHERE generation = 1"
            ),
            0
        );
    }

    #[test]
    fn repositories_do_not_observe_each_others_generations() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let left = store
            .publish("left", Some("l1"), 1, &snapshot(false))
            .unwrap();
        let right = store
            .publish("right", Some("r1"), 2, &snapshot(true))
            .unwrap();
        store
            .publish("left", Some("l2"), 3, &snapshot(true))
            .unwrap();

        assert_eq!(store.load_active("right").unwrap(), Some(right));
        assert_ne!(
            store.load_active("left").unwrap().unwrap().snapshot,
            left.snapshot
        );
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_test_proximity_facts_generations \
                 WHERE repository_id = 'right'"
            ),
            1
        );
    }

    #[test]
    fn failed_candidate_rolls_back_and_preserves_the_active_pointer() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let first = store
            .publish("repo", Some("r1"), 1, &snapshot(false))
            .unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_candidate BEFORE INSERT ON \
                 health_test_proximity_facts_files \
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();

        let error = store
            .publish("repo", Some("r2"), 2, &snapshot(true))
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(store.load_active("repo").unwrap(), Some(first));
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_test_proximity_facts_generations"
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM health_test_proximity_facts_files"
            ),
            i64::try_from(snapshot(false).files.len()).unwrap(),
            "no partial file rows from the aborted candidate survive"
        );
    }

    #[test]
    fn file_delta_since_prior_confines_an_incremental_refresh() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        store
            .publish("repo", Some("r1"), 1, &snapshot(false))
            .unwrap();
        assert_eq!(
            store.file_delta_since_prior("repo").unwrap(),
            None,
            "the first generation has no predecessor to compare against"
        );

        store
            .publish("repo", Some("r2"), 2, &snapshot(true))
            .unwrap();
        let delta = store.file_delta_since_prior("repo").unwrap().unwrap();
        assert_eq!(delta.updated, ["src/orphan.rs"]);
        assert!(delta.added.is_empty());
        assert!(delta.removed.is_empty());
        assert_eq!(delta.touched_paths(), ["src/orphan.rs"]);
    }

    #[test]
    fn recovering_load_removes_only_the_corrupt_repository() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TABLE nodes_for_recovery_proof (value TEXT NOT NULL); \
                 INSERT INTO nodes_for_recovery_proof VALUES ('preserved');",
            )
            .unwrap();
        store
            .publish("broken", Some("b1"), 1, &snapshot(false))
            .unwrap();
        let healthy = store
            .publish("healthy", Some("h1"), 1, &snapshot(true))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE health_test_proximity_facts_generations SET report_json = '{' \
                 WHERE repository_id = 'broken'",
                [],
            )
            .unwrap();

        let recovered = store.load_active_recovering("broken").unwrap();
        assert_eq!(recovered.stored, None);
        assert_eq!(
            recovered.recovery,
            TestProximityFactsRecovery::RebuiltCorruptRows
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
    fn tampered_fact_rows_are_detected_by_the_snapshot_digest() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        store
            .publish("repo", Some("r1"), 1, &snapshot(false))
            .unwrap();
        // Inflate a linked file's count without touching any other column:
        // every column CHECK still passes (count > 0, kind is graph_edge,
        // untested_change is 0), so only the recomputed snapshot digest
        // catches the fabricated evidence.
        store
            .conn
            .execute(
                "UPDATE health_test_proximity_facts_files \
                 SET linked_test_count = 5 \
                 WHERE path = 'src/widget.rs'",
                [],
            )
            .unwrap();
        let error = store.load_active("repo").unwrap_err();
        assert!(matches!(error, LatticeError::CorruptStorage { .. }));
        assert!(error.to_string().contains("snapshot digest"));
    }

    #[test]
    fn rejects_noncanonical_and_internally_inconsistent_candidates() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();

        let mut escaped = snapshot(false);
        escaped.files[0].path = "../outside.rs".to_owned();
        assert!(store.publish("repo", Some("r1"), 1, &escaped).is_err());

        let mut fabricated_link = snapshot(false);
        let orphan = fabricated_link
            .files
            .iter_mut()
            .find(|file| file.path == "src/orphan.rs")
            .unwrap();
        orphan.untested_change = false;
        assert!(store
            .publish("repo", Some("r2"), 2, &fabricated_link)
            .is_err());

        let mut mismatched_kind = snapshot(false);
        let widget = mismatched_kind
            .files
            .iter_mut()
            .find(|file| file.path == "src/widget.rs")
            .unwrap();
        widget.strongest_link_kind = None;
        assert!(store
            .publish("repo", Some("r3"), 3, &mismatched_kind)
            .is_err());

        let mut misordered = snapshot(false);
        misordered.files.swap(0, 1);
        assert!(store.publish("repo", Some("r4"), 4, &misordered).is_err());

        assert!(
            store.load_active("repo").unwrap().is_none(),
            "no rejected candidate ever became active"
        );
    }

    #[test]
    fn rejects_invalid_repository_and_revision_identities() {
        let store = HealthTestProximityFactsStore::open_in_memory().unwrap();
        let facts = snapshot(false);
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
            let store = HealthTestProximityFactsStore::open(&path).unwrap();
            store
                .publish("repo", Some("r1"), 17, &snapshot(false))
                .unwrap()
        };
        let reopened = HealthTestProximityFactsStore::open(&path).unwrap();
        assert_eq!(reopened.load_active("repo").unwrap(), Some(expected));
    }

    #[test]
    fn coexists_with_the_graph_facts_schema_in_one_database() {
        use crate::health::graph_facts::{GraphFactProducer, GraphFactsSnapshot};
        use crate::storage::HealthGraphFactsStore;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.db");
        let graph_facts_store = HealthGraphFactsStore::open(&path).unwrap();
        graph_facts_store
            .publish("repo", None, 1, &GraphFactsSnapshot::empty())
            .unwrap();
        let _ = GraphFactProducer::default();

        let facts = HealthTestProximityFactsStore::open(&path).unwrap();
        let published = facts
            .publish("repo", Some("r1"), 2, &snapshot(false))
            .unwrap();

        assert_eq!(facts.load_active("repo").unwrap(), Some(published));
        assert!(graph_facts_store.load_active("repo").unwrap().is_some());
    }
}
