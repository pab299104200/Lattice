use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::indexer::Indexer;
use lattice_core::query::QueryEngine;
use lattice_core::storage::{GraphStore, SharedVectorIndex};
use lattice_core::workspace::{repo_rel_path, WorkspaceManager};

const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

/// File system watcher that triggers incremental indexing on changes.
pub struct FileWatcher {
    workspace_root: PathBuf,
    indexer: Option<Arc<Mutex<Indexer>>>,
    workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
    graph_store: Arc<Mutex<GraphStore>>,
    query_engine: Arc<Mutex<QueryEngine>>,
    embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
    vector_index: Option<SharedVectorIndex>,
}

impl FileWatcher {
    pub fn new(
        workspace_root: PathBuf,
        indexer: Option<Arc<Mutex<Indexer>>>,
        workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
        graph_store: Arc<Mutex<GraphStore>>,
        query_engine: Arc<Mutex<QueryEngine>>,
        embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
        vector_index: Option<SharedVectorIndex>,
    ) -> Self {
        Self {
            workspace_root,
            indexer,
            workspace_manager,
            graph_store,
            query_engine,
            embedding_engine,
            vector_index,
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
        // Skip directories and obvious non-code files to reduce noise
        if path.is_dir() {
            return false;
        }
        // Basic check to ensure we don't index .git or huge binary blobs
        let s = path.to_string_lossy();
        !s.contains("/.git/") && !s.contains("/target/") && !s.contains("/node_modules/")
    }

    async fn process_changes(&self, paths: Vec<PathBuf>) {
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
                let new_graph = manager.unified_graph();
                drop(manager);
                self.persist_publish_and_sync(new_graph, changed_graph_files)
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

        for path in paths {
            let rel_path = match path.strip_prefix(&self.workspace_root) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if path.exists() {
                // Only re-index the specific file that changed
                if let Ok(content) = std::fs::read_to_string(&path) {
                    info!("Incremental index update: {}", rel_path);
                    if let Ok(_) = indexer.index_file_content(&rel_path, &content) {
                        graph_changed = true;
                        changed_graph_files.push(rel_path.clone());
                    }
                }
            } else {
                info!("Removing file from index: {}", rel_path);
                indexer.remove_file(&rel_path);
                graph_changed = true;
                changed_graph_files.push(rel_path.clone());
            }
        }

        if graph_changed {
            // This clone is now cheap/instant due to Arc<str> optimization
            let new_graph = indexer.graph().clone();
            drop(indexer);
            self.persist_publish_and_sync(new_graph, changed_graph_files)
                .await;
        }
    }

    async fn persist_publish_and_sync(
        &self,
        new_graph: lattice_core::graph::CodeGraph,
        changed_graph_files: Vec<String>,
    ) {
        {
            let graph_store = self.graph_store.lock().await;
            let _ = graph_store.save_graph(&new_graph);
        }

        let mut engine = self.query_engine.lock().await;
        engine.update_graph(new_graph.clone());
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
    }
}
