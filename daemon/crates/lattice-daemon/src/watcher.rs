use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use lattice_core::indexer::Indexer;
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use lattice_core::workspace::WorkspaceManager;

const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

/// File system watcher that triggers incremental indexing on changes.
pub struct FileWatcher {
    workspace_root: PathBuf,
    indexer: Option<Arc<Mutex<Indexer>>>,
    workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
    graph_store: Arc<Mutex<GraphStore>>,
    query_engine: Arc<Mutex<QueryEngine>>,
}

impl FileWatcher {
    pub fn new(
        workspace_root: PathBuf,
        indexer: Option<Arc<Mutex<Indexer>>>,
        workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
        graph_store: Arc<Mutex<GraphStore>>,
        query_engine: Arc<Mutex<QueryEngine>>,
    ) -> Self {
        Self {
            workspace_root,
            indexer,
            workspace_manager,
            graph_store,
            query_engine,
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
        let mut debounce_timer = Box::pin(tokio::time::sleep(DEBOUNCE_DURATION));

        loop {
            tokio::select! {
                Some(event) = rx.recv() => {
                    self.handle_event(event, &mut changed_paths);
                    debounce_timer = Box::pin(tokio::time::sleep(DEBOUNCE_DURATION));
                },
                _ = &mut debounce_timer => {
                    if !changed_paths.is_empty() {
                        let paths = std::mem::take(&mut changed_paths);
                        self.process_changes(paths).await;
                    }
                },
                else => break,
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
            let mut manager = workspace_manager.lock().await;

            for path in paths {
                let rel_path = match path.strip_prefix(&self.workspace_root) {
                    Ok(p) => p.to_string_lossy().to_string(),
                    Err(_) => continue,
                };

                if path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        info!(
                            "Incremental multi-repo index update: {}/{}",
                            repo_name, rel_path
                        );
                        if manager.index_file(&repo_name, &rel_path, &content).is_ok() {
                            graph_changed = true;
                        }
                    }
                } else {
                    info!(
                        "Removing file from multi-repo index: {}/{}",
                        repo_name, rel_path
                    );
                    manager.remove_file(&repo_name, &rel_path);
                    graph_changed = true;
                }
            }

            if graph_changed {
                manager.detect_cross_repo_edges();
                let new_graph = manager.unified_graph();
                drop(manager);
                self.persist_and_publish(new_graph).await;
            }
            return;
        }

        let Some(indexer_handle) = &self.indexer else {
            return;
        };
        let mut graph_changed = false;
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
                    }
                }
            } else {
                info!("Removing file from index: {}", rel_path);
                indexer.remove_file(&rel_path);
                graph_changed = true;
            }
        }

        if graph_changed {
            // This clone is now cheap/instant due to Arc<str> optimization
            let new_graph = indexer.graph().clone();
            drop(indexer);
            self.persist_and_publish(new_graph).await;
        }
    }

    async fn persist_and_publish(&self, new_graph: lattice_core::graph::CodeGraph) {
        {
            let graph_store = self.graph_store.lock().await;
            let _ = graph_store.save_graph(&new_graph);
        }

        let mut engine = self.query_engine.lock().await;
        engine.update_graph(new_graph);
    }
}
