use super::model::{
    Memory, MemoryAssertionType, MemoryEvidence, MemoryFreshnessPolicy, MemoryProvenance,
    MemoryScope, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};
use crate::error::LatticeError;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MEMORY_DB_BUSY_TIMEOUT_SECS: u64 = 5;
const MEMORY_DB_AUTO_CHECKPOINT_PAGES: u32 = 100;
const MEMORY_DB_JOURNAL_SIZE_LIMIT_BYTES: u32 = 1_048_576;
const MEMORIES_FTS_TABLE: &str = "memories_fts";

/// SQLite-backed store for session memories.
pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|e| LatticeError::Storage(format!("Failed to open memory database: {}", e)))?;

        configure_connection(&conn, true)?;

        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            LatticeError::Storage(format!("Failed to open in-memory memory database: {}", e))
        })?;

        configure_connection(&conn, false)?;

        let store = Self { conn };
        store.initialize()?;
        Ok(store)
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
                    refresh_key     TEXT,
                    source_query    TEXT,
                    assertion_type  TEXT NOT NULL DEFAULT 'observation',
                    verification_status TEXT NOT NULL DEFAULT 'unverified',
                    confidence_reason TEXT,
                    supersedes_memory_id TEXT,
                    superseded_by_memory_id TEXT,
                    contradicts_memory_ids TEXT NOT NULL DEFAULT '[]',
                    contradicted_by_memory_ids TEXT NOT NULL DEFAULT '[]',
                    freshness_policy TEXT NOT NULL DEFAULT 'session_scoped',
                    freshness_policy_detail TEXT,
                    provenance_json TEXT NOT NULL DEFAULT '[]',
                    evidence_json   TEXT NOT NULL DEFAULT '[]',
                    created_at      INTEGER NOT NULL,
                    last_accessed   INTEGER NOT NULL,
                    access_count    INTEGER NOT NULL DEFAULT 0,
                    is_stale        INTEGER NOT NULL DEFAULT 0,
                    stale_reason    TEXT,
                    is_invalidated  INTEGER NOT NULL DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS idx_memories_created
                    ON memories(created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_memories_type
                    ON memories(memory_type);",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to initialize memory schema: {}", e))
            })?;

        // Migration: add session_id column if upgrading from older schema
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN session_id TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'session'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN linked_files TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE memories ADD COLUMN workspace_id TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE memories ADD COLUMN branch TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE memories ADD COLUMN refresh_key TEXT", []);
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN assertion_type TEXT NOT NULL DEFAULT 'observation'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN verification_status TEXT NOT NULL DEFAULT 'unverified'",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE memories ADD COLUMN confidence_reason TEXT", []);
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN supersedes_memory_id TEXT",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN superseded_by_memory_id TEXT",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN contradicts_memory_ids TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN contradicted_by_memory_ids TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN freshness_policy TEXT NOT NULL DEFAULT 'session_scoped'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN freshness_policy_detail TEXT",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN provenance_json TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN evidence_json TEXT NOT NULL DEFAULT '[]'",
            [],
        );

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
                 CREATE INDEX IF NOT EXISTS idx_memories_superseded_by
                    ON memories(superseded_by_memory_id);",
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

        self.rebuild_fts()?;

        Ok(())
    }

    /// Store a memory. If the memory's id is empty, a UUID-like id is generated.
    /// If created_at is 0, the current timestamp is used.
    /// Returns the id of the stored memory.
    pub fn store(&self, mut memory: Memory) -> Result<String, LatticeError> {
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

        self.conn
            .execute(
                "INSERT OR REPLACE INTO memories
                    (id, session_id, content, memory_type, scope, confidence, linked_symbols, linked_files,
                     workspace_id, branch, refresh_key, source_query,
                     assertion_type, verification_status, confidence_reason, supersedes_memory_id,
                     superseded_by_memory_id, contradicts_memory_ids, contradicted_by_memory_ids,
                     freshness_policy, freshness_policy_detail, provenance_json, evidence_json,
                     created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated)
                 VALUES
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                      ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                      ?25, ?26, ?27, ?28, 0)",
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
                    memory.refresh_key,
                    memory.source_query,
                    structured_fields.assertion_type.as_str(),
                    structured_fields.verification_status.as_str(),
                    structured_fields.confidence_reason,
                    structured_fields.supersedes_memory_id,
                    structured_fields.superseded_by_memory_id,
                    contradicts_json,
                    contradicted_by_json,
                    structured_fields.freshness_policy.as_str(),
                    structured_fields.freshness_policy_detail,
                    provenance_json,
                    evidence_json,
                    memory.created_at as i64,
                    memory.last_accessed as i64,
                    memory.access_count as i64,
                    memory.is_stale as i32,
                    memory.stale_reason,
                ],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to store memory: {}", e)))?;

        self.upsert_fts_row(&memory)?;

        Ok(memory.id)
    }

    /// List all non-invalidated memories, ordered by created_at DESC.
    pub fn list_all(&self) -> Result<Vec<Memory>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
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
                    refresh_key: row.get(10)?,
                    source_query: row.get(11)?,
                    created_at: row.get(12)?,
                    last_accessed: row.get(13)?,
                    access_count: row.get(14)?,
                    is_stale: row.get(15)?,
                    stale_reason: row.get(16)?,
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
                        linked_files, workspace_id, branch, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
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
                    refresh_key: row.get(10)?,
                    source_query: row.get(11)?,
                    created_at: row.get(12)?,
                    last_accessed: row.get(13)?,
                    access_count: row.get(14)?,
                    is_stale: row.get(15)?,
                    stale_reason: row.get(16)?,
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
                        linked_files, workspace_id, branch, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
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
                    refresh_key: row.get(10)?,
                    source_query: row.get(11)?,
                    created_at: row.get(12)?,
                    last_accessed: row.get(13)?,
                    access_count: row.get(14)?,
                    is_stale: row.get(15)?,
                    stale_reason: row.get(16)?,
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
                "SELECT assertion_type, verification_status, confidence_reason,
                        supersedes_memory_id, superseded_by_memory_id,
                        contradicts_memory_ids, contradicted_by_memory_ids,
                        freshness_policy, freshness_policy_detail,
                        provenance_json, evidence_json
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

    /// Replace structured assertion metadata for an existing, non-invalidated memory.
    pub fn update_structured_fields(
        &self,
        id: &str,
        fields: &MemoryStructuredFields,
    ) -> Result<(), LatticeError> {
        self.persist_structured_fields(id, fields)
    }

    /// Mark a memory as superseded by another memory id and downgrade verification state.
    pub fn mark_memory_superseded(
        &self,
        id: &str,
        superseded_by_memory_id: &str,
    ) -> Result<(), LatticeError> {
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

    /// Search memories by keyword (per-word AND match on content + linked_symbols). Excludes invalidated.
    pub fn search_by_keyword(&self, keyword: &str) -> Result<Vec<Memory>, LatticeError> {
        let memories = if let Some(fts_query) = build_fts_query(keyword) {
            let sql = format!(
                "SELECT memories.id, memories.session_id, memories.content, memories.memory_type,
                        memories.scope, memories.confidence, memories.linked_symbols,
                        memories.linked_files, memories.workspace_id, memories.branch,
                        memories.refresh_key, memories.source_query, memories.created_at,
                        memories.last_accessed, memories.access_count, memories.is_stale,
                        memories.stale_reason
                 FROM memories
                 INNER JOIN {table}
                    ON {table}.memory_id = memories.id
                 WHERE memories.is_invalidated = 0
                   AND {table} MATCH ?1
                 ORDER BY memories.created_at DESC",
                table = MEMORIES_FTS_TABLE,
            );
            self.query_memories(&sql, params![fts_query], "search memories")?
        } else {
            self.query_memories(
                "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                        linked_files, workspace_id, branch, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
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

    /// Mark all memories that reference a given file as stale with the provided reason.
    /// Uses LIKE match on the linked_files JSON column.
    pub fn mark_stale_by_file(&self, file_path: &str, reason: &str) -> Result<u64, LatticeError> {
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
        let count = self
            .conn
            .execute("DELETE FROM memories", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear memories: {}", e)))?;
        self.conn
            .execute(&format!("DELETE FROM {}", MEMORIES_FTS_TABLE), [])
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to clear memory FTS index: {}", e))
            })?;
        Ok(count)
    }

    /// Soft-delete a memory by setting is_invalidated = 1.
    pub fn invalidate(&self, id: &str) -> Result<(), LatticeError> {
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
                        linked_files, workspace_id, branch, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
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
                    refresh_key: row.get(10)?,
                    source_query: row.get(11)?,
                    created_at: row.get(12)?,
                    last_accessed: row.get(13)?,
                    access_count: row.get(14)?,
                    is_stale: row.get(15)?,
                    stale_reason: row.get(16)?,
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
                            memories.refresh_key, memories.source_query, memories.created_at,
                            memories.last_accessed, memories.access_count, memories.is_stale,
                            memories.stale_reason
                     FROM memories
                     INNER JOIN {table}
                        ON {table}.memory_id = memories.id
                     WHERE memories.is_invalidated = 0
                       AND memories.session_id != ?1
                       AND {table} MATCH ?2
                     ORDER BY memories.created_at DESC
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
                            memories.refresh_key, memories.source_query, memories.created_at,
                            memories.last_accessed, memories.access_count, memories.is_stale,
                            memories.stale_reason
                     FROM memories
                     INNER JOIN {table}
                        ON {table}.memory_id = memories.id
                     WHERE memories.is_invalidated = 0
                       AND {table} MATCH ?1
                     ORDER BY memories.created_at DESC
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
                        linked_files, workspace_id, branch, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
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
                        linked_files, workspace_id, branch, refresh_key, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
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
                        memories.refresh_key, memories.source_query, memories.created_at,
                        memories.last_accessed, memories.access_count, memories.is_stale,
                        memories.stale_reason
                 FROM memories
                 INNER JOIN {table}
                    ON {table}.memory_id = memories.id
                 WHERE memories.is_invalidated = 0
                   AND memories.is_stale = 1
                   AND {table} MATCH ?1
                 ORDER BY memories.created_at DESC
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
                    linked_files, workspace_id, branch, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason
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
        let provenance_json = serde_json::to_string(&fields.provenance).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize provenance metadata: {}", e))
        })?;
        let evidence_json = serde_json::to_string(&fields.evidence).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize evidence metadata: {}", e))
        })?;

        let updated = self
            .conn
            .execute(
                "UPDATE memories
                 SET assertion_type = ?1,
                     verification_status = ?2,
                     confidence_reason = ?3,
                     supersedes_memory_id = ?4,
                     superseded_by_memory_id = ?5,
                     contradicts_memory_ids = ?6,
                     contradicted_by_memory_ids = ?7,
                     freshness_policy = ?8,
                     freshness_policy_detail = ?9,
                     provenance_json = ?10,
                     evidence_json = ?11
                 WHERE id = ?12 AND is_invalidated = 0",
                params![
                    fields.assertion_type.as_str(),
                    fields.verification_status.as_str(),
                    fields.confidence_reason,
                    fields.supersedes_memory_id,
                    fields.superseded_by_memory_id,
                    contradicts_json,
                    contradicted_by_json,
                    fields.freshness_policy.as_str(),
                    fields.freshness_policy_detail,
                    provenance_json,
                    evidence_json,
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

        Ok(())
    }

    fn rebuild_fts(&self) -> Result<(), LatticeError> {
        self.conn
            .execute(&format!("DELETE FROM {}", MEMORIES_FTS_TABLE), [])
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to rebuild memory FTS index: {}", e))
            })?;

        for memory in self.list_all()? {
            self.upsert_fts_row(&memory)?;
        }

        Ok(())
    }

    fn upsert_fts_row(&self, memory: &Memory) -> Result<(), LatticeError> {
        let document = build_memory_search_document(memory);
        self.delete_fts_row(&memory.id)?;
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
    refresh_key: Option<String>,
    source_query: Option<String>,
    created_at: i64,
    last_accessed: i64,
    access_count: i64,
    is_stale: i32,
    stale_reason: Option<String>,
}

struct StructuredMemoryRow {
    assertion_type_str: String,
    verification_status_str: String,
    confidence_reason: Option<String>,
    supersedes_memory_id: Option<String>,
    superseded_by_memory_id: Option<String>,
    contradicts_memory_ids_json: String,
    contradicted_by_memory_ids_json: String,
    freshness_policy_str: String,
    freshness_policy_detail: Option<String>,
    provenance_json: String,
    evidence_json: String,
}

fn structured_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StructuredMemoryRow> {
    Ok(StructuredMemoryRow {
        assertion_type_str: row.get(0)?,
        verification_status_str: row.get(1)?,
        confidence_reason: row.get(2)?,
        supersedes_memory_id: row.get(3)?,
        superseded_by_memory_id: row.get(4)?,
        contradicts_memory_ids_json: row.get(5)?,
        contradicted_by_memory_ids_json: row.get(6)?,
        freshness_policy_str: row.get(7)?,
        freshness_policy_detail: row.get(8)?,
        provenance_json: row.get(9)?,
        evidence_json: row.get(10)?,
    })
}

impl StructuredMemoryRow {
    fn into_structured_fields(self) -> MemoryStructuredFields {
        MemoryStructuredFields {
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
            provenance: serde_json::from_str(&self.provenance_json).unwrap_or_default(),
            evidence: serde_json::from_str(&self.evidence_json).unwrap_or_default(),
        }
    }
}

fn memory_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
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
        refresh_key: row.get(10)?,
        source_query: row.get(11)?,
        created_at: row.get(12)?,
        last_accessed: row.get(13)?,
        access_count: row.get(14)?,
        is_stale: row.get(15)?,
        stale_reason: row.get(16)?,
    })
}

impl MemoryRow {
    fn into_memory(self) -> Memory {
        let linked_symbols: Vec<String> =
            serde_json::from_str(&self.linked_json).unwrap_or_default();
        let linked_files: Vec<String> =
            serde_json::from_str(&self.linked_files_json).unwrap_or_default();
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
            refresh_key: self.refresh_key,
            source_query: self.source_query,
            created_at: self.created_at as u64,
            last_accessed: self.last_accessed as u64,
            access_count: self.access_count as u32,
            is_stale: self.is_stale != 0,
            stale_reason: self.stale_reason,
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
    if memory.is_stale {
        return MemoryVerificationStatus::Stale;
    }

    if let Some(query) = memory.source_query.as_deref() {
        if query_is_verification_signal(query) {
            return MemoryVerificationStatus::Verified;
        }
    }

    if memory.confidence >= 0.95 {
        MemoryVerificationStatus::InReview
    } else {
        MemoryVerificationStatus::Unverified
    }
}

fn has_extended_assertion_type(fields: &MemoryStructuredFields) -> bool {
    matches!(
        fields.assertion_type,
        MemoryAssertionType::WorkflowOutcome | MemoryAssertionType::Constraint
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
        });
    }

    for file in &memory.linked_files {
        evidence.push(MemoryEvidence {
            kind: "file".to_string(),
            reference: Some(file.clone()),
            detail: None,
            captured_at: Some(memory.created_at),
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
