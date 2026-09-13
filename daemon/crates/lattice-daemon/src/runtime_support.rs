use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use lattice_core::graph::CodeGraph;
use lattice_core::indexer::{BatchIndexReport, IndexFailure, IndexFailureKind, Indexer};
use lattice_core::parser;
use lattice_core::security::SecurityFilter;
use lattice_core::storage::commit_manifest::{CommitManifestEntry, ManifestLimits};
use lattice_core::storage::{
    content_sha256, FileIndexEntry, GraphStore, ParsedCacheLookup, ParsedFileCache,
    FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION, PARSED_CACHE_CONFIG_VERSION,
    PARSED_CACHE_PARSER_VERSION, PARSED_CACHE_SCHEMA_VERSION,
};
use lattice_core::symbols::{Language, ParsedFile};

pub(crate) const WARM_GRAPH_FILE_LIMIT_ENV: &str = "LATTICE_MAX_WARM_GRAPH_FILES";
pub(crate) const WARM_GRAPH_BYTE_LIMIT_ENV: &str = "LATTICE_MAX_WARM_GRAPH_BYTES";
const DEFAULT_MAX_WARM_GRAPH_BYTES: u64 = 1024 * 1024 * 1024;

#[allow(dead_code)]
pub(crate) struct IncrementalIndexResult {
    pub(crate) graph: Arc<CodeGraph>,
    pub(crate) parsed_files: HashMap<String, ParsedFile>,
    pub(crate) file_index: Vec<FileIndexEntry>,
    pub(crate) changed_count: usize,
    pub(crate) removed_count: usize,
    pub(crate) index_report: BatchIndexReport,
    pub(crate) parsed_cache: ParsedCacheOperationStats,
    cache_membership: Option<(Arc<ParsedFileCache>, String)>,
    pending_base_manifest: Option<PendingBaseManifest>,
    base_authority: Option<crate::worktree_base::WorktreeBase>,
    base_source_guards: Vec<SourceIdentityGuard>,
}

#[derive(Clone, Debug)]
pub(crate) struct BaseReuseContext {
    pub(crate) repository_id: String,
    pub(crate) checkout_id: String,
    pub(crate) git_common_dir: PathBuf,
}

#[derive(Clone)]
struct PendingBaseManifest {
    cache: Arc<ParsedFileCache>,
    checkout_id: String,
    base: crate::worktree_base::WorktreeBase,
    entries: Vec<CommitManifestEntry>,
}

struct SourceIdentityGuard {
    root: PathBuf,
    rel_path: String,
    identity: (u64, u64, i64, i64),
}

impl SourceIdentityGuard {
    fn open(record: &SourceFileRecord) -> std::io::Result<Self> {
        let file = lattice_core::security::workspace::open_source(
            &record.root,
            Path::new(&record.rel_path),
        )?;
        let identity = source_identity(&file.metadata()?);
        if identity.2 != record.size_bytes || identity.3 != record.mtime_ns {
            return Err(std::io::Error::other(
                "source metadata changed before base reuse",
            ));
        }
        Ok(Self {
            root: record.root.clone(),
            rel_path: record.rel_path.clone(),
            identity,
        })
    }

    fn revalidate(&self) -> bool {
        let Ok(reopened) =
            lattice_core::security::workspace::open_source(&self.root, Path::new(&self.rel_path))
                .and_then(|file| file.metadata())
        else {
            return false;
        };
        source_identity(&reopened) == self.identity
    }
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub(crate) struct ParsedCacheOperationStats {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) invalid: u64,
    pub(crate) writes: u64,
    pub(crate) errors: u64,
    pub(crate) source_reads: u64,
    pub(crate) source_hashes: u64,
    pub(crate) base_manifest_hits: u64,
    pub(crate) base_manifest_fallbacks: u64,
}

#[derive(Default)]
pub(crate) struct ParsedCacheMetrics {
    hits: AtomicU64,
    misses: AtomicU64,
    invalid: AtomicU64,
    writes: AtomicU64,
    errors: AtomicU64,
    source_reads: AtomicU64,
    source_hashes: AtomicU64,
    base_manifest_hits: AtomicU64,
    base_manifest_fallbacks: AtomicU64,
}

impl ParsedCacheMetrics {
    pub(crate) fn snapshot(&self) -> ParsedCacheOperationStats {
        ParsedCacheOperationStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            source_reads: self.source_reads.load(Ordering::Relaxed),
            source_hashes: self.source_hashes.load(Ordering::Relaxed),
            base_manifest_hits: self.base_manifest_hits.load(Ordering::Relaxed),
            base_manifest_fallbacks: self.base_manifest_fallbacks.load(Ordering::Relaxed),
        }
    }

    fn record(&self, stats: ParsedCacheOperationStats) {
        self.hits.fetch_add(stats.hits, Ordering::Relaxed);
        self.misses.fetch_add(stats.misses, Ordering::Relaxed);
        self.invalid.fetch_add(stats.invalid, Ordering::Relaxed);
        self.writes.fetch_add(stats.writes, Ordering::Relaxed);
        self.errors.fetch_add(stats.errors, Ordering::Relaxed);
        self.source_reads
            .fetch_add(stats.source_reads, Ordering::Relaxed);
        self.source_hashes
            .fetch_add(stats.source_hashes, Ordering::Relaxed);
        self.base_manifest_hits
            .fetch_add(stats.base_manifest_hits, Ordering::Relaxed);
        self.base_manifest_fallbacks
            .fetch_add(stats.base_manifest_fallbacks, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub(crate) struct ParsedCacheRuntime {
    pub(crate) store: Arc<ParsedFileCache>,
    pub(crate) metrics: Arc<ParsedCacheMetrics>,
}

impl ParsedCacheRuntime {
    pub(crate) fn open(path: &Path) -> Result<Self, lattice_core::LatticeError> {
        let metrics = ParsedCacheMetrics::default();
        match ParsedFileCache::open(path) {
            Ok(store) => Ok(Self {
                store: Arc::new(store),
                metrics: Arc::new(metrics),
            }),
            Err(lattice_core::LatticeError::CorruptStorage { .. }) => {
                // A shared cache is reconstructable, but this process must not
                // delete it while another checkout may hold it open. Parse
                // locally until repository maintenance has exclusive recovery.
                metrics.invalid.store(1, Ordering::Relaxed);
                tracing::warn!(path = %path.display(), "Shared parsed-file cache is corrupt; using local in-memory cache");
                Ok(Self {
                    store: Arc::new(ParsedFileCache::open_in_memory()?),
                    metrics: Arc::new(metrics),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn in_memory() -> Self {
        Self {
            store: Arc::new(ParsedFileCache::open_in_memory().unwrap()),
            metrics: Arc::new(ParsedCacheMetrics::default()),
        }
    }
}

#[derive(Debug, Clone)]
struct SourceFileRecord {
    indexed_path: String,
    rel_path: String,
    root: PathBuf,
    content_hash: Option<String>,
    mtime_ns: i64,
    size_bytes: i64,
}

pub(crate) fn background_vector_sync_enabled() -> bool {
    matches!(
        std::env::var("LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

pub(crate) async fn load_incremental_manifest(
    graph_store: &Arc<Mutex<GraphStore>>,
) -> HashMap<String, FileIndexEntry> {
    let graph_store = Arc::clone(graph_store);
    tokio::task::spawn_blocking(move || {
        let store = graph_store.blocking_lock();
        let manifest = store.load_file_index().unwrap_or_else(|err| {
            tracing::warn!("Failed to load file index manifest: {}", err);
            HashMap::new()
        });
        let max_cached_files = max_cached_parsed_files();
        if manifest.len() > max_cached_files {
            tracing::warn!(
                cached_files = manifest.len(),
                max_cached_files,
                "Skipping persisted parsed-file cache because it exceeds the safety limit"
            );
            return HashMap::new();
        }
        manifest
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "Incremental cache load worker failed");
        HashMap::new()
    })
}

pub(crate) fn max_warm_graph_files() -> usize {
    std::env::var(WARM_GRAPH_FILE_LIMIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(50_000)
}

pub(crate) fn max_warm_graph_bytes() -> u64 {
    std::env::var(WARM_GRAPH_BYTE_LIMIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_WARM_GRAPH_BYTES)
}

pub(crate) async fn persist_incremental_cache(
    graph_store: &Arc<Mutex<GraphStore>>,
    incremental: IncrementalIndexResult,
) -> Option<IncrementalIndexResult> {
    let graph_store = Arc::clone(graph_store);
    tokio::task::spawn_blocking(move || {
        if incremental
            .base_source_guards
            .iter()
            .any(|guard| !guard.revalidate())
        {
            tracing::warn!(
                "A source changed during committed parse reuse; refusing graph publication"
            );
            return None;
        }
        if let Some(base) = &incremental.base_authority {
            match base.revalidate() {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!("Git materialization changed during base reuse; refusing graph publication");
                    return None;
                }
                Err(error) => {
                    tracing::warn!(%error, "could not revalidate Git materialization after base reuse; refusing graph publication");
                    return None;
                }
            }
        }
        let store = graph_store.blocking_lock();
        match store.save_index_snapshot_with_manifest(&incremental.graph, &incremental.file_index) {
            Ok(_) => {
                if let Some((cache, checkout)) = &incremental.cache_membership {
                    if let Err(error) = cache.replace_membership(checkout, &incremental.file_index) {
                        tracing::warn!(%error, "graph committed but derived parse membership publication failed; retry on next index");
                    }
                }
                if let Some(pending) = &incremental.pending_base_manifest {
                    match pending.base.revalidate() {
                        Ok(true) => {
                            if let Err(error) = pending.cache.publish_and_bind_commit_manifest(
                                &pending.base.identity,
                                &pending.entries,
                                ManifestLimits::default(),
                                &pending.checkout_id,
                            ) {
                                tracing::warn!(%error, "graph committed but base manifest publication failed; source reads remain the safe fallback");
                            }
                        }
                        Ok(false) => tracing::warn!("HEAD or Git index changed during indexing; refusing base manifest publication"),
                        Err(error) => tracing::warn!(%error, "could not revalidate Git base; refusing base manifest publication"),
                    }
                }
                Some(incremental)
            },
            Err(error) => {
                tracing::error!(%error, "Graph and manifest commit failed; keeping previously published generation");
                None
            }
        }
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "Incremental cache persistence worker failed; keeping the previously published graph");
        error
    })
    .ok()
    .flatten()
}

pub(crate) fn build_incremental_index_for_roots(
    roots: &[PathBuf],
    manifest: Option<&HashMap<String, FileIndexEntry>>,
    parsed_files: HashMap<String, ParsedFile>,
) -> IncrementalIndexResult {
    build_incremental_index_for_roots_with_cache(
        roots,
        manifest,
        parsed_files,
        &ParsedCacheRuntime::in_memory(),
    )
}

pub(crate) fn build_incremental_index_for_roots_with_cache(
    roots: &[PathBuf],
    manifest: Option<&HashMap<String, FileIndexEntry>>,
    parsed_files: HashMap<String, ParsedFile>,
    parsed_cache: &ParsedCacheRuntime,
) -> IncrementalIndexResult {
    build_incremental_index_for_roots_with_cache_budgeted(
        roots,
        manifest,
        parsed_files,
        parsed_cache,
        None,
    )
    .expect("unbudgeted incremental build cannot be resource limited")
}

/// Builds a replacement generation while accounting for source buffers and
/// parsed node payloads before either is materialized. The logical byte basis
/// is exact source metadata bytes once for the read buffer and once for the
/// parser-owned representation; it is deliberately not described as RSS.
pub(crate) fn build_incremental_index_for_roots_with_cache_budgeted(
    roots: &[PathBuf],
    manifest: Option<&HashMap<String, FileIndexEntry>>,
    mut parsed_files: HashMap<String, ParsedFile>,
    parsed_cache: &ParsedCacheRuntime,
    resource_budget: Option<&Arc<crate::resource_budget::ResourceBudget>>,
) -> Result<IncrementalIndexResult, crate::resource_budget::ResourceLimit> {
    build_incremental_index_for_roots_with_cache_budgeted_base(
        roots,
        manifest,
        parsed_files,
        parsed_cache,
        resource_budget,
        None,
    )
}

pub(crate) fn build_incremental_index_for_roots_with_cache_budgeted_base(
    roots: &[PathBuf],
    manifest: Option<&HashMap<String, FileIndexEntry>>,
    mut parsed_files: HashMap<String, ParsedFile>,
    parsed_cache: &ParsedCacheRuntime,
    resource_budget: Option<&Arc<crate::resource_budget::ResourceBudget>>,
    base_context: Option<&BaseReuseContext>,
) -> Result<IncrementalIndexResult, crate::resource_budget::ResourceLimit> {
    let manifest = manifest.cloned().unwrap_or_default();
    let (records, mut failures) = collect_indexable_file_records(roots);
    let mut cache_stats = ParsedCacheOperationStats::default();
    let mut base = if roots.len() == 1 {
        base_context.and_then(|context| {
            match crate::worktree_base::WorktreeBase::inspect(
                &roots[0],
                &context.repository_id,
                PARSED_CACHE_PARSER_VERSION,
                PARSED_CACHE_SCHEMA_VERSION,
                PARSED_CACHE_CONFIG_VERSION,
                ManifestLimits::default().max_entries,
                &context.git_common_dir,
            ) {
                Ok(base) => Some(base),
                Err(error) => {
                    cache_stats.base_manifest_fallbacks += 1;
                    tracing::info!(%error, "Git base is not provable; reading worktree sources");
                    None
                }
            }
        })
    } else {
        None
    };
    let mut base_entries = HashMap::<String, CommitManifestEntry>::new();
    if let (Some(context), Some(base)) = (base_context, base.as_ref()) {
        match parsed_cache.store.find_commit_manifest(&base.identity) {
            Ok(Some(generation)) => {
                if let Err(error) = parsed_cache
                    .store
                    .bind_commit_manifest(&context.checkout_id, &generation.generation_id)
                {
                    cache_stats.base_manifest_fallbacks += 1;
                    tracing::warn!(%error, "could not pin committed parse manifest; reading worktree sources");
                } else {
                    let eligible = records
                        .iter()
                        .filter_map(|record| {
                            base.reusable_blob(&record.rel_path)
                                .map(|_| record.rel_path.clone())
                        })
                        .collect::<Vec<_>>();
                    let limits = ManifestLimits::default();
                    let mut lookup_failed = false;
                    for chunk in eligible.chunks(limits.max_lookup_paths) {
                        match parsed_cache
                            .store
                            .lookup_commit_paths(&base.identity, chunk, limits)
                        {
                            Ok(found) => {
                                for entry in found.into_iter().flatten() {
                                    if let Some(blob) = base.reusable_blob(&entry.path) {
                                        if blob.mode == entry.mode && blob.oid == entry.blob_oid {
                                            base_entries.insert(entry.path.clone(), entry);
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                lookup_failed = true;
                                tracing::warn!(%error, "committed parse manifest lookup failed; reading worktree sources");
                                break;
                            }
                        }
                    }
                    if lookup_failed {
                        cache_stats.base_manifest_fallbacks += 1;
                        base_entries.clear();
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                cache_stats.base_manifest_fallbacks += 1;
                tracing::warn!(%error, "committed parse manifest validation failed; reading worktree sources");
            }
        }
    }
    let mut proven_base_entries = base_entries.clone();
    let changed_source_bytes = records
        .iter()
        .filter(|record| {
            if base_entries.contains_key(&record.rel_path) {
                return false;
            }
            manifest.get(&record.indexed_path).is_none_or(|previous| {
                previous.mtime_ns != record.mtime_ns
                    || previous.size_bytes != record.size_bytes
                    || previous.parser_version != FILE_INDEX_PARSER_VERSION
                    || previous.schema_version != FILE_INDEX_SCHEMA_VERSION
                    || !parsed_files.contains_key(&record.indexed_path)
            })
        })
        .fold(0_u64, |total, record| {
            total.saturating_add(record.size_bytes.max(0) as u64)
        });
    let eligible_base_paths = base
        .as_ref()
        .map(|base| {
            records
                .iter()
                .filter(|record| base.blobs.contains_key(&record.rel_path))
                .map(|record| record.rel_path.clone())
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let mut input_reservation = resource_budget
        .map(|budget| budget.try_reserve("index_source_payload", changed_source_bytes.max(1)))
        .transpose()?;
    warn_if_indexable_file_count_exceeds_warm_limit(roots, records.len());
    let current_files: HashSet<String> = records
        .iter()
        .map(|record| record.indexed_path.clone())
        .collect();
    let coverage_complete = failures.is_empty();
    let removed_count = if coverage_complete {
        manifest
            .keys()
            .filter(|file| !current_files.contains(*file))
            .count()
    } else {
        0
    };

    if coverage_complete {
        parsed_files.retain(|file, _| current_files.contains(file));
    }

    let mut changed_count = 0usize;
    let mut file_index = Vec::with_capacity(records.len());
    let mut base_source_guards = Vec::new();
    let now = unix_timestamp_secs();
    for record in records {
        let previous = manifest.get(&record.indexed_path);
        let metadata_unchanged = previous
            .map(|entry| entry.mtime_ns == record.mtime_ns && entry.size_bytes == record.size_bytes)
            .unwrap_or(false);
        let parser_unchanged = previous
            .map(|entry| {
                entry.parser_version == FILE_INDEX_PARSER_VERSION
                    && entry.schema_version == FILE_INDEX_SCHEMA_VERSION
            })
            .unwrap_or(false);
        let unchanged = metadata_unchanged
            && parser_unchanged
            && parsed_files.contains_key(&record.indexed_path);

        if !unchanged {
            if let Some(entry) = base_entries.get(&record.rel_path) {
                if let Some(parsed) = lookup_parsed_cache(
                    parsed_cache,
                    &entry.content_hash,
                    language_for_path(&record.indexed_path),
                    &record.indexed_path,
                    &mut cache_stats,
                ) {
                    match SourceIdentityGuard::open(&record) {
                        Ok(guard) => {
                            base_source_guards.push(guard);
                            cache_stats.base_manifest_hits += 1;
                            parsed_files.insert(record.indexed_path.clone(), parsed);
                            file_index.push(FileIndexEntry {
                                file: record.indexed_path,
                                content_hash: entry.content_hash.clone(),
                                mtime_ns: record.mtime_ns,
                                size_bytes: record.size_bytes,
                                parser_version: FILE_INDEX_PARSER_VERSION,
                                schema_version: FILE_INDEX_SCHEMA_VERSION,
                                last_indexed_at: now,
                            });
                            continue;
                        }
                        Err(error) => {
                            tracing::info!(file = record.indexed_path, %error, "source identity changed before base reuse; reading source")
                        }
                    }
                }
                cache_stats.base_manifest_fallbacks += 1;
            }
            let known_hash = metadata_unchanged
                .then(|| previous.map(|entry| entry.content_hash.clone()))
                .flatten();
            let language = language_for_path(&record.indexed_path);
            let cached = known_hash.as_deref().and_then(|content_hash| {
                lookup_parsed_cache(
                    parsed_cache,
                    content_hash,
                    language,
                    &record.indexed_path,
                    &mut cache_stats,
                )
            });
            if let Some(parsed) = cached {
                parsed_files.insert(record.indexed_path.clone(), parsed);
                file_index.push(FileIndexEntry {
                    file: record.indexed_path,
                    content_hash: known_hash.unwrap_or_default(),
                    mtime_ns: record.mtime_ns,
                    size_bytes: record.size_bytes,
                    parser_version: FILE_INDEX_PARSER_VERSION,
                    schema_version: FILE_INDEX_SCHEMA_VERSION,
                    last_indexed_at: now,
                });
                continue;
            }
            if base_entries.contains_key(&record.rel_path) {
                if let Some(reservation) = input_reservation.as_mut() {
                    reservation.try_grow(record.size_bytes.max(0) as u64)?;
                }
            }
            match lattice_core::security::workspace::read_source(
                &record.root,
                Path::new(&record.rel_path),
            ) {
                Ok(content) => {
                    if content.len() as i64 > record.size_bytes {
                        if let Some(reservation) = input_reservation.as_mut() {
                            reservation
                                .try_grow((content.len() as i64 - record.size_bytes) as u64)?;
                        }
                    }
                    cache_stats.source_reads += 1;
                    cache_stats.source_hashes += 1;
                    let content_hash = stable_content_hash(content.as_bytes());
                    if let Some(base) = base.as_ref() {
                        if base
                            .content_matches_blob(&record.rel_path, content.as_bytes())
                            .unwrap_or(false)
                        {
                            if let Some(blob) = base.reusable_blob(&record.rel_path) {
                                proven_base_entries.insert(
                                    record.rel_path.clone(),
                                    CommitManifestEntry {
                                        path: record.rel_path.clone(),
                                        mode: blob.mode,
                                        blob_oid: blob.oid.clone(),
                                        content_hash: content_hash.clone(),
                                        parse_key: ParsedFileCache::parse_key(
                                            &content_hash,
                                            language,
                                        ),
                                    },
                                );
                            }
                        }
                    }
                    if known_hash.as_deref() != Some(content_hash.as_str()) {
                        if let Some(parsed) = lookup_parsed_cache(
                            parsed_cache,
                            &content_hash,
                            language,
                            &record.indexed_path,
                            &mut cache_stats,
                        ) {
                            parsed_files.insert(record.indexed_path.clone(), parsed);
                            file_index.push(FileIndexEntry {
                                file: record.indexed_path,
                                content_hash,
                                mtime_ns: record.mtime_ns,
                                size_bytes: record.size_bytes,
                                parser_version: FILE_INDEX_PARSER_VERSION,
                                schema_version: FILE_INDEX_SCHEMA_VERSION,
                                last_indexed_at: now,
                            });
                            continue;
                        }
                    }

                    match parser::parse_file(&record.indexed_path, &content) {
                        Ok(parsed) => {
                            match parsed_cache.store.put(&content_hash, &parsed) {
                                Ok(()) => cache_stats.writes += 1,
                                Err(error) => {
                                    cache_stats.errors += 1;
                                    tracing::warn!(file = record.indexed_path, %error, "Failed to publish parsed-file cache row");
                                }
                            }
                            parsed_files.insert(record.indexed_path.clone(), parsed);
                            changed_count += 1;
                        }
                        Err(err) => {
                            tracing::warn!("Failed to parse {}: {}", record.indexed_path, err);
                            parsed_files.remove(&record.indexed_path);
                            failures.push(IndexFailure {
                                file: record.indexed_path.clone(),
                                kind: IndexFailureKind::ParseError,
                                message: err.to_string(),
                            });
                        }
                    }
                    file_index.push(FileIndexEntry {
                        file: record.indexed_path,
                        content_hash,
                        mtime_ns: record.mtime_ns,
                        size_bytes: record.size_bytes,
                        parser_version: FILE_INDEX_PARSER_VERSION,
                        schema_version: FILE_INDEX_SCHEMA_VERSION,
                        last_indexed_at: now,
                    });
                    continue;
                }
                Err(err) => {
                    tracing::warn!(
                        "Failed to read {}: {}",
                        record.root.join(&record.rel_path).display(),
                        err
                    );
                    parsed_files.remove(&record.indexed_path);
                    failures.push(IndexFailure {
                        file: record.indexed_path.clone(),
                        kind: IndexFailureKind::ReadError,
                        message: err.to_string(),
                    });
                }
            }
        }

        file_index.push(FileIndexEntry {
            file: record.indexed_path,
            content_hash: record
                .content_hash
                .or_else(|| previous.map(|entry| entry.content_hash.clone()))
                .unwrap_or_default(),
            mtime_ns: record.mtime_ns,
            size_bytes: record.size_bytes,
            parser_version: FILE_INDEX_PARSER_VERSION,
            schema_version: FILE_INDEX_SCHEMA_VERSION,
            last_indexed_at: now,
        });
    }

    if !coverage_complete {
        for (path, prior) in &manifest {
            if !file_index.iter().any(|entry| &entry.file == path) {
                file_index.push(prior.clone());
            }
        }
        file_index.sort_by(|left, right| left.file.cmp(&right.file));
    }

    // Count the exact serialized parsed representation without materializing a
    // second payload. Reserve it before graph construction copies node data.
    if let Some(reservation) = input_reservation.as_mut() {
        let parsed_payload_bytes = parsed_files.values().fold(0_u64, |total, parsed| {
            let mut counter = ByteCounter::default();
            if serde_json::to_writer(&mut counter, parsed).is_ok() {
                total.saturating_add(counter.bytes)
            } else {
                u64::MAX
            }
        });
        reservation.try_grow(parsed_payload_bytes)?;
    }
    let mut indexer = Indexer::new(PathBuf::new());
    indexer.replace_parsed_files(parsed_files);
    let (graph, parsed_files) = indexer.into_parts();
    let indexed_count = parsed_files.len();
    let base_authority = (cache_stats.base_manifest_hits > 0)
        .then(|| base.as_ref().cloned())
        .flatten();
    let pending_base_manifest = match (base_context, base.take()) {
        (Some(context), Some(base)) if coverage_complete => {
            let entries = proven_base_entries.into_values().collect::<Vec<_>>();
            let complete =
                entries.len() == eligible_base_paths.len() && base.unsafe_paths.is_empty();
            complete.then(|| PendingBaseManifest {
                cache: Arc::clone(&parsed_cache.store),
                checkout_id: context.checkout_id.clone(),
                base,
                entries,
            })
        }
        _ => None,
    };
    parsed_cache.metrics.record(cache_stats);
    Ok(IncrementalIndexResult {
        pending_base_manifest,
        base_authority,
        base_source_guards,
        cache_membership: roots
            .first()
            .and_then(|root| crate::workspace_identity::WorkspaceIdentity::resolve(root).ok())
            .map(|identity| (Arc::clone(&parsed_cache.store), identity.checkout_id)),
        graph: Arc::new(graph),
        parsed_files,
        file_index,
        changed_count,
        removed_count,
        index_report: BatchIndexReport {
            requested_count: current_files.len(),
            indexed_count,
            is_partial: !failures.is_empty(),
            indexed_files: Vec::new(),
            removed_files: Vec::new(),
            failures,
        },
        parsed_cache: cache_stats,
    })
}

#[derive(Default)]
struct ByteCounter {
    bytes: u64,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len() as u64);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn lookup_parsed_cache(
    runtime: &ParsedCacheRuntime,
    content_hash: &str,
    language: Language,
    indexed_path: &str,
    stats: &mut ParsedCacheOperationStats,
) -> Option<ParsedFile> {
    match runtime.store.get(content_hash, language, indexed_path) {
        Ok((ParsedCacheLookup::Hit, parsed)) => {
            stats.hits += 1;
            parsed
        }
        Ok((ParsedCacheLookup::Miss, _)) => {
            stats.misses += 1;
            None
        }
        Ok((ParsedCacheLookup::Invalid, _)) => {
            stats.invalid += 1;
            None
        }
        Err(error) => {
            stats.errors += 1;
            tracing::warn!(file = indexed_path, %error, "Parsed-file cache lookup failed");
            None
        }
    }
}

pub(crate) fn parse_with_repository_cache(
    runtime: &ParsedCacheRuntime,
    indexed_path: &str,
    content: &str,
) -> Result<ParsedFile, lattice_core::LatticeError> {
    let hash = stable_content_hash(content.as_bytes());
    let language = language_for_path(indexed_path);
    let mut stats = ParsedCacheOperationStats::default();
    if let Some(parsed) = lookup_parsed_cache(runtime, &hash, language, indexed_path, &mut stats) {
        runtime.metrics.record(stats);
        return Ok(parsed);
    }
    let parsed = match parser::parse_file(indexed_path, content) {
        Ok(parsed) => parsed,
        Err(error) => {
            runtime.metrics.record(stats);
            return Err(error);
        }
    };
    match runtime.store.put(&hash, &parsed) {
        Ok(()) => stats.writes += 1,
        Err(error) => {
            stats.errors += 1;
            tracing::warn!(file = indexed_path, %error, "Failed to publish watcher parsed-file cache row");
        }
    }
    runtime.metrics.record(stats);
    Ok(parsed)
}

fn warn_if_indexable_file_count_exceeds_warm_limit(roots: &[PathBuf], candidate_files: usize) {
    let max_files = max_warm_graph_files();
    if candidate_files <= max_files {
        return;
    }
    let root_list = roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        workspace_roots = %root_list,
        candidate_files,
        max_files,
        env_var = WARM_GRAPH_FILE_LIMIT_ENV,
        "Workspace contains more candidate files than the warm graph safety limit"
    );
}

fn collect_indexable_file_records(roots: &[PathBuf]) -> (Vec<SourceFileRecord>, Vec<IndexFailure>) {
    let multi_repo = roots.len() > 1;
    let mut records = Vec::new();
    let mut failures = Vec::new();
    for root in roots {
        let security_filter = SecurityFilter::new(root);
        let files = match collect_indexable_files(root, &security_filter) {
            Ok(files) => files,
            Err(error) => {
                failures.push(IndexFailure {
                    file: root.display().to_string(),
                    kind: IndexFailureKind::TraversalError,
                    message: format!("workspace traversal incomplete: {error}"),
                });
                continue;
            }
        };
        let repo_name = repo_name_for_root(root);
        for path in files {
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let indexed_path = if multi_repo {
                lattice_core::workspace::repo_rel_path(&repo_name, &rel_path)
            } else {
                rel_path.clone()
            };
            let metadata =
                match lattice_core::security::workspace::open_source(root, Path::new(&rel_path))
                    .and_then(|file| file.metadata())
                {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        failures.push(IndexFailure {
                            file: indexed_path,
                            kind: IndexFailureKind::ReadError,
                            message: format!(
                                "source changed or became unreadable during scan: {error}"
                            ),
                        });
                        continue;
                    }
                };
            if metadata.len() > lattice_core::security::workspace::max_source_bytes() {
                tracing::debug!(
                    file = indexed_path.as_str(),
                    size_bytes = metadata.len(),
                    "Skipping oversized index candidate"
                );
                continue;
            }
            records.push(SourceFileRecord {
                indexed_path,
                rel_path,
                root: root.clone(),
                content_hash: None,
                mtime_ns: metadata_mtime_ns(&metadata),
                size_bytes: metadata.len() as i64,
            });
        }
    }
    records.sort_by(|a, b| a.indexed_path.cmp(&b.indexed_path));
    (records, failures)
}

pub(crate) fn max_cached_parsed_files() -> usize {
    std::env::var("LATTICE_MAX_CACHED_PARSED_FILES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(50_000)
}

fn stable_content_hash(bytes: &[u8]) -> String {
    content_sha256(bytes)
}

fn language_for_path(path: &str) -> Language {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    Language::from_extension(&extension)
}

fn metadata_mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64 * 1_000_000_000 + duration.subsec_nanos() as i64)
        .unwrap_or(0)
}

fn source_identity(metadata: &fs::Metadata) -> (u64, u64, i64, i64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len() as i64,
            metadata_mtime_ns(metadata),
        )
    }
    #[cfg(not(unix))]
    {
        (0, 0, metadata.len() as i64, metadata_mtime_ns(metadata))
    }
}

fn unix_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn collect_indexable_files(
    root: &PathBuf,
    security_filter: &SecurityFilter,
) -> std::io::Result<Vec<PathBuf>> {
    let mut files = collect_files_recursive(root, root, security_filter)?;
    prioritize_indexable_paths(root, &mut files);
    Ok(files)
}

fn collect_files_recursive(
    dir: &PathBuf,
    root: &PathBuf,
    security_filter: &SecurityFilter,
) -> std::io::Result<Vec<PathBuf>> {
    let _ = (root, security_filter);
    lattice_core::security::workspace::collect_sources(dir)
}

fn repo_name_for_root(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("default")
        .to_string()
}

fn prioritize_indexable_paths(root: &Path, files: &mut [PathBuf]) {
    files.sort_by(|a, b| {
        let a_rel = repo_relative_path(root, a);
        let b_rel = repo_relative_path(root, b);
        indexing_priority(&a_rel)
            .cmp(&indexing_priority(&b_rel))
            .then_with(|| a_rel.cmp(&b_rel))
    });
}

fn repo_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn indexing_priority(rel_path: &str) -> (u8, u8) {
    let normalized = rel_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    let lower = normalized.to_ascii_lowercase();
    let is_markdown = lower.ends_with(".md");
    let is_doc_dir = lower.starts_with("docs/");
    let is_repo_guide = matches!(
        file_name,
        "README.md" | "CLAUDE.md" | "AGENTS.md" | "CONTRIBUTING.md"
    );

    if is_repo_guide || (is_markdown && is_doc_dir) {
        (0, 0)
    } else if is_markdown {
        (0, 1)
    } else if lower.ends_with(".py") || lower.ends_with(".pyi") {
        (1, 0)
    } else {
        (2, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_fixture(root: &Path) {
        let repository = git2::Repository::init(root).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("a.ts")).unwrap();
        index.add_path(Path::new("b.ts")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Lattice Test", "lattice@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
    }

    fn commit_all_typescript(root: &Path) -> git2::Repository {
        let repository = git2::Repository::init(root).unwrap();
        let mut index = repository.index().unwrap();
        index
            .add_all(["*.ts"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Lattice Test", "lattice@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        drop(tree);
        repository
    }

    fn has_call(graph: &CodeGraph, from: &str, to: &str) -> bool {
        graph.all_edges().iter().any(|(source, target, kind)| {
            source.name == from
                && target.name == to
                && *kind == lattice_core::graph::EdgeKind::Calls
        })
    }

    #[test]
    fn watcher_cache_counts_a_miss_even_when_parsing_fails() {
        let cache = ParsedCacheRuntime::in_memory();
        let error = parse_with_repository_cache(&cache, "unsupported.txt", "plain text")
            .expect_err("unknown languages are not parseable");

        assert!(error.to_string().contains("Unsupported language"));
        let metrics = cache.metrics.snapshot();
        assert_eq!(metrics.misses, 1);
        assert_eq!(metrics.writes, 0);
    }

    #[test]
    fn cold_start_skips_unparsed_c_family_files() {
        let root = std::env::temp_dir().join(format!(
            "lattice-runtime-support-{}-{}",
            std::process::id(),
            unix_timestamp_secs()
        ));
        std::fs::create_dir_all(&root).expect("create fixture root");
        std::fs::write(root.join("native.c"), "int main(void) { return 0; }\n")
            .expect("write C fixture");
        std::fs::write(root.join("native.hpp"), "int value;\n").expect("write header fixture");
        std::fs::write(root.join("src.ts"), "export const value = 1;\n")
            .expect("write supported fixture");

        let result = build_incremental_index_for_roots(&[root.clone()], None, HashMap::new());

        assert_eq!(result.file_index.len(), 1);
        assert_eq!(result.file_index[0].file, "src.ts");
        assert!(result.index_report.failures.is_empty());
        assert_eq!(result.index_report.requested_count, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn traversal_failure_is_partial_and_does_not_infer_manifest_removals() {
        let root = std::env::temp_dir().join(format!(
            "lattice-missing-runtime-support-{}-{}",
            std::process::id(),
            unix_timestamp_secs()
        ));
        let manifest = HashMap::from([(
            "src/retained.rs".to_string(),
            FileIndexEntry {
                file: "src/retained.rs".to_string(),
                content_hash: "deadbeef".to_string(),
                mtime_ns: 1,
                size_bytes: 10,
                parser_version: FILE_INDEX_PARSER_VERSION,
                schema_version: FILE_INDEX_SCHEMA_VERSION,
                last_indexed_at: 1,
            },
        )]);

        let result = build_incremental_index_for_roots(&[root], Some(&manifest), HashMap::new());

        assert!(result.index_report.is_partial);
        assert_eq!(result.removed_count, 0);
        assert_eq!(result.file_index, vec![manifest["src/retained.rs"].clone()]);
        assert!(result
            .index_report
            .failures
            .iter()
            .any(|failure| failure.kind == IndexFailureKind::TraversalError));
    }

    #[test]
    fn source_growth_is_rejected_before_parse_cache_or_graph_materialization() {
        let root = std::env::temp_dir().join(format!(
            "lattice-budgeted-input-{}-{}",
            std::process::id(),
            unix_timestamp_secs()
        ));
        std::fs::create_dir_all(&root).expect("create fixture root");
        std::fs::write(
            root.join("large.rs"),
            "pub fn value() -> usize { 42 }\n".repeat(16),
        )
        .expect("write fixture");
        let cache = ParsedCacheRuntime::in_memory();
        let budget = Arc::new(crate::resource_budget::ResourceBudget::new(32));

        let error = match build_incremental_index_for_roots_with_cache_budgeted(
            &[root.clone()],
            None,
            HashMap::new(),
            &cache,
            Some(&budget),
        ) {
            Ok(_) => panic!("source bytes exceeded admission"),
            Err(error) => error,
        };

        assert_eq!(error.class, "index_source_payload");
        assert_eq!(cache.metrics.snapshot().writes, 0);
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn committed_manifest_skips_unchanged_reads_and_reads_only_dirty_overlay() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(fixture.path().join("b.ts"), "export const b = 2;\n").unwrap();
        commit_fixture(fixture.path());
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ParsedCacheRuntime::open(&cache_dir.path().join("parsed-cache.db")).unwrap();
        let first_context = BaseReuseContext {
            repository_id: "fixture-repository".into(),
            checkout_id: "checkout-one".into(),
            git_common_dir: fixture.path().join(".git"),
        };
        let first = build_incremental_index_for_roots_with_cache_budgeted_base(
            &[fixture.path().to_path_buf()],
            None,
            HashMap::new(),
            &cache,
            None,
            Some(&first_context),
        )
        .unwrap();
        assert_eq!(first.parsed_cache.source_reads, 2);
        let graph_store = Arc::new(Mutex::new(GraphStore::open_in_memory().unwrap()));
        persist_incremental_cache(&graph_store, first)
            .await
            .expect("publish first graph and committed manifest");

        let cold_context = BaseReuseContext {
            repository_id: "fixture-repository".into(),
            checkout_id: "checkout-two".into(),
            git_common_dir: fixture.path().join(".git"),
        };
        let cold = build_incremental_index_for_roots_with_cache_budgeted_base(
            &[fixture.path().to_path_buf()],
            None,
            HashMap::new(),
            &cache,
            None,
            Some(&cold_context),
        )
        .unwrap();
        assert_eq!(cold.parsed_cache.base_manifest_hits, 2);
        assert_eq!(cold.parsed_cache.source_reads, 0);
        assert_eq!(cold.parsed_cache.source_hashes, 0);

        std::fs::write(fixture.path().join("b.ts"), "export const b = 3;\n").unwrap();
        let dirty = build_incremental_index_for_roots_with_cache_budgeted_base(
            &[fixture.path().to_path_buf()],
            None,
            HashMap::new(),
            &cache,
            None,
            Some(&cold_context),
        )
        .unwrap();
        assert_eq!(dirty.parsed_cache.base_manifest_hits, 1);
        assert_eq!(dirty.parsed_cache.source_reads, 1);
        assert_eq!(dirty.parsed_cache.source_hashes, 1);

        std::fs::write(fixture.path().join("b.ts"), "export const b = 2;\n").unwrap();
        git2::Repository::open(fixture.path())
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.autocrlf", "true")
            .unwrap();
        let converted_checkout = build_incremental_index_for_roots_with_cache_budgeted_base(
            &[fixture.path().to_path_buf()],
            None,
            HashMap::new(),
            &cache,
            None,
            Some(&cold_context),
        )
        .unwrap();
        assert_eq!(converted_checkout.parsed_cache.base_manifest_hits, 0);
        assert_eq!(converted_checkout.parsed_cache.source_reads, 2);

        git2::Repository::open(fixture.path())
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.autocrlf", "false")
            .unwrap();
        rusqlite::Connection::open(cache.store.path().unwrap())
            .unwrap()
            .execute("UPDATE parsed_file_cache SET payload='{' WHERE cache_key=(SELECT cache_key FROM parsed_file_cache ORDER BY cache_key LIMIT 1)", [])
            .unwrap();
        let budget = Arc::new(crate::resource_budget::ResourceBudget::new(1));
        let fallback = build_incremental_index_for_roots_with_cache_budgeted_base(
            &[fixture.path().to_path_buf()],
            None,
            HashMap::new(),
            &cache,
            Some(&budget),
            Some(&cold_context),
        );
        assert!(matches!(fallback, Err(error) if error.class == "index_source_payload"));
    }

    #[tokio::test]
    async fn linked_worktrees_reuse_committed_parses_but_resolve_isolated_graphs() {
        let fixture = tempfile::tempdir().unwrap();
        let primary_root = fixture.path().join("primary");
        let seed_root = fixture.path().join("seed-checkout");
        let changed_root = fixture.path().join("changed-checkout");
        std::fs::create_dir_all(&primary_root).unwrap();
        std::fs::write(
            primary_root.join("app.ts"),
            "import { alpha } from './dep';\nexport function run() { return alpha(); }\n",
        )
        .unwrap();
        std::fs::write(
            primary_root.join("dep.ts"),
            "export function alpha() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            primary_root.join("alt.ts"),
            "export function beta() { return 2; }\n",
        )
        .unwrap();
        let primary = commit_all_typescript(&primary_root);
        let head = primary.head().unwrap().peel_to_commit().unwrap();
        primary.branch("seed", &head, false).unwrap();
        primary.branch("changed", &head, false).unwrap();
        drop(head);
        for (name, path) in [("seed", &seed_root), ("changed", &changed_root)] {
            let reference = primary
                .find_reference(&format!("refs/heads/{name}"))
                .unwrap();
            let mut options = git2::WorktreeAddOptions::new();
            options.reference(Some(&reference));
            primary.worktree(name, path, Some(&options)).unwrap();
        }

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ParsedCacheRuntime::open(&cache_dir.path().join("parsed-cache.db")).unwrap();
        let common_dir = primary.path().canonicalize().unwrap();
        let seed_context = BaseReuseContext {
            repository_id: "linked-fixture-repository".into(),
            checkout_id: "linked-seed".into(),
            git_common_dir: common_dir.clone(),
        };
        let seed = build_incremental_index_for_roots_with_cache_budgeted_base(
            std::slice::from_ref(&seed_root),
            None,
            HashMap::new(),
            &cache,
            None,
            Some(&seed_context),
        )
        .unwrap();
        assert_eq!(seed.parsed_cache.source_reads, 3);
        assert!(has_call(&seed.graph, "run", "alpha"));
        let seed_graph = Arc::clone(&seed.graph);
        let seed_store = Arc::new(Mutex::new(GraphStore::open_in_memory().unwrap()));
        persist_incremental_cache(&seed_store, seed)
            .await
            .expect("publish seed worktree graph and base manifest");

        let changed_context = BaseReuseContext {
            repository_id: "linked-fixture-repository".into(),
            checkout_id: "linked-changed".into(),
            git_common_dir: common_dir,
        };
        let cold = build_incremental_index_for_roots_with_cache_budgeted_base(
            std::slice::from_ref(&changed_root),
            None,
            HashMap::new(),
            &cache,
            None,
            Some(&changed_context),
        )
        .unwrap();
        assert_eq!(cold.parsed_cache.base_manifest_hits, 3);
        assert_eq!(cold.parsed_cache.source_reads, 0);
        assert_eq!(cold.parsed_cache.source_hashes, 0);
        assert_eq!(cold.changed_count, 0);
        assert!(has_call(&cold.graph, "run", "alpha"));

        std::fs::write(
            changed_root.join("app.ts"),
            "import { beta } from './alt';\nexport function run() { return beta(); }\n",
        )
        .unwrap();
        std::fs::remove_file(changed_root.join("dep.ts")).unwrap();
        let changed = build_incremental_index_for_roots_with_cache_budgeted_base(
            std::slice::from_ref(&changed_root),
            None,
            HashMap::new(),
            &cache,
            None,
            Some(&changed_context),
        )
        .unwrap();
        assert_eq!(changed.parsed_cache.base_manifest_hits, 1);
        assert_eq!(changed.parsed_cache.source_reads, 1);
        assert_eq!(changed.parsed_cache.source_hashes, 1);
        assert_eq!(changed.changed_count, 1);
        assert!(!changed
            .file_index
            .iter()
            .any(|entry| entry.file == "dep.ts"));
        assert!(!changed
            .graph
            .all_nodes()
            .iter()
            .any(|node| node.file == "dep.ts"));
        assert!(has_call(&changed.graph, "run", "beta"));
        assert!(!has_call(&changed.graph, "run", "alpha"));

        assert!(seed_root.join("dep.ts").is_file());
        assert!(has_call(&seed_graph, "run", "alpha"));
        assert!(!has_call(&seed_graph, "run", "beta"));
    }
}
