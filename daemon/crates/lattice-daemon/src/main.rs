#![recursion_limit = "256"]

mod rpc;
mod watcher;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryStore;
use lattice_core::query::QueryEngine;
use lattice_core::security::SecurityFilter;
use lattice_core::storage::{GraphStore, VectorStore};
use lattice_core::watcher as core_watcher;
use lattice_core::workspace::WorkspaceManager;
use rpc::mcp::McpHandler;
use rpc::server::StdioServer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Lattice daemon starting...");

    // ── Parse workspace roots ────────────────────────────────────────
    let workspace_roots = parse_workspace_roots();
    let workspace_root = workspace_roots[0].clone();
    let is_multi_repo = workspace_roots.len() > 1;
    if is_multi_repo {
        tracing::info!("Multi-repo mode: {} workspaces", workspace_roots.len());
        for r in &workspace_roots {
            tracing::info!("  - {}", r.display());
        }
    }

    // ── Create EMPTY engine + stores — start server IMMEDIATELY ──────
    // Everything else happens in background so MCP handshake isn't delayed.
    let graph = CodeGraph::new();

    // Create .lattice dir for persistent storage
    let lattice_dir = workspace_root.join(".lattice");
    let _ = std::fs::create_dir_all(&lattice_dir);

    // File-backed memory store — observations persist across daemon restarts
    let memories_path = lattice_dir.join("memories.db");
    let ms = MemoryStore::open(&memories_path).expect("Failed to open memory store");
    let memory_store = Arc::new(Mutex::new(ms));
    let ms_for_engine =
        MemoryStore::open(&memories_path).expect("Failed to open memory store for engine");
    let vector_store = {
        let vs_path = lattice_dir.join("vectors.db");
        match VectorStore::open(&vs_path.to_string_lossy()) {
            Ok(vs) => {
                if let Err(e) = vs.initialize(384) {
                    tracing::warn!("Failed to initialize vector store: {}", e);
                    None
                } else {
                    let _ = vs.load_cache(); // Warm start from previous run
                    Some(vs)
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to open vector store, semantic search disabled: {}",
                    e
                );
                None
            }
        }
    };

    let engine = QueryEngine::new(
        graph,
        vector_store,
        Some(Arc::new(std::sync::Mutex::new(ms_for_engine))),
    );
    let engine = Arc::new(Mutex::new(engine));
    let indexer = Arc::new(Mutex::new(Indexer::new(workspace_root.clone())));
    let graph_store = Arc::new(Mutex::new(
        GraphStore::open_in_memory().expect("Failed to create in-memory graph store"),
    ));

    // Multi-repo workspace manager (only used when multiple workspaces)
    let workspace_manager: Option<Arc<Mutex<WorkspaceManager>>> = if is_multi_repo {
        Some(Arc::new(Mutex::new(WorkspaceManager::new())))
    } else {
        None
    };

    // OnceLock for EmbeddingEngine — populated in background once model loads
    let embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>> = Arc::new(OnceLock::new());

    // Shared indexing state flag
    let indexing = Arc::new(std::sync::atomic::AtomicBool::new(true));

    // ── Spawn background indexing task ────────────────────────────────
    {
        let engine_bg = Arc::clone(&engine);
        let indexer_bg = Arc::clone(&indexer);
        let graph_store_bg = Arc::clone(&graph_store);
        let embedding_engine_bg = Arc::clone(&embedding_engine);
        let indexing_bg = Arc::clone(&indexing);
        let ws_manager_bg = workspace_manager.clone();
        let ws_roots_bg = workspace_roots.clone();
        let ws_root = workspace_root.clone();
        let lattice_dir_bg = ws_root.join(".lattice");

        tokio::spawn(async move {
            tracing::info!("Background indexing starting...");
            let _ = std::fs::create_dir_all(&lattice_dir_bg);

            let files_indexed = if let Some(wm) = &ws_manager_bg {
                // Multi-repo: index each workspace through WorkspaceManager
                let wm_clone = Arc::clone(wm);
                let roots = ws_roots_bg.clone();
                tokio::task::spawn_blocking(move || {
                    let mut wm = wm_clone.blocking_lock();
                    let mut total = 0usize;
                    for root in &roots {
                        let repo_name = repo_name_for_root(root);
                        if let Err(e) = wm.add_repo(repo_name.clone(), root.clone()) {
                            tracing::warn!("Failed to add repo {}: {}", repo_name, e);
                            continue;
                        }
                        let sf = SecurityFilter::new(root);
                        let count = index_workspace_via_manager(root, &repo_name, &mut wm, &sf);
                        tracing::info!("Indexed {} files from repo '{}'", count, repo_name);
                        total += count;
                    }
                    wm.detect_cross_repo_edges();
                    tracing::info!("Detected {} cross-repo edges", wm.cross_repo_edges().len());
                    total
                })
                .await
                .unwrap_or(0)
            } else {
                // Single repo: collect files, then index in batches with engine updates
                let ws = ws_root.clone();
                let sf = SecurityFilter::new(&ws);

                // Collect all file paths first (fast, no lock needed)
                let all_files = collect_indexable_files(&ws, &sf);
                tracing::info!("Found {} files to index", all_files.len());

                let mut total_indexed = 0usize;
                for chunk in all_files.chunks(100) {
                    {
                        let mut idx = indexer_bg.lock().await;
                        for path in chunk {
                            let rel_path = path
                                .strip_prefix(&ws)
                                .unwrap_or(path)
                                .to_string_lossy()
                                .replace('\\', "/");
                            if let Ok(content) = std::fs::read_to_string(path) {
                                if idx.index_file_content(&rel_path, &content).is_ok() {
                                    total_indexed += 1;
                                }
                            }
                        }
                        // Push graph snapshot to engine after each batch
                        let snapshot = idx.graph().clone();
                        let mut eng = engine_bg.lock().await;
                        eng.update_graph(snapshot);
                    }
                    tracing::info!("Indexed {}/{} files", total_indexed, all_files.len());
                }
                total_indexed
            };
            tracing::info!("Indexed {} files total", files_indexed);

            // Final save to graph store
            {
                let new_graph = if let Some(wm) = &ws_manager_bg {
                    let wm = wm.lock().await;
                    wm.unified_graph()
                } else {
                    let idx = indexer_bg.lock().await;
                    idx.graph().clone()
                };

                let stats = new_graph.stats();
                tracing::info!(
                    "Graph ready: {} nodes, {} edges, {} files",
                    stats.node_count,
                    stats.edge_count,
                    stats.file_count
                );

                {
                    let gs = graph_store_bg.lock().await;
                    if let Err(e) = gs.save_graph(&new_graph) {
                        tracing::warn!("Failed to save graph: {}", e);
                    }
                }

                let mut eng = engine_bg.lock().await;
                eng.update_graph(new_graph);
            }

            // Try to load ONNX embedding model
            let model_path = lattice_dir_bg.join("models").join("model.onnx");
            if model_path.exists() {
                match EmbeddingEngine::new(model_path.to_string_lossy().as_ref()) {
                    Ok(emb_engine) => {
                        tracing::info!("ONNX embedding model loaded");
                        let emb = Arc::new(emb_engine);
                        let _ = embedding_engine_bg.set(Arc::clone(&emb));

                        let eng = engine_bg.lock().await;
                        let nodes = eng.graph().all_nodes();
                        let mut embedded = 0usize;
                        for node in &nodes {
                            let text = format!("{} {}", node.name, node.signature);
                            if let Ok(vec) = emb.embed(&text) {
                                if let Some(vs) = eng.vector_store() {
                                    let _ = vs.upsert_vector(
                                        &node.file,
                                        &node.name,
                                        node.id.byte_offset,
                                        &vec,
                                    );
                                }
                                embedded += 1;
                            }
                        }
                        tracing::info!("Embedded {} nodes", embedded);
                    }
                    Err(e) => {
                        tracing::info!("No ONNX model: {}", e);
                    }
                }
            }

            indexing_bg.store(false, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("Background indexing complete");
        });
    }

    // ── Start file watcher(s) entirely in background ────────────────
    // For multi-repo, we merge all watcher events into a single channel.
    {
        let engine = Arc::clone(&engine);
        let indexer = Arc::clone(&indexer);
        let graph_store = Arc::clone(&graph_store);
        let workspace_manager = workspace_manager.clone();
        let workspace_roots = workspace_roots.clone();

        tokio::spawn(async move {
            for root in workspace_roots {
                let watcher = crate::watcher::FileWatcher::new(
                    root.clone(),
                    Some(Arc::clone(&indexer)),
                    workspace_manager.clone(),
                    Arc::clone(&graph_store),
                    Arc::clone(&engine),
                );

                tokio::spawn(async move {
                    if let Err(e) = watcher.run().await {
                        tracing::error!("File watcher failed for {:?}: {}", root, e);
                    }
                });
            }
        });
    }

    // ── Periodic memory decay / prune ─────────────────────────────────
    {
        let memory_store_decay = Arc::clone(&memory_store);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let ms = memory_store_decay.lock().await;
                let decayed = ms.decay_old_memories(7, 0.1).unwrap_or(0);
                let pruned = ms.prune_old_memories(0.2, 30).unwrap_or(0);
                if decayed > 0 || pruned > 0 {
                    tracing::info!("Memory maintenance: decayed {}, pruned {}", decayed, pruned);
                }
            }
        });
    }

    // ── Create McpHandler and start StdioServer ──────────────────────
    let session_id = generate_session_id();
    let context_cache_path = lattice_dir.join("context_handles.json");
    tracing::info!("Creating MCP handler (session: {})", session_id);
    let handler = Arc::new(McpHandler::new(
        engine,
        indexer,
        memory_store,
        graph_store,
        embedding_engine,
        workspace_root,
        context_cache_path,
        session_id,
        workspace_manager,
        workspace_roots,
        indexing,
    ));
    tracing::info!("Starting stdio server");
    let server = StdioServer::new(handler);
    server.run().await?;
    tracing::info!("Stdio server exited");

    Ok(())
}

/// Parse workspace roots from command-line args.
/// Supports multiple `--workspace <path>` flags. Defaults to current directory.
/// Applies anti-double-indexing: if root A is a parent of root B, B is dropped.
fn parse_workspace_roots() -> Vec<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let mut roots = Vec::new();

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--workspace" || args[i] == "-w" {
            if let Some(path) = args.get(i + 1) {
                let p = PathBuf::from(path);
                if p.is_dir() {
                    roots.push(p.canonicalize().unwrap_or(p));
                } else {
                    eprintln!(
                        "Warning: --workspace path '{}' is not a directory, skipping",
                        path
                    );
                }
                i += 2;
                continue;
            }
        }
        i += 1;
    }

    if roots.is_empty() {
        roots.push(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    }

    // Anti-double-indexing: remove any root that is a subdirectory of another
    deduplicate_roots(roots)
}

pub(crate) fn repo_name_for_root(root: &Path) -> String {
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default")
        .to_string()
}

/// Remove roots that are subdirectories of other roots.
fn deduplicate_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort_by(|a, b| a.as_os_str().len().cmp(&b.as_os_str().len()));
    let mut result = Vec::new();
    for root in &roots {
        let is_child = result
            .iter()
            .any(|parent: &PathBuf| root.starts_with(parent));
        if !is_child {
            result.push(root.clone());
        }
    }
    if result.is_empty() && !roots.is_empty() {
        result.push(roots[0].clone());
    }
    result
}

/// Generate a unique session identifier using timestamp + hash.
fn generate_session_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    let mut hasher = DefaultHasher::new();
    now.as_nanos().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let h = hasher.finish();

    format!("s-{:08x}{:08x}", (h >> 32) as u32, now.subsec_nanos())
}

/// Walk the workspace directory and index via WorkspaceManager.
fn index_workspace_via_manager(
    root: &PathBuf,
    repo_name: &str,
    manager: &mut WorkspaceManager,
    security_filter: &SecurityFilter,
) -> usize {
    let mut count = 0;
    let rn = repo_name.to_string();
    walk_and_index(
        root,
        root,
        security_filter,
        &mut count,
        &mut |rel_path, content| manager.index_file(&rn, rel_path, content).is_ok(),
    );
    count
}

/// Collect all indexable file paths from a workspace root.
/// Returns paths filtered by SecurityFilter and supported language extensions.
fn collect_indexable_files(root: &PathBuf, security_filter: &SecurityFilter) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(root, root, security_filter, &mut files);
    files
}

/// Recursively collect indexable files into the output vec.
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

        if path.is_dir() {
            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                if security_filter.is_excluded_dir(dir_name) {
                    continue;
                }
            }
            collect_files_recursive(&path, root, security_filter, out);
        } else if path.is_file() {
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            if security_filter.is_excluded(&rel_path) {
                continue;
            }

            if !core_watcher::should_index_file(&rel_path) {
                continue;
            }

            out.push(path);
        }
    }
}

/// Recursively walk a directory and index files via a callback.
fn walk_and_index(
    dir: &PathBuf,
    root: &PathBuf,
    security_filter: &SecurityFilter,
    count: &mut usize,
    index_fn: &mut dyn FnMut(&str, &str) -> bool,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            // Use SecurityFilter for directory exclusions (includes .gitignore patterns)
            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                if security_filter.is_excluded_dir(dir_name) {
                    continue;
                }
            }
            walk_and_index(&path, root, security_filter, count, index_fn);
        } else if path.is_file() {
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            // Check security filter (.gitignore + .lattice_ignore + default patterns + excluded dirs)
            if security_filter.is_excluded(&rel_path) {
                continue;
            }

            // Check for supported language extension
            if !core_watcher::should_index_file(&rel_path) {
                continue;
            }

            // Read and index
            if let Ok(content) = std::fs::read_to_string(&path) {
                if index_fn(&rel_path, &content) {
                    *count += 1;
                }
            }
        }
    }
}
