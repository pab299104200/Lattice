use std::cell::RefCell;
use std::collections::HashMap;
use rusqlite::{Connection, params};
use crate::error::LatticeError;

const CREATE_VECTORS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS vectors (
    file TEXT NOT NULL,
    name TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    embedding BLOB NOT NULL,
    PRIMARY KEY (file, name, byte_offset)
);
CREATE INDEX IF NOT EXISTS idx_vectors_file ON vectors(file);
"#;

/// Cache key: (file, name, byte_offset).
type CacheKey = (String, String, usize);

/// Stores embedding vectors as BLOBs in SQLite with cosine similarity search.
/// Maintains an in-memory cache for fast similarity lookups without SQLite overhead.
pub struct VectorStore {
    conn: Connection,
    /// In-memory vector cache for fast similarity search.
    /// Loaded via `load_cache()`, updated on upsert/delete.
    cache: RefCell<HashMap<CacheKey, Vec<f32>>>,
}

impl VectorStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &str) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|e| LatticeError::Storage(format!("Failed to open vector store: {}", e)))?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| LatticeError::Storage(format!("Failed to set WAL mode: {}", e)))?;

        Ok(Self { conn, cache: RefCell::new(HashMap::new()) })
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| LatticeError::Storage(format!("Failed to open in-memory vector store: {}", e)))?;

        Ok(Self { conn, cache: RefCell::new(HashMap::new()) })
    }

    /// Create the vectors table if it doesn't exist.
    /// The `_dimension` parameter is reserved for future use (e.g., validation).
    pub fn initialize(&self, _dimension: usize) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(CREATE_VECTORS_TABLE)
            .map_err(|e| LatticeError::Storage(format!("Failed to initialize vector schema: {}", e)))?;
        Ok(())
    }

    /// Load all vectors from SQLite into the in-memory cache.
    /// Call this after initialization to enable fast in-memory searches.
    pub fn load_cache(&self) -> Result<(), LatticeError> {
        let mut cache = self.cache.borrow_mut();
        cache.clear();
        let mut stmt = self.conn.prepare(
            "SELECT file, name, byte_offset, embedding FROM vectors"
        ).map_err(|e| LatticeError::Storage(format!("Failed to prepare cache load: {}", e)))?;

        let rows = stmt.query_map([], |row| {
            let file: String = row.get(0)?;
            let name: String = row.get(1)?;
            let offset: i64 = row.get(2)?;
            let blob: Vec<u8> = row.get(3)?;
            Ok((file, name, offset as usize, blob))
        }).map_err(|e| LatticeError::Storage(format!("Failed to load cache: {}", e)))?;

        for row in rows {
            let (file, name, offset, blob) =
                row.map_err(|e| LatticeError::Storage(format!("Failed to read cache row: {}", e)))?;
            cache.insert((file, name, offset), bytes_to_f32_slice(&blob));
        }
        Ok(())
    }

    /// Insert or replace a vector for a given (file, name, byte_offset) triple.
    pub fn upsert_vector(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        let blob = f32_slice_to_bytes(vector);

        self.conn
            .execute(
                "INSERT OR REPLACE INTO vectors (file, name, byte_offset, embedding) VALUES (?1, ?2, ?3, ?4)",
                params![file, name, byte_offset as i64, blob],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to upsert vector: {}", e)))?;

        // Update in-memory cache
        self.cache.borrow_mut().insert(
            (file.to_string(), name.to_string(), byte_offset),
            vector.to_vec(),
        );

        Ok(())
    }

    /// Delete all vectors belonging to a given file.
    pub fn delete_by_file(&self, file: &str) -> Result<(), LatticeError> {
        self.conn
            .execute("DELETE FROM vectors WHERE file = ?1", params![file])
            .map_err(|e| LatticeError::Storage(format!("Failed to delete vectors by file: {}", e)))?;

        // Remove from in-memory cache
        self.cache.borrow_mut().retain(|(f, _, _), _| f != file);

        Ok(())
    }

    /// Search for the top-k most similar vectors to the query using cosine similarity.
    /// Returns Vec<(name, file, byte_offset, similarity)> sorted by descending similarity.
    ///
    /// Uses the in-memory cache if populated; otherwise falls back to SQLite.
    pub fn search(
        &self,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(String, String, usize, f32)>, LatticeError> {
        let cache = self.cache.borrow();
        if !cache.is_empty() {
            // Fast path: iterate in-memory cache
            let mut results: Vec<(String, String, usize, f32)> = cache
                .iter()
                .map(|((file, name, offset), vec)| {
                    let similarity = cosine_similarity(query, vec);
                    (name.clone(), file.clone(), *offset, similarity)
                })
                .collect();

            results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
            results.truncate(top_k);
            return Ok(results);
        }
        drop(cache);

        // Slow path: query SQLite directly
        let mut stmt = self
            .conn
            .prepare("SELECT file, name, byte_offset, embedding FROM vectors")
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare search query: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                let file: String = row.get(0)?;
                let name: String = row.get(1)?;
                let byte_offset: i64 = row.get(2)?;
                let embedding_blob: Vec<u8> = row.get(3)?;
                Ok((file, name, byte_offset, embedding_blob))
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to execute search query: {}", e)))?;

        let mut results: Vec<(String, String, usize, f32)> = Vec::new();

        for row in rows {
            let (file, name, byte_offset, embedding_blob) =
                row.map_err(|e| LatticeError::Storage(format!("Failed to read vector row: {}", e)))?;

            let stored_vec = bytes_to_f32_slice(&embedding_blob);
            let similarity = cosine_similarity(query, &stored_vec);

            results.push((name, file, byte_offset as usize, similarity));
        }

        // Sort by descending similarity
        results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));

        // Take top-k
        results.truncate(top_k);

        Ok(results)
    }
}

/// Serialize a slice of f32 values to a byte vector (little-endian).
fn f32_slice_to_bytes(slice: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(slice.len() * 4);
    for &val in slice {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

/// Deserialize a byte slice to a vector of f32 values (little-endian).
fn bytes_to_f32_slice(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| {
            let arr: [u8; 4] = chunk.try_into().expect("chunk is exactly 4 bytes");
            f32::from_le_bytes(arr)
        })
        .collect()
}

/// Compute cosine similarity between two vectors: dot(a,b) / (norm(a) * norm(b)).
/// Returns 0.0 if either vector has zero magnitude.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a * norm_b)
}
