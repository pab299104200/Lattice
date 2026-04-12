use crate::error::LatticeError;
use crate::storage::vector_index::{VectorIndex, VectorScope, VectorSearchResult};
use crate::storage::vector_store::{
    cosine_similarity, decode_vector_name_scope, encode_vector_name_for_scope, StoredVectorRecord,
    VectorStore,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use tracing::{info, warn};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const ANN_OVERSAMPLE_FACTOR: usize = 4;

#[derive(serde::Serialize, serde::Deserialize)]
struct UsearchDiskMetadata {
    generation: u64,
    dimension: usize,
}

struct UsearchState {
    dimension: Option<usize>,
    index: Option<Index>,
    key_map: HashMap<u64, (String, String, usize)>,
    dirty: bool,
}

/// Approximate nearest-neighbor backend that keeps SQLite as the durable source
/// of truth for vectors and syncs a USearch graph index alongside it.
pub struct UsearchVectorIndex {
    store: VectorStore,
    index_path: PathBuf,
    metadata_path: PathBuf,
    state: Mutex<UsearchState>,
}

impl UsearchVectorIndex {
    pub fn open(sqlite_path: &str, index_path: PathBuf) -> Result<Self, LatticeError> {
        let store = VectorStore::open(sqlite_path)?;
        Ok(Self::from_store(store, index_path))
    }

    pub fn from_store(store: VectorStore, index_path: PathBuf) -> Self {
        let metadata_path = metadata_path_for(&index_path);
        Self {
            store,
            index_path,
            metadata_path,
            state: Mutex::new(UsearchState {
                dimension: None,
                index: None,
                key_map: HashMap::new(),
                dirty: false,
            }),
        }
    }

    fn rebuild_from_store(&self, dimension: usize) -> Result<(), LatticeError> {
        let records = self.store.load_all_records()?;
        let index = new_usearch_index(dimension)?;
        if !records.is_empty() {
            index.reserve(records.len()).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to reserve USearch capacity for {} vectors: {}",
                    records.len(),
                    e
                ))
            })?;
        }

        let mut key_map = HashMap::with_capacity(records.len());
        for record in records {
            add_record_to_index(&index, &mut key_map, record)?;
        }

        let mut state = self.lock_state("rebuild USearch index")?;
        state.dimension = Some(dimension);
        state.index = Some(index);
        state.key_map = key_map;
        state.dirty = true;
        drop(state);

        self.flush()
    }

    fn dimension(&self) -> Result<usize, LatticeError> {
        if let Some(dimension) = self.lock_state("load USearch dimension")?.dimension {
            return Ok(dimension);
        }

        self.store.configured_dimension()?.ok_or_else(|| {
            LatticeError::Storage(
                "USearch vector index is missing a configured embedding dimension".to_string(),
            )
        })
    }

    fn ensure_index_ready(&self) -> Result<(), LatticeError> {
        let has_index = self.lock_state("check USearch readiness")?.index.is_some();
        if has_index {
            return Ok(());
        }
        self.warm()
    }

    fn load_persisted_state(
        &self,
        dimension: usize,
        generation: u64,
    ) -> Result<bool, LatticeError> {
        if !self.index_path.exists() || !self.metadata_path.exists() {
            return Ok(false);
        }

        let metadata: UsearchDiskMetadata =
            serde_json::from_slice(&fs::read(&self.metadata_path).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to read USearch metadata '{}': {}",
                    self.metadata_path.display(),
                    e
                ))
            })?)
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to parse USearch metadata '{}': {}",
                    self.metadata_path.display(),
                    e
                ))
            })?;

        if metadata.generation != generation || metadata.dimension != dimension {
            return Ok(false);
        }

        let index = new_usearch_index(dimension)?;
        index
            .load(self.index_path.to_string_lossy().as_ref())
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to load USearch index '{}': {}",
                    self.index_path.display(),
                    e
                ))
            })?;

        let records = self.store.load_all_records()?;
        if index.size() != records.len() {
            return Ok(false);
        }

        let key_map = records
            .into_iter()
            .map(|record| {
                (
                    record.ann_key,
                    (record.name, record.file, record.byte_offset),
                )
            })
            .collect();

        let mut state = self.lock_state("hydrate USearch key map")?;
        state.dimension = Some(dimension);
        state.index = Some(index);
        state.key_map = key_map;
        state.dirty = false;
        Ok(true)
    }

    fn fallback_exact_search(
        &self,
        query: &[f32],
        top_k: usize,
        scope: VectorScope,
        reason: &str,
    ) -> Result<Vec<VectorSearchResult>, LatticeError> {
        warn!(
            "USearch semantic lookup fell back to SQLite exact search: {}",
            reason
        );
        self.store.search_in_scope(query, top_k, scope)
    }

    fn lock_state(&self, context: &str) -> Result<MutexGuard<'_, UsearchState>, LatticeError> {
        self.state.lock().map_err(|_| {
            LatticeError::Storage(format!(
                "USearch vector index mutex poisoned while trying to {}",
                context
            ))
        })
    }
}

impl VectorIndex for UsearchVectorIndex {
    fn initialize(&self, dimension: usize) -> Result<(), LatticeError> {
        self.store.initialize(dimension)?;
        let mut state = self.lock_state("initialize USearch index")?;
        state.dimension = Some(dimension);
        if state.index.is_none() {
            state.index = Some(new_usearch_index(dimension)?);
        }
        Ok(())
    }

    fn warm(&self) -> Result<(), LatticeError> {
        let dimension = self.dimension()?;
        let generation = self.store.current_generation()?;
        let count = self.store.vector_count()?;

        if count == 0 {
            let mut state = self.lock_state("warm empty USearch index")?;
            state.dimension = Some(dimension);
            state.index = Some(new_usearch_index(dimension)?);
            state.key_map.clear();
            state.dirty = false;
            let _ = fs::remove_file(&self.index_path);
            let _ = fs::remove_file(&self.metadata_path);
            return Ok(());
        }

        if self.load_persisted_state(dimension, generation)? {
            return Ok(());
        }

        self.rebuild_from_store(dimension)
    }

    fn upsert_vector(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        self.upsert_vector_in_scope(file, name, byte_offset, VectorScope::Symbol, vector)
    }

    fn upsert_vector_in_scope(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        scope: VectorScope,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        let ann_key =
            self.store
                .upsert_vector_with_key_in_scope(file, name, byte_offset, scope, vector)?;
        let dimension = self.dimension()?;
        let mut state = self.lock_state("upsert USearch vector")?;
        state.dimension = Some(dimension);
        if state.index.is_none() {
            state.index = Some(new_usearch_index(dimension)?);
        }

        let index = state.index.as_ref().expect("index set above");
        ensure_capacity(index, 1, file, name, byte_offset)?;
        if index.contains(ann_key) {
            index.remove(ann_key).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to replace existing USearch vector '{}:{}@{}': {}",
                    file, name, byte_offset, e
                ))
            })?;
        }
        index.add(ann_key, vector).map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to add vector to USearch for '{}:{}@{}': {}",
                file, name, byte_offset, e
            ))
        })?;

        state.key_map.insert(
            ann_key,
            (
                encode_vector_name_for_scope(name, scope),
                file.to_string(),
                byte_offset,
            ),
        );
        state.dirty = true;
        Ok(())
    }

    fn delete_by_file(&self, file: &str) -> Result<(), LatticeError> {
        let keys = self.store.ann_keys_for_file(file)?;
        self.store.delete_by_file(file)?;

        if keys.is_empty() {
            return Ok(());
        }

        let mut state = self.lock_state("delete USearch vectors by file")?;
        if let Some(index) = state.index.as_ref() {
            for key in &keys {
                if index.contains(*key) {
                    index.remove(*key).map_err(|e| {
                        LatticeError::Storage(format!(
                            "Failed to remove vector key {} from USearch: {}",
                            key, e
                        ))
                    })?;
                }
            }
        }
        for key in keys {
            state.key_map.remove(&key);
        }
        state.dirty = true;
        Ok(())
    }

    fn clear_all(&self) -> Result<(), LatticeError> {
        self.store.clear_all()?;

        let dimension = self
            .store
            .configured_dimension()?
            .or_else(|| {
                self.lock_state("read USearch dimension for clear")
                    .ok()?
                    .dimension
            })
            .ok_or_else(|| {
                LatticeError::Storage(
                    "Cannot clear USearch index before the embedding dimension is initialized"
                        .to_string(),
                )
            })?;

        let mut state = self.lock_state("clear USearch index")?;
        state.dimension = Some(dimension);
        state.index = Some(new_usearch_index(dimension)?);
        state.key_map.clear();
        state.dirty = true;
        Ok(())
    }

    fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<VectorSearchResult>, LatticeError> {
        self.search_in_scope(query, top_k, VectorScope::Symbol)
    }

    fn search_in_scope(
        &self,
        query: &[f32],
        top_k: usize,
        scope: VectorScope,
    ) -> Result<Vec<VectorSearchResult>, LatticeError> {
        if top_k == 0 {
            return Ok(Vec::new());
        }

        if let Err(err) = self.ensure_index_ready() {
            return self.fallback_exact_search(
                query,
                top_k,
                scope,
                &format!("warm-up failed: {}", err),
            );
        }

        let state = self.lock_state("search USearch index")?;
        let Some(index) = state.index.as_ref() else {
            return self.fallback_exact_search(query, top_k, scope, "index was not initialized");
        };

        if index.size() == 0 {
            return Ok(Vec::new());
        }

        let candidate_k = top_k
            .saturating_mul(ANN_OVERSAMPLE_FACTOR)
            .min(index.size())
            .max(top_k);

        let matches = match index.search(query, candidate_k) {
            Ok(matches) => matches,
            Err(err) => {
                drop(state);
                return self.fallback_exact_search(
                    query,
                    top_k,
                    scope,
                    &format!("ann search failed: {}", err),
                );
            }
        };

        let mut results = Vec::with_capacity(matches.keys.len());
        for (key, distance) in matches.keys.iter().zip(matches.distances.iter()) {
            let Some((name, file, byte_offset)) = state.key_map.get(key) else {
                drop(state);
                return self.fallback_exact_search(
                    query,
                    top_k,
                    scope,
                    &format!("missing metadata for ann key {}", key),
                );
            };
            let (stored_scope, logical_name) = decode_vector_name_scope(name);
            if !scope_matches(stored_scope, scope) {
                continue;
            }

            let mut stored_vec = Vec::new();
            let similarity = match index.export::<f32>(*key, &mut stored_vec) {
                Ok(_) => cosine_similarity(query, &stored_vec),
                Err(err) => {
                    warn!(
                        "Failed to export USearch vector {} for rerank, using approximate score: {}",
                        key, err
                    );
                    (1.0 - distance).clamp(-1.0, 1.0)
                }
            };

            results.push((logical_name, file.clone(), *byte_offset, similarity));
        }

        results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
    }

    fn flush(&self) -> Result<(), LatticeError> {
        let generation = self.store.current_generation()?;
        let mut state = self.lock_state("flush USearch index")?;
        if !state.dirty {
            return Ok(());
        }

        if state.key_map.is_empty() {
            let index_bytes_before = file_size_bytes(&self.index_path);
            let metadata_bytes_before = file_size_bytes(&self.metadata_path);
            let _ = fs::remove_file(&self.index_path);
            let _ = fs::remove_file(&self.metadata_path);
            state.dirty = false;
            info!(
                implementation = self.implementation_name(),
                vectors = 0usize,
                generation,
                index_bytes_before,
                metadata_bytes_before,
                index_bytes_after = 0u64,
                metadata_bytes_after = 0u64,
                index_bytes_delta = -(index_bytes_before as i64),
                metadata_bytes_delta = -(metadata_bytes_before as i64),
                "USearch index flushed and cleared"
            );
            return Ok(());
        }

        let index = state.index.as_ref().ok_or_else(|| {
            LatticeError::Storage(
                "USearch index was marked dirty but no in-memory index was available".to_string(),
            )
        })?;
        let dimension = state.dimension.ok_or_else(|| {
            LatticeError::Storage(
                "USearch index was marked dirty without a configured dimension".to_string(),
            )
        })?;
        let vector_count = state.key_map.len();
        let index_bytes_before = file_size_bytes(&self.index_path);
        let metadata_bytes_before = file_size_bytes(&self.metadata_path);

        if let Some(parent) = self.index_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to create USearch directory '{}': {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        index
            .save(self.index_path.to_string_lossy().as_ref())
            .map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to save USearch index '{}': {}",
                    self.index_path.display(),
                    e
                ))
            })?;

        let metadata = UsearchDiskMetadata {
            generation,
            dimension,
        };
        fs::write(
            &self.metadata_path,
            serde_json::to_vec_pretty(&metadata).map_err(|e| {
                LatticeError::Storage(format!("Failed to serialize USearch metadata: {}", e))
            })?,
        )
        .map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to write USearch metadata '{}': {}",
                self.metadata_path.display(),
                e
            ))
        })?;

        let index_bytes_after = file_size_bytes(&self.index_path);
        let metadata_bytes_after = file_size_bytes(&self.metadata_path);
        info!(
            implementation = self.implementation_name(),
            vectors = vector_count,
            dimension,
            generation,
            index_bytes_before,
            index_bytes_after,
            index_bytes_delta = index_bytes_after as i64 - index_bytes_before as i64,
            metadata_bytes_before,
            metadata_bytes_after,
            metadata_bytes_delta = metadata_bytes_after as i64 - metadata_bytes_before as i64,
            "USearch index flush complete"
        );

        state.dirty = false;
        Ok(())
    }

    fn implementation_name(&self) -> &'static str {
        "usearch-ann"
    }
}

fn add_record_to_index(
    index: &Index,
    key_map: &mut HashMap<u64, (String, String, usize)>,
    record: StoredVectorRecord,
) -> Result<(), LatticeError> {
    index.add(record.ann_key, &record.vector).map_err(|e| {
        LatticeError::Storage(format!(
            "Failed to add vector to rebuilt USearch index for '{}:{}@{}': {}",
            record.file, record.name, record.byte_offset, e
        ))
    })?;
    key_map.insert(
        record.ann_key,
        (record.name, record.file, record.byte_offset),
    );
    Ok(())
}

fn new_usearch_index(dimension: usize) -> Result<Index, LatticeError> {
    let mut options = IndexOptions::default();
    options.dimensions = dimension;
    options.metric = MetricKind::Cos;
    options.quantization = ScalarKind::F32;
    Index::new(&options).map_err(|e| {
        LatticeError::Storage(format!(
            "Failed to create USearch index with {} dimensions: {}",
            dimension, e
        ))
    })
}

fn ensure_capacity(
    index: &Index,
    additional: usize,
    file: &str,
    name: &str,
    byte_offset: usize,
) -> Result<(), LatticeError> {
    let required = index.size().saturating_add(additional);
    if required <= index.capacity() {
        return Ok(());
    }

    let growth_floor = index.capacity().max(32);
    let target = required.max(growth_floor.saturating_mul(2));
    index.reserve(target).map_err(|e| {
        LatticeError::Storage(format!(
            "Failed to grow USearch capacity for '{}:{}@{}' to {} vectors: {}",
            file, name, byte_offset, target, e
        ))
    })
}

fn metadata_path_for(index_path: &Path) -> PathBuf {
    let parent = index_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let file_name = index_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vectors.usearch");
    parent.join(format!("{}.meta.json", file_name))
}

fn file_size_bytes(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn scope_matches(stored: VectorScope, requested: VectorScope) -> bool {
    matches!(requested, VectorScope::All) || stored == requested
}
