use super::model::{
    Memory, MemoryAccessRecord, MemoryAssertionType, MemoryClass, MemoryEvidence,
    MemoryFreshnessPolicy, MemoryLinkRecord, MemoryProvenance, MemoryScope, MemoryScoreKind,
    MemoryScoreRecord, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};
use super::session_capture::{
    SessionCaptureDeletionResult, SessionCaptureRetentionPolicy, SessionCaptureSelector,
    SessionCaptureSelectorKind,
};
use super::session_digest::{SessionDigest, SessionDigestCandidate};
use crate::error::LatticeError;
use crate::storage::managed_sqlite::ManagedSqlite;
use crate::storage::SecureDir;
use crate::verification::{
    allows as scope_allows, MemoryScopeFilteredEvent, ScopeFilter, ScopeFilterError,
};
use crate::working_memory::{
    load_latest_checkpoint_for_scope, save_checkpoint_for_scope, CheckpointId, CheckpointScope,
    WorkingMemoryState,
};
use crate::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Transaction,
    TransactionBehavior,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MEMORY_DB_BUSY_TIMEOUT_SECS: u64 = 5;
const RECALL_VM_BUDGET_ENV: &str = "LATTICE_MEMORY_RECALL_VM_INSTRUCTIONS";
const DEFAULT_RECALL_VM_INSTRUCTIONS: u64 = 5_000_000;
const MAX_RECALL_VM_INSTRUCTIONS: u64 = 1_000_000_000;
const RECALL_PROGRESS_GRANULARITY: u64 = 1_000;
const MAX_RECALL_QUERY_BYTES: usize = 32 * 1024;
const MAX_RECALL_TERM_BYTES: usize = 512;
const MAX_RECALL_TERM_GROUPS: usize = 64;
const MEMORY_DB_AUTO_CHECKPOINT_PAGES: u32 = 100;
const MEMORY_DB_JOURNAL_SIZE_LIMIT_BYTES: u32 = 1_048_576;
const MAX_MEMORY_SCHEMA_VERSION: i64 = 28;
const MEMORIES_FTS_TABLE: &str = "memories_fts";
pub(crate) const MEMORY_FTS_STATE_TABLE: &str = "memory_fts_state";
pub(crate) const SESSION_DIGEST_DELIVERIES_TABLE: &str = "session_digest_deliveries";
const MAX_CAPTURE_RETIREMENTS_PER_PRUNE: usize = 256;
const MIN_TOMBSTONE_RETIREMENTS_PER_PRUNE: usize = 128;

#[derive(Clone, Copy)]
enum SessionCaptureDeletionMode {
    RetireTransport,
    OperatorDeleteKnowledge,
}
pub(crate) const SESSION_DIGEST_CAPTURE_COMMITS_TABLE: &str = "session_digest_capture_commits";
pub(crate) const SESSION_CAPTURE_TOMBSTONES_TABLE: &str = "session_capture_tombstones";
pub(crate) const SESSION_CAPTURE_TOMBSTONE_PROVENANCE_TABLE: &str =
    "session_capture_tombstone_provenance";
const MAX_TRUSTED_CHECK_OBSERVATIONS_PER_MEMORY: i64 = 64;

struct RecallProgressBudget<'a> {
    conn: &'a Connection,
    interrupted: Arc<AtomicBool>,
}

impl<'a> RecallProgressBudget<'a> {
    fn install(conn: &'a Connection, instruction_budget: u64) -> Self {
        let callbacks = Arc::new(AtomicU64::new(0));
        let interrupted = Arc::new(AtomicBool::new(false));
        let callback_count = Arc::clone(&callbacks);
        let callback_interrupted = Arc::clone(&interrupted);
        let callback_budget = instruction_budget
            .max(1)
            .div_ceil(RECALL_PROGRESS_GRANULARITY);
        conn.progress_handler(
            RECALL_PROGRESS_GRANULARITY as i32,
            Some(move || {
                let exceeded = callback_count
                    .fetch_add(1, AtomicOrdering::Relaxed)
                    .saturating_add(1)
                    >= callback_budget;
                if exceeded {
                    callback_interrupted.store(true, AtomicOrdering::Release);
                }
                exceeded
            }),
        );
        Self { conn, interrupted }
    }

    fn was_interrupted(&self) -> bool {
        self.interrupted.load(AtomicOrdering::Acquire)
    }
}

impl Drop for RecallProgressBudget<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedCheckObservationRecord {
    pub observation_id: Option<i64>,
    pub memory_id: String,
    pub repository_id: String,
    pub checkout_id: String,
    pub check_id: String,
    pub evidence_reference: Option<String>,
    pub passed: bool,
    pub revision: Option<String>,
    pub graph_generation: u64,
    pub source_fingerprint: [u8; 32],
    pub target_digest: [u8; 32],
    pub observed_at: u64,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct VerificationCommitBinding {
    pub repository_id: String,
    pub checkout_id: String,
    pub branch: String,
    pub target_digest: [u8; 32],
    pub observations: Vec<TrustedCheckObservationRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedSessionDigestBatch {
    pub delivery_key: String,
    pub memory_ids: Vec<String>,
    pub candidate_count: usize,
    pub committed_count: usize,
    pub dropped_observation_count: usize,
    pub replayed: bool,
}

struct ExistingSessionDigestDelivery {
    normalized_fingerprint: String,
    candidate_count: usize,
    committed_count: usize,
    dropped_observation_count: usize,
}

struct ExistingSessionDigestCommit {
    candidate_idempotency_key: String,
    candidate_fingerprint: String,
    memory_id: String,
}

/// A schema migration is recorded only after its DDL succeeds.  Individual
/// column migrations make interrupted upgrades resumable: a later open skips
/// columns that already exist and retries only the unapplied migration.
struct MemorySchemaMigration {
    version: i64,
    name: &'static str,
    column: &'static str,
    sql: &'static str,
}

const MEMORY_SCHEMA_MIGRATIONS: &[MemorySchemaMigration] = &[
    MemorySchemaMigration { version: 1, name: "add_session_id", column: "session_id", sql: "ALTER TABLE memories ADD COLUMN session_id TEXT NOT NULL DEFAULT ''" },
    MemorySchemaMigration { version: 2, name: "add_scope", column: "scope", sql: "ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'session'" },
    MemorySchemaMigration { version: 3, name: "add_linked_files", column: "linked_files", sql: "ALTER TABLE memories ADD COLUMN linked_files TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 4, name: "add_workspace_id", column: "workspace_id", sql: "ALTER TABLE memories ADD COLUMN workspace_id TEXT" },
    MemorySchemaMigration { version: 5, name: "add_branch", column: "branch", sql: "ALTER TABLE memories ADD COLUMN branch TEXT" },
    MemorySchemaMigration { version: 6, name: "add_scope_organization_id", column: "scope_organization_id", sql: "ALTER TABLE memories ADD COLUMN scope_organization_id TEXT" },
    MemorySchemaMigration { version: 7, name: "add_refresh_key", column: "refresh_key", sql: "ALTER TABLE memories ADD COLUMN refresh_key TEXT" },
    MemorySchemaMigration { version: 8, name: "add_memory_class", column: "memory_class", sql: "ALTER TABLE memories ADD COLUMN memory_class TEXT NOT NULL DEFAULT 'observation'" },
    MemorySchemaMigration { version: 9, name: "add_assertion_type", column: "assertion_type", sql: "ALTER TABLE memories ADD COLUMN assertion_type TEXT NOT NULL DEFAULT 'observation'" },
    MemorySchemaMigration { version: 10, name: "add_verification_status", column: "verification_status", sql: "ALTER TABLE memories ADD COLUMN verification_status TEXT NOT NULL DEFAULT 'unverified'" },
    MemorySchemaMigration { version: 11, name: "add_confidence_reason", column: "confidence_reason", sql: "ALTER TABLE memories ADD COLUMN confidence_reason TEXT" },
    MemorySchemaMigration { version: 12, name: "add_supersedes_memory_id", column: "supersedes_memory_id", sql: "ALTER TABLE memories ADD COLUMN supersedes_memory_id TEXT" },
    MemorySchemaMigration { version: 13, name: "add_superseded_by_memory_id", column: "superseded_by_memory_id", sql: "ALTER TABLE memories ADD COLUMN superseded_by_memory_id TEXT" },
    MemorySchemaMigration { version: 14, name: "add_contradicts_memory_ids", column: "contradicts_memory_ids", sql: "ALTER TABLE memories ADD COLUMN contradicts_memory_ids TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 15, name: "add_contradicted_by_memory_ids", column: "contradicted_by_memory_ids", sql: "ALTER TABLE memories ADD COLUMN contradicted_by_memory_ids TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 16, name: "add_freshness_policy", column: "freshness_policy", sql: "ALTER TABLE memories ADD COLUMN freshness_policy TEXT NOT NULL DEFAULT 'session_scoped'" },
    MemorySchemaMigration { version: 17, name: "add_freshness_policy_detail", column: "freshness_policy_detail", sql: "ALTER TABLE memories ADD COLUMN freshness_policy_detail TEXT" },
    MemorySchemaMigration { version: 18, name: "add_validity_conditions_json", column: "validity_conditions_json", sql: "ALTER TABLE memories ADD COLUMN validity_conditions_json TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 19, name: "add_invalidation_triggers_json", column: "invalidation_triggers_json", sql: "ALTER TABLE memories ADD COLUMN invalidation_triggers_json TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 20, name: "add_provenance_json", column: "provenance_json", sql: "ALTER TABLE memories ADD COLUMN provenance_json TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 21, name: "add_evidence_json", column: "evidence_json", sql: "ALTER TABLE memories ADD COLUMN evidence_json TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 22, name: "add_linked_docs_json", column: "linked_docs_json", sql: "ALTER TABLE memories ADD COLUMN linked_docs_json TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 23, name: "add_linked_tests_json", column: "linked_tests_json", sql: "ALTER TABLE memories ADD COLUMN linked_tests_json TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 24, name: "add_linked_memories_json", column: "linked_memories_json", sql: "ALTER TABLE memories ADD COLUMN linked_memories_json TEXT NOT NULL DEFAULT '[]'" },
    MemorySchemaMigration { version: 25, name: "add_expires_at", column: "expires_at", sql: "ALTER TABLE memories ADD COLUMN expires_at INTEGER" },
    MemorySchemaMigration { version: 26, name: "add_last_verified_at", column: "last_verified_at", sql: "ALTER TABLE memories ADD COLUMN last_verified_at INTEGER" },
    MemorySchemaMigration { version: 27, name: "add_last_verified_graph_snapshot_id", column: "last_verified_graph_snapshot_id", sql: "ALTER TABLE memories ADD COLUMN last_verified_graph_snapshot_id INTEGER" },
    MemorySchemaMigration { version: 28, name: "add_applicable_checkout_id", column: "applicable_checkout_id", sql: "ALTER TABLE memories ADD COLUMN applicable_checkout_id TEXT" },
];

fn now_unix_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(i64::MAX as u128) as i64
}

/// SQLite-backed store for session memories.
pub struct MemoryStore {
    conn: MemoryConnection,
    availability: MemoryStoreAvailability,
    #[cfg(test)]
    direct_write_count: AtomicUsize,
    #[cfg(test)]
    capture_failure_after_step: AtomicUsize,
    #[cfg(test)]
    fail_verification_commit_once: std::sync::atomic::AtomicBool,
}

enum MemoryConnection {
    Managed(ManagedSqlite),
    Direct(Connection),
}
impl std::ops::Deref for MemoryConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            Self::Managed(c) => c,
            Self::Direct(c) => c,
        }
    }
}
impl std::ops::DerefMut for MemoryConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        match self {
            Self::Managed(c) => c,
            Self::Direct(c) => c,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryStoreFailureKind {
    Busy,
    AccessDenied,
    Full,
    UnsupportedSchema,
    Corrupt,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryStoreAvailability {
    Persistent {
        path: PathBuf,
    },
    InMemory,
    Unavailable {
        path: PathBuf,
        kind: MemoryStoreFailureKind,
        reason: String,
    },
}

impl MemoryStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if path.file_name().is_none() {
            return Err(LatticeError::MemoryStorageAccessDenied(format!(
                "memory database {} has no parent",
                path.display()
            )));
        }
        let leaf = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                LatticeError::MemoryStorageAccessDenied(format!(
                    "memory database {} has no valid file name",
                    path.display()
                ))
            })?;
        let directory = SecureDir::open(parent).map_err(|error| {
            LatticeError::MemoryStorageAccessDenied(format!(
                "cannot pin memory directory {}: {error}",
                parent.display()
            ))
        })?;
        Self::open_in(&directory, leaf)
    }

    pub fn open_in(directory: &SecureDir, leaf: &str) -> Result<Self, LatticeError> {
        let managed = ManagedSqlite::open(
            directory,
            leaf,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|e| classify_sqlite_error("open managed memory database", e))?;
        let conn = MemoryConnection::Managed(managed);

        validate_supported_schema(&conn)?;
        configure_connection(&conn, true)?;

        let store = Self {
            conn,
            availability: MemoryStoreAvailability::Persistent {
                path: directory.path().join(leaf),
            },
            #[cfg(test)]
            direct_write_count: AtomicUsize::new(0),
            #[cfg(test)]
            capture_failure_after_step: AtomicUsize::new(usize::MAX),
            #[cfg(test)]
            fail_verification_commit_once: std::sync::atomic::AtomicBool::new(false),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory()
            .map(MemoryConnection::Direct)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to open in-memory memory database: {}", e))
            })?;

        configure_connection(&conn, false)?;

        let store = Self {
            conn,
            availability: MemoryStoreAvailability::InMemory,
            #[cfg(test)]
            direct_write_count: AtomicUsize::new(0),
            #[cfg(test)]
            capture_failure_after_step: AtomicUsize::new(usize::MAX),
            #[cfg(test)]
            fail_verification_commit_once: std::sync::atomic::AtomicBool::new(false),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Construct an empty queryable store that truthfully rejects every write.
    pub fn unavailable(
        path: &Path,
        kind: MemoryStoreFailureKind,
        reason: impl Into<String>,
    ) -> Result<Self, LatticeError> {
        let mut store = Self::open_in_memory()?;
        store.availability = MemoryStoreAvailability::Unavailable {
            path: path.to_path_buf(),
            kind,
            reason: reason.into(),
        };
        store
            .conn
            .pragma_update(None, "query_only", true)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to protect unavailable memory store: {e}"))
            })?;
        Ok(store)
    }

    pub fn availability(&self) -> &MemoryStoreAvailability {
        &self.availability
    }

    pub fn is_persistent_available(&self) -> bool {
        matches!(
            self.availability,
            MemoryStoreAvailability::Persistent { .. }
        )
    }

    #[cfg(test)]
    pub fn reset_direct_write_count(&self) {
        self.direct_write_count.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn direct_write_count(&self) -> usize {
        self.direct_write_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn set_capture_failure_after_step(&self, step: Option<usize>) {
        self.capture_failure_after_step
            .store(step.unwrap_or(usize::MAX), Ordering::Relaxed);
    }

    #[cfg(not(test))]
    fn record_direct_write(&self) {}

    #[cfg(test)]
    fn record_direct_write(&self) {
        self.direct_write_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn with_connection<T, F>(&self, op: F) -> Result<T, LatticeError>
    where
        F: FnOnce(&Connection) -> Result<T, LatticeError>,
    {
        match op(&self.conn) {
            Err(LatticeError::Storage(_))
                if matches!(
                    self.availability,
                    MemoryStoreAvailability::Unavailable { .. }
                ) =>
            {
                let MemoryStoreAvailability::Unavailable { reason, .. } = &self.availability else {
                    unreachable!()
                };
                Err(LatticeError::MemoryStorageUnavailable(reason.clone()))
            }
            result => result,
        }
    }

    pub fn attempt_memory_delivery(
        &self,
        binding: &super::retention::DeliveryBinding<'_>,
        memory_ids: &[String],
        now: i64,
    ) -> Result<(), LatticeError> {
        super::retention::attempt_delivery(
            &self.conn,
            binding,
            memory_ids,
            u64::try_from(now)
                .map_err(|_| LatticeError::Storage("delivery time is negative".into()))?,
        )
    }

    /// Atomically bind an expansion receipt only if the exact snapshot read by
    /// the caller remains eligible under its current delivery authority.
    pub fn attempt_expansion_memory_delivery(
        &self,
        binding: &super::retention::DeliveryBinding<'_>,
        memory_id: &str,
        expected_digest: [u8; 32],
        repository_id: Option<&str>,
        checkout_id: Option<&str>,
        branch: Option<&str>,
        session_id: &str,
        organization_id: Option<&str>,
        now: i64,
    ) -> Result<(), LatticeError> {
        self.attempt_expansion_memories_delivery(
            binding,
            &[(memory_id.to_string(), expected_digest)],
            repository_id,
            checkout_id,
            branch,
            session_id,
            organization_id,
            now,
        )
    }

    /// Atomically bind one receipt to a bounded group of exact memory
    /// snapshots. Every member must remain eligible and byte-identical to the
    /// projection loaded by the caller; otherwise no receipt or item is stored.
    pub fn attempt_expansion_memories_delivery(
        &self,
        binding: &super::retention::DeliveryBinding<'_>,
        targets: &[(String, [u8; 32])],
        repository_id: Option<&str>,
        checkout_id: Option<&str>,
        branch: Option<&str>,
        session_id: &str,
        organization_id: Option<&str>,
        now: i64,
    ) -> Result<(), LatticeError> {
        if targets.is_empty() || targets.len() > 64 {
            return Err(LatticeError::Storage(
                "memory delivery group must contain between 1 and 64 snapshots".into(),
            ));
        }
        let now = u64::try_from(now)
            .map_err(|_| LatticeError::Storage("delivery time is negative".into()))?;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| LatticeError::Storage(error.to_string()))?;
        for (memory_id, expected_digest) in targets {
            let authorized: bool = if let Some(organization_id) = organization_id {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1 AND is_invalidated=0 AND purge_pending=0 AND retention_stale=0 AND is_stale=0 AND verification_status NOT IN ('stale','contradicted','superseded','expired','invalidated') AND scope='organization' AND scope_organization_id=?2)",
                rusqlite::params![memory_id, organization_id],
                |row| row.get(0),
            )
        } else {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1 AND workspace_id=?2 AND is_invalidated=0 AND purge_pending=0 AND retention_stale=0 AND is_stale=0 AND verification_status NOT IN ('stale','contradicted','superseded','expired','invalidated') AND (applicable_checkout_id IS NULL OR applicable_checkout_id=?3) AND ((scope='repo') OR (scope='branch' AND branch=?4) OR (scope='session' AND session_id=?5)))",
                rusqlite::params![memory_id, repository_id, checkout_id, branch, session_id],
                |row| row.get(0),
            )
        }
        .map_err(|error| LatticeError::Storage(error.to_string()))?;
            if !authorized {
                return Err(LatticeError::Storage(
                    "memory expansion is no longer eligible under the current authority".into(),
                ));
            }
            let memory = self.get_by_id(memory_id)?.ok_or_else(|| {
                LatticeError::Storage("memory expansion target disappeared".into())
            })?;
            let fields = self.get_structured_fields(memory_id)?.ok_or_else(|| {
                LatticeError::Storage("memory expansion metadata disappeared".into())
            })?;
            let digest_repository = memory.workspace_id.as_deref().ok_or_else(|| {
                LatticeError::Storage("memory expansion target has no repository provenance".into())
            })?;
            if Self::expansion_delivery_digest_for(&memory, &fields, digest_repository)?
                != *expected_digest
            {
                return Err(LatticeError::Storage(
                    "memory expansion changed before delivery receipt binding".into(),
                ));
            }
        }
        let ids = targets.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        super::retention::attempt_delivery_in_transaction(&tx, binding, &ids, now)
            .map_err(|error| LatticeError::Storage(error.to_string()))?;
        tx.commit()
            .map_err(|error| LatticeError::Storage(error.to_string()))
    }

    /// Digest every memory and structured field that can affect the public
    /// expansion projection or its trust/lifecycle labels.
    pub fn expansion_delivery_digest_for(
        memory: &Memory,
        fields: &MemoryStructuredFields,
        repository_id: &str,
    ) -> Result<[u8; 32], LatticeError> {
        if memory.workspace_id.as_deref() != Some(repository_id) {
            return Err(LatticeError::Storage(
                "expansion target is outside repository authority".into(),
            ));
        }
        let bytes = serde_json::to_vec(&(memory, fields)).map_err(|error| {
            LatticeError::Storage(format!("serialize expansion delivery target: {error}"))
        })?;
        Ok(Sha256::digest(bytes).into())
    }

    pub fn acknowledge_memory_delivery(
        &self,
        binding: &super::retention::DeliveryBinding<'_>,
        now: i64,
    ) -> Result<usize, LatticeError> {
        super::retention::acknowledge_delivery(
            &self.conn,
            binding,
            u64::try_from(now)
                .map_err(|_| LatticeError::Storage("delivery time is negative".into()))?,
        )
    }

    pub fn memory_delivery_acknowledgement_was_recorded(
        &self,
        binding: &super::retention::DeliveryBinding<'_>,
        now: i64,
    ) -> Result<bool, LatticeError> {
        super::retention::acknowledgement_was_recorded(
            &self.conn,
            binding,
            u64::try_from(now)
                .map_err(|_| LatticeError::Storage("delivery time is negative".into()))?,
        )
    }

    pub fn enqueue_verification_job(
        &self,
        workspace_id: &str,
        memory_id: &str,
    ) -> Result<String, LatticeError> {
        let job_id = format!("verify-job-{}-{}", memory_id, now_unix_micros());
        self.conn
            .execute(
                "INSERT INTO verification_jobs
                    (job_id, workspace_id, target_memory_id, check_kind, status, verdict, reason, queued_at)
                 VALUES (?1, ?2, ?3, ?4, 'queued', NULL, NULL, ?5)",
                params![
                    job_id,
                    workspace_id,
                    memory_id,
                    "existence",
                    now_unix_micros(),
                ],
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to enqueue verification job: {error}"))
            })?;
        Ok(job_id)
    }

    /// Create the memories table if it doesn't already exist.
    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS memories (
                    id              TEXT PRIMARY KEY,
                    session_id      TEXT NOT NULL DEFAULT '',
                    content         TEXT NOT NULL,
                    memory_type     TEXT NOT NULL,
                    scope           TEXT NOT NULL DEFAULT 'session',
                    confidence      REAL NOT NULL DEFAULT 1.0,
                    linked_symbols  TEXT NOT NULL DEFAULT '[]',
                    linked_files    TEXT NOT NULL DEFAULT '[]',
                    workspace_id    TEXT,
                    branch          TEXT,
                    scope_organization_id TEXT,
                    refresh_key     TEXT,
                    source_query    TEXT,
                    memory_class    TEXT NOT NULL DEFAULT 'observation',
                    assertion_type  TEXT NOT NULL DEFAULT 'observation',
                    verification_status TEXT NOT NULL DEFAULT 'unverified',
                    confidence_reason TEXT,
                    supersedes_memory_id TEXT,
                    superseded_by_memory_id TEXT,
                    contradicts_memory_ids TEXT NOT NULL DEFAULT '[]',
                    contradicted_by_memory_ids TEXT NOT NULL DEFAULT '[]',
                    freshness_policy TEXT NOT NULL DEFAULT 'session_scoped',
                    freshness_policy_detail TEXT,
                    validity_conditions_json TEXT NOT NULL DEFAULT '[]',
                    invalidation_triggers_json TEXT NOT NULL DEFAULT '[]',
                    provenance_json TEXT NOT NULL DEFAULT '[]',
                    evidence_json   TEXT NOT NULL DEFAULT '[]',
                    linked_docs_json TEXT NOT NULL DEFAULT '[]',
                    linked_tests_json TEXT NOT NULL DEFAULT '[]',
                    linked_memories_json TEXT NOT NULL DEFAULT '[]',
                    expires_at      INTEGER,
                    created_at      INTEGER NOT NULL,
                    last_accessed   INTEGER NOT NULL,
                    access_count    INTEGER NOT NULL DEFAULT 0,
                    is_stale        INTEGER NOT NULL DEFAULT 0,
                    stale_reason    TEXT,
                    is_invalidated  INTEGER NOT NULL DEFAULT 0,
                    last_verified_at INTEGER,
                    last_verified_graph_snapshot_id INTEGER,
                    applicable_checkout_id TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_memories_created
                    ON memories(created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_memories_type
                    ON memories(memory_type);

                CREATE TABLE IF NOT EXISTS memory_evidence (
                    evidence_id TEXT PRIMARY KEY,
                    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL,
                    reference TEXT,
                    detail TEXT,
                    captured_at INTEGER
                );

                CREATE TABLE IF NOT EXISTS memory_links (
                    link_id TEXT PRIMARY KEY,
                    source_memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    target_memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    link_type TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    verification_status TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS memory_accesses (
                    access_id TEXT PRIMARY KEY,
                    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    accessed_at INTEGER NOT NULL,
                    inclusion_reason TEXT NOT NULL,
                    was_used INTEGER
                );

                CREATE TABLE IF NOT EXISTS memory_attribution_retrievals (
                    retrieval_id TEXT PRIMARY KEY,
                    repository_id TEXT NOT NULL,
                    checkout_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    branch TEXT,
                    tool_event_id TEXT NOT NULL,
                    tool_event_kind TEXT NOT NULL,
                    tool_event_sequence INTEGER NOT NULL,
                    tool_event_observed_at INTEGER NOT NULL,
                    retrieval_event_id TEXT NOT NULL,
                    retrieval_event_kind TEXT NOT NULL,
                    retrieval_event_sequence INTEGER NOT NULL,
                    retrieval_event_observed_at INTEGER NOT NULL,
                    accessor TEXT NOT NULL,
                    retrieved_count INTEGER NOT NULL CHECK(retrieved_count BETWEEN 1 AND 256),
                    metric_client TEXT NOT NULL,
                    metric_channel TEXT NOT NULL,
                    payload_hash BLOB NOT NULL,
                    created_at INTEGER NOT NULL,
                    terminal_event_id TEXT,
                    terminal_event_kind TEXT,
                    terminal_event_sequence INTEGER,
                    terminal_event_observed_at INTEGER,
                    disposition TEXT,
                    cited_access_ids_json TEXT,
                    resolved_at INTEGER,
                    retrieval_metric_recorded INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS memory_attribution_accesses (
                    access_id TEXT PRIMARY KEY
                        REFERENCES memory_accesses(access_id) ON DELETE CASCADE,
                    retrieval_id TEXT NOT NULL
                        REFERENCES memory_attribution_retrievals(retrieval_id) ON DELETE CASCADE,
                    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    inclusion_reason TEXT NOT NULL,
                    metric_recorded INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS memory_attribution_expired (
                    retrieval_id TEXT PRIMARY KEY,
                    payload_hash BLOB NOT NULL,
                    expired_at INTEGER NOT NULL,
                    reason TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_memory_attribution_retrievals_retention
                    ON memory_attribution_retrievals(resolved_at, created_at, retrieval_id);
                CREATE INDEX IF NOT EXISTS idx_memory_attribution_retrievals_pending_metric
                    ON memory_attribution_retrievals(disposition, retrieval_metric_recorded, resolved_at, retrieval_id);
                CREATE INDEX IF NOT EXISTS idx_memory_attribution_retrieval_pending_outbox
                    ON memory_attribution_retrievals(retrieval_id)
                    WHERE retrieval_metric_recorded=0;
                CREATE INDEX IF NOT EXISTS idx_memory_attribution_unresolved_age
                    ON memory_attribution_retrievals(created_at,retrieval_id)
                    WHERE disposition IS NULL;
                CREATE INDEX IF NOT EXISTS idx_memory_attribution_accesses_retrieval
                    ON memory_attribution_accesses(retrieval_id, access_id);
                CREATE INDEX IF NOT EXISTS idx_memory_attribution_accesses_pending
                    ON memory_attribution_accesses(metric_recorded, retrieval_id, access_id);
                CREATE INDEX IF NOT EXISTS idx_memory_attribution_expired_time
                    ON memory_attribution_expired(expired_at, retrieval_id);
                CREATE TRIGGER IF NOT EXISTS memory_attribution_access_cleanup
                AFTER DELETE ON memory_attribution_accesses BEGIN
                    INSERT OR IGNORE INTO memory_attribution_expired(retrieval_id,payload_hash,expired_at,reason)
                    SELECT retrieval_id,payload_hash,unixepoch(),'memory_purged'
                    FROM memory_attribution_retrievals
                    WHERE retrieval_id=OLD.retrieval_id
                      AND NOT EXISTS (SELECT 1 FROM memory_attribution_accesses WHERE retrieval_id=OLD.retrieval_id);
                    DELETE FROM memory_attribution_retrievals
                    WHERE retrieval_id=OLD.retrieval_id
                      AND NOT EXISTS (SELECT 1 FROM memory_attribution_accesses WHERE retrieval_id=OLD.retrieval_id);
                END;

                CREATE TABLE IF NOT EXISTS memory_scores (
                    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    score_kind TEXT NOT NULL,
                    value REAL NOT NULL,
                    computed_at INTEGER NOT NULL,
                    computed_from_window_secs INTEGER NOT NULL,
                    sample_size INTEGER NOT NULL,
                    PRIMARY KEY (memory_id, score_kind, computed_at)
                );

                CREATE TABLE IF NOT EXISTS memory_scope_filter_events (
                    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    memory_id TEXT NOT NULL,
                    attempted_workspace_id TEXT NOT NULL,
                    attempted_branch TEXT,
                    memory_scope TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS trusted_check_observations (
                    observation_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                    repository_id TEXT NOT NULL,
                    checkout_id TEXT NOT NULL,
                    check_id TEXT NOT NULL,
                    evidence_reference TEXT,
                    passed INTEGER NOT NULL CHECK(passed IN (0,1)),
                    revision TEXT,
                    graph_generation INTEGER NOT NULL CHECK(graph_generation >= 0),
                    source_fingerprint BLOB NOT NULL CHECK(length(source_fingerprint)=32),
                    target_digest BLOB NOT NULL CHECK(length(target_digest)=32),
                    observed_at INTEGER NOT NULL CHECK(observed_at >= 0),
                    exit_code INTEGER
                );
                CREATE INDEX IF NOT EXISTS trusted_check_observations_target
                    ON trusted_check_observations(memory_id, observed_at DESC, observation_id DESC);

                CREATE TABLE IF NOT EXISTS session_digest_deliveries (
                    delivery_key TEXT PRIMARY KEY,
                    repository_id TEXT NOT NULL,
                    checkout_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    branch TEXT,
                    revision TEXT NOT NULL,
                    segment INTEGER NOT NULL CHECK (segment >= 0),
                    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
                    payload_hash TEXT NOT NULL,
                    extractor_version TEXT NOT NULL,
                    normalized_fingerprint TEXT NOT NULL,
                    candidate_count INTEGER NOT NULL CHECK (candidate_count >= 0),
                    committed_count INTEGER NOT NULL CHECK (
                        committed_count >= 0 AND committed_count <= candidate_count
                    ),
                    dropped_observation_count INTEGER NOT NULL CHECK (dropped_observation_count >= 0),
                    created_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS session_digest_capture_commits (
                    delivery_key TEXT NOT NULL
                        REFERENCES session_digest_deliveries(delivery_key) ON DELETE CASCADE,
                    candidate_ordinal INTEGER NOT NULL CHECK (candidate_ordinal >= 0),
                    candidate_idempotency_key TEXT NOT NULL UNIQUE,
                    candidate_fingerprint TEXT NOT NULL,
                    memory_id TEXT NOT NULL UNIQUE
                        REFERENCES memories(id) ON DELETE RESTRICT,
                    PRIMARY KEY (delivery_key, candidate_ordinal)
                );

                CREATE TABLE IF NOT EXISTS session_capture_tombstones (
                    delivery_key TEXT PRIMARY KEY,
                    repository_id TEXT NOT NULL,
                    deleted_at INTEGER NOT NULL,
                    deletion_reason TEXT NOT NULL CHECK (
                        deletion_reason IN ('retention', 'operator')
                    )
                );

                CREATE TABLE IF NOT EXISTS session_capture_tombstone_provenance (
                    delivery_key TEXT NOT NULL
                        REFERENCES session_capture_tombstones(delivery_key) ON DELETE RESTRICT,
                    derived_kind TEXT NOT NULL CHECK (derived_kind IN ('memory', 'proposal')),
                    derived_id TEXT NOT NULL,
                    source_memory_id TEXT NOT NULL,
                    PRIMARY KEY (delivery_key, derived_kind, derived_id, source_memory_id)
                );",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to initialize memory schema: {}", e))
            })?;

        if !self.table_column_exists("trusted_check_observations", "target_digest")? {
            self.conn
                .execute_batch(
                    "ALTER TABLE trusted_check_observations ADD COLUMN target_digest BLOB",
                )
                .map_err(|e| {
                    LatticeError::Storage(format!(
                        "Failed to migrate trusted observation target binding: {e}"
                    ))
                })?;
        }

        self.initialize_migration_table()?;
        self.apply_schema_migrations()?;

        self.conn
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_memories_created
                    ON memories(created_at DESC);
                 CREATE INDEX IF NOT EXISTS idx_memories_type
                    ON memories(memory_type);
                 CREATE INDEX IF NOT EXISTS idx_memories_session
                    ON memories(session_id);
                 CREATE INDEX IF NOT EXISTS idx_memories_verification_status
                    ON memories(verification_status);
                 CREATE INDEX IF NOT EXISTS idx_memories_expires_at
                    ON memories(expires_at);
                 CREATE INDEX IF NOT EXISTS idx_memories_last_verified_graph_snapshot
                    ON memories(last_verified_graph_snapshot_id);
                 CREATE INDEX IF NOT EXISTS idx_memories_superseded_by
                    ON memories(superseded_by_memory_id);
                 CREATE INDEX IF NOT EXISTS idx_memory_evidence_kind_reference
                    ON memory_evidence(kind, reference);
                 CREATE INDEX IF NOT EXISTS idx_memory_evidence_memory
                    ON memory_evidence(memory_id);
                 CREATE INDEX IF NOT EXISTS idx_memory_links_source
                    ON memory_links(source_memory_id);
                 CREATE INDEX IF NOT EXISTS idx_memory_links_target
                    ON memory_links(target_memory_id);
                 CREATE INDEX IF NOT EXISTS idx_memory_accesses_memory_time
                    ON memory_accesses(memory_id, accessed_at DESC);
                 CREATE INDEX IF NOT EXISTS idx_memory_scores_memory_kind
                    ON memory_scores(memory_id, score_kind, computed_at DESC);
                 CREATE INDEX IF NOT EXISTS idx_memories_applicable_checkout
                    ON memories(applicable_checkout_id);
                 CREATE INDEX IF NOT EXISTS idx_session_digest_deliveries_session
                    ON session_digest_deliveries(repository_id, checkout_id, session_id, segment);
                 CREATE INDEX IF NOT EXISTS idx_session_digest_consolidation_scan
                    ON session_digest_deliveries(repository_id,checkout_id,COALESCE(branch,'unknown'),created_at,delivery_key);
                 CREATE INDEX IF NOT EXISTS idx_session_digest_deliveries_retention
                    ON session_digest_deliveries(repository_id, created_at DESC, delivery_key DESC);
                 CREATE INDEX IF NOT EXISTS idx_session_digest_capture_memory
                    ON session_digest_capture_commits(memory_id);
                 CREATE INDEX IF NOT EXISTS idx_session_capture_tombstones_repository_time
                    ON session_capture_tombstones(repository_id, deleted_at, delivery_key);
                 CREATE INDEX IF NOT EXISTS idx_session_capture_tombstones_retention_time
                    ON session_capture_tombstones(deletion_reason, deleted_at, delivery_key);
                 CREATE INDEX IF NOT EXISTS idx_session_capture_tombstone_derived
                    ON session_capture_tombstone_provenance(derived_kind, derived_id);",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to initialize memory indexes: {}", e))
            })?;

        self.conn
            .execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                    memory_id UNINDEXED,
                    content,
                    linked_symbols,
                    linked_files,
                    tokenize = 'unicode61 remove_diacritics 2'
                );",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to initialize memory FTS5 schema: {}", e))
            })?;

        self.initialize_fts_state()?;
        self.rebuild_fts_if_dirty()?;
        crate::working_memory::initialize_schema(&self.conn).map_err(|e| {
            LatticeError::Storage(format!("Failed to initialize working memory schema: {}", e))
        })?;
        crate::consolidation::initialize_schema(&self.conn)?;
        crate::verification::initialize_schema(&self.conn)?;
        super::retention::initialize(&self.conn, now_epoch_secs())?;
        super::retrieval::initialize(&self.conn)?;

        Ok(())
    }

    pub fn record_trusted_check_observation(
        &self,
        record: &TrustedCheckObservationRecord,
    ) -> Result<(), LatticeError> {
        let generation = i64::try_from(record.graph_generation).map_err(|_| {
            LatticeError::Storage("trusted check graph generation exceeds SQLite range".into())
        })?;
        let observed_at = i64::try_from(record.observed_at).map_err(|_| {
            LatticeError::Storage("trusted check timestamp exceeds SQLite range".into())
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(
            |e| LatticeError::Storage(format!("Failed to begin trusted observation write: {e}")),
        )?;
        let scoped: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1 AND workspace_id=?2)",
                params![record.memory_id, record.repository_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to validate trusted observation target: {e}"
                ))
            })?;
        if !scoped {
            return Err(LatticeError::Storage(
                "trusted check target is absent or outside repository authority".into(),
            ));
        }
        if self.verification_target_digest(&record.memory_id, &record.repository_id)?
            != record.target_digest
        {
            return Err(LatticeError::Storage(
                "trusted check target changed before observation commit".into(),
            ));
        }
        tx.execute(
            "INSERT INTO trusted_check_observations(memory_id,repository_id,checkout_id,check_id,evidence_reference,passed,revision,graph_generation,source_fingerprint,target_digest,observed_at,exit_code) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![record.memory_id,record.repository_id,record.checkout_id,record.check_id,record.evidence_reference,record.passed as i64,record.revision,generation,record.source_fingerprint.as_slice(),record.target_digest.as_slice(),observed_at,record.exit_code],
        ).map_err(|e| LatticeError::Storage(format!("Failed to persist trusted check observation: {e}")))?;
        tx.execute(
            "DELETE FROM trusted_check_observations WHERE memory_id=?1 AND observation_id NOT IN (SELECT observation_id FROM trusted_check_observations WHERE memory_id=?1 ORDER BY observed_at DESC,observation_id DESC LIMIT ?2)",
            params![record.memory_id, MAX_TRUSTED_CHECK_OBSERVATIONS_PER_MEMORY],
        ).map_err(|e| LatticeError::Storage(format!("Failed to bound trusted check observations: {e}")))?;
        tx.commit().map_err(|e| {
            LatticeError::Storage(format!("Failed to commit trusted check observation: {e}"))
        })
    }

    fn table_column_exists(&self, table: &str, column: &str) -> Result<bool, LatticeError> {
        let mut statement = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|e| LatticeError::Storage(format!("inspect {table} schema: {e}")))?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| LatticeError::Storage(format!("inspect {table} columns: {e}")))?;
        for name in names {
            if name.map_err(|e| LatticeError::Storage(e.to_string()))? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn trusted_check_observations(
        &self,
        memory_id: &str,
    ) -> Result<Vec<TrustedCheckObservationRecord>, LatticeError> {
        let mut statement = self.conn.prepare(
            "SELECT observation_id,memory_id,repository_id,checkout_id,check_id,evidence_reference,passed,revision,graph_generation,source_fingerprint,target_digest,observed_at,exit_code FROM trusted_check_observations WHERE memory_id=?1 ORDER BY observed_at DESC,observation_id DESC LIMIT ?2",
        ).map_err(|e| LatticeError::Storage(format!("Failed to prepare trusted observation read: {e}")))?;
        let rows = statement
            .query_map(
                params![memory_id, MAX_TRUSTED_CHECK_OBSERVATIONS_PER_MEMORY],
                |row| {
                    let fingerprint: Vec<u8> = row.get(9)?;
                    let fingerprint: [u8; 32] = fingerprint.try_into().map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            9,
                            "source_fingerprint".into(),
                            rusqlite::types::Type::Blob,
                        )
                    })?;
                    let target: Vec<u8> = row.get(10)?;
                    let target_digest: [u8; 32] = target.try_into().map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            10,
                            "target_digest".into(),
                            rusqlite::types::Type::Blob,
                        )
                    })?;
                    Ok(TrustedCheckObservationRecord {
                        observation_id: Some(row.get(0)?),
                        memory_id: row.get(1)?,
                        repository_id: row.get(2)?,
                        checkout_id: row.get(3)?,
                        check_id: row.get(4)?,
                        evidence_reference: row.get(5)?,
                        passed: row.get::<_, i64>(6)? != 0,
                        revision: row.get(7)?,
                        graph_generation: row.get::<_, i64>(8)? as u64,
                        source_fingerprint: fingerprint,
                        target_digest,
                        observed_at: row.get::<_, i64>(11)? as u64,
                        exit_code: row.get(12)?,
                    })
                },
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to query trusted observations: {e}"))
            })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| LatticeError::Storage(format!("Failed to read trusted observations: {e}")))
    }

    pub fn verification_target_digest(
        &self,
        memory_id: &str,
        repository_id: &str,
    ) -> Result<[u8; 32], LatticeError> {
        self.conn
            .execute_batch("SAVEPOINT verification_target_digest")
            .map_err(|e| LatticeError::Storage(e.to_string()))?;
        let result = (|| {
            let memory = self
                .get_by_id(memory_id)?
                .filter(|memory| memory.workspace_id.as_deref() == Some(repository_id))
                .ok_or_else(|| {
                    LatticeError::Storage(
                        "verification target is absent or outside repository authority".into(),
                    )
                })?;
            let fields = self.get_structured_fields(memory_id)?.ok_or_else(|| {
                LatticeError::Storage("verification target is invalidated".into())
            })?;
            Self::verification_target_digest_for(&memory, &fields, repository_id)
        })();
        match result {
            Ok(digest) => {
                self.conn
                    .execute_batch("RELEASE verification_target_digest")
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
                Ok(digest)
            }
            Err(error) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO verification_target_digest; RELEASE verification_target_digest",
                );
                Err(error)
            }
        }
    }

    /// Digest the exact target snapshot supplied to a verifier. Callers use
    /// this rather than re-reading the row after loading it, then the final
    /// transactional CAS compares the digest with current database state.
    pub fn verification_target_digest_for(
        memory: &Memory,
        fields: &MemoryStructuredFields,
        repository_id: &str,
    ) -> Result<[u8; 32], LatticeError> {
        if memory.workspace_id.as_deref() != Some(repository_id) {
            return Err(LatticeError::Storage(
                "verification target is outside repository authority".into(),
            ));
        }
        let semantic = serde_json::json!({
            "session_id": memory.session_id, "content": memory.content, "memory_type": memory.memory_type,
            "scope": memory.scope, "confidence": memory.confidence,
            "linked_symbols": memory.linked_symbols, "linked_files": memory.linked_files,
            "workspace_id": memory.workspace_id, "branch": memory.branch,
            "organization_id": memory.scope_organization_id, "refresh_key": memory.refresh_key,
            "source_query": memory.source_query, "memory_class": fields.memory_class,
            "assertion_type": fields.assertion_type, "confidence_reason": fields.confidence_reason,
            "supersedes": fields.supersedes_memory_id, "superseded_by": fields.superseded_by_memory_id,
            "contradicts": fields.contradicts_memory_ids, "contradicted_by": fields.contradicted_by_memory_ids,
            "freshness_policy": fields.freshness_policy, "freshness_detail": fields.freshness_policy_detail,
            "validity_conditions": fields.validity_conditions, "invalidation_triggers": fields.invalidation_triggers,
            "provenance": fields.provenance, "evidence": fields.evidence, "linked_docs": fields.linked_docs,
            "linked_tests": fields.linked_tests, "linked_memories": fields.linked_memories,
        });
        let bytes = serde_json::to_vec(&semantic)
            .map_err(|e| LatticeError::Storage(format!("serialize verification target: {e}")))?;
        Ok(Sha256::digest(bytes).into())
    }

    pub fn persist_verification_result(
        &self,
        id: &str,
        fields: &MemoryStructuredFields,
        verification_status: MemoryVerificationStatus,
        is_stale: bool,
        stale_reason: Option<&str>,
        last_verified_at: u64,
        graph_snapshot_id: Option<u64>,
        expected: &VerificationCommitBinding,
    ) -> Result<(), LatticeError> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| LatticeError::Storage(format!("begin verification result: {e}")))?;
        let result = (|| {
            let authorized: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1 AND workspace_id=?2 AND is_invalidated=0 AND (applicable_checkout_id IS NULL OR applicable_checkout_id=?3) AND (scope!='branch' OR branch=?4))",
                params![id, expected.repository_id, expected.checkout_id, expected.branch], |row| row.get(0),
            ).map_err(|e| LatticeError::Storage(format!("verify verification authority: {e}")))?;
            if !authorized {
                return Err(LatticeError::Storage(
                    "verification target is outside repository, checkout, or branch authority"
                        .into(),
                ));
            }
            let digest = self.verification_target_digest(id, &expected.repository_id)?;
            if digest != expected.target_digest {
                return Err(LatticeError::Storage(
                    "verification target changed before commit".into(),
                ));
            }
            if self.trusted_check_observations(id)? != expected.observations {
                return Err(LatticeError::Storage(
                    "trusted observations changed before commit".into(),
                ));
            }
            self.persist_structured_fields(id, fields)?;
            let verified_at = i64::try_from(last_verified_at).map_err(|_| {
                LatticeError::Storage("verification timestamp exceeds SQLite range".into())
            })?;
            let snapshot = graph_snapshot_id
                .map(i64::try_from)
                .transpose()
                .map_err(|_| LatticeError::Storage("graph snapshot exceeds SQLite range".into()))?;
            let changed = self.conn.execute("UPDATE memories SET verification_status=?1,is_stale=?2,stale_reason=?3,last_verified_at=?4,last_verified_graph_snapshot_id=?5 WHERE id=?6 AND is_invalidated=0", params![verification_status.as_str(),is_stale as i64,stale_reason,verified_at,snapshot,id]).map_err(|e| LatticeError::Storage(format!("update verification state: {e}")))?;
            if changed != 1 {
                return Err(LatticeError::Storage(format!(
                    "Memory '{id}' not found or invalidated"
                )));
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                #[cfg(test)]
                let commit_result = if self
                    .fail_verification_commit_once
                    .swap(false, Ordering::SeqCst)
                {
                    Err(rusqlite::Error::ExecuteReturnedResults)
                } else {
                    self.conn.execute_batch("COMMIT")
                };
                #[cfg(not(test))]
                let commit_result = self.conn.execute_batch("COMMIT");
                if let Err(commit_error) = commit_result {
                    let rollback_error = self.conn.execute_batch("ROLLBACK").err();
                    let detail = rollback_error.map_or_else(String::new, |error| {
                        format!("; rollback after commit failure also failed: {error}")
                    });
                    return Err(LatticeError::Storage(format!(
                        "commit verification result: {commit_error}{detail}"
                    )));
                }
                self.record_direct_write();
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Store a memory. If the memory's id is empty, a UUID-like id is generated.
    /// If created_at is 0, the current timestamp is used.
    /// Returns the id of the stored memory.
    pub fn store(&self, mut memory: Memory) -> Result<String, LatticeError> {
        if let MemoryStoreAvailability::Unavailable { reason, .. } = &self.availability {
            return Err(LatticeError::MemoryStorageUnavailable(reason.clone()));
        }
        self.record_direct_write();
        if memory.id.is_empty() {
            memory.id = generate_id();
        }

        if memory.created_at == 0 {
            memory.created_at = now_epoch_secs();
        }

        if memory.last_accessed == 0 {
            memory.last_accessed = memory.created_at;
        }

        let linked_json = serde_json::to_string(&memory.linked_symbols).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize linked_symbols: {}", e))
        })?;
        let linked_files_json = serde_json::to_string(&memory.linked_files).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize linked_files: {}", e))
        })?;
        let structured_fields = self.resolve_structured_fields_for_store(&memory)?;
        let contradicts_json = serde_json::to_string(&structured_fields.contradicts_memory_ids)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize contradicts_memory_ids: {}", e))
            })?;
        let contradicted_by_json =
            serde_json::to_string(&structured_fields.contradicted_by_memory_ids).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to serialize contradicted_by_memory_ids: {}",
                    e
                ))
            })?;
        let provenance_json =
            serde_json::to_string(&structured_fields.provenance).map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize provenance_json: {}", e))
            })?;
        let evidence_json = serde_json::to_string(&structured_fields.evidence).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize evidence_json: {}", e))
        })?;
        let validity_conditions_json =
            serde_json::to_string(&structured_fields.validity_conditions).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to serialize validity_conditions_json: {}",
                    e
                ))
            })?;
        let invalidation_triggers_json =
            serde_json::to_string(&structured_fields.invalidation_triggers).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to serialize invalidation_triggers_json: {}",
                    e
                ))
            })?;
        let linked_docs_json =
            serde_json::to_string(&structured_fields.linked_docs).map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize linked_docs_json: {}", e))
            })?;
        let linked_tests_json =
            serde_json::to_string(&structured_fields.linked_tests).map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize linked_tests_json: {}", e))
            })?;
        let linked_memories_json = serde_json::to_string(&structured_fields.linked_memories)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize linked_memories_json: {}", e))
            })?;
        let metadata = self.load_existing_verification_metadata(&memory.id)?;

        // Set recovery state before mutating the source of truth. If this
        // process stops between the memory write and its FTS update, the next
        // open rebuilds rather than serving a stale derived index.
        self.set_fts_dirty(true)?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO memories
                    (id, session_id, content, memory_type, scope, confidence, linked_symbols, linked_files,
                     workspace_id, branch, scope_organization_id, refresh_key, source_query,
                     memory_class, assertion_type, verification_status, confidence_reason, supersedes_memory_id,
                     superseded_by_memory_id, contradicts_memory_ids, contradicted_by_memory_ids,
                     freshness_policy, freshness_policy_detail, validity_conditions_json, invalidation_triggers_json,
                     provenance_json, evidence_json, linked_docs_json, linked_tests_json, linked_memories_json, expires_at,
                     created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated,
                     applicable_checkout_id)
                 VALUES
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                      ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
                      ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, 0, ?37)",
                params![
                    memory.id,
                    memory.session_id,
                    memory.content,
                    memory.memory_type.as_str(),
                    memory.scope.as_str(),
                    memory.confidence,
                    linked_json,
                    linked_files_json,
                    memory.workspace_id,
                    memory.branch,
                    memory.scope_organization_id,
                    memory.refresh_key,
                    memory.source_query,
                    structured_fields.memory_class.as_str(),
                    structured_fields.assertion_type.as_str(),
                    structured_fields.verification_status.as_str(),
                    structured_fields.confidence_reason,
                    structured_fields.supersedes_memory_id,
                    structured_fields.superseded_by_memory_id,
                    contradicts_json,
                    contradicted_by_json,
                    structured_fields.freshness_policy.as_str(),
                    structured_fields.freshness_policy_detail,
                    validity_conditions_json,
                    invalidation_triggers_json,
                    provenance_json,
                    evidence_json,
                    linked_docs_json,
                    linked_tests_json,
                    linked_memories_json,
                    metadata.expires_at.map(|value| value.unix_seconds()),
                    memory.created_at as i64,
                    memory.last_accessed as i64,
                    memory.access_count as i64,
                    memory.is_stale as i32,
                    memory.stale_reason,
                    metadata.applicable_checkout_id,
                ],
            )
            .map_err(|e| classify_sqlite_error("store memory", e))?;

        self.sync_memory_evidence(&memory.id, &structured_fields.evidence)?;

        self.upsert_fts_row(&memory)?;

        Ok(memory.id)
    }

    /// Atomically persist one already-normalized automatic session digest.
    ///
    /// This primitive is crate-private so only the authority-bound router can
    /// reach it. The transaction owns the journal row, memory rows, structured
    /// metadata, normalized evidence, FTS documents, and capture-commit rows.
    pub(crate) fn persist_session_digest_candidate_batch(
        &self,
        digest: &SessionDigest,
        candidates: &[SessionDigestCandidate],
        extractor_version: &str,
    ) -> Result<PersistedSessionDigestBatch, LatticeError> {
        let received_at = digest.received_at.unix_seconds();
        let now = now_epoch_secs() as i64;
        if received_at > now
            || received_at < now.saturating_sub(super::retention::MAX_REPLAY_AGE_SECS as i64)
        {
            return Err(LatticeError::Storage(
                "session digest is outside the permitted replay window".into(),
            ));
        }
        let checkout_id = digest.checkout_id.as_deref().ok_or_else(|| {
            LatticeError::Storage(
                "automatic session capture requires exact checkout applicability".to_string(),
            )
        })?;
        let delivery_key = session_digest_delivery_key(digest);
        let candidate_fingerprints = candidates
            .iter()
            .map(session_digest_candidate_fingerprint)
            .collect::<Result<Vec<_>, _>>()?;
        let normalized_fingerprint =
            session_digest_batch_fingerprint(digest, extractor_version, &candidate_fingerprints)?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(
            |error| {
                LatticeError::Storage(format!(
                    "Failed to begin automatic session capture transaction: {error}"
                ))
            },
        )?;

        let was_deleted = tx
            .query_row(
                &format!(
                    "SELECT 1 FROM {SESSION_CAPTURE_TOMBSTONES_TABLE}
                     WHERE delivery_key = ?1 AND repository_id = ?2"
                ),
                params![delivery_key, digest.repository_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to inspect automatic capture deletion tombstone: {error}"
                ))
            })?
            .is_some();
        if was_deleted {
            return Err(LatticeError::Storage(
                "automatic session capture delivery was deleted and cannot be replayed".to_string(),
            ));
        }

        if let Some(existing) = load_session_digest_delivery(&tx, &delivery_key)? {
            if existing.normalized_fingerprint != normalized_fingerprint
                || existing.candidate_count != candidates.len()
                || existing.committed_count != candidates.len()
                || existing.dropped_observation_count != digest.dropped_observation_count
            {
                return Err(LatticeError::Storage(
                    "automatic session capture delivery key was replayed with different normalized content"
                        .to_string(),
                ));
            }
            let commits = load_session_digest_commits(&tx, &delivery_key)?;
            if commits.len() != candidates.len()
                || commits
                    .iter()
                    .zip(candidates.iter().zip(&candidate_fingerprints))
                    .any(|(commit, (candidate, fingerprint))| {
                        commit.candidate_idempotency_key != candidate.idempotency_key
                            || commit.candidate_fingerprint != *fingerprint
                    })
            {
                return Err(LatticeError::Storage(
                    "automatic session capture replay does not match its committed candidate set"
                        .to_string(),
                ));
            }
            return Ok(PersistedSessionDigestBatch {
                delivery_key,
                memory_ids: commits.into_iter().map(|commit| commit.memory_id).collect(),
                candidate_count: existing.candidate_count,
                committed_count: existing.committed_count,
                dropped_observation_count: existing.dropped_observation_count,
                replayed: true,
            });
        }

        for (candidate, fingerprint) in candidates.iter().zip(&candidate_fingerprints) {
            if let Some(existing) =
                load_session_digest_commit_by_candidate(&tx, &candidate.idempotency_key)?
            {
                let reason = if existing.candidate_fingerprint == *fingerprint {
                    "automatic session capture candidate key is already bound to another delivery"
                } else {
                    "automatic session capture candidate key was reused for different normalized content"
                };
                return Err(LatticeError::Storage(reason.to_string()));
            }
        }

        tx.execute(
            &format!(
                "INSERT INTO {SESSION_DIGEST_DELIVERIES_TABLE}
                    (delivery_key, repository_id, checkout_id, session_id, branch, revision,
                     segment, schema_version, payload_hash, extractor_version,
                     normalized_fingerprint, candidate_count, committed_count,
                     dropped_observation_count, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13, ?14)"
            ),
            params![
                delivery_key,
                digest.repository_id,
                checkout_id,
                digest.session_id,
                digest.branch,
                digest.revision,
                digest.segment as i64,
                digest.schema_version as i64,
                digest.payload_hash,
                extractor_version,
                normalized_fingerprint,
                candidates.len() as i64,
                digest.dropped_observation_count as i64,
                digest.received_at.unix_seconds(),
            ],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to journal automatic session capture delivery: {error}"
            ))
        })?;
        self.inject_capture_failure(1)?;

        tx.execute(
            &format!("UPDATE {MEMORY_FTS_STATE_TABLE} SET is_dirty = 1 WHERE singleton = 1"),
            [],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to enter automatic session capture FTS recovery state: {error}"
            ))
        })?;

        let mut memory_ids = Vec::with_capacity(candidates.len());
        for (ordinal, (candidate, fingerprint)) in
            candidates.iter().zip(&candidate_fingerprints).enumerate()
        {
            let memory_id = session_digest_memory_id(&candidate.idempotency_key);
            insert_session_digest_memory(
                &tx,
                digest,
                candidate,
                &memory_id,
                checkout_id,
                extractor_version,
            )?;
            self.inject_capture_failure(2)?;
            insert_session_digest_evidence(&tx, digest, candidate, &memory_id)?;
            self.inject_capture_failure(3)?;
            insert_session_digest_fts(&tx, candidate, &memory_id)?;
            self.inject_capture_failure(4)?;
            tx.execute(
                &format!(
                    "INSERT INTO {SESSION_DIGEST_CAPTURE_COMMITS_TABLE}
                        (delivery_key, candidate_ordinal, candidate_idempotency_key,
                         candidate_fingerprint, memory_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)"
                ),
                params![
                    delivery_key,
                    ordinal as i64,
                    candidate.idempotency_key,
                    fingerprint,
                    memory_id,
                ],
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to commit automatic session capture candidate: {error}"
                ))
            })?;
            memory_ids.push(memory_id);
        }
        self.inject_capture_failure(5)?;

        tx.execute(
            &format!(
                "UPDATE {SESSION_DIGEST_DELIVERIES_TABLE}
                 SET committed_count = ?1
                 WHERE delivery_key = ?2"
            ),
            params![candidates.len() as i64, delivery_key],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to reduce automatic session capture result: {error}"
            ))
        })?;
        tx.execute(
            &format!("UPDATE {MEMORY_FTS_STATE_TABLE} SET is_dirty = 0 WHERE singleton = 1"),
            [],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to finalize automatic session capture FTS state: {error}"
            ))
        })?;
        self.inject_capture_failure(6)?;

        tx.commit().map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to commit automatic session capture transaction: {error}"
            ))
        })?;

        Ok(PersistedSessionDigestBatch {
            delivery_key,
            memory_ids,
            candidate_count: candidates.len(),
            committed_count: candidates.len(),
            dropped_observation_count: digest.dropped_observation_count,
            replayed: false,
        })
    }

    pub(crate) fn prune_session_captures(
        &self,
        repository_id: &str,
        policy: SessionCaptureRetentionPolicy,
        now: i64,
    ) -> Result<SessionCaptureDeletionResult, LatticeError> {
        // Reserve half of the shared work budget for expired replay fences so
        // a sustained delivery backlog cannot starve tombstone retirement.
        let retired_tombstones =
            self.retire_expired_capture_tombstones(now, MIN_TOMBSTONE_RETIREMENTS_PER_PRUNE)?;
        let delivery_budget = MAX_CAPTURE_RETIREMENTS_PER_PRUNE - retired_tombstones;
        let max_age_secs = policy.max_age().as_secs().min(i64::MAX as u64) as i64;
        let cutoff = now.saturating_sub(max_age_secs);
        let max_captures = i64::try_from(policy.max_captures()).unwrap_or(i64::MAX);
        let mut statement = self
            .conn
            .prepare(&format!(
                "WITH excess_boundary AS (
                     SELECT created_at, delivery_key
                     FROM {SESSION_DIGEST_DELIVERIES_TABLE}
                     WHERE repository_id = ?1
                     ORDER BY created_at DESC, delivery_key DESC
                     LIMIT 1 OFFSET ?3
                 )
                 SELECT delivery_key
                 FROM {SESSION_DIGEST_DELIVERIES_TABLE} AS delivery
                 WHERE repository_id = ?1 AND (
                     created_at < ?2 OR EXISTS (
                         SELECT 1 FROM excess_boundary
                         WHERE (delivery.created_at, delivery.delivery_key)
                             <= (excess_boundary.created_at, excess_boundary.delivery_key)
                     )
                 )
                 ORDER BY created_at ASC, delivery_key ASC
                 LIMIT ?4"
            ))
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to prepare automatic capture retention query: {error}"
                ))
            })?;
        let delivery_keys = statement
            .query_map(
                params![
                    repository_id,
                    cutoff,
                    max_captures,
                    i64::try_from(delivery_budget).unwrap_or(i64::MAX)
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to query automatic capture retention candidates: {error}"
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to decode automatic capture retention candidate: {error}"
                ))
            })?;
        drop(statement);
        let result = self.delete_session_capture_deliveries(
            repository_id,
            &delivery_keys,
            now,
            "retention",
            SessionCaptureDeletionMode::RetireTransport,
        )?;
        let remaining = MAX_CAPTURE_RETIREMENTS_PER_PRUNE
            .saturating_sub(retired_tombstones + result.deleted_capture_ids.len());
        if remaining > 0 {
            self.retire_expired_capture_tombstones(now, remaining)?;
        }
        Ok(result)
    }

    pub(crate) fn delete_session_captures(
        &self,
        repository_id: &str,
        selector: &SessionCaptureSelector,
        deleted_at: i64,
    ) -> Result<SessionCaptureDeletionResult, LatticeError> {
        let (predicate, selector_value) = match selector.kind() {
            SessionCaptureSelectorKind::Session => ("session_id", selector.opaque_id()),
            SessionCaptureSelectorKind::Capture => ("delivery_key", selector.opaque_id()),
        };
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT delivery_key
                 FROM {SESSION_DIGEST_DELIVERIES_TABLE}
                 WHERE repository_id = ?1 AND {predicate} = ?2
                 ORDER BY created_at ASC, delivery_key ASC"
            ))
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to prepare automatic capture deletion query: {error}"
                ))
            })?;
        let delivery_keys = statement
            .query_map(params![repository_id, selector_value], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to query automatic capture deletion candidates: {error}"
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to decode automatic capture deletion candidate: {error}"
                ))
            })?;
        drop(statement);
        self.delete_session_capture_deliveries(
            repository_id,
            &delivery_keys,
            deleted_at,
            "operator",
            SessionCaptureDeletionMode::OperatorDeleteKnowledge,
        )
    }

    fn delete_session_capture_deliveries(
        &self,
        repository_id: &str,
        delivery_keys: &[String],
        deleted_at: i64,
        deletion_reason: &str,
        mode: SessionCaptureDeletionMode,
    ) -> Result<SessionCaptureDeletionResult, LatticeError> {
        if delivery_keys.is_empty() {
            return Ok(SessionCaptureDeletionResult::default());
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(
            |error| {
                LatticeError::Storage(format!(
                    "Failed to begin automatic capture deletion transaction: {error}"
                ))
            },
        )?;

        let mut source_delivery_by_memory = HashMap::<String, String>::new();
        for delivery_key in delivery_keys {
            let owning_repository = tx
                .query_row(
                    &format!(
                        "SELECT repository_id FROM {SESSION_DIGEST_DELIVERIES_TABLE}
                         WHERE delivery_key = ?1"
                    ),
                    params![delivery_key],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to verify automatic capture deletion authority: {error}"
                    ))
                })?;
            if owning_repository.as_deref() != Some(repository_id) {
                return Err(LatticeError::Storage(
                    "automatic capture deletion crossed repository-store authority".to_string(),
                ));
            }
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO {SESSION_CAPTURE_TOMBSTONES_TABLE}
                        (delivery_key, repository_id, deleted_at, deletion_reason)
                     VALUES (?1, ?2, ?3, ?4)"
                ),
                params![delivery_key, repository_id, deleted_at, deletion_reason],
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to retain automatic capture tombstone: {error}"
                ))
            })?;
            let mut commits = tx
                .prepare(&format!(
                    "SELECT memory_id FROM {SESSION_DIGEST_CAPTURE_COMMITS_TABLE}
                     WHERE delivery_key = ?1 ORDER BY candidate_ordinal ASC"
                ))
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to prepare automatic capture dependency query: {error}"
                    ))
                })?;
            let memory_ids = commits
                .query_map(params![delivery_key], |row| row.get::<_, String>(0))
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to query automatic capture dependencies: {error}"
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to decode automatic capture dependency: {error}"
                    ))
                })?;
            for memory_id in memory_ids {
                source_delivery_by_memory.insert(memory_id, delivery_key.clone());
            }
        }

        let source_memory_ids = source_delivery_by_memory
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let (retained_derived_memory_count, retained_proposal_count) = match mode {
            SessionCaptureDeletionMode::RetireTransport => (0, 0),
            SessionCaptureDeletionMode::OperatorDeleteKnowledge => {
                let derived = retain_derived_memory_provenance(
                    &tx,
                    &source_delivery_by_memory,
                    &source_memory_ids,
                    deleted_at,
                )?;
                let proposals = retain_proposal_tombstone_provenance(
                    &tx,
                    &source_delivery_by_memory,
                    &source_memory_ids,
                )?;
                tx.execute(
                    &format!(
                        "UPDATE {MEMORY_FTS_STATE_TABLE} SET is_dirty = 1 WHERE singleton = 1"
                    ),
                    [],
                )
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to enter automatic capture deletion FTS recovery state: {error}"
                    ))
                })?;
                for memory_id in source_delivery_by_memory.keys() {
                    tx.execute(
                        "DELETE FROM verification_jobs WHERE target_memory_id = ?1",
                        params![memory_id],
                    )
                    .map_err(|error| {
                        LatticeError::Storage(format!(
                            "Failed to delete automatic capture verification jobs: {error}"
                        ))
                    })?;
                    tx.execute("DELETE FROM memory_links WHERE source_memory_id = ?1 OR target_memory_id = ?1", params![memory_id]).map_err(|error| LatticeError::Storage(format!("Failed to delete automatic capture memory links: {error}")))?;
                    tx.execute(
                        &format!("DELETE FROM {MEMORIES_FTS_TABLE} WHERE memory_id = ?1"),
                        params![memory_id],
                    )
                    .map_err(|error| {
                        LatticeError::Storage(format!(
                            "Failed to delete automatic capture FTS document: {error}"
                        ))
                    })?;
                }
                (derived, proposals)
            }
        };
        for delivery_key in delivery_keys {
            tx.execute(
                &format!(
                    "DELETE FROM {SESSION_DIGEST_CAPTURE_COMMITS_TABLE} WHERE delivery_key = ?1"
                ),
                params![delivery_key],
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to delete automatic capture commit journal: {error}"
                ))
            })?;
            tx.execute(
                &format!(
                    "DELETE FROM {SESSION_DIGEST_DELIVERIES_TABLE}
                     WHERE delivery_key = ?1 AND repository_id = ?2"
                ),
                params![delivery_key, repository_id],
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to delete automatic capture delivery journal: {error}"
                ))
            })?;
        }
        if matches!(mode, SessionCaptureDeletionMode::OperatorDeleteKnowledge) {
            for memory_id in source_delivery_by_memory.keys() {
                tx.execute("DELETE FROM memories WHERE id = ?1", params![memory_id])
                    .map_err(|error| {
                        LatticeError::Storage(format!(
                            "Failed to delete automatic capture memory: {error}"
                        ))
                    })?;
            }
            tx.execute(
                &format!("UPDATE {MEMORY_FTS_STATE_TABLE} SET is_dirty = 0 WHERE singleton = 1"),
                [],
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to finalize automatic capture deletion FTS state: {error}"
                ))
            })?;
        }
        tx.commit().map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to commit automatic capture deletion transaction: {error}"
            ))
        })?;

        Ok(SessionCaptureDeletionResult {
            deleted_capture_ids: delivery_keys.to_vec(),
            deleted_memory_count: if matches!(
                mode,
                SessionCaptureDeletionMode::OperatorDeleteKnowledge
            ) {
                source_delivery_by_memory.len()
            } else {
                0
            },
            retained_derived_memory_count,
            retained_proposal_count,
        })
    }

    fn retire_expired_capture_tombstones(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<usize, LatticeError> {
        if limit == 0 {
            return Ok(0);
        }
        let replay_cutoff = now.saturating_sub(super::retention::MAX_REPLAY_AGE_SECS as i64);
        let retired = self
            .conn
            .execute(
                &format!(
                    "DELETE FROM {SESSION_CAPTURE_TOMBSTONES_TABLE}
                    WHERE delivery_key IN (
                        SELECT delivery_key FROM {SESSION_CAPTURE_TOMBSTONES_TABLE}
                        WHERE deletion_reason = 'retention' AND deleted_at < ?1
                        ORDER BY deleted_at, delivery_key
                        LIMIT ?2
                    )"
                ),
                params![replay_cutoff, i64::try_from(limit).unwrap_or(i64::MAX)],
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to retire expired capture replay tombstones: {error}"
                ))
            })?;
        Ok(retired)
    }

    #[cfg(not(test))]
    fn inject_capture_failure(&self, _step: usize) -> Result<(), LatticeError> {
        Ok(())
    }

    #[cfg(test)]
    fn inject_capture_failure(&self, step: usize) -> Result<(), LatticeError> {
        if self.capture_failure_after_step.load(Ordering::Relaxed) == step {
            return Err(LatticeError::Storage(
                "injected automatic session capture failure".to_string(),
            ));
        }
        Ok(())
    }

    pub fn save_working_memory_checkpoint_for_scope(
        &self,
        state: &WorkingMemoryState,
        name: &str,
        scope: &CheckpointScope,
    ) -> Result<CheckpointId, LatticeError> {
        save_checkpoint_for_scope(state, name, scope, &self.conn)
    }

    pub fn load_latest_working_memory_checkpoint(
        &self,
        scope: &CheckpointScope,
    ) -> Result<Option<(CheckpointId, WorkingMemoryState)>, LatticeError> {
        load_latest_checkpoint_for_scope(scope, &self.conn)
    }

    /// Scope-enforced memory query. Callers must pass an explicit scope filter.
    pub fn query(
        &self,
        keyword: Option<&str>,
        limit: usize,
        scope: &ScopeFilter,
    ) -> Result<Vec<Memory>, LatticeError> {
        self.query_with_applicable_checkout(keyword, limit, scope, None)
    }

    /// Apply authority, lifecycle and navigation exclusion before candidate limits.
    pub fn recall_candidates(
        &self,
        keyword: Option<&str>,
        limit: usize,
        scope: &ScopeFilter,
        checkout_id: Option<&str>,
        options: super::retrieval::RecallOptions,
    ) -> Result<Vec<Memory>, LatticeError> {
        let instruction_budget = recall_vm_instruction_budget()?;
        self.recall_candidates_with_instruction_budget(
            keyword,
            limit,
            scope,
            checkout_id,
            options,
            instruction_budget,
        )
    }

    pub(super) fn recall_candidates_with_instruction_budget(
        &self,
        keyword: Option<&str>,
        limit: usize,
        scope: &ScopeFilter,
        checkout_id: Option<&str>,
        options: super::retrieval::RecallOptions,
        instruction_budget: u64,
    ) -> Result<Vec<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let keyword = keyword.unwrap_or_default();
        if keyword.len() > MAX_RECALL_QUERY_BYTES {
            return Err(LatticeError::Storage(format!(
                "memory recall query is {} UTF-8 bytes; the maximum is {MAX_RECALL_QUERY_BYTES}",
                keyword.len()
            )));
        }
        if let Some(term) = keyword
            .split_whitespace()
            .find(|term| term.len() > MAX_RECALL_TERM_BYTES)
        {
            return Err(LatticeError::Storage(format!(
                "memory recall term is {} UTF-8 bytes; the maximum is {MAX_RECALL_TERM_BYTES}",
                term.len()
            )));
        }
        let predicate = scope_sql_predicate(scope, checkout_id);
        let mut values = Vec::new();
        let mut candidates = Vec::new();
        let mut seen_terms = HashSet::new();
        let terms: Vec<String> = keyword
            .split_whitespace()
            .map(|term| {
                term.trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | ',' | ';' | '(' | ')'))
                    .replace('\\', "/")
                    .to_lowercase()
            })
            .filter(|term| recall_exact_term_is_usable(term))
            .filter(|term| seen_terms.insert(term.clone()))
            .take(MAX_RECALL_TERM_GROUPS)
            .collect();
        if !terms.is_empty() {
            let placeholders = std::iter::repeat("?")
                .take(terms.len())
                .collect::<Vec<_>>()
                .join(",");
            for (table, column, rank) in [
                ("memory_retrieval_paths", "path", 0),
                ("memory_retrieval_symbols", "symbol", 1),
                ("memory_retrieval_failures", "failure", 1),
            ] {
                candidates.push(format!(
                    "SELECT memory_id,{rank} AS rank,0 AS lexical_matches FROM {table} WHERE {column} IN ({placeholders})"
                ));
                values.extend(terms.iter().cloned().map(Value::Text));
            }
        }
        // Recall accepts natural-language prompts.  Search each sanitized lexical
        // group separately so one unrelated word cannot hide a relevant lesson;
        // the grouped query below ranks broader coverage before confidence.
        let mut seen_fts_groups = HashSet::new();
        let fts_groups: Vec<String> = keyword
            .split_whitespace()
            .filter_map(build_fts_group)
            .filter(|group| seen_fts_groups.insert(group.clone()))
            .take(MAX_RECALL_TERM_GROUPS)
            .collect();
        for query in fts_groups {
            candidates.push(format!(
                "SELECT memory_id,2 AS rank,1 AS lexical_matches FROM {MEMORIES_FTS_TABLE} WHERE {MEMORIES_FTS_TABLE} MATCH ?"
            ));
            values.push(Value::Text(query));
        }
        if candidates.is_empty() {
            if !keyword.trim().is_empty() {
                return Ok(Vec::new());
            }
            candidates
                .push("SELECT id AS memory_id,3 AS rank,0 AS lexical_matches FROM memories".into());
        }
        values.extend(predicate.bind_values);
        values.push(Value::Integer(i64::from(options.include_retention_stale)));
        values.push(Value::Integer(limit.min(4096) as i64));
        let sql=format!("WITH candidates AS ({})
            SELECT memories.id,session_id,content,memory_type,scope,confidence,linked_symbols,
                   linked_files,workspace_id,branch,scope_organization_id,refresh_key,source_query,
                   created_at,last_accessed,access_count,is_stale,stale_reason,verification_status
            FROM memories INNER JOIN candidates ON candidates.memory_id=memories.id
            WHERE is_invalidated=0 AND {} AND (?=1 OR retention_stale=0)
              AND is_stale=0 AND verification_status NOT IN ('stale','contradicted','superseded','expired','invalidated')
              AND (refresh_key IS NULL OR (refresh_key != 'repo_playbook' AND refresh_key NOT LIKE 'subsystem_playbook::%'))
            GROUP BY memories.id
            ORDER BY min(candidates.rank),
                     CASE WHEN min(candidates.rank)=2
                          THEN sum(candidates.lexical_matches)
                          ELSE 0 END DESC,
                     confidence DESC,created_at DESC,memories.id LIMIT ?",
            candidates.join(" UNION ALL "),predicate.where_clause);
        let progress = RecallProgressBudget::install(&self.conn, instruction_budget);
        let query = self.query_memories_values(&sql, values, "indexed recall candidates");
        let was_interrupted = progress.was_interrupted();
        drop(progress);
        let rows = match query {
            Ok(rows) => rows,
            Err(_) if was_interrupted => {
                return Err(LatticeError::Storage(format!(
                    "indexed recall exceeded the bounded SQLite work allowance ({instruction_budget} virtual-machine instructions); narrow the query or reduce broad high-frequency terms"
                )))
            }
            Err(error) => return Err(error),
        };
        self.enforce_scope_boundary_with_checkout(
            rows,
            scope,
            checkout_id,
            "indexed recall candidates",
        )
    }

    fn query_with_applicable_checkout(
        &self,
        keyword: Option<&str>,
        limit: usize,
        scope: &ScopeFilter,
        checkout_id: Option<&str>,
    ) -> Result<Vec<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let predicate = scope_sql_predicate(scope, checkout_id);
        let mut bind_values = predicate.bind_values;
        let sql = if let Some(fts_query) = build_fts_query(keyword.unwrap_or_default()) {
            let sql = format!(
                "SELECT memories.id, memories.session_id, memories.content, memories.memory_type,
                        memories.scope, memories.confidence, memories.linked_symbols,
                        memories.linked_files, memories.workspace_id, memories.branch,
                        memories.scope_organization_id, memories.refresh_key, memories.source_query, memories.created_at,
                        memories.last_accessed, memories.access_count, memories.is_stale,
                        memories.stale_reason, memories.verification_status
                 FROM memories
                 INNER JOIN {table}
                    ON {table}.memory_id = memories.id
                 WHERE memories.is_invalidated = 0 AND memories.retention_stale=0
                   AND (refresh_key IS NULL OR (refresh_key != 'repo_playbook' AND refresh_key NOT LIKE 'subsystem_playbook::%'))
                   AND {scope_predicate}
                   AND {table} MATCH ?
                 ORDER BY bm25({table}), memories.created_at DESC
                 LIMIT ?",
                table = MEMORIES_FTS_TABLE,
                scope_predicate = predicate.where_clause,
            );
            bind_values.push(Value::Text(fts_query));
            bind_values.push(Value::Integer(limit as i64));
            sql
        } else {
            let sql = format!(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE is_invalidated = 0 AND retention_stale=0
                   AND (refresh_key IS NULL OR (refresh_key != 'repo_playbook' AND refresh_key NOT LIKE 'subsystem_playbook::%'))
                   AND {}
                 ORDER BY created_at DESC
                 LIMIT ?",
                predicate.where_clause,
            );
            bind_values.push(Value::Integer(limit as i64));
            sql
        };

        let memories = self.query_memories_values(&sql, bind_values, "scoped memory query")?;
        self.enforce_scope_boundary_with_checkout(
            memories,
            scope,
            checkout_id,
            "scoped memory query",
        )
    }

    pub fn list_all_scoped(&self, scope: &ScopeFilter) -> Result<Vec<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let memories = self.list_all()?;
        self.filter_scope_boundary(memories, scope, None)
    }

    pub fn get_by_id_scoped(
        &self,
        id: &str,
        scope: &ScopeFilter,
    ) -> Result<Option<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let Some(memory) = self.get_by_id(id)? else {
            return Ok(None);
        };
        if scope_allows(&memory, scope) && self.applicable_checkout_id(id)?.is_none() {
            return Ok(Some(memory));
        }
        self.record_scope_filtered(&memory, scope)?;
        Ok(None)
    }

    /// Read a memory through both its visibility scope and its captured
    /// checkout applicability. Callers must supply a trusted checkout ID.
    pub fn get_by_id_scoped_for_checkout(
        &self,
        id: &str,
        scope: &ScopeFilter,
        checkout_id: &str,
    ) -> Result<Option<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let Some(memory) = self.get_by_id(id)? else {
            return Ok(None);
        };
        if scope_allows(&memory, scope)
            && self.checkout_applicability_allows(id, Some(checkout_id))?
        {
            return Ok(Some(memory));
        }
        self.record_scope_filtered(&memory, scope)?;
        Ok(None)
    }

    pub fn search_by_keyword_scoped(
        &self,
        keyword: &str,
        scope: &ScopeFilter,
    ) -> Result<Vec<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let memories = self.search_by_keyword(keyword)?;
        self.filter_scope_boundary(memories, scope, None)
    }

    /// Explicit unscoped path for migrations, snapshots, and legacy admin flows.
    pub fn query_unscoped_admin(
        &self,
        keyword: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>, LatticeError> {
        if let Some(fts_query) = build_fts_query(keyword.unwrap_or_default()) {
            let sql = format!(
                "SELECT memories.id, memories.session_id, memories.content, memories.memory_type,
                        memories.scope, memories.confidence, memories.linked_symbols,
                        memories.linked_files, memories.workspace_id, memories.branch,
                        memories.scope_organization_id, memories.refresh_key, memories.source_query, memories.created_at,
                        memories.last_accessed, memories.access_count, memories.is_stale,
                        memories.stale_reason, memories.verification_status
                 FROM memories
                 INNER JOIN {table}
                    ON {table}.memory_id = memories.id
                 WHERE memories.is_invalidated = 0
                   AND {table} MATCH ?1
                 ORDER BY bm25({table}), memories.created_at DESC
                 LIMIT ?2",
                table = MEMORIES_FTS_TABLE,
            );
            return self.query_memories(&sql, params![fts_query, limit as i64], "admin query");
        }

        self.query_memories(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
             FROM memories
             WHERE is_invalidated = 0
             ORDER BY created_at DESC
             LIMIT ?1",
            params![limit as i64],
            "admin query",
        )
    }

    /// List all non-invalidated memories, ordered by created_at DESC.
    pub fn list_all(&self) -> Result<Vec<Memory>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE is_invalidated = 0
                 ORDER BY created_at DESC",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare list query: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    content: row.get(2)?,
                    memory_type_str: row.get(3)?,
                    scope_str: row.get(4)?,
                    confidence: row.get(5)?,
                    linked_json: row.get(6)?,
                    linked_files_json: row.get(7)?,
                    workspace_id: row.get(8)?,
                    branch: row.get(9)?,
                    scope_organization_id: row.get(10)?,
                    refresh_key: row.get(11)?,
                    source_query: row.get(12)?,
                    created_at: row.get(13)?,
                    last_accessed: row.get(14)?,
                    access_count: row.get(15)?,
                    is_stale: row.get(16)?,
                    stale_reason: row.get(17)?,
                    verification_status: Some(MemoryVerificationStatus::from_str(
                        &row.get::<_, String>(18)?,
                    )),
                })
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query memories: {}", e)))?;

        let mut memories = Vec::new();
        for row in rows {
            let r = row
                .map_err(|e| LatticeError::Storage(format!("Failed to read memory row: {}", e)))?;
            memories.push(r.into_memory());
        }

        Ok(memories)
    }

    /// Retrieve a single non-invalidated memory by id.
    pub fn get_by_id(&self, id: &str) -> Result<Option<Memory>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE id = ?1 AND is_invalidated = 0
                 LIMIT 1",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare get-by-id query: {}", e))
            })?;

        let row = stmt
            .query_row(params![id], |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    content: row.get(2)?,
                    memory_type_str: row.get(3)?,
                    scope_str: row.get(4)?,
                    confidence: row.get(5)?,
                    linked_json: row.get(6)?,
                    linked_files_json: row.get(7)?,
                    workspace_id: row.get(8)?,
                    branch: row.get(9)?,
                    scope_organization_id: row.get(10)?,
                    refresh_key: row.get(11)?,
                    source_query: row.get(12)?,
                    created_at: row.get(13)?,
                    last_accessed: row.get(14)?,
                    access_count: row.get(15)?,
                    is_stale: row.get(16)?,
                    stale_reason: row.get(17)?,
                    verification_status: Some(MemoryVerificationStatus::from_str(
                        &row.get::<_, String>(18)?,
                    )),
                })
            })
            .optional()
            .map_err(|e| LatticeError::Storage(format!("Failed to load memory: {}", e)))?;

        Ok(row.map(MemoryRow::into_memory))
    }

    /// Retrieve the most recent non-invalidated memory by refresh key, optionally scoped
    /// to a workspace and branch.
    pub fn find_by_refresh_key(
        &self,
        refresh_key: &str,
        workspace_id: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Option<Memory>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE is_invalidated = 0
                   AND refresh_key = ?1
                   AND (?2 IS NULL OR workspace_id = ?2)
                   AND (?3 IS NULL OR branch = ?3)
                 ORDER BY
                    CASE verification_status
                        WHEN 'verified' THEN 60
                        WHEN 'in_review' THEN 50
                        WHEN 'unverified' THEN 40
                        WHEN 'superseded' THEN 20
                        WHEN 'stale' THEN 10
                        WHEN 'contradicted' THEN 0
                        ELSE 30
                    END DESC,
                    CASE WHEN superseded_by_memory_id IS NULL THEN 1 ELSE 0 END DESC,
                    CASE WHEN is_stale = 0 THEN 1 ELSE 0 END DESC,
                    CASE scope
                        WHEN 'repo' THEN 2
                        WHEN 'branch' THEN 1
                        ELSE 0
                    END DESC,
                    CASE assertion_type
                        WHEN 'workflow_outcome' THEN 6
                        WHEN 'constraint' THEN 5
                        WHEN 'pattern' THEN 4
                        WHEN 'decision' THEN 4
                        WHEN 'anti_pattern' THEN 3
                        WHEN 'observation' THEN 2
                        WHEN 'exploration' THEN 1
                        ELSE 2
                    END DESC,
                    confidence DESC,
                    created_at DESC
                 LIMIT 1",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare refresh-key lookup query: {}", e))
            })?;

        let row = stmt
            .query_row(params![refresh_key, workspace_id, branch], |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    content: row.get(2)?,
                    memory_type_str: row.get(3)?,
                    scope_str: row.get(4)?,
                    confidence: row.get(5)?,
                    linked_json: row.get(6)?,
                    linked_files_json: row.get(7)?,
                    workspace_id: row.get(8)?,
                    branch: row.get(9)?,
                    scope_organization_id: row.get(10)?,
                    refresh_key: row.get(11)?,
                    source_query: row.get(12)?,
                    created_at: row.get(13)?,
                    last_accessed: row.get(14)?,
                    access_count: row.get(15)?,
                    is_stale: row.get(16)?,
                    stale_reason: row.get(17)?,
                    verification_status: Some(MemoryVerificationStatus::from_str(
                        &row.get::<_, String>(18)?,
                    )),
                })
            })
            .optional()
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to load memory by refresh key '{}': {}",
                    refresh_key, e
                ))
            })?;

        Ok(row.map(MemoryRow::into_memory))
    }

    /// Return structured assertion metadata for a single non-invalidated memory.
    pub fn get_structured_fields(
        &self,
        id: &str,
    ) -> Result<Option<MemoryStructuredFields>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT memory_class, assertion_type, verification_status, confidence_reason,
                        supersedes_memory_id, superseded_by_memory_id,
                        contradicts_memory_ids, contradicted_by_memory_ids,
                        freshness_policy, freshness_policy_detail,
                        validity_conditions_json, invalidation_triggers_json,
                        provenance_json, evidence_json, linked_docs_json, linked_tests_json,
                        linked_memories_json
                 FROM memories
                 WHERE id = ?1 AND is_invalidated = 0
                 LIMIT 1",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare structured-fields query: {}", e))
            })?;

        let row = stmt
            .query_row(params![id], structured_row_from_row)
            .optional()
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to load structured memory fields: {}", e))
            })?;

        Ok(row.map(StructuredMemoryRow::into_structured_fields))
    }

    pub fn get_last_verified_at(&self, id: &str) -> Result<Option<u64>, LatticeError> {
        let value = self
            .conn
            .query_row(
                "SELECT last_verified_at
                 FROM memories
                 WHERE id = ?1 AND is_invalidated = 0
                 LIMIT 1",
                params![id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(|e| LatticeError::Storage(format!("Failed to load last_verified_at: {}", e)))?
            .flatten();
        Ok(value.map(|timestamp| timestamp.max(0) as u64))
    }

    pub fn get_last_verified_graph_snapshot_id(
        &self,
        id: &str,
    ) -> Result<Option<u64>, LatticeError> {
        let value = self
            .conn
            .query_row(
                "SELECT last_verified_graph_snapshot_id
                 FROM memories
                 WHERE id = ?1 AND is_invalidated = 0
                 LIMIT 1",
                params![id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to load last_verified_graph_snapshot_id: {}",
                    e
                ))
            })?
            .flatten();
        Ok(value.map(|snapshot_id| snapshot_id.max(0) as u64))
    }

    pub fn get_expires_at(&self, id: &str) -> Result<Option<DateTime<Utc>>, LatticeError> {
        let value = self
            .conn
            .query_row(
                "SELECT expires_at
                 FROM memories
                 WHERE id = ?1 AND is_invalidated = 0
                 LIMIT 1",
                params![id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(|e| LatticeError::Storage(format!("Failed to load expires_at: {}", e)))?
            .flatten();
        Ok(value.map(DateTime::from_unix_seconds))
    }

    pub fn set_last_verified_at(
        &self,
        id: &str,
        last_verified_at: u64,
    ) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET last_verified_at = ?1
                 WHERE id = ?2 AND is_invalidated = 0",
                params![last_verified_at as i64, id],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to update last_verified_at: {}", e))
            })?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    pub fn set_last_verified_graph_snapshot_id(
        &self,
        id: &str,
        snapshot_id: u64,
    ) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET last_verified_graph_snapshot_id = ?1
                 WHERE id = ?2 AND is_invalidated = 0",
                params![snapshot_id as i64, id],
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to update last_verified_graph_snapshot_id: {}",
                    e
                ))
            })?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    pub fn set_expires_at(&self, id: &str, expires_at: DateTime<Utc>) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET expires_at = ?1
                 WHERE id = ?2 AND is_invalidated = 0",
                params![expires_at.unix_seconds(), id],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to update expires_at: {}", e)))?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    pub fn clear_last_verified_at(&self, id: &str) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET last_verified_at = NULL
                 WHERE id = ?1 AND is_invalidated = 0",
                params![id],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to clear last_verified_at: {}", e))
            })?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    pub fn clear_last_verified_graph_snapshot_id(&self, id: &str) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET last_verified_graph_snapshot_id = NULL
                 WHERE id = ?1 AND is_invalidated = 0",
                params![id],
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to clear last_verified_graph_snapshot_id: {}",
                    e
                ))
            })?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    pub fn clear_expires_at(&self, id: &str) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET expires_at = NULL
                 WHERE id = ?1 AND is_invalidated = 0",
                params![id],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to clear expires_at: {}", e)))?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    pub fn list_workspace_memories(&self, workspace_id: &str) -> Result<Vec<Memory>, LatticeError> {
        self.query_memories(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
             FROM memories
             WHERE is_invalidated = 0
               AND workspace_id = ?1
             ORDER BY created_at DESC",
            params![workspace_id],
            "workspace memories",
        )
    }

    pub fn list_applicable_workspace_memories(
        &self,
        repository_id: &str,
        checkout_id: &str,
    ) -> Result<Vec<Memory>, LatticeError> {
        self.query_memories(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
             FROM memories
             WHERE is_invalidated = 0
               AND workspace_id = ?1
               AND (applicable_checkout_id IS NULL OR applicable_checkout_id = ?2)
             ORDER BY created_at DESC",
            params![repository_id, checkout_id],
            "checkout-applicable workspace memories",
        )
    }

    pub fn list_memories_expired_before(
        &self,
        workspace_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<Memory>, LatticeError> {
        self.query_memories(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
             FROM memories
             WHERE is_invalidated = 0
               AND workspace_id = ?1
               AND expires_at IS NOT NULL
               AND expires_at <= ?2
               AND verification_status != 'expired'
             ORDER BY created_at DESC",
            params![workspace_id, now.unix_seconds()],
            "expired memories",
        )
    }

    pub fn find_impacted_memory_ids_for_graph_delta(
        &self,
        workspace_id: &str,
        changed_files: &[String],
        changed_symbols: &[String],
    ) -> Result<Vec<String>, LatticeError> {
        self.find_impacted_memory_ids_impl(workspace_id, changed_files, changed_symbols, true)
    }

    pub fn count_memory_evidence_rows_for_graph_delta(
        &self,
        workspace_id: &str,
        changed_files: &[String],
        changed_symbols: &[String],
    ) -> Result<usize, LatticeError> {
        Ok(self
            .find_impacted_memory_ids_impl(workspace_id, changed_files, changed_symbols, false)?
            .len())
    }

    pub fn scope_filter_events(&self) -> Result<Vec<MemoryScopeFilteredEvent>, LatticeError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT memory_id, attempted_workspace_id, attempted_branch, memory_scope
                 FROM memory_scope_filter_events
                 ORDER BY event_id ASC",
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to prepare memory scope filter event query: {}",
                    e
                ))
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok(MemoryScopeFilteredEvent {
                    memory_id: row.get(0)?,
                    attempted_workspace_id: row.get(1)?,
                    attempted_branch: row.get(2)?,
                    memory_scope: MemoryScope::from_str(&row.get::<_, String>(3)?),
                })
            })
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to query memory scope filter events: {}", e))
            })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to read memory scope filter event row: {}",
                e
            ))
        })
    }

    pub fn record_memory_access(
        &self,
        memory_id: &str,
        access_id: &str,
        accessed_at: u64,
        inclusion_reason: &str,
        was_used: Option<bool>,
    ) -> Result<(), LatticeError> {
        let journal_owned: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_attribution_accesses WHERE access_id=?1)",
                [access_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to inspect attribution-owned memory access: {e}"
                ))
            })?;
        if journal_owned {
            return Err(LatticeError::Storage(format!(
                "Memory access `{access_id}` is owned by the attribution journal and cannot be replaced"
            )));
        }
        self.conn
            .execute(
                "INSERT OR REPLACE INTO memory_accesses
                    (access_id, memory_id, accessed_at, inclusion_reason, was_used)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    access_id,
                    memory_id,
                    accessed_at as i64,
                    inclusion_reason,
                    was_used.map(|value| if value { 1 } else { 0 }),
                ],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to record memory access: {}", e)))?;
        Ok(())
    }

    pub fn count_memory_accesses_since(
        &self,
        memory_id: &str,
        cutoff: u64,
    ) -> Result<u64, LatticeError> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*)
                 FROM memory_accesses
                 WHERE memory_id = ?1
                   AND accessed_at >= ?2",
                params![memory_id, cutoff as i64],
                |row| row.get(0),
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to count memory accesses: {}", e))
            })?;
        Ok(count.max(0) as u64)
    }

    pub fn list_memory_accesses(
        &self,
        memory_id: &str,
    ) -> Result<Vec<MemoryAccessRecord>, LatticeError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT access_id, accessed_at, inclusion_reason, was_used
                 FROM memory_accesses
                 WHERE memory_id = ?1
                 ORDER BY accessed_at DESC, access_id DESC",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare memory access query: {}", e))
            })?;
        let rows = statement
            .query_map(params![memory_id], |row| {
                Ok(MemoryAccessRecord {
                    access_id: row.get(0)?,
                    accessed_at: row.get::<_, i64>(1)?.max(0) as u64,
                    inclusion_reason: row.get(2)?,
                    was_used: row.get::<_, Option<i64>>(3)?.map(|value| value != 0),
                })
            })
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to execute memory access query: {}", e))
            })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LatticeError::Storage(format!("Failed to read memory access row: {}", e)))
    }

    pub fn write_memory_score(
        &self,
        memory_id: &str,
        score: &MemoryScoreRecord,
    ) -> Result<(), LatticeError> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO memory_scores
                    (memory_id, score_kind, value, computed_at, computed_from_window_secs, sample_size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    memory_id,
                    score.score_kind.as_str(),
                    score.value,
                    score.computed_at as i64,
                    score.computed_from_window_secs as i64,
                    score.sample_size as i64,
                ],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to write memory score: {}", e)))?;
        Ok(())
    }

    pub fn latest_memory_score(
        &self,
        memory_id: &str,
        score_kind: MemoryScoreKind,
    ) -> Result<Option<MemoryScoreRecord>, LatticeError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT score_kind, value, computed_at, computed_from_window_secs, sample_size
                 FROM memory_scores
                 WHERE memory_id = ?1
                   AND score_kind = ?2
                 ORDER BY computed_at DESC
                 LIMIT 1",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare memory score query: {}", e))
            })?;
        let score = statement
            .query_row(params![memory_id, score_kind.as_str()], |row| {
                Ok(MemoryScoreRecord {
                    score_kind: MemoryScoreKind::from_str(&row.get::<_, String>(0)?),
                    value: row.get(1)?,
                    computed_at: row.get::<_, i64>(2)?.max(0) as u64,
                    computed_from_window_secs: row.get::<_, i64>(3)?.max(0) as u64,
                    sample_size: row.get::<_, i64>(4)?.max(0) as u32,
                })
            })
            .optional()
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to load latest memory score: {}", e))
            })?;
        Ok(score)
    }

    pub fn insert_memory_link(&self, link: &MemoryLinkRecord) -> Result<(), LatticeError> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO memory_links
                    (link_id, source_memory_id, target_memory_id, link_type, reason, created_at, verification_status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    link.link_id,
                    link.source_memory_id,
                    link.target_memory_id,
                    link.link_type,
                    link.reason,
                    link.created_at as i64,
                    link.verification_status,
                ],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to insert memory link: {}", e)))?;
        Ok(())
    }

    pub fn list_memory_links_from(
        &self,
        source_memory_id: &str,
    ) -> Result<Vec<MemoryLinkRecord>, LatticeError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT link_id, source_memory_id, target_memory_id, link_type, reason, created_at, verification_status
                 FROM memory_links
                 WHERE source_memory_id = ?1
                 ORDER BY created_at DESC, link_id DESC",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare memory link query: {}", e))
            })?;
        let rows = statement
            .query_map(params![source_memory_id], |row| {
                Ok(MemoryLinkRecord {
                    link_id: row.get(0)?,
                    source_memory_id: row.get(1)?,
                    target_memory_id: row.get(2)?,
                    link_type: row.get(3)?,
                    reason: row.get(4)?,
                    created_at: row.get::<_, i64>(5)?.max(0) as u64,
                    verification_status: row.get(6)?,
                })
            })
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to execute memory link query: {}", e))
            })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LatticeError::Storage(format!("Failed to read memory link row: {}", e)))
    }

    pub fn list_memory_links_to(
        &self,
        target_memory_id: &str,
    ) -> Result<Vec<MemoryLinkRecord>, LatticeError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT link_id, source_memory_id, target_memory_id, link_type, reason, created_at, verification_status
                 FROM memory_links
                 WHERE target_memory_id = ?1
                 ORDER BY created_at DESC, link_id DESC",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare inbound memory link query: {}", e))
            })?;
        let rows = statement
            .query_map(params![target_memory_id], |row| {
                Ok(MemoryLinkRecord {
                    link_id: row.get(0)?,
                    source_memory_id: row.get(1)?,
                    target_memory_id: row.get(2)?,
                    link_type: row.get(3)?,
                    reason: row.get(4)?,
                    created_at: row.get::<_, i64>(5)?.max(0) as u64,
                    verification_status: row.get(6)?,
                })
            })
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to execute inbound memory link query: {}",
                    e
                ))
            })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| {
            LatticeError::Storage(format!("Failed to read inbound memory link row: {}", e))
        })
    }

    pub fn delete_memory_links_from(&self, source_memory_id: &str) -> Result<(), LatticeError> {
        self.conn
            .execute(
                "DELETE FROM memory_links WHERE source_memory_id = ?1",
                params![source_memory_id],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to delete memory links: {}", e)))?;
        Ok(())
    }

    /// Replace structured assertion metadata for an existing, non-invalidated memory.
    pub fn update_structured_fields(
        &self,
        id: &str,
        fields: &MemoryStructuredFields,
    ) -> Result<(), LatticeError> {
        self.record_direct_write();
        self.persist_structured_fields(id, fields)
    }

    /// Mark a memory as superseded by another memory id and downgrade verification state.
    pub fn mark_memory_superseded(
        &self,
        id: &str,
        superseded_by_memory_id: &str,
    ) -> Result<(), LatticeError> {
        self.record_direct_write();
        let mut fields = self.get_structured_fields(id)?.ok_or_else(|| {
            LatticeError::Storage(format!("Memory '{}' not found or invalidated", id))
        })?;
        fields.superseded_by_memory_id = Some(superseded_by_memory_id.to_string());
        fields.verification_status = MemoryVerificationStatus::Superseded;
        self.persist_structured_fields(id, &fields)
    }

    /// Mark a contradiction edge between two memory rows.
    pub fn mark_memory_contradicted(
        &self,
        id: &str,
        contradicted_by_memory_id: &str,
    ) -> Result<(), LatticeError> {
        self.record_direct_write();
        let mut fields = self.get_structured_fields(id)?.ok_or_else(|| {
            LatticeError::Storage(format!("Memory '{}' not found or invalidated", id))
        })?;
        if !fields
            .contradicted_by_memory_ids
            .iter()
            .any(|v| v == contradicted_by_memory_id)
        {
            fields
                .contradicted_by_memory_ids
                .push(contradicted_by_memory_id.to_string());
        }
        fields.verification_status = MemoryVerificationStatus::Contradicted;
        self.persist_structured_fields(id, &fields)?;

        if let Some(mut contradictor) = self.get_structured_fields(contradicted_by_memory_id)? {
            if !contradictor.contradicts_memory_ids.iter().any(|v| v == id) {
                contradictor.contradicts_memory_ids.push(id.to_string());
            }
            self.persist_structured_fields(contradicted_by_memory_id, &contradictor)?;
        }

        Ok(())
    }

    pub fn set_verification_state(
        &self,
        id: &str,
        verification_status: MemoryVerificationStatus,
        is_stale: bool,
        stale_reason: Option<&str>,
        last_verified_at: u64,
        last_verified_graph_snapshot_id: Option<u64>,
    ) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                    SET verification_status = ?1,
                        is_stale = ?2,
                        stale_reason = ?3,
                        last_verified_at = ?4,
                        last_verified_graph_snapshot_id = ?5
                 WHERE is_invalidated = 0 AND id = ?6",
                params![
                    verification_status.as_str(),
                    if is_stale { 1 } else { 0 },
                    stale_reason,
                    last_verified_at as i64,
                    last_verified_graph_snapshot_id.map(|value| value as i64),
                    id,
                ],
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to update verification state: {error}"))
            })?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    /// Search memories by keyword (per-word AND match on content + linked_symbols). Excludes invalidated.
    pub fn search_by_keyword(&self, keyword: &str) -> Result<Vec<Memory>, LatticeError> {
        let memories = if let Some(fts_query) = build_fts_query(keyword) {
            let sql = format!(
                "SELECT memories.id, memories.session_id, memories.content, memories.memory_type,
                        memories.scope, memories.confidence, memories.linked_symbols,
                        memories.linked_files, memories.workspace_id, memories.branch,
                        memories.scope_organization_id, memories.refresh_key, memories.source_query, memories.created_at,
                        memories.last_accessed, memories.access_count, memories.is_stale,
                        memories.stale_reason, memories.verification_status
                 FROM memories
                 INNER JOIN {table}
                    ON {table}.memory_id = memories.id
                 WHERE memories.is_invalidated = 0
                   AND {table} MATCH ?1
                 ORDER BY bm25({table}), memories.created_at DESC",
                table = MEMORIES_FTS_TABLE,
            );
            self.query_memories(&sql, params![fts_query], "search memories")?
        } else {
            self.query_memories(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE is_invalidated = 0
                 ORDER BY created_at DESC",
                [],
                "search memories",
            )?
        };

        // Touch each returned memory to update last_accessed
        for mem in &memories {
            let _ = self.touch_memory(&mem.id);
        }

        Ok(memories)
    }

    /// Mark all memories that reference a given symbol as stale with the provided reason.
    /// Uses LIKE match on the linked_symbols JSON column.
    pub fn mark_stale_by_symbol(
        &self,
        symbol_name: &str,
        reason: &str,
    ) -> Result<u64, LatticeError> {
        self.record_direct_write();
        // The linked_symbols column stores JSON arrays like ["foo","bar"].
        // We match symbol names contained inside the JSON string.
        let escaped = symbol_name.replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%\"{}\"%", escaped);

        let updated = self
            .conn
            .execute(
                "UPDATE memories
                    SET is_stale = 1,
                        stale_reason = ?1,
                        verification_status = 'stale'
                 WHERE is_invalidated = 0 AND linked_symbols LIKE ?2 ESCAPE '\\'",
                params![reason, pattern],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to mark stale: {}", e)))?;

        Ok(updated as u64)
    }

    /// Mark a specific memory as stale with the provided reason.
    pub fn mark_stale_by_id(&self, id: &str, reason: &str) -> Result<(), LatticeError> {
        self.record_direct_write();
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                    SET is_stale = 1,
                        stale_reason = ?1,
                        verification_status = 'stale'
                 WHERE is_invalidated = 0 AND id = ?2",
                params![reason, id],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to mark memory stale: {}", e)))?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        Ok(())
    }

    /// Mark all memories that reference a given file as stale with the provided reason.
    /// Uses LIKE match on the linked_files JSON column.
    pub fn mark_stale_by_file(&self, file_path: &str, reason: &str) -> Result<u64, LatticeError> {
        self.record_direct_write();
        let escaped = file_path.replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%\"{}\"%", escaped);

        let updated = self
            .conn
            .execute(
                "UPDATE memories
                    SET is_stale = 1,
                        stale_reason = ?1,
                        verification_status = 'stale'
                 WHERE is_invalidated = 0 AND linked_files LIKE ?2 ESCAPE '\\'",
                params![reason, pattern],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to mark stale by file: {}", e)))?;

        Ok(updated as u64)
    }

    /// Delete all memories and return the number deleted.
    pub fn clear_all(&self) -> Result<usize, LatticeError> {
        self.set_fts_dirty(true)?;
        let count = self
            .conn
            .execute("DELETE FROM memories", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear memories: {}", e)))?;
        self.conn
            .execute(&format!("DELETE FROM {}", MEMORIES_FTS_TABLE), [])
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to clear memory FTS index: {}", e))
            })?;
        self.set_fts_dirty(false)?;
        Ok(count)
    }

    /// Soft-delete a memory by setting is_invalidated = 1.
    pub fn invalidate(&self, id: &str) -> Result<(), LatticeError> {
        self.set_fts_dirty(true)?;
        self.conn
            .execute(
                "UPDATE memories SET is_invalidated = 1 WHERE id = ?1",
                params![id],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to invalidate memory: {}", e)))?;
        self.delete_fts_row(id)?;
        Ok(())
    }

    /// Update the content of a memory in-place. Clears stale flags since the content is now fresh.
    pub fn update_content(&self, id: &str, new_content: &str) -> Result<(), LatticeError> {
        self.record_direct_write();
        self.set_fts_dirty(true)?;
        let updated = self
            .conn
            .execute(
                "UPDATE memories
                    SET content = ?1,
                        is_stale = 0,
                        stale_reason = NULL,
                        verification_status = CASE
                            WHEN verification_status = 'stale' THEN 'unverified'
                            ELSE verification_status
                        END
                 WHERE id = ?2 AND is_invalidated = 0",
                params![new_content, id],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to update memory: {}", e)))?;
        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }
        self.sync_fts_by_id(id)?;
        Ok(())
    }

    /// Refresh a memory in-place with fresh evidence and metadata while preserving its identity.
    pub fn refresh_memory(
        &self,
        id: &str,
        content: Option<&str>,
        memory_type: Option<MemoryType>,
        scope: Option<MemoryScope>,
        linked_symbols: Option<&[String]>,
        linked_files: Option<&[String]>,
        workspace_id: Option<&str>,
        branch: Option<&str>,
        refresh_key: Option<&str>,
        source_query: Option<&str>,
        confidence: Option<f64>,
    ) -> Result<Memory, LatticeError> {
        self.record_direct_write();
        let mut memory = self.get_by_id(id)?.ok_or_else(|| {
            LatticeError::Storage(format!("Memory '{}' not found or invalidated", id))
        })?;

        if let Some(content) = content {
            memory.content = content.to_string();
        }
        if let Some(memory_type) = memory_type {
            memory.memory_type = memory_type;
        }
        if let Some(scope) = scope {
            memory.scope = scope;
        }
        if let Some(linked_symbols) = linked_symbols {
            memory.linked_symbols = linked_symbols.to_vec();
        }
        if let Some(linked_files) = linked_files {
            memory.linked_files = linked_files.to_vec();
        }
        if let Some(workspace_id) = workspace_id {
            memory.workspace_id = Some(workspace_id.to_string());
        }
        if let Some(branch) = branch {
            memory.branch = Some(branch.to_string());
        }
        if let Some(refresh_key) = refresh_key {
            memory.refresh_key = Some(refresh_key.to_string());
        }
        if let Some(source_query) = source_query {
            memory.source_query = Some(source_query.to_string());
        }
        if let Some(confidence) = confidence {
            memory.confidence = confidence;
        }

        memory.last_accessed = now_epoch_secs();
        memory.access_count = memory.access_count.saturating_add(1);
        memory.is_stale = false;
        memory.stale_reason = None;

        self.store(memory.clone())?;
        Ok(memory)
    }

    /// Promote a memory from ephemeral session scope into branch/repo scope and update durable metadata.
    pub fn promote_memory(
        &self,
        id: &str,
        scope: MemoryScope,
        linked_files: Option<&[String]>,
        workspace_id: Option<&str>,
        branch: Option<&str>,
        refresh_key: Option<&str>,
    ) -> Result<(), LatticeError> {
        self.record_direct_write();
        self.set_fts_dirty(true)?;
        let freshness_policy = MemoryFreshnessPolicy::from_scope(&scope);
        let updated = if let Some(linked_files) = linked_files {
            let linked_files_json = serde_json::to_string(linked_files).map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize linked_files: {}", e))
            })?;

            self.conn.execute(
                "UPDATE memories
                     SET scope = ?1,
                         linked_files = ?2,
                         workspace_id = COALESCE(?3, workspace_id),
                         branch = COALESCE(?4, branch),
                         refresh_key = COALESCE(?5, refresh_key),
                         freshness_policy = CASE
                            WHEN freshness_policy IN ('session_scoped', 'branch_scoped', 'repo_scoped')
                            THEN ?6
                            ELSE freshness_policy
                         END,
                         is_stale = 0,
                         stale_reason = NULL,
                         verification_status = CASE
                            WHEN verification_status = 'stale' THEN 'unverified'
                            ELSE verification_status
                         END
                     WHERE id = ?7 AND is_invalidated = 0",
                params![
                    scope.as_str(),
                    linked_files_json,
                    workspace_id,
                    branch,
                    refresh_key,
                    freshness_policy.as_str(),
                    id,
                ],
            )
        } else {
            self.conn.execute(
                "UPDATE memories
                     SET scope = ?1,
                         workspace_id = COALESCE(?2, workspace_id),
                         branch = COALESCE(?3, branch),
                         refresh_key = COALESCE(?4, refresh_key),
                         freshness_policy = CASE
                            WHEN freshness_policy IN ('session_scoped', 'branch_scoped', 'repo_scoped')
                            THEN ?5
                            ELSE freshness_policy
                         END,
                         is_stale = 0,
                         stale_reason = NULL,
                         verification_status = CASE
                            WHEN verification_status = 'stale' THEN 'unverified'
                            ELSE verification_status
                         END
                     WHERE id = ?6 AND is_invalidated = 0",
                params![
                    scope.as_str(),
                    workspace_id,
                    branch,
                    refresh_key,
                    freshness_policy.as_str(),
                    id
                ],
            )
        }
        .map_err(|e| LatticeError::Storage(format!("Failed to promote memory: {}", e)))?;

        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }

        self.sync_fts_by_id(id)?;
        Ok(())
    }

    /// Update last_accessed timestamp when a memory is retrieved.
    pub fn touch_memory(&self, id: &str) -> Result<(), LatticeError> {
        self.conn
            .execute(
                "UPDATE memories SET last_accessed = ?1, access_count = access_count + 1 WHERE id = ?2",
                params![now_epoch_secs() as i64, id],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to touch memory: {}", e)))?;
        Ok(())
    }

    /// Get memories for a specific session, ordered by most recent first.
    pub fn get_session_memories(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE is_invalidated = 0 AND session_id = ?1
                 ORDER BY created_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare session query: {}", e))
            })?;

        let rows = stmt
            .query_map(params![session_id, limit as i64], |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    content: row.get(2)?,
                    memory_type_str: row.get(3)?,
                    scope_str: row.get(4)?,
                    confidence: row.get(5)?,
                    linked_json: row.get(6)?,
                    linked_files_json: row.get(7)?,
                    workspace_id: row.get(8)?,
                    branch: row.get(9)?,
                    scope_organization_id: row.get(10)?,
                    refresh_key: row.get(11)?,
                    source_query: row.get(12)?,
                    created_at: row.get(13)?,
                    last_accessed: row.get(14)?,
                    access_count: row.get(15)?,
                    is_stale: row.get(16)?,
                    stale_reason: row.get(17)?,
                    verification_status: Some(MemoryVerificationStatus::from_str(
                        &row.get::<_, String>(18)?,
                    )),
                })
            })
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to query session memories: {}", e))
            })?;

        let mut memories = Vec::new();
        for row in rows {
            let r = row
                .map_err(|e| LatticeError::Storage(format!("Failed to read memory row: {}", e)))?;
            memories.push(r.into_memory());
        }
        Ok(memories)
    }

    /// Search memories across all sessions by keyword, optionally excluding a session.
    pub fn search_across_sessions(
        &self,
        keyword: &str,
        exclude_session: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>, LatticeError> {
        let memories = match (build_fts_query(keyword), exclude_session) {
            (Some(fts_query), Some(session_id)) => {
                let sql = format!(
                    "SELECT memories.id, memories.session_id, memories.content, memories.memory_type,
                            memories.scope, memories.confidence, memories.linked_symbols,
                            memories.linked_files, memories.workspace_id, memories.branch,
                        memories.scope_organization_id, memories.refresh_key, memories.source_query, memories.created_at,
                            memories.last_accessed, memories.access_count, memories.is_stale,
                            memories.stale_reason, memories.verification_status
                     FROM memories
                     INNER JOIN {table}
                        ON {table}.memory_id = memories.id
                     WHERE memories.is_invalidated = 0
                       AND memories.session_id != ?1
                       AND {table} MATCH ?2
                     ORDER BY bm25({table}), memories.created_at DESC
                     LIMIT ?3",
                    table = MEMORIES_FTS_TABLE,
                );
                self.query_memories(
                    &sql,
                    params![session_id, fts_query, limit as i64],
                    "search across sessions",
                )?
            }
            (Some(fts_query), None) => {
                let sql = format!(
                    "SELECT memories.id, memories.session_id, memories.content, memories.memory_type,
                            memories.scope, memories.confidence, memories.linked_symbols,
                            memories.linked_files, memories.workspace_id, memories.branch,
                        memories.scope_organization_id, memories.refresh_key, memories.source_query, memories.created_at,
                            memories.last_accessed, memories.access_count, memories.is_stale,
                            memories.stale_reason, memories.verification_status
                     FROM memories
                     INNER JOIN {table}
                        ON {table}.memory_id = memories.id
                     WHERE memories.is_invalidated = 0
                       AND {table} MATCH ?1
                     ORDER BY bm25({table}), memories.created_at DESC
                     LIMIT ?2",
                    table = MEMORIES_FTS_TABLE,
                );
                self.query_memories(
                    &sql,
                    params![fts_query, limit as i64],
                    "search across sessions",
                )?
            }
            (None, Some(session_id)) => self.query_memories(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE is_invalidated = 0
                   AND session_id != ?1
                 ORDER BY created_at DESC
                 LIMIT ?2",
                params![session_id, limit as i64],
                "search across sessions",
            )?,
            (None, None) => self.query_memories(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
                 FROM memories
                 WHERE is_invalidated = 0
                 ORDER BY created_at DESC
                 LIMIT ?1",
                params![limit as i64],
                "search across sessions",
            )?,
        };

        // Touch returned memories
        for mem in &memories {
            let _ = self.touch_memory(&mem.id);
        }

        Ok(memories)
    }

    /// List stale memories, optionally filtered by keyword, newest first.
    pub fn list_stale(
        &self,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>, LatticeError> {
        if let Some(fts_query) = build_fts_query(query.unwrap_or_default()) {
            let sql = format!(
                "SELECT memories.id, memories.session_id, memories.content, memories.memory_type,
                        memories.scope, memories.confidence, memories.linked_symbols,
                        memories.linked_files, memories.workspace_id, memories.branch,
                        memories.scope_organization_id, memories.refresh_key, memories.source_query, memories.created_at,
                        memories.last_accessed, memories.access_count, memories.is_stale,
                        memories.stale_reason, memories.verification_status
                 FROM memories
                 INNER JOIN {table}
                    ON {table}.memory_id = memories.id
                 WHERE memories.is_invalidated = 0
                   AND memories.is_stale = 1
                   AND {table} MATCH ?1
                 ORDER BY bm25({table}), memories.created_at DESC
                 LIMIT ?2",
                table = MEMORIES_FTS_TABLE,
            );
            return self.query_memories(
                &sql,
                params![fts_query, limit as i64],
                "list stale memories",
            );
        }

        self.query_memories(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, scope_organization_id, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason, verification_status
             FROM memories
             WHERE is_invalidated = 0
               AND is_stale = 1
             ORDER BY created_at DESC
             LIMIT ?1",
            params![limit as i64],
            "list stale memories",
        )
    }

    /// Authority-scoped lifecycle inspection. Both evidence-stale and
    /// retention-stale rows are discoverable, with authority applied before
    /// the caller's result bound.
    pub fn list_stale_scoped(
        &self,
        query: Option<&str>,
        limit: usize,
        scope: &ScopeFilter,
    ) -> Result<Vec<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let predicate = scope_sql_predicate(scope, None);
        let mut values = predicate.bind_values;
        let mut sql = format!(
            "SELECT memories.id,session_id,content,memory_type,scope,confidence,linked_symbols,
                    linked_files,workspace_id,branch,scope_organization_id,refresh_key,source_query,
                    created_at,last_accessed,access_count,is_stale,stale_reason,verification_status
             FROM memories WHERE is_invalidated=0 AND ({})
               AND (is_stale=1 OR retention_stale=1)",
            predicate.where_clause
        );
        if let Some(fts_query) = build_fts_query(query.unwrap_or_default()) {
            sql.push_str(&format!(
                " AND id IN (SELECT memory_id FROM {MEMORIES_FTS_TABLE} WHERE {MEMORIES_FTS_TABLE} MATCH ?)"
            ));
            values.push(Value::Text(fts_query));
        }
        sql.push_str(" ORDER BY created_at DESC,id LIMIT ?");
        values.push(Value::Integer(limit.min(4096) as i64));
        let rows = self.query_memories_values(&sql, values, "list scoped stale memories")?;
        self.enforce_scope_boundary(rows, scope, "list scoped stale memories")
    }

    fn resolve_structured_fields_for_store(
        &self,
        memory: &Memory,
    ) -> Result<MemoryStructuredFields, LatticeError> {
        let mut fields = self
            .get_structured_fields(&memory.id)?
            .unwrap_or_else(|| self.derive_default_structured_fields(memory));

        if matches!(fields.memory_class, MemoryClass::Observation) {
            fields.memory_class = MemoryClass::from_memory_type(&memory.memory_type);
        }
        if !has_extended_assertion_type(&fields) {
            fields.assertion_type = MemoryAssertionType::from_memory_type(&memory.memory_type);
        }
        if fields.freshness_policy.is_scope_derived() {
            fields.freshness_policy = MemoryFreshnessPolicy::from_scope(&memory.scope);
        }
        if memory.is_stale {
            fields.verification_status = MemoryVerificationStatus::Stale;
        } else if fields.verification_status == MemoryVerificationStatus::Stale {
            fields.verification_status = infer_verification_status(memory);
        }
        if fields.confidence_reason.is_none() && memory.confidence < 0.5 {
            fields.confidence_reason = Some(format!(
                "Low-confidence memory (confidence={:.2})",
                memory.confidence
            ));
        }
        if fields.provenance.is_empty() {
            fields.provenance = build_default_provenance(memory);
        }
        if fields.evidence.is_empty() {
            fields.evidence = build_default_evidence(memory);
        }

        Ok(fields)
    }

    fn derive_default_structured_fields(&self, memory: &Memory) -> MemoryStructuredFields {
        let mut fields = MemoryStructuredFields {
            memory_class: MemoryClass::from_memory_type(&memory.memory_type),
            assertion_type: MemoryAssertionType::from_memory_type(&memory.memory_type),
            verification_status: infer_verification_status(memory),
            confidence_reason: memory
                .source_query
                .as_ref()
                .filter(|query| query_is_verification_signal(query))
                .map(|query| format!("Verification signal from source query: '{}'", query)),
            freshness_policy: MemoryFreshnessPolicy::from_scope(&memory.scope),
            freshness_policy_detail: None,
            provenance: build_default_provenance(memory),
            evidence: build_default_evidence(memory),
            ..MemoryStructuredFields::default()
        };

        if fields.confidence_reason.is_none() && memory.confidence < 0.5 {
            fields.confidence_reason = Some(format!(
                "Low-confidence memory (confidence={:.2})",
                memory.confidence
            ));
        }

        fields
    }

    fn persist_structured_fields(
        &self,
        id: &str,
        fields: &MemoryStructuredFields,
    ) -> Result<(), LatticeError> {
        let contradicts_json =
            serde_json::to_string(&fields.contradicts_memory_ids).map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize contradicts memory ids: {}", e))
            })?;
        let contradicted_by_json = serde_json::to_string(&fields.contradicted_by_memory_ids)
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to serialize contradicted-by memory ids: {}",
                    e
                ))
            })?;
        let validity_conditions_json =
            serde_json::to_string(&fields.validity_conditions).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to serialize validity conditions metadata: {}",
                    e
                ))
            })?;
        let invalidation_triggers_json = serde_json::to_string(&fields.invalidation_triggers)
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to serialize invalidation triggers metadata: {}",
                    e
                ))
            })?;
        let provenance_json = serde_json::to_string(&fields.provenance).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize provenance metadata: {}", e))
        })?;
        let evidence_json = serde_json::to_string(&fields.evidence).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize evidence metadata: {}", e))
        })?;
        let linked_docs_json = serde_json::to_string(&fields.linked_docs).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize linked docs metadata: {}", e))
        })?;
        let linked_tests_json = serde_json::to_string(&fields.linked_tests).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize linked tests metadata: {}", e))
        })?;
        let linked_memories_json = serde_json::to_string(&fields.linked_memories).map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to serialize linked memories metadata: {}",
                e
            ))
        })?;

        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET memory_class = ?1,
                     assertion_type = ?2,
                     verification_status = ?3,
                     confidence_reason = ?4,
                     supersedes_memory_id = ?5,
                     superseded_by_memory_id = ?6,
                     contradicts_memory_ids = ?7,
                     contradicted_by_memory_ids = ?8,
                     freshness_policy = ?9,
                     freshness_policy_detail = ?10,
                     validity_conditions_json = ?11,
                     invalidation_triggers_json = ?12,
                     provenance_json = ?13,
                     evidence_json = ?14,
                     linked_docs_json = ?15,
                     linked_tests_json = ?16,
                     linked_memories_json = ?17
                 WHERE id = ?18 AND is_invalidated = 0",
                params![
                    fields.memory_class.as_str(),
                    fields.assertion_type.as_str(),
                    fields.verification_status.as_str(),
                    fields.confidence_reason,
                    fields.supersedes_memory_id,
                    fields.superseded_by_memory_id,
                    contradicts_json,
                    contradicted_by_json,
                    fields.freshness_policy.as_str(),
                    fields.freshness_policy_detail,
                    validity_conditions_json,
                    invalidation_triggers_json,
                    provenance_json,
                    evidence_json,
                    linked_docs_json,
                    linked_tests_json,
                    linked_memories_json,
                    id,
                ],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to update structured memory fields: {}", e))
            })?;

        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }

        self.sync_memory_evidence(id, &fields.evidence)?;

        Ok(())
    }

    fn sync_memory_evidence(
        &self,
        memory_id: &str,
        evidence: &[MemoryEvidence],
    ) -> Result<(), LatticeError> {
        self.conn
            .execute(
                "DELETE FROM memory_evidence WHERE memory_id = ?1",
                params![memory_id],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to clear memory evidence rows: {}", e))
            })?;
        for (index, entry) in evidence.iter().enumerate() {
            self.conn
                .execute(
                    "INSERT INTO memory_evidence
                        (evidence_id, memory_id, kind, reference, detail, captured_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        format!("{memory_id}:evidence:{index}"),
                        memory_id,
                        entry.kind,
                        entry.reference,
                        entry.detail,
                        entry.captured_at.map(|value| value as i64),
                    ],
                )
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to sync memory evidence row: {}", e))
                })?;
        }
        Ok(())
    }

    fn initialize_migration_table(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS memory_schema_migrations (
                    version INTEGER PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    applied_at INTEGER NOT NULL
                );",
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to initialize memory schema migration table: {error}"
                ))
            })
    }

    fn apply_schema_migrations(&self) -> Result<(), LatticeError> {
        for migration in MEMORY_SCHEMA_MIGRATIONS {
            let applied = self
                .conn
                .query_row(
                    "SELECT 1 FROM memory_schema_migrations WHERE version = ?1",
                    params![migration.version],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to read memory schema migrations: {error}"
                    ))
                })?
                .is_some();
            if applied {
                if !self.memory_column_exists(migration.column)? {
                    return Err(LatticeError::Storage(format!(
                        "Memory schema migration {} ({}) is recorded but column '{}' is missing",
                        migration.version, migration.name, migration.column
                    )));
                }
                continue;
            }

            if !self.memory_column_exists(migration.column)? {
                self.conn.execute(migration.sql, []).map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to apply memory schema migration {} ({}): {error}",
                        migration.version, migration.name
                    ))
                })?;
            }

            self.conn
                .execute(
                    "INSERT INTO memory_schema_migrations (version, name, applied_at)
                     VALUES (?1, ?2, ?3)",
                    params![migration.version, migration.name, now_unix_micros()],
                )
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to record memory schema migration {} ({}): {error}",
                        migration.version, migration.name
                    ))
                })?;
        }
        Ok(())
    }

    fn memory_column_exists(&self, column: &str) -> Result<bool, LatticeError> {
        let mut statement = self
            .conn
            .prepare("PRAGMA table_info(memories)")
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to inspect memory schema: {error}"))
            })?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to inspect memory schema columns: {error}"))
            })?;
        for name in columns {
            if name.map_err(|error| {
                LatticeError::Storage(format!("Failed to read memory schema column: {error}"))
            })? == column
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn initialize_fts_state(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {MEMORY_FTS_STATE_TABLE} (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    is_dirty INTEGER NOT NULL DEFAULT 1
                );
                 INSERT OR IGNORE INTO {MEMORY_FTS_STATE_TABLE} (singleton, is_dirty)
                 VALUES (1, 1);"
            ))
            .map_err(|error| classify_sqlite_error("initialize memory FTS state", error))
    }

    fn rebuild_fts_if_dirty(&self) -> Result<(), LatticeError> {
        let dirty: i64 = self
            .conn
            .query_row(
                &format!("SELECT is_dirty FROM {MEMORY_FTS_STATE_TABLE} WHERE singleton = 1"),
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to read memory FTS state: {error}"))
            })?;
        if dirty != 0 {
            self.rebuild_fts()?;
        }
        Ok(())
    }

    fn rebuild_fts(&self) -> Result<(), LatticeError> {
        self.set_fts_dirty(true)?;
        self.conn
            .execute(&format!("DELETE FROM {}", MEMORIES_FTS_TABLE), [])
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to rebuild memory FTS index: {}", e))
            })?;

        for memory in self.list_all()? {
            self.upsert_fts_row_raw(&memory)?;
        }

        self.set_fts_dirty(false)?;

        Ok(())
    }

    fn upsert_fts_row(&self, memory: &Memory) -> Result<(), LatticeError> {
        self.set_fts_dirty(true)?;
        self.upsert_fts_row_raw(memory)?;
        self.set_fts_dirty(false)
    }

    fn upsert_fts_row_raw(&self, memory: &Memory) -> Result<(), LatticeError> {
        let document = build_memory_search_document(memory);
        self.delete_fts_row_raw(&memory.id)?;
        self.conn
            .execute(
                &format!(
                    "INSERT INTO {} (memory_id, content, linked_symbols, linked_files)
                     VALUES (?1, ?2, ?3, ?4)",
                    MEMORIES_FTS_TABLE
                ),
                params![
                    memory.id,
                    document.content,
                    document.linked_symbols,
                    document.linked_files,
                ],
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to upsert memory FTS row for '{}': {}",
                    memory.id, e
                ))
            })?;
        Ok(())
    }

    fn delete_fts_row(&self, id: &str) -> Result<(), LatticeError> {
        self.set_fts_dirty(true)?;
        self.delete_fts_row_raw(id)?;
        self.set_fts_dirty(false)
    }

    fn delete_fts_row_raw(&self, id: &str) -> Result<(), LatticeError> {
        self.conn
            .execute(
                &format!("DELETE FROM {} WHERE memory_id = ?1", MEMORIES_FTS_TABLE),
                params![id],
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to delete memory FTS row for '{}': {}",
                    id, e
                ))
            })?;
        Ok(())
    }

    fn set_fts_dirty(&self, is_dirty: bool) -> Result<(), LatticeError> {
        self.conn
            .execute(
                &format!("UPDATE {MEMORY_FTS_STATE_TABLE} SET is_dirty = ?1 WHERE singleton = 1"),
                params![i64::from(is_dirty)],
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to update memory FTS state: {error}"))
            })?;
        Ok(())
    }

    fn sync_fts_by_id(&self, id: &str) -> Result<(), LatticeError> {
        match self.get_by_id(id)? {
            Some(memory) => self.upsert_fts_row(&memory),
            None => self.delete_fts_row(id),
        }
    }

    fn query_memories<P>(
        &self,
        sql: &str,
        params: P,
        context: &str,
    ) -> Result<Vec<Memory>, LatticeError>
    where
        P: rusqlite::Params,
    {
        let mut stmt = self.conn.prepare(sql).map_err(|e| {
            LatticeError::Storage(format!("Failed to prepare {} query: {}", context, e))
        })?;

        let rows = stmt.query_map(params, memory_row_from_row).map_err(|e| {
            LatticeError::Storage(format!("Failed to execute {} query: {}", context, e))
        })?;

        let mut memories = Vec::new();
        for row in rows {
            memories.push(
                row.map_err(|e| {
                    LatticeError::Storage(format!("Failed to read {} row: {}", context, e))
                })?
                .into_memory(),
            );
        }
        Ok(memories)
    }

    fn query_memories_values(
        &self,
        sql: &str,
        bind_values: Vec<Value>,
        context: &str,
    ) -> Result<Vec<Memory>, LatticeError> {
        let mut stmt = self.conn.prepare(sql).map_err(|e| {
            LatticeError::Storage(format!("Failed to prepare {} query: {}", context, e))
        })?;

        let rows = stmt
            .query_map(params_from_iter(bind_values), memory_row_from_row)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to execute {} query: {}", context, e))
            })?;

        let mut memories = Vec::new();
        for row in rows {
            memories.push(
                row.map_err(|e| {
                    LatticeError::Storage(format!("Failed to read {} row: {}", context, e))
                })?
                .into_memory(),
            );
        }
        Ok(memories)
    }

    fn filter_scope_boundary(
        &self,
        memories: Vec<Memory>,
        scope: &ScopeFilter,
        checkout_id: Option<&str>,
    ) -> Result<Vec<Memory>, LatticeError> {
        let mut allowed = Vec::with_capacity(memories.len());
        for memory in memories {
            if scope_allows(&memory, scope)
                && self.checkout_applicability_allows(&memory.id, checkout_id)?
            {
                allowed.push(memory);
                continue;
            }
            self.record_scope_filtered(&memory, scope)?;
        }
        Ok(allowed)
    }

    pub fn enforce_scope_boundary(
        &self,
        memories: Vec<Memory>,
        scope: &ScopeFilter,
        context: &str,
    ) -> Result<Vec<Memory>, LatticeError> {
        self.enforce_scope_boundary_with_checkout(memories, scope, None, context)
    }

    fn enforce_scope_boundary_with_checkout(
        &self,
        memories: Vec<Memory>,
        scope: &ScopeFilter,
        checkout_id: Option<&str>,
        context: &str,
    ) -> Result<Vec<Memory>, LatticeError> {
        let mut allowed = Vec::with_capacity(memories.len());
        for memory in memories {
            if scope_allows(&memory, scope)
                && self.checkout_applicability_allows(&memory.id, checkout_id)?
            {
                allowed.push(memory);
                continue;
            }
            self.handle_scope_boundary_failure(&memory, scope, context)?;
        }
        Ok(allowed)
    }

    fn handle_scope_boundary_failure(
        &self,
        memory: &Memory,
        scope: &ScopeFilter,
        context: &str,
    ) -> Result<(), LatticeError> {
        let _ = context;
        #[cfg(debug_assertions)]
        {
            let _ = scope;
            panic!(
                "scope leak blocked in {context}: memory_id={} scope={}",
                memory.id,
                memory.scope.as_str()
            );
        }

        #[cfg(not(debug_assertions))]
        {
            tracing::error!(
                target: "security",
                memory_id = memory.id.as_str(),
                attempted_workspace_id = scope.workspace_id.as_str(),
                attempted_branch = scope.branch.as_ref().map(|branch| branch.name.as_str()),
                memory_scope = memory.scope.as_str(),
                "scope_leak_blocked"
            );
            self.record_scope_filtered(memory, scope)?;
            Ok(())
        }
    }

    fn record_scope_filtered(
        &self,
        memory: &Memory,
        scope: &ScopeFilter,
    ) -> Result<(), LatticeError> {
        tracing::warn!(
            target: "security",
            memory_id = memory.id.as_str(),
            attempted_workspace_id = scope.workspace_id.as_str(),
            attempted_branch = scope.branch.as_ref().map(|branch| branch.name.as_str()),
            memory_scope = memory.scope.as_str(),
            "scope_leak_blocked"
        );
        self.conn
            .execute(
                "INSERT INTO memory_scope_filter_events
                    (memory_id, attempted_workspace_id, attempted_branch, memory_scope, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    memory.id,
                    scope.workspace_id,
                    scope.branch.as_ref().map(|branch| branch.name.as_str()),
                    memory.scope.as_str(),
                    now_epoch_secs() as i64,
                ],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to record memory scope filter event: {}", e))
            })?;
        Ok(())
    }

    fn find_impacted_memory_ids_impl(
        &self,
        workspace_id: &str,
        changed_files: &[String],
        changed_symbols: &[String],
        distinct_memories: bool,
    ) -> Result<Vec<String>, LatticeError> {
        if changed_files.is_empty() && changed_symbols.is_empty() {
            return Ok(Vec::new());
        }

        let reference_filters = build_graph_delta_reference_filters(changed_files, changed_symbols);
        if reference_filters.is_empty() {
            return Ok(Vec::new());
        }

        let select = if distinct_memories {
            "SELECT DISTINCT e.memory_id"
        } else {
            "SELECT e.memory_id"
        };
        let conditions = reference_filters
            .iter()
            .map(|filter| {
                if filter.is_like {
                    "(e.kind = ? AND e.reference LIKE ? ESCAPE '\\')"
                } else {
                    "(e.kind = ? AND e.reference = ?)"
                }
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "{select}
             FROM memory_evidence e
             INNER JOIN memories m ON m.id = e.memory_id
             WHERE m.is_invalidated = 0
               AND m.workspace_id = ?
               AND ({conditions})"
        );

        let mut bind_values = Vec::with_capacity(1 + reference_filters.len() * 2);
        bind_values.push(Value::Text(workspace_id.to_string()));
        for filter in reference_filters {
            bind_values.push(Value::Text(filter.kind));
            bind_values.push(Value::Text(filter.reference));
        }

        let mut statement = self.conn.prepare(&sql).map_err(|e| {
            LatticeError::Storage(format!("Failed to prepare impacted-memory query: {}", e))
        })?;
        let rows = statement
            .query_map(params_from_iter(bind_values.iter()), |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to query impacted memories: {}", e))
            })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| {
            LatticeError::Storage(format!("Failed to decode impacted memory row: {}", e))
        })
    }

    fn load_existing_verification_metadata(
        &self,
        id: &str,
    ) -> Result<ExistingVerificationMetadata, LatticeError> {
        self.conn
            .query_row(
                "SELECT expires_at, applicable_checkout_id
                 FROM memories
                 WHERE id = ?1
                 LIMIT 1",
                params![id],
                |row| {
                    Ok(ExistingVerificationMetadata {
                        expires_at: row
                            .get::<_, Option<i64>>(0)?
                            .map(DateTime::from_unix_seconds),
                        applicable_checkout_id: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to load existing verification metadata: {}",
                    e
                ))
            })?
            .map_or(Ok(ExistingVerificationMetadata::default()), Ok)
    }

    fn applicable_checkout_id(&self, id: &str) -> Result<Option<String>, LatticeError> {
        self.conn
            .query_row(
                "SELECT applicable_checkout_id FROM memories WHERE id = ?1 AND is_invalidated = 0",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to load memory checkout applicability: {error}"
                ))
            })
            .map(Option::flatten)
    }

    fn checkout_applicability_allows(
        &self,
        id: &str,
        checkout_id: Option<&str>,
    ) -> Result<bool, LatticeError> {
        Ok(match self.applicable_checkout_id(id)? {
            None => true,
            Some(required) => checkout_id.is_some_and(|actual| actual == required),
        })
    }
}

fn retain_derived_memory_provenance(
    tx: &Transaction<'_>,
    source_delivery_by_memory: &HashMap<String, String>,
    source_memory_ids: &HashSet<String>,
    deleted_at: i64,
) -> Result<usize, LatticeError> {
    let mut statement = tx
        .prepare(
            "SELECT id, provenance_json, evidence_json, linked_memories_json,
                    supersedes_memory_id, superseded_by_memory_id,
                    contradicts_memory_ids, contradicted_by_memory_ids
             FROM memories ORDER BY id ASC",
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to prepare derived-memory provenance query: {error}"
            ))
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to query derived-memory provenance: {error}"
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to decode derived-memory provenance: {error}"
            ))
        })?;
    drop(statement);

    let mut retained = 0;
    for (
        memory_id,
        provenance_json,
        evidence_json,
        linked_memories_json,
        supersedes_memory_id,
        superseded_by_memory_id,
        contradicts_memory_ids,
        contradicted_by_memory_ids,
    ) in rows
    {
        if source_memory_ids.contains(&memory_id) {
            continue;
        }
        let mut referenced = HashSet::new();
        for encoded in [
            &provenance_json,
            &evidence_json,
            &linked_memories_json,
            &contradicts_memory_ids,
            &contradicted_by_memory_ids,
        ] {
            let value: serde_json::Value = serde_json::from_str(encoded).map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to decode derived-memory references: {error}"
                ))
            })?;
            collect_capture_references(&value, source_memory_ids, &mut referenced);
        }
        for reference in [supersedes_memory_id, superseded_by_memory_id]
            .into_iter()
            .flatten()
        {
            if source_memory_ids.contains(&reference) {
                referenced.insert(reference);
            }
        }
        if referenced.is_empty() {
            continue;
        }

        let mut provenance: Vec<MemoryProvenance> = serde_json::from_str(&provenance_json)
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to decode derived-memory provenance JSON: {error}"
                ))
            })?;
        let mut ordered_references = referenced.into_iter().collect::<Vec<_>>();
        ordered_references.sort();
        for source_memory_id in ordered_references {
            let delivery_key = &source_delivery_by_memory[&source_memory_id];
            let tombstone_source = "lattice.deleted_session_capture.v1";
            if !provenance.iter().any(|entry| {
                entry.source == tombstone_source
                    && entry.reference.as_deref() == Some(delivery_key.as_str())
            }) {
                provenance.push(MemoryProvenance {
                    source: tombstone_source.to_string(),
                    reference: Some(delivery_key.clone()),
                    captured_at: Some(deleted_at.max(0) as u64),
                    note: None,
                });
            }
            insert_capture_tombstone_provenance(
                tx,
                delivery_key,
                "memory",
                &memory_id,
                &source_memory_id,
            )?;
        }
        let encoded = serde_json::to_string(&provenance).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to encode derived-memory tombstone provenance: {error}"
            ))
        })?;
        tx.execute(
            "UPDATE memories SET provenance_json = ?1 WHERE id = ?2",
            params![encoded, memory_id],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to retain derived-memory tombstone provenance: {error}"
            ))
        })?;
        retained += 1;
    }
    Ok(retained)
}

fn retain_proposal_tombstone_provenance(
    tx: &Transaction<'_>,
    source_delivery_by_memory: &HashMap<String, String>,
    source_memory_ids: &HashSet<String>,
) -> Result<usize, LatticeError> {
    let mut statement = tx
        .prepare(
            "SELECT proposal_id, target_memory_id, prior_state, proposed_state, evidence
             FROM consolidation_proposals ORDER BY proposal_id ASC",
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to prepare capture-derived proposal query: {error}"
            ))
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to query capture-derived proposals: {error}"
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to decode capture-derived proposal: {error}"
            ))
        })?;
    drop(statement);

    let mut retained = 0;
    for (proposal_id, target_memory_id, prior_state, proposed_state, evidence) in rows {
        let mut prior: serde_json::Value = serde_json::from_str(&prior_state).map_err(|error| {
            LatticeError::Storage(format!("Failed to decode proposal prior state: {error}"))
        })?;
        let mut proposed: serde_json::Value =
            serde_json::from_str(&proposed_state).map_err(|error| {
                LatticeError::Storage(format!("Failed to decode proposal proposed state: {error}"))
            })?;
        let evidence_value: serde_json::Value =
            serde_json::from_str(&evidence).map_err(|error| {
                LatticeError::Storage(format!("Failed to decode proposal evidence: {error}"))
            })?;
        let mut referenced = HashSet::new();
        collect_capture_references(&prior, source_memory_ids, &mut referenced);
        collect_capture_references(&proposed, source_memory_ids, &mut referenced);
        collect_capture_references(&evidence_value, source_memory_ids, &mut referenced);
        if target_memory_id
            .as_ref()
            .is_some_and(|id| source_memory_ids.contains(id))
        {
            referenced.insert(target_memory_id.clone().expect("checked as some"));
        }
        if referenced.is_empty() {
            continue;
        }

        tx.execute(
            "DELETE FROM consolidation_event_outbox WHERE proposal_id=?1",
            [&proposal_id],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to delete tombstoned proposal event outbox: {error}"
            ))
        })?;

        scrub_capture_memory_snapshots(&mut prior, source_memory_ids);
        scrub_capture_memory_snapshots(&mut proposed, source_memory_ids);
        let mut ordered_references = referenced.into_iter().collect::<Vec<_>>();
        ordered_references.sort();
        for source_memory_id in ordered_references {
            insert_capture_tombstone_provenance(
                tx,
                &source_delivery_by_memory[&source_memory_id],
                "proposal",
                &proposal_id,
                &source_memory_id,
            )?;
        }
        tx.execute(
            "UPDATE consolidation_proposals
             SET target_memory_id = CASE
                     WHEN target_memory_id IN (
                         SELECT source_memory_id
                         FROM session_capture_tombstone_provenance
                         WHERE derived_kind = 'proposal' AND derived_id = ?1
                     ) THEN NULL
                     ELSE target_memory_id
                 END,
                 prior_state = ?2,
                 proposed_state = ?3
             WHERE proposal_id = ?1",
            params![
                proposal_id,
                serde_json::to_string(&prior).map_err(|error| LatticeError::Storage(format!(
                    "Failed to encode scrubbed proposal prior state: {error}"
                )))?,
                serde_json::to_string(&proposed).map_err(|error| LatticeError::Storage(
                    format!("Failed to encode scrubbed proposal proposed state: {error}")
                ))?,
            ],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to retain capture-derived proposal: {error}"
            ))
        })?;
        retained += 1;
    }
    Ok(retained)
}

fn insert_capture_tombstone_provenance(
    tx: &Transaction<'_>,
    delivery_key: &str,
    derived_kind: &str,
    derived_id: &str,
    source_memory_id: &str,
) -> Result<(), LatticeError> {
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO {SESSION_CAPTURE_TOMBSTONE_PROVENANCE_TABLE}
                (delivery_key, derived_kind, derived_id, source_memory_id)
             VALUES (?1, ?2, ?3, ?4)"
        ),
        params![delivery_key, derived_kind, derived_id, source_memory_id],
    )
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to persist content-free capture tombstone provenance: {error}"
        ))
    })?;
    Ok(())
}

fn collect_capture_references(
    value: &serde_json::Value,
    source_memory_ids: &HashSet<String>,
    found: &mut HashSet<String>,
) {
    match value {
        serde_json::Value::String(value) if source_memory_ids.contains(value) => {
            found.insert(value.clone());
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_capture_references(value, source_memory_ids, found);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_capture_references(value, source_memory_ids, found);
            }
        }
        _ => {}
    }
}

fn scrub_capture_memory_snapshots(
    value: &mut serde_json::Value,
    source_memory_ids: &HashSet<String>,
) {
    match value {
        serde_json::Value::Object(object) => {
            let captured_id = object
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| source_memory_ids.contains(*id))
                .map(str::to_string);
            if let Some(source_memory_id) = captured_id {
                *value = serde_json::json!({
                    "deleted_session_capture": {
                        "source_memory_id": source_memory_id
                    }
                });
                return;
            }
            for value in object.values_mut() {
                scrub_capture_memory_snapshots(value, source_memory_ids);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                scrub_capture_memory_snapshots(value, source_memory_ids);
            }
        }
        _ => {}
    }
}

struct ScopeSqlPredicate {
    where_clause: String,
    bind_values: Vec<Value>,
}

fn session_digest_delivery_key(digest: &SessionDigest) -> String {
    hash_capture_parts(&[
        "lattice.session-digest.delivery.v1",
        &digest.repository_id,
        digest.checkout_id.as_deref().unwrap_or_default(),
        &digest.session_id,
        &digest.segment.to_string(),
    ])
}

fn session_digest_candidate_fingerprint(
    candidate: &SessionDigestCandidate,
) -> Result<String, LatticeError> {
    let encoded = serde_json::to_vec(candidate).map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to encode normalized automatic capture candidate: {error}"
        ))
    })?;
    Ok(hash_capture_bytes(
        b"lattice.session-digest.normalized-candidate.v1\0",
        &encoded,
    ))
}

fn session_digest_batch_fingerprint(
    digest: &SessionDigest,
    extractor_version: &str,
    candidate_fingerprints: &[String],
) -> Result<String, LatticeError> {
    let encoded = serde_json::to_vec(&(digest, extractor_version, candidate_fingerprints))
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to encode normalized automatic capture batch: {error}"
            ))
        })?;
    Ok(hash_capture_bytes(
        b"lattice.session-digest.normalized-batch.v1\0",
        &encoded,
    ))
}

fn hash_capture_parts(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_capture_bytes(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

fn session_digest_memory_id(idempotency_key: &str) -> String {
    let digest = idempotency_key
        .strip_prefix("sha256:")
        .unwrap_or(idempotency_key);
    format!("session-digest-{digest}")
}

fn load_session_digest_delivery(
    tx: &Transaction<'_>,
    delivery_key: &str,
) -> Result<Option<ExistingSessionDigestDelivery>, LatticeError> {
    tx.query_row(
        &format!(
            "SELECT normalized_fingerprint, candidate_count, committed_count,
                    dropped_observation_count
             FROM {SESSION_DIGEST_DELIVERIES_TABLE}
             WHERE delivery_key = ?1"
        ),
        params![delivery_key],
        |row| {
            Ok(ExistingSessionDigestDelivery {
                normalized_fingerprint: row.get(0)?,
                candidate_count: row.get::<_, i64>(1)? as usize,
                committed_count: row.get::<_, i64>(2)? as usize,
                dropped_observation_count: row.get::<_, i64>(3)? as usize,
            })
        },
    )
    .optional()
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to load automatic session capture delivery: {error}"
        ))
    })
}

fn load_session_digest_commits(
    tx: &Transaction<'_>,
    delivery_key: &str,
) -> Result<Vec<ExistingSessionDigestCommit>, LatticeError> {
    let mut statement = tx
        .prepare(&format!(
            "SELECT commits.candidate_idempotency_key, commits.candidate_fingerprint,
                    commits.memory_id
             FROM {SESSION_DIGEST_CAPTURE_COMMITS_TABLE} commits
             INNER JOIN memories ON memories.id = commits.memory_id
             WHERE commits.delivery_key = ?1
             ORDER BY candidate_ordinal"
        ))
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to prepare automatic session capture replay: {error}"
            ))
        })?;
    let rows = statement
        .query_map(params![delivery_key], |row| {
            Ok(ExistingSessionDigestCommit {
                candidate_idempotency_key: row.get(0)?,
                candidate_fingerprint: row.get(1)?,
                memory_id: row.get(2)?,
            })
        })
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to query automatic session capture replay: {error}"
            ))
        })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to decode automatic session capture replay: {error}"
        ))
    })
}

fn load_session_digest_commit_by_candidate(
    tx: &Transaction<'_>,
    candidate_key: &str,
) -> Result<Option<ExistingSessionDigestCommit>, LatticeError> {
    tx.query_row(
        &format!(
            "SELECT candidate_idempotency_key, candidate_fingerprint, memory_id
             FROM {SESSION_DIGEST_CAPTURE_COMMITS_TABLE}
             WHERE candidate_idempotency_key = ?1"
        ),
        params![candidate_key],
        |row| {
            Ok(ExistingSessionDigestCommit {
                candidate_idempotency_key: row.get(0)?,
                candidate_fingerprint: row.get(1)?,
                memory_id: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to inspect automatic session capture idempotency: {error}"
        ))
    })
}

fn insert_session_digest_memory(
    tx: &Transaction<'_>,
    digest: &SessionDigest,
    candidate: &SessionDigestCandidate,
    memory_id: &str,
    checkout_id: &str,
    extractor_version: &str,
) -> Result<(), LatticeError> {
    let (memory_type, assertion_type) = match candidate.memory_class {
        MemoryClass::FailurePattern => (MemoryType::AntiPattern, MemoryAssertionType::AntiPattern),
        _ => (
            MemoryType::Observation,
            MemoryAssertionType::WorkflowOutcome,
        ),
    };
    let (scope, freshness_policy) = if digest.branch.is_some() {
        (MemoryScope::Branch, MemoryFreshnessPolicy::BranchScoped)
    } else {
        (MemoryScope::Session, MemoryFreshnessPolicy::SessionScoped)
    };
    let linked_symbols = "[]";
    let linked_files =
        serde_json::to_string(&candidate.evidence.edited_paths).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to encode automatic session capture linked files: {error}"
            ))
        })?;
    let evidence = session_digest_memory_evidence(candidate)?;
    let evidence_json = serde_json::to_string(&[evidence]).map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to encode automatic session capture evidence: {error}"
        ))
    })?;
    let provenance = vec![
        MemoryProvenance {
            source: format!("lattice.session_digest.schema.v{}", digest.schema_version),
            reference: Some(digest.payload_hash.clone()),
            captured_at: Some(digest.received_at.unix_seconds().max(0) as u64),
            note: None,
        },
        MemoryProvenance {
            source: "lattice.session_digest.extractor".to_string(),
            reference: Some(extractor_version.to_string()),
            captured_at: Some(digest.received_at.unix_seconds().max(0) as u64),
            note: None,
        },
    ];
    let provenance_json = serde_json::to_string(&provenance).map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to encode automatic session capture provenance: {error}"
        ))
    })?;
    let captured_at = digest.received_at.unix_seconds();

    tx.execute(
        "INSERT INTO memories
            (id, session_id, content, memory_type, scope, confidence, linked_symbols,
             linked_files, workspace_id, branch, scope_organization_id, refresh_key,
             source_query, memory_class, assertion_type, verification_status,
             confidence_reason, freshness_policy, provenance_json, evidence_json,
             created_at, last_accessed, applicable_checkout_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12,
                 ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?20, ?21)",
        params![
            memory_id,
            digest.session_id,
            candidate.claim,
            memory_type.as_str(),
            scope.as_str(),
            0.7_f64,
            linked_symbols,
            linked_files,
            digest.repository_id,
            digest.branch,
            candidate.assertion_fingerprint,
            "automatic_session_digest",
            candidate.memory_class.as_str(),
            assertion_type.as_str(),
            MemoryVerificationStatus::Unverified.as_str(),
            "deterministically extracted from a sanitized, authority-bound session digest",
            freshness_policy.as_str(),
            provenance_json,
            evidence_json,
            captured_at,
            checkout_id,
        ],
    )
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to store automatic session capture memory: {error}"
        ))
    })?;
    Ok(())
}

fn session_digest_memory_evidence(
    candidate: &SessionDigestCandidate,
) -> Result<MemoryEvidence, LatticeError> {
    let detail = serde_json::to_string(&candidate.evidence).map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to encode typed automatic session capture evidence: {error}"
        ))
    })?;
    Ok(MemoryEvidence {
        kind: "session_digest".to_string(),
        reference: Some(candidate.idempotency_key.clone()),
        detail: Some(detail),
        captured_at: Some(candidate.evidence.captured_at.unix_seconds().max(0) as u64),
        span: None,
        evidence_content_hash: None,
    })
}

fn insert_session_digest_evidence(
    tx: &Transaction<'_>,
    _digest: &SessionDigest,
    candidate: &SessionDigestCandidate,
    memory_id: &str,
) -> Result<(), LatticeError> {
    let evidence = session_digest_memory_evidence(candidate)?;
    tx.execute(
        "INSERT INTO memory_evidence
            (evidence_id, memory_id, kind, reference, detail, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            format!("{memory_id}:evidence:0"),
            memory_id,
            evidence.kind,
            evidence.reference,
            evidence.detail,
            evidence.captured_at.map(|value| value as i64),
        ],
    )
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to store automatic session capture evidence row: {error}"
        ))
    })?;
    Ok(())
}

fn insert_session_digest_fts(
    tx: &Transaction<'_>,
    candidate: &SessionDigestCandidate,
    memory_id: &str,
) -> Result<(), LatticeError> {
    let linked_files = candidate.evidence.edited_paths.join(" ");
    tx.execute(
        &format!(
            "INSERT INTO {MEMORIES_FTS_TABLE}
                (memory_id, content, linked_symbols, linked_files)
             VALUES (?1, ?2, '', ?3)"
        ),
        params![
            memory_id,
            augment_search_text(&candidate.claim),
            linked_files
        ],
    )
    .map_err(|error| {
        LatticeError::Storage(format!(
            "Failed to index automatic session capture memory: {error}"
        ))
    })?;
    Ok(())
}

#[derive(Default)]
struct ExistingVerificationMetadata {
    expires_at: Option<DateTime<Utc>>,
    applicable_checkout_id: Option<String>,
}

struct GraphDeltaReferenceFilter {
    kind: String,
    reference: String,
    is_like: bool,
}

fn scope_sql_predicate(scope: &ScopeFilter, checkout_id: Option<&str>) -> ScopeSqlPredicate {
    let mut predicates = Vec::new();
    let mut bind_values = Vec::new();

    if let Some(session_id) = &scope.session_id {
        predicates.push("(scope = ? AND session_id = ?)".to_string());
        bind_values.push(Value::Text(MemoryScope::Session.as_str().to_string()));
        bind_values.push(Value::Text(session_id.clone()));
    }
    if let Some(branch) = &scope.branch {
        predicates.push("(scope = ? AND workspace_id = ? AND branch = ?)".to_string());
        bind_values.push(Value::Text(MemoryScope::Branch.as_str().to_string()));
        bind_values.push(Value::Text(scope.workspace_id.clone()));
        bind_values.push(Value::Text(branch.name.clone()));
    }
    predicates.push("(scope = ? AND workspace_id = ?)".to_string());
    bind_values.push(Value::Text(MemoryScope::Repo.as_str().to_string()));
    bind_values.push(Value::Text(scope.workspace_id.clone()));

    if let Some(organization_id) = &scope.organization_id {
        predicates.push("(scope = ? AND scope_organization_id = ?)".to_string());
        bind_values.push(Value::Text(MemoryScope::Organization.as_str().to_string()));
        bind_values.push(Value::Text(organization_id.clone()));
    }

    let applicability = if let Some(checkout_id) = checkout_id {
        bind_values.push(Value::Text(checkout_id.to_string()));
        "(applicable_checkout_id IS NULL OR applicable_checkout_id = ?)"
    } else {
        "applicable_checkout_id IS NULL"
    };

    ScopeSqlPredicate {
        where_clause: format!("({}) AND {applicability}", predicates.join(" OR ")),
        bind_values,
    }
}

fn build_graph_delta_reference_filters(
    changed_files: &[String],
    changed_symbols: &[String],
) -> Vec<GraphDeltaReferenceFilter> {
    let mut filters = Vec::new();
    let mut seen = HashSet::new();

    for file in changed_files {
        push_graph_delta_filter(&mut filters, &mut seen, "file", file, false);
        push_graph_delta_filter(
            &mut filters,
            &mut seen,
            "file",
            &format!("file:%/{}@%", escape_like_literal(file)),
            true,
        );
        push_graph_delta_filter(
            &mut filters,
            &mut seen,
            "symbol",
            &format!("symbol:%/{}@%", escape_like_literal(file)),
            true,
        );
    }

    for symbol in changed_symbols {
        push_graph_delta_filter(&mut filters, &mut seen, "symbol", symbol, false);
    }

    filters
}

fn push_graph_delta_filter(
    filters: &mut Vec<GraphDeltaReferenceFilter>,
    seen: &mut HashSet<(String, String)>,
    kind: &str,
    reference: &str,
    is_like: bool,
) {
    let key = (kind.to_string(), reference.to_string());
    if !seen.insert(key.clone()) {
        return;
    }
    filters.push(GraphDeltaReferenceFilter {
        kind: key.0,
        reference: key.1,
        is_like,
    });
}

fn escape_like_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn scope_filter_error(error: ScopeFilterError) -> LatticeError {
    LatticeError::Storage(format!("Invalid memory scope filter: {}", error))
}

/// Internal helper to reduce row-mapping boilerplate.
struct MemoryRow {
    id: String,
    session_id: String,
    content: String,
    memory_type_str: String,
    scope_str: String,
    confidence: f64,
    linked_json: String,
    linked_files_json: String,
    workspace_id: Option<String>,
    branch: Option<String>,
    scope_organization_id: Option<String>,
    refresh_key: Option<String>,
    source_query: Option<String>,
    created_at: i64,
    last_accessed: i64,
    access_count: i64,
    is_stale: i32,
    stale_reason: Option<String>,
    verification_status: Option<MemoryVerificationStatus>,
}

struct StructuredMemoryRow {
    memory_class_str: String,
    assertion_type_str: String,
    verification_status_str: String,
    confidence_reason: Option<String>,
    supersedes_memory_id: Option<String>,
    superseded_by_memory_id: Option<String>,
    contradicts_memory_ids_json: String,
    contradicted_by_memory_ids_json: String,
    freshness_policy_str: String,
    freshness_policy_detail: Option<String>,
    validity_conditions_json: String,
    invalidation_triggers_json: String,
    provenance_json: String,
    evidence_json: String,
    linked_docs_json: String,
    linked_tests_json: String,
    linked_memories_json: String,
}

fn structured_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StructuredMemoryRow> {
    Ok(StructuredMemoryRow {
        memory_class_str: row.get(0)?,
        assertion_type_str: row.get(1)?,
        verification_status_str: row.get(2)?,
        confidence_reason: row.get(3)?,
        supersedes_memory_id: row.get(4)?,
        superseded_by_memory_id: row.get(5)?,
        contradicts_memory_ids_json: row.get(6)?,
        contradicted_by_memory_ids_json: row.get(7)?,
        freshness_policy_str: row.get(8)?,
        freshness_policy_detail: row.get(9)?,
        validity_conditions_json: row.get(10)?,
        invalidation_triggers_json: row.get(11)?,
        provenance_json: row.get(12)?,
        evidence_json: row.get(13)?,
        linked_docs_json: row.get(14)?,
        linked_tests_json: row.get(15)?,
        linked_memories_json: row.get(16)?,
    })
}

impl StructuredMemoryRow {
    fn into_structured_fields(self) -> MemoryStructuredFields {
        MemoryStructuredFields {
            memory_class: MemoryClass::from_str(&self.memory_class_str),
            assertion_type: MemoryAssertionType::from_str(&self.assertion_type_str),
            verification_status: MemoryVerificationStatus::from_str(&self.verification_status_str),
            confidence_reason: self.confidence_reason,
            supersedes_memory_id: self.supersedes_memory_id,
            superseded_by_memory_id: self.superseded_by_memory_id,
            contradicts_memory_ids: serde_json::from_str(&self.contradicts_memory_ids_json)
                .unwrap_or_default(),
            contradicted_by_memory_ids: serde_json::from_str(&self.contradicted_by_memory_ids_json)
                .unwrap_or_default(),
            freshness_policy: MemoryFreshnessPolicy::from_str(&self.freshness_policy_str),
            freshness_policy_detail: self.freshness_policy_detail,
            validity_conditions: serde_json::from_str(&self.validity_conditions_json)
                .unwrap_or_default(),
            invalidation_triggers: serde_json::from_str(&self.invalidation_triggers_json)
                .unwrap_or_default(),
            provenance: serde_json::from_str(&self.provenance_json).unwrap_or_default(),
            evidence: serde_json::from_str(&self.evidence_json).unwrap_or_default(),
            linked_docs: serde_json::from_str(&self.linked_docs_json).unwrap_or_default(),
            linked_tests: serde_json::from_str(&self.linked_tests_json).unwrap_or_default(),
            linked_memories: serde_json::from_str(&self.linked_memories_json).unwrap_or_default(),
        }
    }
}

fn memory_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
    let verification_status = if row.as_ref().column_count() > 18 {
        let status: String = row.get(18)?;
        Some(MemoryVerificationStatus::from_str(&status))
    } else {
        None
    };

    Ok(MemoryRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        content: row.get(2)?,
        memory_type_str: row.get(3)?,
        scope_str: row.get(4)?,
        confidence: row.get(5)?,
        linked_json: row.get(6)?,
        linked_files_json: row.get(7)?,
        workspace_id: row.get(8)?,
        branch: row.get(9)?,
        scope_organization_id: row.get(10)?,
        refresh_key: row.get(11)?,
        source_query: row.get(12)?,
        created_at: row.get(13)?,
        last_accessed: row.get(14)?,
        access_count: row.get(15)?,
        is_stale: row.get(16)?,
        stale_reason: row.get(17)?,
        verification_status,
    })
}

impl MemoryRow {
    fn into_memory(self) -> Memory {
        let linked_symbols: Vec<String> =
            serde_json::from_str(&self.linked_json).unwrap_or_default();
        let linked_files: Vec<String> =
            serde_json::from_str(&self.linked_files_json).unwrap_or_default();
        let is_stale = self.is_stale != 0;
        let verification_status = self.verification_status.unwrap_or_else(|| {
            infer_legacy_row_verification_status(
                is_stale,
                self.confidence,
                self.source_query.as_deref(),
            )
        });
        Memory {
            id: self.id,
            session_id: self.session_id,
            content: self.content,
            memory_type: MemoryType::from_str(&self.memory_type_str),
            scope: MemoryScope::from_str(&self.scope_str),
            confidence: self.confidence,
            linked_symbols,
            linked_files,
            workspace_id: self.workspace_id,
            branch: self.branch,
            scope_organization_id: self.scope_organization_id,
            refresh_key: self.refresh_key,
            source_query: self.source_query,
            created_at: self.created_at as u64,
            last_accessed: self.last_accessed as u64,
            access_count: self.access_count as u32,
            is_stale,
            stale_reason: self.stale_reason,
            verification_status,
        }
    }
}

struct MemorySearchDocument {
    content: String,
    linked_symbols: String,
    linked_files: String,
}

fn build_memory_search_document(memory: &Memory) -> MemorySearchDocument {
    MemorySearchDocument {
        content: augment_search_text(&memory.content),
        linked_symbols: memory
            .linked_symbols
            .iter()
            .map(|symbol| augment_search_text(symbol))
            .collect::<Vec<_>>()
            .join(" "),
        linked_files: memory
            .linked_files
            .iter()
            .map(|file| augment_search_text(file))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn build_fts_query(keyword: &str) -> Option<String> {
    let groups: Vec<String> = keyword
        .split_whitespace()
        .filter_map(build_fts_group)
        .collect();

    if groups.is_empty() {
        None
    } else {
        Some(groups.join(" AND "))
    }
}

fn recall_exact_term_is_usable(term: &str) -> bool {
    term.len() <= MAX_RECALL_TERM_BYTES && term.chars().any(|character| character.is_alphanumeric())
}

fn recall_vm_instruction_budget() -> Result<u64, LatticeError> {
    parse_recall_vm_instruction_budget(std::env::var_os(RECALL_VM_BUDGET_ENV))
}

pub(super) fn parse_recall_vm_instruction_budget(
    raw: Option<OsString>,
) -> Result<u64, LatticeError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_RECALL_VM_INSTRUCTIONS);
    };
    let raw = raw.into_string().map_err(|_| {
        LatticeError::Storage(format!(
            "{RECALL_VM_BUDGET_ENV} must contain a UTF-8 positive integer"
        ))
    })?;
    let value = raw.parse::<u64>().map_err(|_| {
        LatticeError::Storage(format!(
            "{RECALL_VM_BUDGET_ENV} must be a positive integer no greater than {MAX_RECALL_VM_INSTRUCTIONS}"
        ))
    })?;
    if value == 0 || value > MAX_RECALL_VM_INSTRUCTIONS {
        return Err(LatticeError::Storage(format!(
            "{RECALL_VM_BUDGET_ENV} must be between 1 and {MAX_RECALL_VM_INSTRUCTIONS}"
        )));
    }
    Ok(value)
}

fn build_fts_group(raw: &str) -> Option<String> {
    let variants = expand_search_terms(raw);
    if variants.is_empty() {
        return None;
    }

    if variants.len() == 1 || is_identifier_search_token(raw) {
        return Some(format!("{}*", variants[0]));
    }

    Some(format!(
        "({})",
        variants
            .into_iter()
            .map(|term| format!("{}*", term))
            .collect::<Vec<_>>()
            .join(" OR ")
    ))
}

fn augment_search_text(text: &str) -> String {
    let expansions = expand_search_terms(text);
    if expansions.is_empty() {
        text.to_string()
    } else {
        format!("{} {}", text, expansions.join(" "))
    }
}

fn expand_search_terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();

    for raw in text.split_whitespace() {
        let compact: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        push_term(&mut seen, &mut terms, compact);

        let mut current = String::new();
        let mut prev_lowercase = false;
        for ch in raw.chars() {
            if ch.is_ascii_alphanumeric() {
                let is_uppercase = ch.is_ascii_uppercase();
                if is_uppercase && prev_lowercase && !current.is_empty() {
                    push_term(&mut seen, &mut terms, std::mem::take(&mut current));
                }
                current.push(ch.to_ascii_lowercase());
                prev_lowercase = ch.is_ascii_lowercase();
            } else {
                push_term(&mut seen, &mut terms, std::mem::take(&mut current));
                prev_lowercase = false;
            }
        }
        push_term(&mut seen, &mut terms, current);
    }

    terms
}

fn is_identifier_search_token(raw: &str) -> bool {
    raw.contains('_')
        || raw.contains("::")
        || raw.contains('/')
        || raw.contains('\\')
        || raw.contains('.')
        || raw.chars().any(|ch| ch.is_ascii_uppercase())
}

fn push_term(seen: &mut HashSet<String>, terms: &mut Vec<String>, term: String) {
    if term.len() < 2 {
        return;
    }
    if seen.insert(term.clone()) {
        terms.push(term);
    }
}

fn infer_verification_status(memory: &Memory) -> MemoryVerificationStatus {
    infer_legacy_row_verification_status(
        memory.is_stale,
        memory.confidence,
        memory.source_query.as_deref(),
    )
}

fn infer_legacy_row_verification_status(
    is_stale: bool,
    confidence: f64,
    source_query: Option<&str>,
) -> MemoryVerificationStatus {
    if is_stale {
        return MemoryVerificationStatus::Stale;
    }

    if let Some(query) = source_query {
        if query_is_verification_signal(query) {
            return MemoryVerificationStatus::Verified;
        }
    }

    if confidence >= 0.95 {
        MemoryVerificationStatus::InReview
    } else {
        MemoryVerificationStatus::Unverified
    }
}

fn has_extended_assertion_type(fields: &MemoryStructuredFields) -> bool {
    matches!(
        fields.assertion_type,
        MemoryAssertionType::WorkflowOutcome
            | MemoryAssertionType::Constraint
            | MemoryAssertionType::Hypothesis
            | MemoryAssertionType::Procedure
            | MemoryAssertionType::Outcome
            | MemoryAssertionType::Preference
            | MemoryAssertionType::Question
            | MemoryAssertionType::Counter
    )
}

fn query_is_verification_signal(source_query: &str) -> bool {
    let query = source_query.to_ascii_lowercase();
    query.contains("verified")
        || query.contains("validated")
        || query.contains("code and tests")
        || query.contains("from tests")
        || query.contains("from code")
}

fn build_default_provenance(memory: &Memory) -> Vec<MemoryProvenance> {
    let mut provenance = Vec::new();

    if let Some(source_query) = memory.source_query.as_ref() {
        provenance.push(MemoryProvenance {
            source: "source_query".to_string(),
            reference: Some(source_query.clone()),
            captured_at: Some(memory.created_at),
            note: None,
        });
    }

    if let Some(refresh_key) = memory.refresh_key.as_ref() {
        provenance.push(MemoryProvenance {
            source: "refresh_key".to_string(),
            reference: Some(refresh_key.clone()),
            captured_at: Some(memory.created_at),
            note: None,
        });
    }

    if provenance.is_empty() {
        provenance.push(MemoryProvenance {
            source: "assistant_observation".to_string(),
            reference: None,
            captured_at: Some(memory.created_at),
            note: None,
        });
    }

    provenance
}

fn build_default_evidence(memory: &Memory) -> Vec<MemoryEvidence> {
    let mut evidence = Vec::new();

    for symbol in &memory.linked_symbols {
        evidence.push(MemoryEvidence {
            kind: "symbol".to_string(),
            reference: Some(symbol.clone()),
            detail: None,
            captured_at: Some(memory.created_at),
            span: None,
            evidence_content_hash: None,
        });
    }

    for file in &memory.linked_files {
        evidence.push(MemoryEvidence {
            kind: "file".to_string(),
            reference: Some(file.clone()),
            detail: None,
            captured_at: Some(memory.created_at),
            span: None,
            evidence_content_hash: None,
        });
    }

    evidence
}

/// Generate a unique identifier using timestamp, thread ID, and an atomic counter
/// to avoid collisions even when called rapidly from the same or different threads.
fn generate_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = DefaultHasher::new();
    now.as_nanos().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    seq.hash(&mut hasher);
    let h = hasher.finish();

    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (h >> 32) as u32,
        (h >> 16) as u16 & 0xffff,
        (h & 0xffff) as u16,
        ((h >> 48) as u16) ^ ((h >> 8) as u16),
        now.as_nanos() as u64 & 0xffffffffffff,
    )
}

/// Current time as seconds since UNIX epoch.
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn configure_connection(conn: &Connection, enable_wal: bool) -> Result<(), LatticeError> {
    conn.busy_timeout(Duration::from_secs(MEMORY_DB_BUSY_TIMEOUT_SECS))
        .map_err(|e| classify_sqlite_error("set memory busy timeout", e))?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .map_err(|e| classify_sqlite_error("set memory incremental auto-vacuum", e))?;

    if enable_wal {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| classify_sqlite_error("set memory WAL mode", e))?;
        conn.pragma_update(None, "wal_autocheckpoint", MEMORY_DB_AUTO_CHECKPOINT_PAGES)
            .map_err(|e| classify_sqlite_error("set memory WAL auto-checkpoint", e))?;
        conn.pragma_update(
            None,
            "journal_size_limit",
            MEMORY_DB_JOURNAL_SIZE_LIMIT_BYTES,
        )
        .map_err(|e| classify_sqlite_error("set memory journal size limit", e))?;
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|e| classify_sqlite_error("checkpoint memory WAL", e))?;
    }

    Ok(())
}

fn validate_supported_schema(conn: &Connection) -> Result<(), LatticeError> {
    let has_migrations: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_schema_migrations')",
        [], |row| row.get(0),
    ).map_err(|e| classify_sqlite_error("inspect memory schema", e))?;
    if !has_migrations {
        return Ok(());
    }
    let version: Option<i64> = conn
        .query_row(
            "SELECT MAX(version) FROM memory_schema_migrations",
            [],
            |row| row.get(0),
        )
        .map_err(|e| classify_sqlite_error("read memory schema version", e))?;
    if version.unwrap_or(0) > MAX_MEMORY_SCHEMA_VERSION {
        return Err(LatticeError::UnsupportedMemorySchema(format!(
            "database version {} is newer than supported version {}",
            version.unwrap_or(0),
            MAX_MEMORY_SCHEMA_VERSION
        )));
    }
    Ok(())
}

fn classify_sqlite_error(operation: &str, error: rusqlite::Error) -> LatticeError {
    use rusqlite::ErrorCode;
    let detail = format!("{operation}: {error}");
    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            LatticeError::MemoryStorageBusy(detail)
        }
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly | ErrorCode::CannotOpen) => {
            LatticeError::MemoryStorageAccessDenied(detail)
        }
        Some(ErrorCode::DiskFull) => LatticeError::MemoryStorageFull(detail),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            LatticeError::CorruptMemoryStorage(detail)
        }
        _ => LatticeError::Storage(detail),
    }
}

#[cfg(test)]
mod availability_tests {
    use super::*;

    #[test]
    fn unavailable_store_is_queryable_but_rejects_acknowledged_writes() {
        let store = MemoryStore::unavailable(
            Path::new("/unavailable/memories.db"),
            MemoryStoreFailureKind::AccessDenied,
            "injected denial",
        )
        .unwrap();
        assert!(!store.is_persistent_available());
        assert!(store.list_all().unwrap().is_empty());
        assert!(store
            .enqueue_verification_job("workspace", "memory")
            .is_err());
        let direct = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO memories (id, content, memory_type) VALUES ('direct', 'x', 'fact')",
                [],
            )
            .map_err(|error| classify_sqlite_error("direct unavailable write", error))?;
            Ok(())
        });
        assert!(
            direct.is_err(),
            "query_only must cover direct connection writes"
        );
        assert!(store.list_all().unwrap().is_empty());
    }

    #[test]
    fn newer_schema_is_rejected_without_changing_the_artifact() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("memories.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE memory_schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL); INSERT INTO memory_schema_migrations VALUES (999, 'future', 1);").unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        let error = MemoryStore::open(&path)
            .err()
            .expect("future schema must fail closed");
        assert!(matches!(error, LatticeError::UnsupportedMemorySchema(_)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn corrupt_artifact_is_typed_and_preserved() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("memories.db");
        let bytes = b"not a sqlite database";
        std::fs::write(&path, bytes).unwrap();
        let error = MemoryStore::open(&path)
            .err()
            .expect("corrupt store must fail closed");
        assert!(matches!(error, LatticeError::CorruptMemoryStorage(_)));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!PathBuf::from(format!("{}-wal", path.display())).exists());
        assert!(!PathBuf::from(format!("{}-shm", path.display())).exists());
    }

    #[test]
    fn sqlite_full_is_returned_as_typed_memory_failure() {
        let root = tempfile::tempdir().unwrap();
        let conn = Connection::open(root.path().join("bounded.db")).unwrap();
        conn.execute_batch("CREATE TABLE bounded (payload BLOB NOT NULL);")
            .unwrap();
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        conn.pragma_update(None, "max_page_count", pages).unwrap();
        let sqlite_error = conn
            .execute("INSERT INTO bounded VALUES (zeroblob(1048576))", [])
            .err()
            .expect("max_page_count must inject SQLITE_FULL");
        let error = classify_sqlite_error("injected full write", sqlite_error);
        assert!(matches!(error, LatticeError::MemoryStorageFull(_)));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_database_open_is_typed_and_preserves_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("memories.db");
        drop(MemoryStore::open(&path).unwrap());
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let result = MemoryStore::open(&path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = result
            .err()
            .expect("read-only store must fail opening WAL mode");
        assert!(
            matches!(error, LatticeError::MemoryStorageAccessDenied(_)),
            "{error:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn trusted_check_observations_are_scoped_bounded_and_cascade_with_memory() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed) VALUES('m','claim','observation','repo',1,1)",[])
                .map_err(|error| LatticeError::Storage(error.to_string()))?;
            Ok(())
        }).unwrap();
        let target_digest = store.verification_target_digest("m", "repo").unwrap();
        for observed_at in 0..70 {
            store
                .record_trusted_check_observation(&TrustedCheckObservationRecord {
                    observation_id: None,
                    memory_id: "m".into(),
                    repository_id: "repo".into(),
                    checkout_id: "checkout".into(),
                    check_id: "unit".into(),
                    evidence_reference: Some("test:unit".into()),
                    passed: observed_at % 2 == 0,
                    revision: Some("abc".into()),
                    graph_generation: 7,
                    source_fingerprint: [observed_at as u8; 32],
                    target_digest,
                    observed_at,
                    exit_code: Some(0),
                })
                .unwrap();
        }
        let records = store.trusted_check_observations("m").unwrap();
        assert_eq!(records.len(), 64);
        assert_eq!(records[0].observed_at, 69);
        assert_eq!(records.last().unwrap().observed_at, 6);
        let wrong = TrustedCheckObservationRecord {
            repository_id: "other".into(),
            ..records[0].clone()
        };
        assert!(store.record_trusted_check_observation(&wrong).is_err());
        store
            .with_connection(|connection| {
                connection
                    .execute("DELETE FROM memories WHERE id='m'", [])
                    .map_err(|error| LatticeError::Storage(error.to_string()))?;
                Ok(())
            })
            .unwrap();
        assert!(store.trusted_check_observations("m").unwrap().is_empty());
    }

    #[test]
    fn verification_result_rolls_back_structured_fields_when_status_write_fails() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed) VALUES('atomic','claim','observation','repo',1,1)",[])
                .map_err(|error| LatticeError::Storage(error.to_string()))?;
            connection.execute_batch("CREATE TRIGGER reject_verification_status BEFORE UPDATE OF is_stale ON memories BEGIN SELECT RAISE(ABORT,'injected status failure'); END;")
                .map_err(|error| LatticeError::Storage(error.to_string()))?;
            Ok(())
        }).unwrap();
        let before = store.get_structured_fields("atomic").unwrap().unwrap();
        let mut proposed = before.clone();
        proposed.confidence_reason = Some("must roll back".into());
        proposed.verification_status = MemoryVerificationStatus::Verified;
        let binding = VerificationCommitBinding {
            repository_id: "repo".into(),
            checkout_id: "checkout-main".into(),
            branch: "main".into(),
            target_digest: store.verification_target_digest("atomic", "repo").unwrap(),
            observations: Vec::new(),
        };
        assert!(store
            .persist_verification_result(
                "atomic",
                &proposed,
                MemoryVerificationStatus::Verified,
                false,
                None,
                2,
                Some(3),
                &binding,
            )
            .is_err());
        let after = store.get_structured_fields("atomic").unwrap().unwrap();
        assert_eq!(after, before);
        assert_eq!(
            store
                .get_by_id("atomic")
                .unwrap()
                .unwrap()
                .verification_status,
            MemoryVerificationStatus::Unverified
        );
    }

    #[test]
    fn corrupt_fingerprint_fails_closed_even_alongside_valid_observation() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed) VALUES('corrupt','claim','observation','repo',1,1)",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            connection.execute("INSERT INTO trusted_check_observations(memory_id,repository_id,checkout_id,check_id,evidence_reference,passed,graph_generation,source_fingerprint,target_digest,observed_at) VALUES('corrupt','repo','checkout','valid','test',1,1,zeroblob(32),zeroblob(32),1)",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            connection.execute("PRAGMA ignore_check_constraints=ON",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            connection.execute("INSERT INTO trusted_check_observations(memory_id,repository_id,checkout_id,check_id,evidence_reference,passed,graph_generation,source_fingerprint,target_digest,observed_at) VALUES('corrupt','repo','checkout','bad','test',0,1,zeroblob(3),zeroblob(32),2)",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            Ok(())
        }).unwrap();
        assert!(store.trusted_check_observations("corrupt").is_err());
    }

    #[test]
    fn verification_target_digest_tracks_claim_inputs_but_not_verification_outputs() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed) VALUES('digest','claim','observation','repo',1,1)",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            Ok(())
        }).unwrap();
        let original = store.verification_target_digest("digest", "repo").unwrap();
        store
            .set_verification_state(
                "digest",
                MemoryVerificationStatus::Verified,
                false,
                None,
                99,
                Some(7),
            )
            .unwrap();
        assert_eq!(
            store.verification_target_digest("digest", "repo").unwrap(),
            original
        );
        let mut fields = store.get_structured_fields("digest").unwrap().unwrap();
        fields.provenance.push(MemoryProvenance {
            source: "test".into(),
            reference: Some("changed".into()),
            captured_at: Some(1),
            note: None,
        });
        store.update_structured_fields("digest", &fields).unwrap();
        assert_ne!(
            store.verification_target_digest("digest", "repo").unwrap(),
            original
        );
        let after_provenance = store.verification_target_digest("digest", "repo").unwrap();
        store
            .with_connection(|connection| {
                connection
                    .execute(
                        "UPDATE memories SET last_accessed=88,access_count=12 WHERE id='digest'",
                        [],
                    )
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.verification_target_digest("digest", "repo").unwrap(),
            after_provenance
        );
        store
            .with_connection(|connection| {
                connection
                    .execute(
                        "UPDATE memories SET content='replacement' WHERE id='digest'",
                        [],
                    )
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
                Ok(())
            })
            .unwrap();
        assert_ne!(
            store.verification_target_digest("digest", "repo").unwrap(),
            original
        );
    }

    #[test]
    fn expansion_delivery_atomically_checks_authority_lifecycle_and_snapshot() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,session_id,content,memory_type,scope,workspace_id,branch,created_at,last_accessed,applicable_checkout_id) VALUES('expand','session-a','complete lesson','observation','branch','repo','feature',1,1,'checkout-a')",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            Ok(())
        }).unwrap();
        let memory = store.get_by_id("expand").unwrap().unwrap();
        let fields = store.get_structured_fields("expand").unwrap().unwrap();
        let digest = MemoryStore::expansion_delivery_digest_for(&memory, &fields, "repo").unwrap();
        let binding = crate::memory::retention::DeliveryBinding {
            delivery_id: "expand-delivery",
            repository_id: "repository:repo",
            session_id: "session-a",
            payload_hash: "sha256:expansion",
        };
        store
            .attempt_expansion_memory_delivery(
                &binding,
                "expand",
                digest,
                Some("repo"),
                Some("checkout-a"),
                Some("feature"),
                "session-a",
                None,
                10,
            )
            .unwrap();

        let grouped = crate::memory::retention::DeliveryBinding {
            delivery_id: "grouped-rollback",
            ..binding.clone()
        };
        assert!(store
            .attempt_expansion_memories_delivery(
                &grouped,
                &[
                    ("expand".to_string(), digest),
                    ("expand".to_string(), [0; 32])
                ],
                Some("repo"),
                Some("checkout-a"),
                Some("feature"),
                "session-a",
                None,
                10,
            )
            .is_err());
        store.with_connection(|connection| {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM memory_deliveries WHERE delivery_id='grouped-rollback')",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            assert!(!exists);
            Ok(())
        }).unwrap();

        store
            .with_connection(|connection| {
                connection
                    .execute(
                        "UPDATE memories SET content='changed' WHERE id='expand'",
                        [],
                    )
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
                Ok(())
            })
            .unwrap();
        let changed = crate::memory::retention::DeliveryBinding {
            delivery_id: "changed-delivery",
            ..binding.clone()
        };
        assert!(store
            .attempt_expansion_memory_delivery(
                &changed,
                "expand",
                digest,
                Some("repo"),
                Some("checkout-a"),
                Some("feature"),
                "session-a",
                None,
                11,
            )
            .is_err());
        store.with_connection(|connection| {
            connection.execute("UPDATE memories SET content='complete lesson',verification_status='verified' WHERE id='expand'",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            Ok(())
        }).unwrap();
        let trust_changed = crate::memory::retention::DeliveryBinding {
            delivery_id: "trust-changed-delivery",
            ..binding.clone()
        };
        assert!(store
            .attempt_expansion_memory_delivery(
                &trust_changed,
                "expand",
                digest,
                Some("repo"),
                Some("checkout-a"),
                Some("feature"),
                "session-a",
                None,
                12,
            )
            .is_err());
        store.with_connection(|connection| {
            connection.execute("UPDATE memories SET verification_status='unverified',retention_stale=1 WHERE id='expand'",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            let count:i64=connection.query_row("SELECT COUNT(*) FROM memory_deliveries",[],|row|row.get(0)).map_err(|e|LatticeError::Storage(e.to_string()))?;
            assert_eq!(count,1);
            Ok(())
        }).unwrap();
        let stale = crate::memory::retention::DeliveryBinding {
            delivery_id: "stale-delivery",
            ..binding
        };
        assert!(store
            .attempt_expansion_memory_delivery(
                &stale,
                "expand",
                digest,
                Some("repo"),
                Some("checkout-a"),
                Some("feature"),
                "session-a",
                None,
                13,
            )
            .is_err());
        store
            .with_connection(|connection| {
                let count: i64 = connection
                    .query_row("SELECT COUNT(*) FROM memory_deliveries", [], |row| {
                        row.get(0)
                    })
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
                assert_eq!(count, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn independent_connection_mutation_rejects_observation_and_status_commit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("memories.db");
        let primary = MemoryStore::open(&path).unwrap();
        let concurrent = MemoryStore::open(&path).unwrap();
        primary.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed) VALUES('cas','original','observation','repo',1,1)",[]).map_err(|e| LatticeError::Storage(e.to_string()))?; Ok(())
        }).unwrap();
        let digest = primary.verification_target_digest("cas", "repo").unwrap();
        let loaded_memory = primary.get_by_id("cas").unwrap().unwrap();
        let loaded_fields = primary.get_structured_fields("cas").unwrap().unwrap();
        let record = TrustedCheckObservationRecord {
            observation_id: None,
            memory_id: "cas".into(),
            repository_id: "repo".into(),
            checkout_id: "checkout".into(),
            check_id: "check".into(),
            evidence_reference: Some("test".into()),
            passed: true,
            revision: Some("abc".into()),
            graph_generation: 1,
            source_fingerprint: [1; 32],
            target_digest: digest,
            observed_at: 1,
            exit_code: Some(0),
        };
        concurrent
            .with_connection(|connection| {
                connection
                    .execute(
                        "UPDATE memories SET content='replacement' WHERE id='cas'",
                        [],
                    )
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
                Ok(())
            })
            .unwrap();
        let loaded_digest =
            MemoryStore::verification_target_digest_for(&loaded_memory, &loaded_fields, "repo")
                .unwrap();
        assert_eq!(loaded_digest, digest);
        assert_ne!(
            loaded_digest,
            primary.verification_target_digest("cas", "repo").unwrap()
        );
        assert!(primary.record_trusted_check_observation(&record).is_err());
        assert!(primary
            .trusted_check_observations("cas")
            .unwrap()
            .is_empty());

        let fields = primary.get_structured_fields("cas").unwrap().unwrap();
        let binding = VerificationCommitBinding {
            repository_id: "repo".into(),
            checkout_id: "checkout-main".into(),
            branch: "main".into(),
            target_digest: loaded_digest,
            observations: Vec::new(),
        };
        assert!(primary
            .persist_verification_result(
                "cas",
                &fields,
                MemoryVerificationStatus::Verified,
                false,
                None,
                2,
                Some(1),
                &binding
            )
            .is_err());
        assert_eq!(
            primary
                .get_by_id("cas")
                .unwrap()
                .unwrap()
                .verification_status,
            MemoryVerificationStatus::Unverified
        );
        let current_digest = primary.verification_target_digest("cas", "repo").unwrap();
        let empty_binding = VerificationCommitBinding {
            repository_id: "repo".into(),
            checkout_id: "checkout-main".into(),
            branch: "main".into(),
            target_digest: current_digest,
            observations: Vec::new(),
        };
        let mut newer_failure = record.clone();
        newer_failure.target_digest = current_digest;
        newer_failure.passed = false;
        newer_failure.observed_at = 2;
        concurrent
            .record_trusted_check_observation(&newer_failure)
            .unwrap();
        assert!(primary
            .persist_verification_result(
                "cas",
                &fields,
                MemoryVerificationStatus::Verified,
                false,
                None,
                3,
                Some(1),
                &empty_binding
            )
            .is_err());
        assert_eq!(
            primary
                .get_by_id("cas")
                .unwrap()
                .unwrap()
                .verification_status,
            MemoryVerificationStatus::Unverified
        );
    }

    #[test]
    fn verification_commit_failure_rolls_back_and_releases_file_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("memories.db");
        let store = MemoryStore::open(&path).unwrap();
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed) VALUES('commit-failure','claim','observation','repo',1,1)",[]).map_err(|e| LatticeError::Storage(e.to_string()))?;
            Ok(())
        }).unwrap();
        let fields = store
            .get_structured_fields("commit-failure")
            .unwrap()
            .unwrap();
        let binding = VerificationCommitBinding {
            repository_id: "repo".into(),
            checkout_id: "checkout-main".into(),
            branch: "main".into(),
            target_digest: store
                .verification_target_digest("commit-failure", "repo")
                .unwrap(),
            observations: Vec::new(),
        };

        store
            .fail_verification_commit_once
            .store(true, Ordering::SeqCst);
        assert!(store
            .persist_verification_result(
                "commit-failure",
                &fields,
                MemoryVerificationStatus::Verified,
                false,
                None,
                2,
                Some(1),
                &binding,
            )
            .is_err());
        assert_eq!(
            store
                .get_by_id("commit-failure")
                .unwrap()
                .unwrap()
                .verification_status,
            MemoryVerificationStatus::Unverified
        );
        let reopened = MemoryStore::open(&path).unwrap();
        assert_eq!(
            reopened
                .get_by_id("commit-failure")
                .unwrap()
                .unwrap()
                .verification_status,
            MemoryVerificationStatus::Unverified
        );
        store
            .persist_verification_result(
                "commit-failure",
                &fields,
                MemoryVerificationStatus::Verified,
                false,
                None,
                3,
                Some(1),
                &binding,
            )
            .unwrap();
        assert_eq!(
            reopened
                .get_by_id("commit-failure")
                .unwrap()
                .unwrap()
                .verification_status,
            MemoryVerificationStatus::Verified
        );
    }

    #[test]
    fn capture_transport_and_tombstone_retirement_are_page_bounded() {
        fn count(store: &MemoryStore, table: &str) -> i64 {
            store
                .with_connection(|connection| {
                    connection
                        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })
                        .map_err(|error| LatticeError::Storage(error.to_string()))
                })
                .unwrap()
        }
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .with_connection(|connection| {
                let tx = connection.unchecked_transaction().unwrap();
                for ordinal in 0..300 {
                    tx.execute(
                        "INSERT INTO session_digest_deliveries(delivery_key,repository_id,checkout_id,session_id,revision,segment,schema_version,payload_hash,extractor_version,normalized_fingerprint,candidate_count,committed_count,dropped_observation_count,created_at) VALUES(?1,'repo','checkout','session','revision',0,1,'hash','extractor','fingerprint',0,0,0,1)",
                        params![format!("delivery-{ordinal:03}")],
                    )
                    .unwrap();
                }
                tx.commit().unwrap();
                Ok(())
            })
            .unwrap();
        let policy = SessionCaptureRetentionPolicy::new(Duration::from_secs(1), 1).unwrap();
        let first = store.prune_session_captures("repo", policy, 10).unwrap();
        assert_eq!(first.deleted_capture_ids.len(), 256);
        assert_eq!(count(&store, SESSION_DIGEST_DELIVERIES_TABLE), 44);
        let second = store.prune_session_captures("repo", policy, 10).unwrap();
        assert_eq!(second.deleted_capture_ids.len(), 44);
        assert_eq!(count(&store, SESSION_DIGEST_DELIVERIES_TABLE), 0);
        assert_eq!(count(&store, SESSION_CAPTURE_TOMBSTONES_TABLE), 300);

        store
            .with_connection(|connection| {
                let tx = connection.unchecked_transaction().unwrap();
                for ordinal in 0..300 {
                    tx.execute(
                        "INSERT INTO session_digest_deliveries(delivery_key,repository_id,checkout_id,session_id,revision,segment,schema_version,payload_hash,extractor_version,normalized_fingerprint,candidate_count,committed_count,dropped_observation_count,created_at) VALUES(?1,'repo','checkout','session','revision',0,1,'hash','extractor','fingerprint',0,0,0,1)",
                        params![format!("second-delivery-{ordinal:03}")],
                    )
                    .unwrap();
                }
                tx.commit().unwrap();
                Ok(())
            })
            .unwrap();

        let after_replay_window = 11 + crate::memory::retention::MAX_REPLAY_AGE_SECS as i64;
        store
            .prune_session_captures("repo", policy, after_replay_window)
            .unwrap();
        assert_eq!(count(&store, SESSION_DIGEST_DELIVERIES_TABLE), 172);
        assert_eq!(count(&store, SESSION_CAPTURE_TOMBSTONES_TABLE), 300);
        store
            .prune_session_captures("repo", policy, after_replay_window)
            .unwrap();
        assert_eq!(count(&store, SESSION_DIGEST_DELIVERIES_TABLE), 44);
        assert_eq!(count(&store, SESSION_CAPTURE_TOMBSTONES_TABLE), 300);
        store
            .prune_session_captures("repo", policy, after_replay_window)
            .unwrap();
        assert_eq!(count(&store, SESSION_DIGEST_DELIVERIES_TABLE), 0);
        assert_eq!(count(&store, SESSION_CAPTURE_TOMBSTONES_TABLE), 300);
        let after_second_replay_window =
            after_replay_window + crate::memory::retention::MAX_REPLAY_AGE_SECS as i64 + 1;
        store
            .prune_session_captures("repo", policy, after_second_replay_window)
            .unwrap();
        assert_eq!(count(&store, SESSION_CAPTURE_TOMBSTONES_TABLE), 44);
        store
            .prune_session_captures("repo", policy, after_second_replay_window)
            .unwrap();
        assert_eq!(count(&store, SESSION_CAPTURE_TOMBSTONES_TABLE), 0);
    }
}
