use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::indexer::Indexer;
use lattice_core::query::QueryEngine;
use lattice_core::storage::{GraphStore, SharedVectorIndex};
use lattice_core::workspace::{repo_rel_path, WorkspaceManager};

use crate::repo_state::RepoStateTracker;
use crate::watcher_health::WatcherHealth;

const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);
const WORKSPACE_INVALIDATION_BATCH_THRESHOLD: usize = 20;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// File system watcher that triggers incremental indexing on changes.
pub struct FileWatcher {
    workspace_root: PathBuf,
    repo_name: Option<String>,
    indexer: Option<Arc<Mutex<Indexer>>>,
    workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
    graph_store: Arc<Mutex<GraphStore>>,
    query_engine: Arc<Mutex<QueryEngine>>,
    embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
    vector_index: Option<SharedVectorIndex>,
    repo_state: Arc<Mutex<RepoStateTracker>>,
    indexing: Arc<AtomicBool>,
    watcher_health: Arc<WatcherHealth>,
    #[cfg(test)]
    forced_watch_failure: Option<String>,
}

impl FileWatcher {
    pub fn new(
        workspace_root: PathBuf,
        repo_name: Option<String>,
        indexer: Option<Arc<Mutex<Indexer>>>,
        workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
        graph_store: Arc<Mutex<GraphStore>>,
        query_engine: Arc<Mutex<QueryEngine>>,
        embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
        vector_index: Option<SharedVectorIndex>,
        repo_state: Arc<Mutex<RepoStateTracker>>,
        indexing: Arc<AtomicBool>,
        watcher_health: Arc<WatcherHealth>,
    ) -> Self {
        Self {
            workspace_root,
            repo_name,
            indexer,
            workspace_manager,
            graph_store,
            query_engine,
            embedding_engine,
            vector_index,
            repo_state,
            indexing,
            watcher_health,
            #[cfg(test)]
            forced_watch_failure: None,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        if let Err(error) = self.run_notify().await {
            let reason = error.to_string();
            let interval = poll_interval();
            tracing::warn!(
                workspace = %self.workspace_root.display(),
                reason = %reason,
                interval_secs = interval.as_secs(),
                "File watcher degraded; falling back to polling"
            );
            self.watcher_health
                .mark_degraded(reason, interval.as_secs());
            return self.run_polling(interval).await;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_forced_watch_failure(mut self, reason: impl Into<String>) -> Self {
        self.forced_watch_failure = Some(reason.into());
        self
    }

    async fn run_notify(&self) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(reason) = &self.forced_watch_failure {
            anyhow::bail!("{}", reason);
        }

        let (tx, mut rx) = mpsc::channel(100);

        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
            if let Ok(event) = res {
                let _ = tx.blocking_send(event);
            }
        })?;

        watcher.watch(&self.workspace_root, RecursiveMode::Recursive)?;
        self.watcher_health.mark_healthy();
        info!("File watcher started for: {:?}", self.workspace_root);

        let mut changed_paths: Vec<PathBuf> = Vec::new();
        let debounce_timer = tokio::time::sleep(DEBOUNCE_DURATION);
        tokio::pin!(debounce_timer);

        loop {
            tokio::select! {
                Some(event) = rx.recv() => {
                    self.handle_event(event, &mut changed_paths);
                    debounce_timer.as_mut().reset(tokio::time::Instant::now() + DEBOUNCE_DURATION);
                },
                _ = &mut debounce_timer, if !changed_paths.is_empty() => {
                    let paths = std::mem::take(&mut changed_paths);
                    self.process_changes(paths).await;
                },
                else => {
                    if !changed_paths.is_empty() {
                        let paths = std::mem::take(&mut changed_paths);
                        self.process_changes(paths).await;
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    async fn run_polling(&self, interval: Duration) -> anyhow::Result<()> {
        let mut previous = self.poll_snapshot();
        self.watcher_health.mark_poll();
        let mut ticker = tokio::time::interval(interval);

        loop {
            ticker.tick().await;
            let current = self.poll_snapshot();
            let mut changed = Vec::new();
            for (path, state) in &current {
                if previous.get(path) != Some(state) {
                    changed.push(path.clone());
                }
            }
            for path in previous.keys() {
                if !current.contains_key(path) {
                    changed.push(path.clone());
                }
            }
            self.watcher_health.mark_poll();
            if !changed.is_empty() {
                self.process_changes(changed).await;
            }
            previous = current;
        }
    }

    fn poll_snapshot(&self) -> HashMap<PathBuf, PollFileState> {
        let mut snapshot = HashMap::new();
        self.collect_poll_snapshot(&self.workspace_root, &mut snapshot);
        snapshot
    }

    fn collect_poll_snapshot(&self, dir: &Path, snapshot: &mut HashMap<PathBuf, PollFileState>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(dir_name) = path.file_name().and_then(|name| name.to_str()) {
                    if matches!(
                        dir_name,
                        ".git" | "target" | "node_modules" | ".venv" | "venv" | "__pycache__"
                    ) {
                        continue;
                    }
                }
                self.collect_poll_snapshot(&path, snapshot);
            } else if path.is_file() && self.should_process(&path) {
                if let Ok(metadata) = std::fs::metadata(&path) {
                    let modified_ns = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default();
                    snapshot.insert(
                        path,
                        PollFileState {
                            modified_ns,
                            size_bytes: metadata.len(),
                        },
                    );
                }
            }
        }
    }

    fn handle_event(&self, event: Event, changed_paths: &mut Vec<PathBuf>) {
        if matches!(
            event.kind,
            notify::EventKind::Create(_)
                | notify::EventKind::Modify(_)
                | notify::EventKind::Remove(_)
        ) {
            for path in event.paths {
                if !changed_paths.contains(&path) && self.should_process(&path) {
                    changed_paths.push(path);
                }
            }
        }
    }

    fn should_process(&self, path: &Path) -> bool {
        if path.is_dir() {
            return false;
        }
        let rel = path
            .strip_prefix(&self.workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if is_git_state_path(&rel) {
            return true;
        }
        !rel.contains("/target/") && !rel.contains("/node_modules/") && !rel.starts_with(".git/")
    }

    async fn process_changes(&self, paths: Vec<PathBuf>) {
        let requires_workspace_invalidation =
            should_invalidate_workspace(&self.workspace_root, &paths);
        let target_epoch = if requires_workspace_invalidation {
            self.indexing.store(true, Ordering::Relaxed);
            let mut repo_state = self.repo_state.lock().await;
            Some(repo_state.mark_workspace_change())
        } else {
            None
        };

        if let Some(workspace_manager) = &self.workspace_manager {
            let repo_name = crate::repo_name_for_root(&self.workspace_root);
            let mut graph_changed = false;
            let mut changed_graph_files = Vec::new();
            let mut manager = workspace_manager.lock().await;

            for path in paths {
                let rel_path = match path.strip_prefix(&self.workspace_root) {
                    Ok(p) => p.to_string_lossy().to_string(),
                    Err(_) => continue,
                };
                if is_git_state_path(&rel_path) {
                    continue;
                }
                let graph_path = repo_rel_path(&repo_name, &rel_path);

                if path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        info!(
                            "Incremental multi-repo index update: {}/{}",
                            repo_name, rel_path
                        );
                        if manager.index_file(&repo_name, &rel_path, &content).is_ok() {
                            graph_changed = true;
                            changed_graph_files.push(graph_path);
                        }
                    }
                } else {
                    info!(
                        "Removing file from multi-repo index: {}/{}",
                        repo_name, rel_path
                    );
                    manager.remove_file(&repo_name, &rel_path);
                    graph_changed = true;
                    changed_graph_files.push(graph_path);
                }
            }

            if graph_changed {
                manager.detect_cross_repo_edges();
                let new_graph = Arc::new(manager.unified_graph());
                drop(manager);
                self.persist_publish_and_sync(new_graph, changed_graph_files, target_epoch)
                    .await;
            }
            return;
        }

        let Some(indexer_handle) = &self.indexer else {
            return;
        };
        let mut graph_changed = false;
        let mut changed_graph_files = Vec::new();
        let mut indexer = indexer_handle.lock().await;
        let repo_name = self.repo_name.as_deref();

        for path in paths {
            let rel_path = match path.strip_prefix(&self.workspace_root) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            if is_git_state_path(&rel_path) {
                continue;
            }
            let graph_path = repo_name
                .map(|repo| repo_rel_path(repo, &rel_path))
                .unwrap_or_else(|| rel_path.clone());

            if path.exists() {
                // Only re-index the specific file that changed
                if let Ok(content) = std::fs::read_to_string(&path) {
                    info!("Incremental index update: {}", graph_path);
                    if let Ok(_) = indexer.index_file_content(&graph_path, &content) {
                        graph_changed = true;
                        changed_graph_files.push(graph_path.clone());
                    }
                }
            } else {
                info!("Removing file from index: {}", graph_path);
                indexer.remove_file(&graph_path);
                graph_changed = true;
                changed_graph_files.push(graph_path.clone());
            }
        }

        if graph_changed {
            let new_graph = indexer.graph_arc();
            drop(indexer);
            self.persist_publish_and_sync(new_graph, changed_graph_files, target_epoch)
                .await;
        }
    }

    async fn persist_publish_and_sync(
        &self,
        new_graph: Arc<lattice_core::graph::CodeGraph>,
        changed_graph_files: Vec<String>,
        target_epoch: Option<u64>,
    ) {
        if let Some(epoch) = target_epoch {
            let repo_state = self.repo_state.lock().await;
            if !repo_state.can_publish_epoch(epoch) {
                return;
            }
        }

        {
            let graph_store = self.graph_store.lock().await;
            let _ = graph_store.save_graph(&new_graph);
        }

        let mut engine = self.query_engine.lock().await;
        engine.update_graph_arc(Arc::clone(&new_graph));
        drop(engine);

        let Some(vector_index) = self.vector_index.as_ref() else {
            return;
        };
        let Some(embedding_engine) = self.embedding_engine.get() else {
            return;
        };

        let sync_started = std::time::Instant::now();
        match crate::vector_sync::sync_changed_files_embeddings(
            &new_graph,
            &changed_graph_files,
            embedding_engine.as_ref(),
            vector_index.as_ref(),
        ) {
            Ok(stats) => {
                let watcher_sync_elapsed_ms = sync_started.elapsed().as_millis();
                if !changed_graph_files.is_empty() {
                    info!(
                        mode = stats.mode,
                        implementation = stats.implementation,
                        changed_files = changed_graph_files.len(),
                        graph_nodes = stats.graph_nodes,
                        files_deleted = stats.files_deleted,
                        nodes_considered = stats.nodes_considered,
                        embedded_nodes = stats.embedded_nodes,
                        failed_nodes = stats.failed_nodes,
                        payload_chars_total = stats.payload_chars_total,
                        payload_chars_avg = stats.payload_chars_avg,
                        payload_chars_max = stats.payload_chars_max,
                        vector_sync_elapsed_ms = stats.elapsed_ms as u64,
                        watcher_sync_elapsed_ms = watcher_sync_elapsed_ms as u64,
                        throughput_nodes_per_sec = stats.throughput_nodes_per_sec(),
                        "Watcher semantic sync complete"
                    );
                }
            }
            Err(err) => tracing::warn!("Failed to sync semantic index after file change: {}", err),
        }

        if let Some(epoch) = target_epoch {
            let mut repo_state = self.repo_state.lock().await;
            repo_state.mark_published_epoch(epoch);
            self.indexing.store(false, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PollFileState {
    modified_ns: u128,
    size_bytes: u64,
}

fn poll_interval() -> Duration {
    std::env::var("LATTICE_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL)
}

fn should_invalidate_workspace(workspace_root: &Path, paths: &[PathBuf]) -> bool {
    paths.len() >= WORKSPACE_INVALIDATION_BATCH_THRESHOLD
        || paths.iter().any(|path| {
            let rel = path
                .strip_prefix(workspace_root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            is_git_state_path(&rel)
        })
}

fn is_git_state_path(rel_path: &str) -> bool {
    let normalized = rel_path.replace('\\', "/");
    matches!(
        normalized.as_str(),
        ".git/HEAD"
            | ".git/index"
            | ".git/ORIG_HEAD"
            | ".git/MERGE_HEAD"
            | ".git/REBASE_HEAD"
            | ".git/packed-refs"
    ) || normalized.starts_with(".git/rebase-apply/")
        || normalized.starts_with(".git/rebase-merge/")
        || normalized.starts_with(".git/refs/heads/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_core::graph::CodeGraph;
    use lattice_core::memory::MemoryStore;

    #[tokio::test]
    async fn forced_watch_failure_enters_degraded_polling_mode() {
        let root = unique_test_root("watch-degraded");
        std::fs::write(root.join("src.rs"), "fn main() {}\n").expect("write source");
        let graph = Arc::new(CodeGraph::new());
        let memory_store = Arc::new(std::sync::Mutex::new(
            MemoryStore::open_in_memory().expect("memory store"),
        ));
        let engine = Arc::new(Mutex::new(QueryEngine::new_shared(
            graph,
            None,
            Some(memory_store),
        )));
        let indexer = Arc::new(Mutex::new(Indexer::new(root.clone())));
        let graph_store = Arc::new(Mutex::new(
            GraphStore::open_in_memory().expect("graph store"),
        ));
        let repo_state = Arc::new(Mutex::new(RepoStateTracker::new(&root)));
        let indexing = Arc::new(AtomicBool::new(false));
        let health = Arc::new(WatcherHealth::default());
        let watcher = FileWatcher::new(
            root.clone(),
            None,
            Some(indexer),
            None,
            graph_store,
            engine,
            Arc::new(OnceLock::new()),
            None,
            repo_state,
            indexing,
            Arc::clone(&health),
        )
        .with_forced_watch_failure("forced notify setup failure");

        let task = tokio::spawn(async move { watcher.run().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let snapshot = health.snapshot();
        task.abort();
        let _ = std::fs::remove_dir_all(root);

        assert!(snapshot.watch_degraded);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("forced notify setup failure")
        );
        assert_eq!(snapshot.polling_interval_secs, Some(30));
        assert!(snapshot.last_poll_epoch_secs.is_some());
    }

    fn unique_test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lattice-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create test root");
        path
    }
}
