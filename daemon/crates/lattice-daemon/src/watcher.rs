use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::indexer::Indexer;
use lattice_core::query::QueryEngine;
use lattice_core::storage::{GraphStore, SharedVectorIndex};
use lattice_core::workspace::{repo_rel_path, WorkspaceManager};

use crate::repo_state::RepoStateTracker;

const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);
const WORKSPACE_INVALIDATION_BATCH_THRESHOLD: usize = 20;

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
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let (tx, mut rx) = mpsc::channel(100);

        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
            if let Ok(event) = res {
                let _ = tx.blocking_send(event);
            }
        })?;

        watcher.watch(&self.workspace_root, RecursiveMode::Recursive)?;
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
