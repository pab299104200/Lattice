use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use lattice_core::indexer::Indexer;
use lattice_core::query::QueryEngine;

const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

/// File system watcher that triggers incremental indexing on changes.
pub struct FileWatcher {
    workspace_root: PathBuf,
    indexer: Arc<Mutex<Indexer>>,
    query_engine: Arc<Mutex<QueryEngine>>,
}

impl FileWatcher {
    pub fn new(
        workspace_root: PathBuf,
        indexer: Arc<Mutex<Indexer>>,
        query_engine: Arc<Mutex<QueryEngine>>,
    ) -> Self {
        Self {
            workspace_root,
            indexer,
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
        let mut debounce_timer = tokio::time::sleep(DEBOUNCE_DURATION);

        loop {
            tokio::select! {
                Some(event) = rx.recv() => {
                    self.handle_event(event, &mut changed_paths);
                    debounce_timer = tokio::time::sleep(DEBOUNCE_DURATION);
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
        if matches!(event.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)) {
            for path in event.paths {
                if !changed_paths.contains(&path) && self.should_process(&path) {
                    changed_paths.push(path);
                }
            }
        }
    }

    fn should_process(&self, path: &Path) -> bool {
        // Skip directories and obvious non-code files to reduce noise
        if path.is_dir() { return false; }
        true
    }

    async fn process_changes(&self, paths: Vec<PathBuf>) {
        let mut graph_changed = false;
        let mut indexer = self.indexer.lock().expect("Indexer lock failed");

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
            
            let mut engine = self.query_engine.lock().expect("Engine lock failed");
            engine.update_graph(new_graph);
        }
    }
}