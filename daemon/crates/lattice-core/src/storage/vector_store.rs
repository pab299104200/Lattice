use crate::error::LatticeError;
use crate::storage::vector_index::{VectorIndex, VectorScope, VectorSearchResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

const CREATE_VECTORS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS vectors (
    file TEXT NOT NULL,
    name TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    embedding BLOB NOT NULL,
    PRIMARY KEY (file, name, byte_offset)
);
CREATE INDEX IF NOT EXISTS idx_vectors_file ON vectors(file);

CREATE TABLE IF NOT EXISTS vector_keys (
    ann_key INTEGER PRIMARY KEY,
    file TEXT NOT NULL,
    name TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    UNIQUE (file, name, byte_offset)
);
CREATE INDEX IF NOT EXISTS idx_vector_keys_file ON vector_keys(file);

CREATE TABLE IF NOT EXISTS vector_index_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const VECTOR_META_DIMENSION: &str = "dimension";
const VECTOR_META_GENERATION: &str = "generation";
const FILE_SUMMARY_SCOPE_PREFIX: &str = "__lattice_file_summary__::";

type CacheKey = (String, String, usize);

pub(crate) struct StoredVectorRecord {
    pub ann_key: u64,
    pub file: String,
    pub name: String,
    pub byte_offset: usize,
    pub vector: Vec<f32>,
}

struct SqliteVectorState {
    conn: Connection,
    cache: HashMap<CacheKey, Vec<f32>>,
}

/// Compatibility vector backend that stores embeddings as SQLite BLOBs and
/// performs an exact brute-force cosine search.
pub struct VectorStore {
    state: Mutex<SqliteVectorState>,
}

impl VectorStore {
    pub fn open(path: &str) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|e| LatticeError::Storage(format!("Failed to open vector store: {}", e)))?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| LatticeError::Storage(format!("Failed to set WAL mode: {}", e)))?;

        Ok(Self {
            state: Mutex::new(SqliteVectorState {
                conn,
                cache: HashMap::new(),
            }),
        })
    }

    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            LatticeError::Storage(format!("Failed to open in-memory vector store: {}", e))
        })?;

        Ok(Self {
            state: Mutex::new(SqliteVectorState {
                conn,
                cache: HashMap::new(),
            }),
        })
    }

    pub fn initialize(&self, dimension: usize) -> Result<(), LatticeError> {
        let state = self.lock_state("initialize vector store")?;
        state
            .conn
            .execute_batch(CREATE_VECTORS_SCHEMA)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to initialize vector schema: {}", e))
            })?;

        ensure_meta_value(&state.conn, VECTOR_META_GENERATION, "0")?;
        ensure_dimension(&state.conn, dimension)?;
        backfill_vector_keys(&state.conn)?;
        remove_orphaned_vector_keys(&state.conn)?;

        Ok(())
    }

    pub fn load_cache(&self) -> Result<(), LatticeError> {
        let mut state = self.lock_state("load vector cache")?;
        state.cache.clear();

        let cached_rows = {
            let mut stmt = state
                .conn
                .prepare("SELECT file, name, byte_offset, embedding FROM vectors")
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to prepare cache load: {}", e))
                })?;

            let rows = stmt
                .query_map([], |row| {
                    let file: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    let offset: i64 = row.get(2)?;
                    let blob: Vec<u8> = row.get(3)?;
                    Ok((file, name, offset as usize, blob))
                })
                .map_err(|e| LatticeError::Storage(format!("Failed to load cache: {}", e)))?;

            let mut cached_rows = Vec::new();
            for row in rows {
                cached_rows.push(row.map_err(|e| {
                    LatticeError::Storage(format!("Failed to read cache row: {}", e))
                })?);
            }
            cached_rows
        };

        for (file, name, offset, blob) in cached_rows {
            state
                .cache
                .insert((file, name, offset), bytes_to_f32_slice(&blob));
        }

        Ok(())
    }

    pub fn upsert_vector(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        self.upsert_vector_in_scope(file, name, byte_offset, VectorScope::Symbol, vector)
    }

    pub fn upsert_vector_in_scope(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        scope: VectorScope,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        self.upsert_vector_with_key_in_scope(file, name, byte_offset, scope, vector)
            .map(|_| ())
    }

    pub(crate) fn upsert_vector_with_key_in_scope(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        scope: VectorScope,
        vector: &[f32],
    ) -> Result<u64, LatticeError> {
        let mut state = self.lock_state("upsert vector")?;
        let blob = f32_slice_to_bytes(vector);
        let stored_name = encode_vector_name_for_scope(name, scope);

        let tx = state.conn.transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to start vector upsert transaction: {}", e))
        })?;
        let ann_key = load_or_create_ann_key(&tx, file, &stored_name, byte_offset)?;
        tx.execute(
            "INSERT INTO vectors (file, name, byte_offset, embedding)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(file, name, byte_offset) DO UPDATE SET embedding = excluded.embedding",
            params![file, stored_name, byte_offset as i64, blob],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to upsert vector: {}", e)))?;
        bump_generation(&tx)?;
        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit vector upsert: {}", e)))?;

        state.cache.insert(
            (file.to_string(), stored_name, byte_offset),
            vector.to_vec(),
        );

        Ok(ann_key)
    }

    pub fn delete_by_file(&self, file: &str) -> Result<(), LatticeError> {
        let mut state = self.lock_state("delete vectors by file")?;
        let tx = state.conn.transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to start vector delete transaction: {}", e))
        })?;

        let deleted = tx
            .execute("DELETE FROM vectors WHERE file = ?1", params![file])
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to delete vectors by file '{}': {}",
                    file, e
                ))
            })?;
        tx.execute("DELETE FROM vector_keys WHERE file = ?1", params![file])
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to delete vector keys for file '{}': {}",
                    file, e
                ))
            })?;
        if deleted > 0 {
            bump_generation(&tx)?;
        }
        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit vector delete: {}", e)))?;

        state
            .cache
            .retain(|(cached_file, _, _), _| cached_file != file);
        Ok(())
    }

    pub fn clear_all(&self) -> Result<(), LatticeError> {
        let mut state = self.lock_state("clear vector store")?;
        let tx = state.conn.transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to start vector clear transaction: {}", e))
        })?;

        let deleted = tx
            .execute("DELETE FROM vectors", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear vectors: {}", e)))?;
        tx.execute("DELETE FROM vector_keys", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear vector keys: {}", e)))?;
        if deleted > 0 {
            bump_generation(&tx)?;
        }
        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit vector clear: {}", e)))?;

        state.cache.clear();
        Ok(())
    }

    pub fn search(
        &self,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<VectorSearchResult>, LatticeError> {
        self.search_in_scope(query, top_k, VectorScope::Symbol)
    }

    pub fn search_in_scope(
        &self,
        query: &[f32],
        top_k: usize,
        scope: VectorScope,
    ) -> Result<Vec<VectorSearchResult>, LatticeError> {
        if top_k == 0 {
            return Ok(Vec::new());
        }

        let state = self.lock_state("search vectors")?;
        if !state.cache.is_empty() {
            let mut results: Vec<VectorSearchResult> = state
                .cache
                .iter()
                .filter_map(|((file, stored_name, offset), vec)| {
                    let (stored_scope, logical_name) = decode_vector_name_scope(stored_name);
                    if !scope_matches(stored_scope, scope) {
                        return None;
                    }
                    let similarity = cosine_similarity(query, vec);
                    Some((logical_name, file.clone(), *offset, similarity))
                })
                .collect();

            results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
            results.truncate(top_k);
            return Ok(results);
        }

        let mut stmt = state
            .conn
            .prepare("SELECT file, name, byte_offset, embedding FROM vectors")
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare search query: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                let file: String = row.get(0)?;
                let name: String = row.get(1)?;
                let byte_offset: i64 = row.get(2)?;
                let embedding_blob: Vec<u8> = row.get(3)?;
                Ok((file, name, byte_offset as usize, embedding_blob))
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to execute search query: {}", e)))?;

        let mut results: Vec<VectorSearchResult> = Vec::new();
        for row in rows {
            let (file, stored_name, byte_offset, embedding_blob) = row
                .map_err(|e| LatticeError::Storage(format!("Failed to read vector row: {}", e)))?;
            let (stored_scope, logical_name) = decode_vector_name_scope(&stored_name);
            if !scope_matches(stored_scope, scope) {
                continue;
            }
            let stored_vec = bytes_to_f32_slice(&embedding_blob);
            let similarity = cosine_similarity(query, &stored_vec);
            results.push((logical_name, file, byte_offset, similarity));
        }

        results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
    }

    pub(crate) fn load_all_records(&self) -> Result<Vec<StoredVectorRecord>, LatticeError> {
        let state = self.lock_state("load vectors for ann sync")?;
        let mut stmt = state
            .conn
            .prepare(
                "SELECT k.ann_key, v.file, v.name, v.byte_offset, v.embedding
                 FROM vectors v
                 INNER JOIN vector_keys k
                   ON k.file = v.file
                  AND k.name = v.name
                  AND k.byte_offset = v.byte_offset",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare vector record load: {}", e))
            })?;

        let rows = stmt
            .query_map([], |row| {
                let ann_key: i64 = row.get(0)?;
                let file: String = row.get(1)?;
                let name: String = row.get(2)?;
                let byte_offset: i64 = row.get(3)?;
                let embedding: Vec<u8> = row.get(4)?;
                Ok(StoredVectorRecord {
                    ann_key: ann_key as u64,
                    file,
                    name,
                    byte_offset: byte_offset as usize,
                    vector: bytes_to_f32_slice(&embedding),
                })
            })
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to query vector records for ann sync: {}",
                    e
                ))
            })?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|e| {
                LatticeError::Storage(format!("Failed to read vector record: {}", e))
            })?);
        }

        Ok(records)
    }

    pub(crate) fn ann_keys_for_file(&self, file: &str) -> Result<Vec<u64>, LatticeError> {
        let state = self.lock_state("load ann keys by file")?;
        let mut stmt = state
            .conn
            .prepare("SELECT ann_key FROM vector_keys WHERE file = ?1")
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare ann-key lookup: {}", e))
            })?;

        let rows = stmt
            .query_map(params![file], |row| {
                let key: i64 = row.get(0)?;
                Ok(key as u64)
            })
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to query ann keys for file '{}': {}",
                    file, e
                ))
            })?;

        let mut keys = Vec::new();
        for row in rows {
            keys.push(row.map_err(|e| {
                LatticeError::Storage(format!("Failed to read ann key for file '{}': {}", file, e))
            })?);
        }
        Ok(keys)
    }

    pub(crate) fn vector_count(&self) -> Result<usize, LatticeError> {
        let state = self.lock_state("count vectors")?;
        let count: i64 = state
            .conn
            .query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0))
            .map_err(|e| LatticeError::Storage(format!("Failed to count vectors: {}", e)))?;
        Ok(count as usize)
    }

    pub(crate) fn configured_dimension(&self) -> Result<Option<usize>, LatticeError> {
        let state = self.lock_state("load vector dimension")?;
        meta_u64(&state.conn, VECTOR_META_DIMENSION).map(|value| value.map(|v| v as usize))
    }

    pub(crate) fn current_generation(&self) -> Result<u64, LatticeError> {
        let state = self.lock_state("load vector generation")?;
        Ok(meta_u64(&state.conn, VECTOR_META_GENERATION)?.unwrap_or(0))
    }

    fn lock_state(&self, context: &str) -> Result<MutexGuard<'_, SqliteVectorState>, LatticeError> {
        self.state.lock().map_err(|_| {
            LatticeError::Storage(format!(
                "Vector store mutex poisoned while trying to {}",
                context
            ))
        })
    }
}

pub(crate) fn encode_vector_name_for_scope(name: &str, scope: VectorScope) -> String {
    match scope {
        VectorScope::Symbol | VectorScope::All => name.to_string(),
        VectorScope::FileSummary => format!("{FILE_SUMMARY_SCOPE_PREFIX}{name}"),
    }
}

pub(crate) fn decode_vector_name_scope(stored_name: &str) -> (VectorScope, String) {
    if let Some(rest) = stored_name.strip_prefix(FILE_SUMMARY_SCOPE_PREFIX) {
        return (VectorScope::FileSummary, rest.to_string());
    }
    (VectorScope::Symbol, stored_name.to_string())
}

fn scope_matches(stored: VectorScope, requested: VectorScope) -> bool {
    matches!(requested, VectorScope::All) || stored == requested
}

impl VectorIndex for VectorStore {
    fn initialize(&self, dimension: usize) -> Result<(), LatticeError> {
        VectorStore::initialize(self, dimension)
    }

    fn warm(&self) -> Result<(), LatticeError> {
        self.load_cache()
    }

    fn upsert_vector(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        VectorStore::upsert_vector(self, file, name, byte_offset, vector)
    }

    fn upsert_vector_in_scope(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        scope: VectorScope,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        VectorStore::upsert_vector_in_scope(self, file, name, byte_offset, scope, vector)
    }

    fn delete_by_file(&self, file: &str) -> Result<(), LatticeError> {
        VectorStore::delete_by_file(self, file)
    }

    fn clear_all(&self) -> Result<(), LatticeError> {
        VectorStore::clear_all(self)
    }

    fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<VectorSearchResult>, LatticeError> {
        VectorStore::search(self, query, top_k)
    }

    fn search_in_scope(
        &self,
        query: &[f32],
        top_k: usize,
        scope: VectorScope,
    ) -> Result<Vec<VectorSearchResult>, LatticeError> {
        VectorStore::search_in_scope(self, query, top_k, scope)
    }

    fn implementation_name(&self) -> &'static str {
        "sqlite-exact"
    }
}

fn ensure_dimension(conn: &Connection, dimension: usize) -> Result<(), LatticeError> {
    if dimension == 0 {
        return Err(LatticeError::Storage(
            "Vector store dimension must be greater than zero".to_string(),
        ));
    }

    match meta_u64(conn, VECTOR_META_DIMENSION)? {
        Some(existing) if existing != dimension as u64 => Err(LatticeError::Storage(format!(
            "Vector store dimension mismatch: database has {}, requested {}",
            existing, dimension
        ))),
        Some(_) => Ok(()),
        None => conn
            .execute(
                "INSERT INTO vector_index_meta (key, value) VALUES (?1, ?2)",
                params![VECTOR_META_DIMENSION, dimension.to_string()],
            )
            .map(|_| ())
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to persist vector dimension: {}", e))
            }),
    }
}

fn ensure_meta_value(conn: &Connection, key: &str, value: &str) -> Result<(), LatticeError> {
    conn.execute(
        "INSERT OR IGNORE INTO vector_index_meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|e| {
        LatticeError::Storage(format!(
            "Failed to initialize vector metadata '{}': {}",
            key, e
        ))
    })
}

fn meta_u64(conn: &Connection, key: &str) -> Result<Option<u64>, LatticeError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM vector_index_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| {
            LatticeError::Storage(format!("Failed to read vector metadata '{}': {}", key, e))
        })?;

    raw.map(|value| {
        value.parse::<u64>().map_err(|e| {
            LatticeError::Storage(format!(
                "Invalid vector metadata '{}' value '{}': {}",
                key, value, e
            ))
        })
    })
    .transpose()
}

fn backfill_vector_keys(conn: &Connection) -> Result<(), LatticeError> {
    conn.execute(
        "INSERT OR IGNORE INTO vector_keys (file, name, byte_offset)
         SELECT file, name, byte_offset FROM vectors",
        [],
    )
    .map(|_| ())
    .map_err(|e| LatticeError::Storage(format!("Failed to backfill vector keys: {}", e)))
}

fn remove_orphaned_vector_keys(conn: &Connection) -> Result<(), LatticeError> {
    conn.execute(
        "DELETE FROM vector_keys
         WHERE NOT EXISTS (
             SELECT 1
             FROM vectors
             WHERE vectors.file = vector_keys.file
               AND vectors.name = vector_keys.name
               AND vectors.byte_offset = vector_keys.byte_offset
         )",
        [],
    )
    .map(|_| ())
    .map_err(|e| LatticeError::Storage(format!("Failed to clean orphaned vector keys: {}", e)))
}

fn load_or_create_ann_key(
    tx: &Transaction<'_>,
    file: &str,
    name: &str,
    byte_offset: usize,
) -> Result<u64, LatticeError> {
    let existing: Option<i64> = tx
        .query_row(
            "SELECT ann_key FROM vector_keys
             WHERE file = ?1 AND name = ?2 AND byte_offset = ?3",
            params![file, name, byte_offset as i64],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to look up vector key for '{}:{}@{}': {}",
                file, name, byte_offset, e
            ))
        })?;

    if let Some(key) = existing {
        return Ok(key as u64);
    }

    tx.execute(
        "INSERT INTO vector_keys (file, name, byte_offset) VALUES (?1, ?2, ?3)",
        params![file, name, byte_offset as i64],
    )
    .map_err(|e| {
        LatticeError::Storage(format!(
            "Failed to create vector key for '{}:{}@{}': {}",
            file, name, byte_offset, e
        ))
    })?;

    Ok(tx.last_insert_rowid() as u64)
}

fn bump_generation(tx: &Transaction<'_>) -> Result<(), LatticeError> {
    tx.execute(
        "UPDATE vector_index_meta
         SET value = CAST(value AS INTEGER) + 1
         WHERE key = ?1",
        params![VECTOR_META_GENERATION],
    )
    .map(|_| ())
    .map_err(|e| LatticeError::Storage(format!("Failed to bump vector generation: {}", e)))
}

fn f32_slice_to_bytes(slice: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(slice.len() * 4);
    for &val in slice {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

pub(crate) fn bytes_to_f32_slice(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| {
            let arr: [u8; 4] = chunk.try_into().expect("chunk is exactly 4 bytes");
            f32::from_le_bytes(arr)
        })
        .collect()
}

pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a * norm_b)
}
