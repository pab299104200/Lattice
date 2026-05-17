use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;
use tracing::{error, info_span, warn};

use crate::events::EventWriteError;
use crate::identity::{Identity, MemoryId};
use crate::memory_graph::{
    encode_identity_text, AssertionType, MemoryClass, MemoryScope, VerificationStatus,
};

use super::migration_mapping::{map_row, migration_actor};

pub(super) const MIGRATION_GUIDE: &str =
    "docs/architecture/2026-05-16-memory-migration-guide.md#column-mapping";
const MIGRATION_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationTableCounts {
    pub memories: usize,
    pub memory_links: usize,
    pub memory_evidence: usize,
    pub memory_accesses: usize,
    pub memory_scores: usize,
}

impl MigrationTableCounts {
    fn empty() -> Self {
        Self {
            memories: 0,
            memory_links: 0,
            memory_evidence: 0,
            memory_accesses: 0,
            memory_scores: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRow {
    pub source_row_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    pub source_rows: usize,
    pub already_migrated_rows: usize,
    pub destination_inserts: MigrationTableCounts,
    pub skipped_rows: Vec<SkippedRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub dry_run: bool,
    pub source_rows: usize,
    pub migrated_rows: usize,
    pub already_migrated_rows: usize,
    pub destination_inserts: MigrationTableCounts,
    pub skipped_rows: Vec<SkippedRow>,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("legacy memory source is unreadable: {0}")]
    SourceUnreadable(String),
    #[error("memory graph destination is not ready: {0}")]
    DestinationUnready(String),
    #[error("legacy memory row {source_row_id} could not be mapped: {reason}")]
    RowMappingFailed { source_row_id: i64, reason: String },
    #[error("constraint violation in {table}: {detail}")]
    ConstraintViolation { table: &'static str, detail: String },
    #[error("migration idempotence conflict for source row {source_row_id}")]
    IdempotenceConflict { source_row_id: i64 },
    #[error("failed to emit migration event: {0}")]
    EventEmitFailed(#[from] EventWriteError),
}

pub struct MemoryMigrator {
    pub source_conn: Arc<Mutex<Connection>>,
    pub dest_conn: Arc<Mutex<Connection>>,
    pub batch_size: usize,
    pub dry_run: bool,
}

impl MemoryMigrator {
    pub fn new(
        source_conn: Arc<Mutex<Connection>>,
        dest_conn: Arc<Mutex<Connection>>,
        batch_size: usize,
        dry_run: bool,
    ) -> Self {
        Self {
            source_conn,
            dest_conn,
            batch_size: batch_size.max(1),
            dry_run,
        }
    }

    pub fn plan(&self) -> Result<MigrationPlan, MigrationError> {
        self.prepare_destination()?;
        let already_migrated_rows = self.last_migrated_source_row_id()?.max(0) as usize;
        let mut counts = MigrationTableCounts::empty();
        let mut skipped_rows = Vec::new();
        let mut after_row_id = 0;
        loop {
            let rows = self.read_source_rows(after_row_id)?;
            if rows.is_empty() {
                break;
            }
            after_row_id = rows
                .last()
                .map(|row| row.source_row_id)
                .unwrap_or(after_row_id);
            for row in rows {
                if self.is_already_migrated(row.source_row_id)? {
                    continue;
                }
                match map_row(&row) {
                    Ok(mapped) => counts.add(&mapped),
                    Err(reason) => skipped_rows.push(SkippedRow {
                        source_row_id: row.source_row_id,
                        reason,
                    }),
                }
            }
        }

        Ok(MigrationPlan {
            source_rows: self.count_source_rows()?,
            already_migrated_rows: already_migrated_rows.min(self.count_source_rows()?),
            destination_inserts: counts,
            skipped_rows,
        })
    }

    pub fn run(&self, plan: &MigrationPlan) -> Result<MigrationReport, MigrationError> {
        self.prepare_destination()?;
        let mut report = MigrationReport {
            dry_run: self.dry_run,
            source_rows: plan.source_rows,
            migrated_rows: 0,
            already_migrated_rows: plan.already_migrated_rows,
            destination_inserts: MigrationTableCounts::empty(),
            skipped_rows: Vec::new(),
        };
        let mut after_row_id = self.last_migrated_source_row_id()?;

        loop {
            let batch = self.read_source_rows(after_row_id)?;
            if batch.is_empty() {
                return Ok(report);
            }
            let _span = info_span!("memory_migration", batch = batch.len()).entered();
            after_row_id = batch
                .last()
                .map(|row| row.source_row_id)
                .unwrap_or(after_row_id);
            self.run_batch(&batch, &mut report)?;
        }
    }

    fn run_batch(
        &self,
        rows: &[LegacyMemoryRow],
        report: &mut MigrationReport,
    ) -> Result<(), MigrationError> {
        if self.dry_run {
            return self.validate_batch(rows, report);
        }
        let mut dest = self.lock_dest()?;
        let tx = dest
            .transaction()
            .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
        tx.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
        for row in rows {
            self.migrate_row(&tx, row, report)?;
        }
        tx.commit()
            .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
        Ok(())
    }

    fn migrate_row(
        &self,
        conn: &Connection,
        row: &LegacyMemoryRow,
        report: &mut MigrationReport,
    ) -> Result<(), MigrationError> {
        if self.is_migrated_in(conn, row.source_row_id)? {
            report.already_migrated_rows += 1;
            return Ok(());
        }
        match map_row(row) {
            Ok(mapped) => {
                ensure_no_conflict(conn, row.source_row_id, &mapped.memory_id)?;
                insert_mapped_row(conn, &mapped)?;
                mark_progress(conn, row.source_row_id, &mapped.memory_id)?;
                report.destination_inserts.add(&mapped);
                report.migrated_rows += 1;
            }
            Err(reason) => {
                warn!(
                    source_row_id = row.source_row_id,
                    mapping_guide = MIGRATION_GUIDE,
                    %reason,
                    "legacy memory row skipped during migration"
                );
                let skip_reason = reason.clone();
                report.skipped_rows.push(SkippedRow {
                    source_row_id: row.source_row_id,
                    reason,
                });
                mark_skipped(conn, row.source_row_id, &skip_reason)?;
            }
        }
        Ok(())
    }

    fn validate_batch(
        &self,
        rows: &[LegacyMemoryRow],
        report: &mut MigrationReport,
    ) -> Result<(), MigrationError> {
        let mut validation = Connection::open_in_memory()
            .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
        initialize_destination(&validation)?;
        let tx = validation
            .transaction()
            .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
        for row in rows {
            match map_row(row) {
                Ok(mapped) => {
                    insert_mapped_row(&tx, &mapped)?;
                    report.destination_inserts.add(&mapped);
                    report.migrated_rows += 1;
                }
                Err(reason) => report.skipped_rows.push(SkippedRow {
                    source_row_id: row.source_row_id,
                    reason,
                }),
            }
        }
        Ok(())
    }

    fn prepare_destination(&self) -> Result<(), MigrationError> {
        let dest = self.lock_dest()?;
        initialize_destination(&dest)
    }

    fn count_source_rows(&self) -> Result<usize, MigrationError> {
        let source = self.lock_source()?;
        source
            .query_row("SELECT COUNT(*) FROM memories", params![], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as usize)
            .map_err(|error| MigrationError::SourceUnreadable(error.to_string()))
    }

    fn read_source_rows(&self, after_row_id: i64) -> Result<Vec<LegacyMemoryRow>, MigrationError> {
        let source = self.lock_source()?;
        let mut statement = source
            .prepare(LEGACY_SELECT_SQL)
            .map_err(|error| MigrationError::SourceUnreadable(error.to_string()))?;
        let rows = statement
            .query_map(
                params![after_row_id, self.batch_size as i64],
                LegacyMemoryRow::from_row,
            )
            .map_err(|error| MigrationError::SourceUnreadable(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| MigrationError::SourceUnreadable(error.to_string()))
    }

    fn last_migrated_source_row_id(&self) -> Result<i64, MigrationError> {
        let dest = self.lock_dest()?;
        dest.query_row(
            "SELECT COALESCE(MAX(source_row_id), 0) FROM migration_progress WHERE status = 'migrated'",
            params![],
            |row| row.get(0),
        )
        .map_err(|error| MigrationError::DestinationUnready(error.to_string()))
    }

    fn is_already_migrated(&self, source_row_id: i64) -> Result<bool, MigrationError> {
        let dest = self.lock_dest()?;
        self.is_migrated_in(&dest, source_row_id)
    }

    fn is_migrated_in(
        &self,
        conn: &Connection,
        source_row_id: i64,
    ) -> Result<bool, MigrationError> {
        conn.query_row(
            "SELECT 1 FROM migration_progress WHERE source_row_id = ?1 AND status = 'migrated'",
            params![source_row_id],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(|error| MigrationError::DestinationUnready(error.to_string()))
    }

    fn lock_source(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MigrationError> {
        self.source_conn
            .lock()
            .map_err(|_| MigrationError::SourceUnreadable("source connection lock poisoned".into()))
    }

    fn lock_dest(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MigrationError> {
        self.dest_conn.lock().map_err(|_| {
            MigrationError::DestinationUnready("destination connection lock poisoned".into())
        })
    }
}

impl MigrationTableCounts {
    fn add(&mut self, row: &MappedMemoryRow) {
        self.memories += 1;
        self.memory_links += row.links.len();
        self.memory_evidence += row.evidence.len();
        self.memory_accesses += row.accesses.len();
        self.memory_scores += row.scores.len();
    }
}

#[derive(Clone, Debug)]
pub(super) struct LegacyMemoryRow {
    pub(super) source_row_id: i64,
    pub(super) id: String,
    pub(super) session_id: String,
    pub(super) content: String,
    pub(super) memory_type: String,
    pub(super) scope: String,
    pub(super) confidence: f64,
    pub(super) linked_symbols: String,
    pub(super) linked_files: String,
    pub(super) workspace_id: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) _refresh_key: Option<String>,
    pub(super) source_query: Option<String>,
    pub(super) assertion_type: String,
    pub(super) verification_status: String,
    pub(super) confidence_reason: Option<String>,
    pub(super) supersedes_memory_id: Option<String>,
    pub(super) superseded_by_memory_id: Option<String>,
    pub(super) contradicts_memory_ids: String,
    pub(super) freshness_policy: String,
    pub(super) _freshness_policy_detail: Option<String>,
    pub(super) _provenance_json: String,
    pub(super) evidence_json: String,
    pub(super) created_at: i64,
    pub(super) last_accessed: i64,
    pub(super) access_count: i64,
    pub(super) is_stale: i64,
    pub(super) stale_reason: Option<String>,
    pub(super) is_invalidated: i64,
}

impl LegacyMemoryRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            source_row_id: row.get(0)?,
            id: row.get(1)?,
            session_id: row.get(2)?,
            content: row.get(3)?,
            memory_type: row.get(4)?,
            scope: row.get(5)?,
            confidence: row.get(6)?,
            linked_symbols: row.get(7)?,
            linked_files: row.get(8)?,
            workspace_id: row.get(9)?,
            branch: row.get(10)?,
            _refresh_key: row.get(11)?,
            source_query: row.get(12)?,
            assertion_type: row.get(13)?,
            verification_status: row.get(14)?,
            confidence_reason: row.get(15)?,
            supersedes_memory_id: row.get(16)?,
            superseded_by_memory_id: row.get(17)?,
            contradicts_memory_ids: row.get(18)?,
            freshness_policy: row.get(20)?,
            _freshness_policy_detail: row.get(21)?,
            _provenance_json: row.get(22)?,
            evidence_json: row.get(23)?,
            created_at: row.get(24)?,
            last_accessed: row.get(25)?,
            access_count: row.get(26)?,
            is_stale: row.get(27)?,
            stale_reason: row.get(28)?,
            is_invalidated: row.get(29)?,
        })
    }
}

#[derive(Clone, Debug)]
pub(super) struct MappedMemoryRow {
    pub(super) source_row_id: i64,
    pub(super) memory_id: MemoryId,
    pub(super) class: MemoryClass,
    pub(super) assertion_type: AssertionType,
    pub(super) scope: MemoryScope,
    pub(super) scope_session_id: Option<String>,
    pub(super) scope_branch: Option<String>,
    pub(super) scope_workspace_id: Option<String>,
    pub(super) verification_status: VerificationStatus,
    pub(super) confidence: f64,
    pub(super) confidence_reason: String,
    pub(super) content: String,
    pub(super) freshness_policy_json: String,
    pub(super) validity_conditions_json: String,
    pub(super) invalidation_triggers_json: String,
    pub(super) provenance_event_ids_json: String,
    pub(super) evidence_references_json: String,
    pub(super) linked_files_json: String,
    pub(super) linked_symbols_json: String,
    pub(super) linked_docs_json: String,
    pub(super) linked_tests_json: String,
    pub(super) linked_memories_json: String,
    pub(super) contradiction_links_json: String,
    pub(super) supersession_links_json: String,
    pub(super) access_history_json: String,
    pub(super) usefulness_score: f64,
    pub(super) created_at: i64,
    pub(super) updated_at: i64,
    pub(super) superseded_by: Option<MemoryId>,
    pub(super) links: Vec<MappedLink>,
    pub(super) evidence: Vec<MappedEvidence>,
    pub(super) accesses: Vec<MappedAccess>,
    pub(super) scores: Vec<MappedScore>,
}

#[derive(Clone, Debug)]
pub(super) struct MappedLink {
    pub(super) link_id: String,
    pub(super) target_kind: &'static str,
    pub(super) target_id: String,
    pub(super) link_type: &'static str,
    pub(super) reason: String,
}

#[derive(Clone, Debug)]
pub(super) struct MappedEvidence {
    pub(super) evidence_id: String,
    pub(super) event_id: Option<String>,
    pub(super) anchor_kind: &'static str,
    pub(super) anchor_json: String,
    pub(super) captured_at: i64,
}

#[derive(Clone, Debug)]
pub(super) struct MappedAccess {
    pub(super) access_id: String,
    pub(super) accessed_at: i64,
    pub(super) accessed_in_event: String,
    pub(super) inclusion_reason: String,
    pub(super) was_used: Option<i64>,
}

#[derive(Clone, Debug)]
pub(super) struct MappedScore {
    pub(super) score_kind: &'static str,
    pub(super) value: f64,
    pub(super) computed_at: i64,
    pub(super) computed_from_window_secs: i64,
    pub(super) sample_size: i64,
}

fn initialize_destination(conn: &Connection) -> Result<(), MigrationError> {
    super::initialize_schema(conn)
        .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS migration_progress (
            source_row_id INTEGER PRIMARY KEY,
            dest_memory_id TEXT NULL,
            status TEXT NOT NULL CHECK(status IN ('migrated', 'skipped')),
            migrated_at INTEGER NOT NULL,
            schema_version INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS migration_quarantine (
            source_row_id INTEGER PRIMARY KEY,
            reason TEXT NOT NULL,
            quarantined_at INTEGER NOT NULL,
            schema_version INTEGER NOT NULL
         );",
    )
    .map_err(|error| MigrationError::DestinationUnready(error.to_string()))
}

fn insert_mapped_row(conn: &Connection, row: &MappedMemoryRow) -> Result<(), MigrationError> {
    insert_memory(conn, row)?;
    for link in &row.links {
        insert_link_row(conn, row, link)?;
    }
    for evidence in &row.evidence {
        insert_evidence_row(conn, row, evidence)?;
    }
    for access in &row.accesses {
        insert_access_row(conn, row, access)?;
    }
    for score in &row.scores {
        insert_score_row(conn, row, score)?;
    }
    Ok(())
}

fn insert_memory(conn: &Connection, row: &MappedMemoryRow) -> Result<(), MigrationError> {
    let memory_id = encode_identity_text(&Identity::Memory(row.memory_id.clone()));
    let created_by = migration_actor(row.source_row_id);
    let superseded_by = row
        .superseded_by
        .as_ref()
        .map(|id| encode_identity_text(&Identity::Memory(id.clone())));
    conn.execute(
        MEMORY_INSERT_SQL,
        params![
            memory_id,
            row.content,
            row.class.as_str(),
            row.assertion_type.as_str(),
            row.scope.as_str(),
            row.scope_session_id,
            row.scope_branch,
            row.scope_workspace_id,
            Option::<String>::None,
            Option::<String>::None,
            row.verification_status.as_str(),
            row.confidence,
            row.confidence_reason,
            row.freshness_policy_json,
            row.validity_conditions_json,
            row.invalidation_triggers_json,
            row.provenance_event_ids_json,
            row.evidence_references_json,
            row.linked_files_json,
            row.linked_symbols_json,
            row.linked_docs_json,
            row.linked_tests_json,
            row.linked_memories_json,
            row.contradiction_links_json,
            row.supersession_links_json,
            row.access_history_json,
            Option::<i64>::None,
            Option::<String>::None,
            row.usefulness_score,
            row.updated_at,
            row.created_at,
            created_by,
            row.updated_at,
            created_by,
            superseded_by,
            MIGRATION_SCHEMA_VERSION,
        ],
    )
    .map_err(|error| constraint("memories", error))
    .map(|_| ())
}

fn insert_link_row(
    conn: &Connection,
    row: &MappedMemoryRow,
    link: &MappedLink,
) -> Result<(), MigrationError> {
    conn.execute(
        "INSERT INTO memory_links
            (link_id, source_memory_id, target_kind, target_id, link_type, strength, reason,
             evidence_event_id, created_by_kind, created_by_detail, created_at, verification_status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'daemon', ?8, ?9, ?10)",
        params![
            link.link_id,
            encode_identity_text(&Identity::Memory(row.memory_id.clone())),
            link.target_kind,
            link.target_id,
            link.link_type,
            1.0_f64,
            link.reason,
            migration_actor(row.source_row_id),
            row.created_at,
            row.verification_status.as_str(),
        ],
    )
    .map_err(|error| constraint("memory_links", error))
    .map(|_| ())
}

fn insert_evidence_row(
    conn: &Connection,
    row: &MappedMemoryRow,
    evidence: &MappedEvidence,
) -> Result<(), MigrationError> {
    conn.execute(
        "INSERT INTO memory_evidence
            (evidence_id, memory_id, event_id, anchor_kind, anchor_json, captured_at,
             captured_by_kind, captured_by_detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'daemon', ?7)",
        params![
            evidence.evidence_id,
            encode_identity_text(&Identity::Memory(row.memory_id.clone())),
            evidence.event_id,
            evidence.anchor_kind,
            evidence.anchor_json,
            evidence.captured_at,
            migration_actor(row.source_row_id),
        ],
    )
    .map_err(|error| constraint("memory_evidence", error))
    .map(|_| ())
}

fn insert_access_row(
    conn: &Connection,
    row: &MappedMemoryRow,
    access: &MappedAccess,
) -> Result<(), MigrationError> {
    conn.execute(
        "INSERT INTO memory_accesses
            (access_id, memory_id, accessed_at, accessed_in_event, accessor_kind, accessor_detail,
             inclusion_reason, was_used, downstream_outcome_event)
         VALUES (?1, ?2, ?3, ?4, 'daemon', ?5, ?6, ?7, NULL)",
        params![
            access.access_id,
            encode_identity_text(&Identity::Memory(row.memory_id.clone())),
            access.accessed_at,
            access.accessed_in_event,
            migration_actor(row.source_row_id),
            access.inclusion_reason,
            access.was_used,
        ],
    )
    .map_err(|error| constraint("memory_accesses", error))
    .map(|_| ())
}

fn insert_score_row(
    conn: &Connection,
    row: &MappedMemoryRow,
    score: &MappedScore,
) -> Result<(), MigrationError> {
    conn.execute(
        "INSERT INTO memory_scores
            (memory_id, score_kind, value, computed_at, computed_from_window_secs, sample_size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            encode_identity_text(&Identity::Memory(row.memory_id.clone())),
            score.score_kind,
            score.value,
            score.computed_at,
            score.computed_from_window_secs,
            score.sample_size,
        ],
    )
    .map_err(|error| constraint("memory_scores", error))
    .map(|_| ())
}

fn ensure_no_conflict(
    conn: &Connection,
    source_row_id: i64,
    memory_id: &MemoryId,
) -> Result<(), MigrationError> {
    let encoded = encode_identity_text(&Identity::Memory(memory_id.clone()));
    let exists = conn
        .query_row(
            "SELECT 1 FROM memories WHERE memory_id = ?1",
            params![encoded],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
    if exists.is_some() {
        return Err(MigrationError::IdempotenceConflict { source_row_id });
    }
    Ok(())
}

fn mark_progress(
    conn: &Connection,
    source_row_id: i64,
    memory_id: &MemoryId,
) -> Result<(), MigrationError> {
    conn.execute(
        "INSERT OR REPLACE INTO migration_progress
            (source_row_id, dest_memory_id, status, migrated_at, schema_version)
         VALUES (?1, ?2, 'migrated', unixepoch(), ?3)",
        params![
            source_row_id,
            encode_identity_text(&Identity::Memory(memory_id.clone())),
            MIGRATION_SCHEMA_VERSION,
        ],
    )
    .map_err(|error| MigrationError::DestinationUnready(error.to_string()))
    .map(|_| ())
}

fn mark_skipped(conn: &Connection, source_row_id: i64, reason: &str) -> Result<(), MigrationError> {
    conn.execute(
        "INSERT OR REPLACE INTO migration_progress
            (source_row_id, dest_memory_id, status, migrated_at, schema_version)
         VALUES (?1, NULL, 'skipped', unixepoch(), ?2)",
        params![source_row_id, MIGRATION_SCHEMA_VERSION],
    )
    .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
    conn.execute(
        "INSERT OR REPLACE INTO migration_quarantine
            (source_row_id, reason, quarantined_at, schema_version)
         VALUES (?1, ?2, unixepoch(), ?3)",
        params![source_row_id, reason, MIGRATION_SCHEMA_VERSION],
    )
    .map_err(|error| MigrationError::DestinationUnready(error.to_string()))?;
    tracing::warn!(
        source_row_id,
        reason,
        "legacy memory row quarantined during migration"
    );
    Ok(())
}

fn constraint(table: &'static str, error: rusqlite::Error) -> MigrationError {
    error!(table, detail = %error, "memory migration constraint violation");
    MigrationError::ConstraintViolation {
        table,
        detail: error.to_string(),
    }
}

const LEGACY_SELECT_SQL: &str = "SELECT
    rowid, id, session_id, content, memory_type, scope, confidence, linked_symbols,
    linked_files, workspace_id, branch, refresh_key, source_query, assertion_type,
    verification_status, confidence_reason, supersedes_memory_id, superseded_by_memory_id,
    contradicts_memory_ids, contradicted_by_memory_ids, freshness_policy,
    freshness_policy_detail, provenance_json, evidence_json, created_at, last_accessed,
    access_count, is_stale, stale_reason, is_invalidated
 FROM memories
 WHERE rowid > ?1
 ORDER BY rowid ASC
 LIMIT ?2";

const MEMORY_INSERT_SQL: &str = "INSERT INTO memories
    (memory_id, content, class, assertion_type, scope, scope_session_id, scope_branch,
     scope_workspace_id, scope_user_id, scope_org_id, verification_status, confidence,
     confidence_reason, freshness_policy_json, validity_conditions_json,
     invalidation_triggers_json, provenance_event_ids_json, evidence_references_json,
     linked_files_json, linked_symbols_json, linked_docs_json, linked_tests_json,
     linked_memories_json, contradiction_links_json, supersession_links_json,
     access_history_json, last_verified_event_id, last_verified_state, usefulness_score,
     usefulness_score_updated_at, created_at, created_by, updated_at, updated_by,
     superseded_by, schema_version)
 VALUES
    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32,
     ?33, ?34, ?35, ?36)";
