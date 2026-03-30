use super::model::{Memory, MemoryScope, MemoryType};
use crate::error::LatticeError;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MEMORY_DB_BUSY_TIMEOUT_SECS: u64 = 5;
const MEMORY_DB_AUTO_CHECKPOINT_PAGES: u32 = 100;
const MEMORY_DB_JOURNAL_SIZE_LIMIT_BYTES: u32 = 1_048_576;

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
                    ON memories(memory_type);
                CREATE INDEX IF NOT EXISTS idx_memories_session
                    ON memories(session_id);",
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

        self.conn
            .execute(
                "INSERT OR REPLACE INTO memories
                    (id, session_id, content, memory_type, scope, confidence, linked_symbols, linked_files,
                     workspace_id, branch, refresh_key, source_query, created_at, last_accessed,
                     access_count, is_stale, stale_reason, is_invalidated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, 0)",
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
                    memory.created_at as i64,
                    memory.last_accessed as i64,
                    memory.access_count as i64,
                    memory.is_stale as i32,
                    memory.stale_reason,
                ],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to store memory: {}", e)))?;

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
                 ORDER BY created_at DESC
                 LIMIT 1",
            )
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to prepare refresh-key lookup query: {}",
                    e
                ))
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

    /// Search memories by keyword (per-word AND match on content + linked_symbols). Excludes invalidated.
    pub fn search_by_keyword(&self, keyword: &str) -> Result<Vec<Memory>, LatticeError> {
        // Split into individual words — each must match content or linked_symbols
        let words: Vec<String> = keyword
            .split_whitespace()
            .filter(|w| w.len() >= 2)
            .map(|w| {
                let escaped = w.replace('%', "\\%").replace('_', "\\_");
                format!("%{}%", escaped)
            })
            .collect();

        let mut where_clauses = vec!["is_invalidated = 0".to_string()];
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1usize;

        for pattern in &words {
            where_clauses.push(format!(
                "(content LIKE ?{p} ESCAPE '\\' OR linked_symbols LIKE ?{p} ESCAPE '\\' OR linked_files LIKE ?{p} ESCAPE '\\')",
                p = param_idx
            ));
            params_vec.push(Box::new(pattern.clone()));
            param_idx += 1;
        }

        let sql = format!(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason
             FROM memories
             WHERE {}
             ORDER BY created_at DESC",
            where_clauses.join(" AND ")
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let memories = {
            let mut stmt = self.conn.prepare(&sql).map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare search query: {}", e))
            })?;

            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
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
                .map_err(|e| LatticeError::Storage(format!("Failed to search memories: {}", e)))?;

            let mut memories = Vec::new();
            for row in rows {
                let r = row.map_err(|e| {
                    LatticeError::Storage(format!("Failed to read memory row: {}", e))
                })?;
                memories.push(r.into_memory());
            }
            memories
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
                "UPDATE memories SET is_stale = 1, stale_reason = ?1
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
                "UPDATE memories SET is_stale = 1, stale_reason = ?1
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
        Ok(())
    }

    /// Update the content of a memory in-place. Clears stale flags since the content is now fresh.
    pub fn update_content(&self, id: &str, new_content: &str) -> Result<(), LatticeError> {
        let updated = self
            .conn
            .execute(
                "UPDATE memories SET content = ?1, is_stale = 0, stale_reason = NULL
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
                         is_stale = 0,
                         stale_reason = NULL
                     WHERE id = ?6 AND is_invalidated = 0",
                params![
                    scope.as_str(),
                    linked_files_json,
                    workspace_id,
                    branch,
                    refresh_key,
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
                         is_stale = 0,
                         stale_reason = NULL
                     WHERE id = ?5 AND is_invalidated = 0",
                params![scope.as_str(), workspace_id, branch, refresh_key, id],
            )
        }
        .map_err(|e| LatticeError::Storage(format!("Failed to promote memory: {}", e)))?;

        if updated == 0 {
            return Err(LatticeError::Storage(format!(
                "Memory '{}' not found or invalidated",
                id
            )));
        }

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
        // Split query into individual words and AND them together.
        // "project architecture patterns" → content LIKE '%project%' AND content LIKE '%architecture%' AND content LIKE '%patterns%'
        // Also search linked_symbols JSON column for each word.
        let words: Vec<String> = keyword
            .split_whitespace()
            .filter(|w| w.len() >= 2) // skip single-char noise
            .map(|w| {
                let escaped = w.replace('%', "\\%").replace('_', "\\_");
                format!("%{}%", escaped)
            })
            .collect();

        let mut where_clauses = vec!["is_invalidated = 0".to_string()];
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1usize;

        // Per-word: match content OR linked_symbols
        for pattern in &words {
            where_clauses.push(format!(
                "(content LIKE ?{p} ESCAPE '\\' OR linked_symbols LIKE ?{p} ESCAPE '\\' OR linked_files LIKE ?{p} ESCAPE '\\')",
                p = param_idx
            ));
            params_vec.push(Box::new(pattern.clone()));
            param_idx += 1;
        }

        if let Some(excl) = exclude_session {
            where_clauses.push(format!("session_id != ?{}", param_idx));
            params_vec.push(Box::new(excl.to_string()));
            param_idx += 1;
        }

        let sql = format!(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason
             FROM memories
             WHERE {}
             ORDER BY created_at DESC
             LIMIT ?{}",
            where_clauses.join(" AND "),
            param_idx
        );
        params_vec.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let memories = {
            let mut stmt = self.conn.prepare(&sql).map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare cross-session query: {}", e))
            })?;

            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
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
                    LatticeError::Storage(format!("Failed to search across sessions: {}", e))
                })?;

            let mut memories = Vec::new();
            for row in rows {
                let r = row.map_err(|e| {
                    LatticeError::Storage(format!("Failed to read memory row: {}", e))
                })?;
                memories.push(r.into_memory());
            }
            memories
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
        let words: Vec<String> = query
            .unwrap_or("")
            .split_whitespace()
            .filter(|w| w.len() >= 2)
            .map(|w| {
                let escaped = w.replace('%', "\\%").replace('_', "\\_");
                format!("%{}%", escaped)
            })
            .collect();

        let mut where_clauses = vec!["is_invalidated = 0".to_string(), "is_stale = 1".to_string()];
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1usize;

        for pattern in &words {
            where_clauses.push(format!(
                "(content LIKE ?{p} ESCAPE '\\' OR linked_symbols LIKE ?{p} ESCAPE '\\' OR linked_files LIKE ?{p} ESCAPE '\\')",
                p = param_idx
            ));
            params_vec.push(Box::new(pattern.clone()));
            param_idx += 1;
        }

        let sql = format!(
            "SELECT id, session_id, content, memory_type, scope, confidence, linked_symbols,
                    linked_files, workspace_id, branch, refresh_key, source_query,
                    created_at, last_accessed, access_count, is_stale, stale_reason
             FROM memories
             WHERE {}
             ORDER BY created_at DESC
             LIMIT ?{}",
            where_clauses.join(" AND "),
            param_idx
        );
        params_vec.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql).map_err(|e| {
            LatticeError::Storage(format!("Failed to prepare stale memory query: {}", e))
        })?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
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
            .map_err(|e| LatticeError::Storage(format!("Failed to list stale memories: {}", e)))?;

        let mut memories = Vec::new();
        for row in rows {
            let r = row.map_err(|e| {
                LatticeError::Storage(format!("Failed to read stale memory row: {}", e))
            })?;
            memories.push(r.into_memory());
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
