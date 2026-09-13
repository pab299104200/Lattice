use crate::error::LatticeError;
use crate::storage::vector_index::{VectorIndex, VectorScope, VectorSearchResult};
use crate::storage::vector_store::{
    cosine_similarity, decode_vector_name_scope, encode_vector_name_for_scope, StoredVectorRecord,
    VectorStore,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, MutexGuard,
};
use tracing::{info, warn};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const ANN_OVERSAMPLE_FACTOR: usize = 4;
const MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 64 * 1024;
static MANAGED_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
thread_local! {
    static BEFORE_REPLACE: std::cell::RefCell<Option<Box<dyn FnOnce(&super::SecureDir, &str) -> std::io::Result<()>>>> = const { std::cell::RefCell::new(None) };
}

fn read_managed(
    directory: &super::SecureDir,
    leaf: &str,
    limit: u64,
) -> Result<Vec<u8>, LatticeError> {
    let file = directory
        .open_file(leaf, false)
        .map_err(|e| LatticeError::Storage(e.to_string()))?;
    if file
        .metadata()
        .map_err(|e| LatticeError::Storage(e.to_string()))?
        .len()
        > limit
    {
        return Err(LatticeError::Storage(format!(
            "USearch {leaf} exceeds {limit}-byte read limit"
        )));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| LatticeError::Storage(e.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(LatticeError::Storage(format!(
            "USearch {leaf} grew beyond {limit}-byte read limit"
        )));
    }
    Ok(bytes)
}

fn replace_managed(
    directory: &super::SecureDir,
    leaf: &str,
    bytes: &[u8],
) -> Result<(), LatticeError> {
    let sequence = MANAGED_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = format!(".{leaf}.{}.{sequence}.tmp", std::process::id());
    let mut file = directory
        .open_new_file(&temp)
        .map_err(|e| LatticeError::Storage(e.to_string()))?;
    let source =
        super::SecureDir::file_identity(&file).map_err(|e| LatticeError::Storage(e.to_string()))?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(test)]
        BEFORE_REPLACE.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook(directory, &temp)?;
            }
            Ok::<(), std::io::Error>(())
        })?;
        let destination = directory.metadata(leaf)?.map(|entry| entry.identity);
        directory.replace_from(&temp, directory, leaf, source, destination)?;
        directory.sync()
    })();
    if let Err(error) = result {
        if let Err(cleanup_error) = directory.remove_file(&temp, source) {
            if cleanup_error.kind() != std::io::ErrorKind::NotFound {
                warn!(stage = %temp, %cleanup_error, "USearch staging cleanup failed; preserving unproven entry");
            }
        }
        return Err(LatticeError::Storage(format!(
            "Publish USearch {leaf}: {error}"
        )));
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UsearchDiskMetadata {
    index_sha256: String,
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
    publication: Option<(
        super::cache_publication::CachePublicationAuthority,
        String,
        String,
    )>,
    publication_error: Option<String>,
}

impl UsearchVectorIndex {
    fn remove_persisted_files(&self) -> Result<(), LatticeError> {
        if let Some(error) = &self.publication_error {
            return Err(LatticeError::Storage(format!(
                "Cannot access managed USearch persistence: {error}"
            )));
        }
        if let Some((authority, index_leaf, metadata_leaf)) = &self.publication {
            let directory = authority
                .cache_dir()
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            for leaf in [index_leaf, metadata_leaf] {
                if let Some(meta) = directory
                    .metadata(leaf)
                    .map_err(|e| LatticeError::Storage(e.to_string()))?
                {
                    directory
                        .remove_file(leaf, meta.identity)
                        .map_err(|e| LatticeError::Storage(e.to_string()))?;
                }
            }
        } else {
            let _ = fs::remove_file(&self.index_path);
            let _ = fs::remove_file(&self.metadata_path);
        }
        Ok(())
    }
    pub fn open(sqlite_path: &str, index_path: PathBuf) -> Result<Self, LatticeError> {
        let store = VectorStore::open(sqlite_path)?;
        Ok(Self::from_store(store, index_path))
    }

    pub fn from_store(store: VectorStore, index_path: PathBuf) -> Self {
        let metadata_path = metadata_path_for(&index_path);
        let (publication, publication_error) =
            match super::cache_publication::CachePublicationAuthority::for_path(&index_path) {
                Ok(Some((authority, leaf))) => {
                    let metadata_leaf = metadata_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("vectors.usearch.meta.json")
                        .to_owned();
                    (Some((authority, leaf, metadata_leaf)), None)
                }
                Ok(None) => (None, None),
                Err(error) => (None, Some(error.to_string())),
            };
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
            publication,
            publication_error,
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
        if let Some(error) = &self.publication_error {
            return Err(LatticeError::Storage(format!(
                "Cannot access managed USearch persistence: {error}"
            )));
        }
        if let Some((authority, index_leaf, metadata_leaf)) = &self.publication {
            let directory = authority
                .cache_dir()
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            let (Some(_), Some(_)) = (
                directory
                    .metadata(index_leaf)
                    .map_err(|e| LatticeError::Storage(e.to_string()))?,
                directory
                    .metadata(metadata_leaf)
                    .map_err(|e| LatticeError::Storage(e.to_string()))?,
            ) else {
                return Ok(false);
            };
            let metadata_bytes = read_managed(&directory, metadata_leaf, MAX_METADATA_BYTES)?;
            let metadata: UsearchDiskMetadata = match serde_json::from_slice(&metadata_bytes) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warn!(%error, "Invalid USearch metadata; rebuilding from committed vectors");
                    return Ok(false);
                }
            };
            if metadata.generation != generation || metadata.dimension != dimension {
                return Ok(false);
            }
            let index = new_usearch_index(dimension)?;
            let bytes = read_managed(&directory, index_leaf, MAX_INDEX_BYTES)?;
            if format!("{:x}", Sha256::digest(&bytes)) != metadata.index_sha256 {
                warn!("USearch index/metadata digest mismatch; rebuilding from committed vectors");
                return Ok(false);
            }
            if let Err(error) = index.load_from_buffer(&bytes) {
                warn!(%error, "Invalid USearch index; rebuilding from committed vectors");
                return Ok(false);
            }
            return self.install_loaded_index(index, dimension);
        }
        if !self.index_path.exists() || !self.metadata_path.exists() {
            return Ok(false);
        }

        let directory = super::SecureDir::open(self.index_path.parent().unwrap_or(Path::new(".")))
            .map_err(|error| LatticeError::Storage(error.to_string()))?;
        let metadata_leaf = self
            .metadata_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| LatticeError::Storage("Invalid USearch metadata filename".into()))?;
        let metadata: UsearchDiskMetadata = match serde_json::from_slice(&read_managed(
            &directory,
            metadata_leaf,
            MAX_METADATA_BYTES,
        )?) {
            Ok(metadata) => metadata,
            Err(error) => {
                warn!(%error, "Invalid USearch metadata; rebuilding from committed vectors");
                return Ok(false);
            }
        };
        if metadata.generation != generation || metadata.dimension != dimension {
            return Ok(false);
        }
        let index_leaf = self
            .index_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| LatticeError::Storage("Invalid USearch index filename".into()))?;
        let bytes = read_managed(&directory, index_leaf, MAX_INDEX_BYTES)?;
        if format!("{:x}", Sha256::digest(&bytes)) != metadata.index_sha256 {
            return Ok(false);
        }
        let index = new_usearch_index(dimension)?;
        if let Err(error) = index.load_from_buffer(&bytes) {
            warn!(%error, "Invalid USearch index; rebuilding from committed vectors");
            return Ok(false);
        }

        self.install_loaded_index(index, dimension)
    }

    fn install_loaded_index(&self, index: Index, dimension: usize) -> Result<bool, LatticeError> {
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
    fn begin_publication(
        &self,
    ) -> Result<Box<dyn super::vector_index::VectorPublicationLease + '_>, LatticeError> {
        self.store.begin_publication()
    }
    fn bind_embedding_identity(&self, identity: &str) -> Result<(), LatticeError> {
        self.store.bind_embedding_identity(identity)?;
        self.warm()
    }

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
            self.remove_persisted_files()?;
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
        if let Some(error) = &self.publication_error {
            return Err(LatticeError::Storage(format!(
                "Cannot publish managed USearch persistence: {error}"
            )));
        }
        let _accounting = if self.store.publication_active() {
            None
        } else {
            super::CachePublicationGuard::for_path(&self.index_path)?
        };

        if state.key_map.is_empty() {
            let index_bytes_before = file_size_bytes(&self.index_path);
            let metadata_bytes_before = file_size_bytes(&self.metadata_path);
            self.remove_persisted_files()?;
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

        if let Some(parent) = self
            .index_path
            .parent()
            .filter(|_| self.publication.is_none())
        {
            fs::create_dir_all(parent).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to create USearch directory '{}': {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        if index.serialized_length() as u64 > MAX_INDEX_BYTES {
            return Err(LatticeError::Storage(format!(
                "USearch index exceeds {MAX_INDEX_BYTES}-byte publication limit"
            )));
        }
        let mut index_bytes = vec![0; index.serialized_length()];
        index.save_to_buffer(&mut index_bytes).map_err(|error| {
            LatticeError::Storage(format!("Failed to serialize USearch index: {error}"))
        })?;
        let metadata = UsearchDiskMetadata {
            index_sha256: format!("{:x}", Sha256::digest(&index_bytes)),
            generation,
            dimension,
        };
        let metadata_bytes = serde_json::to_vec_pretty(&metadata).map_err(|e| {
            LatticeError::Storage(format!("Failed to serialize USearch metadata: {}", e))
        })?;
        if let Some((authority, index_leaf, metadata_leaf)) = &self.publication {
            let directory = authority
                .cache_dir()
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            replace_managed(&directory, index_leaf, &index_bytes)?;
            replace_managed(&directory, metadata_leaf, &metadata_bytes)?;
        } else {
            fs::write(&self.index_path, &index_bytes).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to save USearch index '{}': {e}",
                    self.index_path.display()
                ))
            })?;
            fs::write(&self.metadata_path, metadata_bytes).map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to write USearch metadata '{}': {e}",
                    self.metadata_path.display()
                ))
            })?;
        }

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

#[cfg(test)]
mod managed_publication_tests {
    use super::*;

    #[test]
    fn failed_publication_cleans_owned_stage_and_preserves_destination() {
        let root = tempfile::tempdir().unwrap();
        let directory = crate::storage::SecureDir::open(root.path()).unwrap();
        fs::write(root.path().join("vectors.usearch"), b"previous").unwrap();
        BEFORE_REPLACE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|_, _| {
                Err(std::io::Error::other("injected publication failure"))
            }))
        });
        assert!(replace_managed(&directory, "vectors.usearch", b"replacement").is_err());
        assert_eq!(
            fs::read(root.path().join("vectors.usearch")).unwrap(),
            b"previous"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn interrupted_index_metadata_pair_rebuilds_from_committed_vectors() {
        for corruption in [
            "old_metadata",
            "old_index",
            "malformed_metadata",
            "corrupt_index",
        ] {
            let root = tempfile::tempdir().unwrap();
            let checkout = tempfile::tempdir().unwrap();
            let id = format!("checkout_{:064x}", 1);
            let mut registry = crate::storage::StorageRegistry::open(root.path(), "repo").unwrap();
            let _lease = registry
                .register_and_lease(&id, checkout.path(), 1)
                .unwrap();
            let cache = root.path().join("checkouts").join(&id).join("cache");
            let database = cache.join("vectors.db");
            let ann = cache.join("vectors.usearch");
            {
                let index =
                    UsearchVectorIndex::open(database.to_str().unwrap(), ann.clone()).unwrap();
                index.initialize(3).unwrap();
                index.warm().unwrap();
                index
                    .upsert_vector("old.rs", "old", 0, &[1.0, 0.0, 0.0])
                    .unwrap();
                index.flush().unwrap();
                let old_metadata = fs::read(cache.join("vectors.usearch.meta.json")).unwrap();
                let old_index = fs::read(&ann).unwrap();
                index.delete_by_file("old.rs").unwrap();
                index
                    .upsert_vector("new.rs", "new", 0, &[0.0, 1.0, 0.0])
                    .unwrap();
                index.flush().unwrap();
                match corruption {
                    "old_metadata" => {
                        fs::write(cache.join("vectors.usearch.meta.json"), old_metadata).unwrap()
                    }
                    "old_index" => fs::write(&ann, old_index).unwrap(),
                    "malformed_metadata" => {
                        fs::write(cache.join("vectors.usearch.meta.json"), b"invalid").unwrap()
                    }
                    "corrupt_index" => fs::write(&ann, b"invalid").unwrap(),
                    _ => unreachable!(),
                }
            }
            let index = UsearchVectorIndex::open(database.to_str().unwrap(), ann).unwrap();
            index.initialize(3).unwrap();
            index.warm().unwrap();
            let hits = index.search(&[0.0, 1.0, 0.0], 10).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].0, "new", "{corruption}");
        }
    }

    #[test]
    fn oversized_managed_payload_is_rejected_before_allocation() {
        let root = tempfile::tempdir().unwrap();
        let directory = crate::storage::SecureDir::open(root.path()).unwrap();
        for (name, limit) in [
            ("vectors.usearch", MAX_INDEX_BYTES),
            ("vectors.usearch.meta.json", MAX_METADATA_BYTES),
        ] {
            directory
                .open_new_file(name)
                .unwrap()
                .set_len(limit + 1)
                .unwrap();
            assert!(read_managed(&directory, name, limit)
                .unwrap_err()
                .to_string()
                .contains("read limit"));
        }
    }

    #[test]
    fn replaced_staging_identity_is_preserved_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let directory = crate::storage::SecureDir::open(root.path()).unwrap();
        fs::write(root.path().join("vectors.usearch"), b"previous").unwrap();
        BEFORE_REPLACE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|directory, stage| {
                let source = directory.metadata(stage)?.unwrap().identity;
                directory.rename_to(stage, directory, "owned-original", source)?;
                let mut foreign = directory.open_new_file(stage)?;
                foreign.write_all(b"foreign")?;
                Ok(())
            }))
        });
        assert!(replace_managed(&directory, "vectors.usearch", b"replacement").is_err());
        assert_eq!(
            fs::read(root.path().join("vectors.usearch")).unwrap(),
            b"previous"
        );
        let entries = directory.read_dir_page(None, 10).unwrap();
        let stage = entries
            .entries
            .iter()
            .find(|entry| entry.name.ends_with(".tmp"))
            .unwrap();
        assert_eq!(
            read_managed(&directory, &stage.name, 1024).unwrap(),
            b"foreign"
        );
    }
}
