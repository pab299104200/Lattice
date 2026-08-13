use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use lattice_core::graph::CodeGraph;
use lattice_core::indexer::{BatchIndexReport, IndexFailure, IndexFailureKind, Indexer};
use lattice_core::parser;
use lattice_core::security::SecurityFilter;
use lattice_core::storage::{
    FileIndexEntry, GraphStore, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use lattice_core::symbols::ParsedFile;

pub(crate) const WARM_GRAPH_FILE_LIMIT_ENV: &str = "LATTICE_MAX_WARM_GRAPH_FILES";
pub(crate) const WARM_GRAPH_BYTE_LIMIT_ENV: &str = "LATTICE_MAX_WARM_GRAPH_BYTES";
const DEFAULT_MAX_WARM_GRAPH_BYTES: u64 = 1024 * 1024 * 1024;

#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct IncrementalIndexResult {
    pub(crate) graph: Arc<CodeGraph>,
    pub(crate) parsed_files: HashMap<String, ParsedFile>,
    pub(crate) file_index: Vec<FileIndexEntry>,
    pub(crate) changed_count: usize,
    pub(crate) removed_count: usize,
    pub(crate) index_report: BatchIndexReport,
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

pub(crate) async fn load_incremental_cache(
    graph_store: &Arc<Mutex<GraphStore>>,
) -> (HashMap<String, FileIndexEntry>, HashMap<String, ParsedFile>) {
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
            return (HashMap::new(), HashMap::new());
        }
        let parsed_files = store.load_parsed_files().unwrap_or_else(|err| {
            tracing::warn!("Failed to load cached parsed files: {}", err);
            HashMap::new()
        });
        (manifest, parsed_files)
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "Incremental cache load worker failed");
        (HashMap::new(), HashMap::new())
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
        let store = graph_store.blocking_lock();
        if let Err(err) = store.save_graph(&incremental.graph) {
            tracing::warn!("Failed to save graph: {}", err);
        }
        if let Err(err) = store.save_file_index(&incremental.file_index) {
            tracing::warn!("Failed to save file index manifest: {}", err);
        }
        if let Err(err) = store.save_parsed_files(&incremental.parsed_files) {
            tracing::warn!("Failed to save cached parsed files: {}", err);
        }
        incremental
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "Incremental cache persistence worker failed; keeping the previously published graph");
        error
    })
    .ok()
}

pub(crate) fn build_incremental_index_for_roots(
    roots: &[PathBuf],
    manifest: Option<&HashMap<String, FileIndexEntry>>,
    mut parsed_files: HashMap<String, ParsedFile>,
) -> IncrementalIndexResult {
    let manifest = manifest.cloned().unwrap_or_default();
    let records = collect_indexable_file_records(roots);
    warn_if_indexable_file_count_exceeds_warm_limit(roots, records.len());
    let current_files: HashSet<String> = records
        .iter()
        .map(|record| record.indexed_path.clone())
        .collect();
    let removed_count = manifest
        .keys()
        .filter(|file| !current_files.contains(*file))
        .count();

    parsed_files.retain(|file, _| current_files.contains(file));

    let mut changed_count = 0usize;
    let mut failures = Vec::new();
    let mut file_index = Vec::with_capacity(records.len());
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
            let abs_path = record.root.join(&record.rel_path);
            match fs::read_to_string(&abs_path) {
                Ok(content) => {
                    let content_hash = stable_content_hash(content.as_bytes());
                    if previous
                        .map(|entry| entry.content_hash == content_hash && parser_unchanged)
                        .unwrap_or(false)
                        && parsed_files.contains_key(&record.indexed_path)
                    {
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

                    match parser::parse_file(&record.indexed_path, &content) {
                        Ok(parsed) => {
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
                    tracing::warn!("Failed to read {}: {}", abs_path.display(), err);
                    parsed_files.remove(&record.indexed_path);
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

    let mut indexer = Indexer::new(PathBuf::new());
    indexer.replace_parsed_files(parsed_files);
    let (graph, parsed_files) = indexer.into_parts();
    let indexed_count = parsed_files.len();
    IncrementalIndexResult {
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
    }
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

fn collect_indexable_file_records(roots: &[PathBuf]) -> Vec<SourceFileRecord> {
    let multi_repo = roots.len() > 1;
    let mut records = Vec::new();
    for root in roots {
        let security_filter = SecurityFilter::new(root);
        let files = collect_indexable_files(root, &security_filter);
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
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > max_index_file_bytes() {
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
    records
}

fn max_index_file_bytes() -> u64 {
    std::env::var("LATTICE_MAX_INDEX_FILE_BYTES")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(1_000_000)
}

pub(crate) fn max_cached_parsed_files() -> usize {
    std::env::var("LATTICE_MAX_CACHED_PARSED_FILES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(50_000)
}

fn stable_content_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn metadata_mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64 * 1_000_000_000 + duration.subsec_nanos() as i64)
        .unwrap_or(0)
}

fn unix_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn collect_indexable_files(root: &PathBuf, security_filter: &SecurityFilter) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(root, root, security_filter, &mut files);
    prioritize_indexable_paths(root, &mut files);
    files
}

fn collect_files_recursive(
    dir: &PathBuf,
    root: &PathBuf,
    security_filter: &SecurityFilter,
    out: &mut Vec<PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");

        if security_filter.is_excluded(&rel_path) {
            continue;
        }

        if path.is_dir() {
            collect_files_recursive(&path, root, security_filter, out);
        } else if path.is_file() && lattice_core::watcher::should_index_file(&rel_path) {
            out.push(path);
        }
    }
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
