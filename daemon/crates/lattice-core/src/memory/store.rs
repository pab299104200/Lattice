use super::model::{
    Memory, MemoryAccessRecord, MemoryAssertionType, MemoryClass, MemoryEvidence,
    MemoryFreshnessPolicy, MemoryLinkRecord, MemoryProvenance, MemoryScope, MemoryScoreKind,
    MemoryScoreRecord, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};
use crate::error::LatticeError;
use crate::verification::{
    allows as scope_allows, MemoryScopeFilteredEvent, ScopeFilter, ScopeFilterError,
};
use crate::working_memory::{
    load_latest_checkpoint_for_scope, save_checkpoint_for_scope, CheckpointId, CheckpointScope,
    WorkingMemoryState,
};
use crate::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MEMORY_DB_BUSY_TIMEOUT_SECS: u64 = 5;
const MEMORY_DB_AUTO_CHECKPOINT_PAGES: u32 = 100;
const MEMORY_DB_JOURNAL_SIZE_LIMIT_BYTES: u32 = 1_048_576;
const MEMORIES_FTS_TABLE: &str = "memories_fts";
const MEMORY_FTS_STATE_TABLE: &str = "memory_fts_state";

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
    conn: Connection,
    #[cfg(test)]
    direct_write_count: AtomicUsize,
}

impl MemoryStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|e| LatticeError::Storage(format!("Failed to open memory database: {}", e)))?;

        configure_connection(&conn, true)?;

        let store = Self {
            conn,
            #[cfg(test)]
            direct_write_count: AtomicUsize::new(0),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            LatticeError::Storage(format!("Failed to open in-memory memory database: {}", e))
        })?;

        configure_connection(&conn, false)?;

        let store = Self {
            conn,
            #[cfg(test)]
            direct_write_count: AtomicUsize::new(0),
        };
        store.initialize()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn reset_direct_write_count(&self) {
        self.direct_write_count.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn direct_write_count(&self) -> usize {
        self.direct_write_count.load(Ordering::Relaxed)
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
        op(&self.conn)
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
                    last_verified_graph_snapshot_id INTEGER
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
                );",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to initialize memory schema: {}", e))
            })?;

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
                    ON memory_scores(memory_id, score_kind, computed_at DESC);",
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

        Ok(())
    }

    /// Store a memory. If the memory's id is empty, a UUID-like id is generated.
    /// If created_at is 0, the current timestamp is used.
    /// Returns the id of the stored memory.
    pub fn store(&self, mut memory: Memory) -> Result<String, LatticeError> {
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
                     created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated)
                 VALUES
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                      ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
                      ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, 0)",
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
                ],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to store memory: {}", e)))?;

        self.sync_memory_evidence(&memory.id, &structured_fields.evidence)?;

        self.upsert_fts_row(&memory)?;

        Ok(memory.id)
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
        scope.validate().map_err(scope_filter_error)?;
        let predicate = scope_sql_predicate(scope);
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
                 WHERE memories.is_invalidated = 0
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
                 WHERE is_invalidated = 0
                   AND {}
                 ORDER BY created_at DESC
                 LIMIT ?",
                predicate.where_clause,
            );
            bind_values.push(Value::Integer(limit as i64));
            sql
        };

        let memories = self.query_memories_values(&sql, bind_values, "scoped memory query")?;
        self.enforce_scope_boundary(memories, scope, "scoped memory query")
    }

    pub fn list_all_scoped(&self, scope: &ScopeFilter) -> Result<Vec<Memory>, LatticeError> {
        scope.validate().map_err(scope_filter_error)?;
        let memories = self.list_all()?;
        self.filter_scope_boundary(memories, scope)
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
        if scope_allows(&memory, scope) {
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
        self.filter_scope_boundary(memories, scope)
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

    /// Decay confidence of memories that haven't been accessed recently.
    /// Reduces confidence by `decay_rate` for each memory not accessed in `stale_days` days.
    pub fn decay_old_memories(
        &self,
        stale_days: u64,
        decay_rate: f64,
    ) -> Result<usize, LatticeError> {
        let cutoff = now_epoch_secs().saturating_sub(stale_days * 86400);
        let count = self
            .conn
            .execute(
                "UPDATE memories SET confidence = MAX(0.1, confidence - ?1)
                 WHERE last_accessed < ?2 AND is_invalidated = 0 AND confidence > 0.1",
                params![decay_rate, cutoff as i64],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to decay memories: {}", e)))?;
        Ok(count)
    }

    /// Prune (archive) memories with low confidence that haven't been accessed in N days.
    pub fn prune_old_memories(
        &self,
        min_confidence: f64,
        stale_days: u64,
    ) -> Result<usize, LatticeError> {
        let cutoff = now_epoch_secs().saturating_sub(stale_days * 86400);
        self.set_fts_dirty(true)?;
        self.conn
            .execute(
                &format!(
                    "DELETE FROM {table}
                     WHERE memory_id IN (
                         SELECT id FROM memories
                         WHERE confidence < ?1 AND last_accessed < ?2 AND is_invalidated = 0
                     )",
                    table = MEMORIES_FTS_TABLE,
                ),
                params![min_confidence, cutoff as i64],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prune memory FTS entries: {}", e))
            })?;
        let count = self
            .conn
            .execute(
                "UPDATE memories SET is_invalidated = 1
                 WHERE confidence < ?1 AND last_accessed < ?2 AND is_invalidated = 0",
                params![min_confidence, cutoff as i64],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prune memories: {}", e)))?;
        self.set_fts_dirty(false)?;
        Ok(count)
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
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to initialize memory FTS state: {error}"))
            })
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
    ) -> Result<Vec<Memory>, LatticeError> {
        let mut allowed = Vec::with_capacity(memories.len());
        for memory in memories {
            if scope_allows(&memory, scope) {
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
        let mut allowed = Vec::with_capacity(memories.len());
        for memory in memories {
            if scope_allows(&memory, scope) {
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
                "SELECT expires_at
                 FROM memories
                 WHERE id = ?1
                 LIMIT 1",
                params![id],
                |row| {
                    Ok(ExistingVerificationMetadata {
                        expires_at: row
                            .get::<_, Option<i64>>(0)?
                            .map(DateTime::from_unix_seconds),
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
}

struct ScopeSqlPredicate {
    where_clause: String,
    bind_values: Vec<Value>,
}

#[derive(Default)]
struct ExistingVerificationMetadata {
    expires_at: Option<DateTime<Utc>>,
}

struct GraphDeltaReferenceFilter {
    kind: String,
    reference: String,
    is_like: bool,
}

fn scope_sql_predicate(scope: &ScopeFilter) -> ScopeSqlPredicate {
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

    ScopeSqlPredicate {
        where_clause: format!("({})", predicates.join(" OR ")),
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
        .map_err(|e| LatticeError::Storage(format!("Failed to set busy timeout: {}", e)))?;

    if enable_wal {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| LatticeError::Storage(format!("Failed to set WAL mode: {}", e)))?;
        conn.pragma_update(None, "wal_autocheckpoint", MEMORY_DB_AUTO_CHECKPOINT_PAGES)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to set WAL auto-checkpoint: {}", e))
            })?;
        conn.pragma_update(
            None,
            "journal_size_limit",
            MEMORY_DB_JOURNAL_SIZE_LIMIT_BYTES,
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to set journal size limit: {}", e)))?;
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|e| LatticeError::Storage(format!("Failed to checkpoint WAL: {}", e)))?;
    }

    Ok(())
}
