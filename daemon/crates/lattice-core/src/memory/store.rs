use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use rusqlite::{Connection, params};
use crate::error::LatticeError;
use super::model::{Memory, MemoryType};

/// SQLite-backed store for session memories.
pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|e| LatticeError::Storage(format!("Failed to open memory database: {}", e)))?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| LatticeError::Storage(format!("Failed to set WAL mode: {}", e)))?;

        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| LatticeError::Storage(format!("Failed to open in-memory memory database: {}", e)))?;

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
                    confidence      REAL NOT NULL DEFAULT 1.0,
                    linked_symbols  TEXT NOT NULL DEFAULT '[]',
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
            .map_err(|e| LatticeError::Storage(format!("Failed to initialize memory schema: {}", e)))?;

        // Migration: add session_id column if upgrading from older schema
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN session_id TEXT NOT NULL DEFAULT ''",
            [],
        );

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

        let linked_json = serde_json::to_string(&memory.linked_symbols)
            .map_err(|e| LatticeError::Storage(format!("Failed to serialize linked_symbols: {}", e)))?;

        self.conn
            .execute(
                "INSERT OR REPLACE INTO memories
                    (id, session_id, content, memory_type, confidence, linked_symbols, source_query,
                     created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0)",
                params![
                    memory.id,
                    memory.session_id,
                    memory.content,
                    memory.memory_type.as_str(),
                    memory.confidence,
                    linked_json,
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
                "SELECT id, session_id, content, memory_type, confidence, linked_symbols, source_query,
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
                    confidence: row.get(4)?,
                    linked_json: row.get(5)?,
                    source_query: row.get(6)?,
                    created_at: row.get(7)?,
                    last_accessed: row.get(8)?,
                    access_count: row.get(9)?,
                    is_stale: row.get(10)?,
                    stale_reason: row.get(11)?,
                })
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query memories: {}", e)))?;

        let mut memories = Vec::new();
        for row in rows {
            let r = row.map_err(|e| LatticeError::Storage(format!("Failed to read memory row: {}", e)))?;
            memories.push(r.into_memory());
        }

        Ok(memories)
    }

    /// Search memories by keyword (LIKE match on content). Excludes invalidated.
    pub fn search_by_keyword(&self, keyword: &str) -> Result<Vec<Memory>, LatticeError> {
        let pattern = format!("%{}%", keyword);

        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, content, memory_type, confidence, linked_symbols, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
                 FROM memories
                 WHERE is_invalidated = 0 AND content LIKE ?1
                 ORDER BY created_at DESC",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare search query: {}", e)))?;

        let rows = stmt
            .query_map(params![pattern], |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    content: row.get(2)?,
                    memory_type_str: row.get(3)?,
                    confidence: row.get(4)?,
                    linked_json: row.get(5)?,
                    source_query: row.get(6)?,
                    created_at: row.get(7)?,
                    last_accessed: row.get(8)?,
                    access_count: row.get(9)?,
                    is_stale: row.get(10)?,
                    stale_reason: row.get(11)?,
                })
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to search memories: {}", e)))?;

        let mut memories = Vec::new();
        for row in rows {
            let r = row.map_err(|e| LatticeError::Storage(format!("Failed to read memory row: {}", e)))?;
            memories.push(r.into_memory());
        }

        // Touch each returned memory to update last_accessed
        for mem in &memories {
            let _ = self.touch_memory(&mem.id);
        }

        Ok(memories)
    }

    /// Mark all memories that reference a given symbol as stale with the provided reason.
    /// Uses LIKE match on the linked_symbols JSON column.
    pub fn mark_stale_by_symbol(&self, symbol_name: &str, reason: &str) -> Result<u64, LatticeError> {
        // The linked_symbols column stores JSON arrays like ["foo","bar"].
        // We match symbol names contained inside the JSON string.
        let pattern = format!("%\"{}\"%" , symbol_name);

        let updated = self
            .conn
            .execute(
                "UPDATE memories SET is_stale = 1, stale_reason = ?1
                 WHERE is_invalidated = 0 AND linked_symbols LIKE ?2",
                params![reason, pattern],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to mark stale: {}", e)))?;

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

    /// Decay confidence of memories that haven't been accessed recently.
    /// Reduces confidence by `decay_rate` for each memory not accessed in `stale_days` days.
    pub fn decay_old_memories(&self, stale_days: u64, decay_rate: f64) -> Result<usize, LatticeError> {
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
    pub fn prune_old_memories(&self, min_confidence: f64, stale_days: u64) -> Result<usize, LatticeError> {
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
    pub fn get_session_memories(&self, session_id: &str, limit: usize) -> Result<Vec<Memory>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, content, memory_type, confidence, linked_symbols, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
                 FROM memories
                 WHERE is_invalidated = 0 AND session_id = ?1
                 ORDER BY created_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare session query: {}", e)))?;

        let rows = stmt
            .query_map(params![session_id, limit as i64], |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    content: row.get(2)?,
                    memory_type_str: row.get(3)?,
                    confidence: row.get(4)?,
                    linked_json: row.get(5)?,
                    source_query: row.get(6)?,
                    created_at: row.get(7)?,
                    last_accessed: row.get(8)?,
                    access_count: row.get(9)?,
                    is_stale: row.get(10)?,
                    stale_reason: row.get(11)?,
                })
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query session memories: {}", e)))?;

        let mut memories = Vec::new();
        for row in rows {
            let r = row.map_err(|e| LatticeError::Storage(format!("Failed to read memory row: {}", e)))?;
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
        let pattern = format!("%{}%", keyword);

        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(excl) = exclude_session {
            (
                "SELECT id, session_id, content, memory_type, confidence, linked_symbols, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
                 FROM memories
                 WHERE is_invalidated = 0 AND content LIKE ?1 AND session_id != ?2
                 ORDER BY created_at DESC
                 LIMIT ?3",
                vec![Box::new(pattern), Box::new(excl.to_string()), Box::new(limit as i64)],
            )
        } else {
            (
                "SELECT id, session_id, content, memory_type, confidence, linked_symbols, source_query,
                        created_at, last_accessed, access_count, is_stale, stale_reason
                 FROM memories
                 WHERE is_invalidated = 0 AND content LIKE ?1
                 ORDER BY created_at DESC
                 LIMIT ?2",
                vec![Box::new(pattern), Box::new(limit as i64)],
            )
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare cross-session query: {}", e)))?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    content: row.get(2)?,
                    memory_type_str: row.get(3)?,
                    confidence: row.get(4)?,
                    linked_json: row.get(5)?,
                    source_query: row.get(6)?,
                    created_at: row.get(7)?,
                    last_accessed: row.get(8)?,
                    access_count: row.get(9)?,
                    is_stale: row.get(10)?,
                    stale_reason: row.get(11)?,
                })
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to search across sessions: {}", e)))?;

        let mut memories = Vec::new();
        for row in rows {
            let r = row.map_err(|e| LatticeError::Storage(format!("Failed to read memory row: {}", e)))?;
            memories.push(r.into_memory());
        }

        // Touch returned memories
        for mem in &memories {
            let _ = self.touch_memory(&mem.id);
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
    confidence: f64,
    linked_json: String,
    source_query: Option<String>,
    created_at: i64,
    last_accessed: i64,
    access_count: i64,
    is_stale: i32,
    stale_reason: Option<String>,
}

impl MemoryRow {
    fn into_memory(self) -> Memory {
        let linked_symbols: Vec<String> = serde_json::from_str(&self.linked_json)
            .unwrap_or_default();
        Memory {
            id: self.id,
            session_id: self.session_id,
            content: self.content,
            memory_type: MemoryType::from_str(&self.memory_type_str),
            confidence: self.confidence,
            linked_symbols,
            source_query: self.source_query,
            created_at: self.created_at as u64,
            last_accessed: self.last_accessed as u64,
            access_count: self.access_count as u32,
            is_stale: self.is_stale != 0,
            stale_reason: self.stale_reason,
        }
    }
}

/// Generate a simple UUID-like identifier (without external crate dependency).
fn generate_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    let mut hasher = DefaultHasher::new();
    now.as_nanos().hash(&mut hasher);
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
