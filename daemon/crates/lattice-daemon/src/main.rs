mod rpc;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use anyhow::Result;
use tracing_subscriber::EnvFilter;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryStore;
use lattice_core::query::QueryEngine;
use lattice_core::security::SecurityFilter;
use lattice_core::storage::{GraphStore, VectorStore};
use lattice_core::watcher::{self, FileEventKind};
use rpc::mcp::McpHandler;
use rpc::server::StdioServer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr) // stderr for logs, stdout for JSON-RPC
        .init();

    tracing::info!("Lattice daemon starting...");

    // ── Parse workspace root from args or use cwd ────────────────────
    let workspace_root = parse_workspace_root();
    tracing::info!("Workspace root: {}", workspace_root.display());

    // ── Create .lattice/ directory ───────────────────────────────────
    let lattice_dir = workspace_root.join(".lattice");
    if !lattice_dir.exists() {
        std::fs::create_dir_all(&lattice_dir)?;
        tracing::info!("Created .lattice/ directory");
    }

    // ── Open SQLite stores ───────────────────────────────────────────
    let graph_store = GraphStore::open(&lattice_dir.join("graph.db"))
        .expect("Failed to open graph store");
    let graph_store = Arc::new(Mutex::new(graph_store));

    let memory_store = MemoryStore::open(&lattice_dir.join("memory.db"))
        .expect("Failed to open memory store");
    let memory_store = Arc::new(Mutex::new(memory_store));

    let vector_store = VectorStore::open(
        lattice_dir.join("vectors.db").to_string_lossy().as_ref(),
    )
    .expect("Failed to open vector store");
    vector_store
        .initialize(384) // default embedding dimension
        .expect("Failed to initialize vector store");

    // ── Try to load existing graph from graph_store, or create new ──
    let graph = {
        let gs = graph_store.lock().await;
        match gs.load_graph() {
            Ok(g) if g.node_count() > 0 => {
                tracing::info!(
                    "Loaded existing graph: {} nodes, {} edges",
                    g.node_count(),
                    g.edge_count()
                );
                g
            }
            _ => {
                tracing::info!("No existing graph found, starting fresh");
                CodeGraph::new()
            }
        }
    };

    // ── Create Indexer and index workspace directory ──────────────────
    let security_filter = SecurityFilter::new(&workspace_root);
    let mut indexer = Indexer::new(workspace_root.clone());

    // Walk workspace and index all supported files
    let files_indexed = index_workspace(&workspace_root, &mut indexer, &security_filter);
    tracing::info!("Indexed {} files", files_indexed);

    // Use the indexer's graph if we indexed files, otherwise use the loaded one
    let graph = if files_indexed > 0 {
        let new_graph = indexer.graph().clone();
        // Save to graph store
        {
            let gs = graph_store.lock().await;
            if let Err(e) = gs.save_graph(&new_graph) {
                tracing::warn!("Failed to save graph: {}", e);
            }
        }
        new_graph
    } else {
        graph
    };

    let stats = graph.stats();
    tracing::info!(
        "Graph ready: {} nodes, {} edges, {} files",
        stats.node_count,
        stats.edge_count,
        stats.file_count
    );

    // ── Try to load ONNX embedding model and embed graph nodes ───────
    let model_path = lattice_dir.join("models").join("model.onnx");
    let embedding_engine: Option<Arc<EmbeddingEngine>> = if model_path.exists() {
        match EmbeddingEngine::new(model_path.to_string_lossy().as_ref()) {
            Ok(eng) => {
                tracing::info!("ONNX embedding model loaded from {}", model_path.display());
                Some(Arc::new(eng))
            }
            Err(e) => {
                tracing::warn!("Failed to load ONNX model: {}. Semantic search disabled.", e);
                None
            }
        }
    } else {
        tracing::info!(
            "No ONNX model found at {}. Run scripts/download-model.sh to enable semantic search.",
            model_path.display()
        );
        None
    };

    // Embed all graph nodes into the vector store if embedding engine is available
    if let Some(ref emb_engine) = embedding_engine {
        let mut embedded_count = 0usize;
        for node in graph.all_nodes() {
            let text = format!("{} {}", node.name, node.signature);
            match emb_engine.embed(&text) {
                Ok(vec) => {
                    if let Err(e) = vector_store.upsert_vector(&node.file, &node.name, node.id.byte_offset, &vec) {
                        tracing::debug!("Failed to store embedding for {}: {}", node.name, e);
                    } else {
                        embedded_count += 1;
                    }
                }
                Err(e) => {
                    tracing::debug!("Failed to embed {}: {}", node.name, e);
                }
            }
        }
        tracing::info!("Embedded {} graph nodes into vector store", embedded_count);
    }

    // ── Create QueryEngine ───────────────────────────────────────────
    let vector_store_opt = if embedding_engine.is_some() {
        Some(vector_store)
    } else {
        None
    };
    // Open a second MemoryStore connection for the QueryEngine (std::sync::Mutex)
    // so it can be used in the synchronous query() method
    let memory_store_for_engine = {
        let ms = MemoryStore::open(&lattice_dir.join("memory.db"))
            .expect("Failed to open memory store for query engine");
        Arc::new(std::sync::Mutex::new(ms))
    };
    let engine = QueryEngine::new(graph, vector_store_opt, Some(memory_store_for_engine));
    let engine = Arc::new(Mutex::new(engine));
    let embedding_engine_for_watcher = embedding_engine.clone();
    let indexer = Arc::new(Mutex::new(indexer));

    // ── Start file watcher ───────────────────────────────────────────
    let watcher_result = watcher::start_watcher(workspace_root.clone());
    if let Ok((_watcher, mut rx)) = watcher_result {
        let engine_clone = Arc::clone(&engine);
        let indexer_clone = Arc::clone(&indexer);
        let graph_store_clone = Arc::clone(&graph_store);
        let emb_engine_clone = embedding_engine_for_watcher;
        let ws_root = workspace_root.clone();

        // Spawn a task that listens for FileEvents and incrementally re-indexes
        tokio::spawn(async move {
            // Keep the _watcher alive so it keeps watching
            let _watcher_handle = _watcher;

            while let Some(event) = rx.recv().await {
                let rel_path = event
                    .path
                    .strip_prefix(&ws_root)
                    .unwrap_or(&event.path)
                    .to_string_lossy()
                    .replace('\\', "/");

                tracing::debug!("File event: {:?} {}", event.kind, rel_path);

                let mut idx = indexer_clone.lock().await;

                match event.kind {
                    FileEventKind::Created | FileEventKind::Modified => {
                        if let Ok(content) = std::fs::read_to_string(&event.path) {
                            if let Err(e) = idx.index_file_content(&rel_path, &content) {
                                tracing::warn!("Failed to re-index {}: {}", rel_path, e);
                                continue;
                            }
                        }
                    }
                    FileEventKind::Deleted => {
                        idx.remove_file(&rel_path);
                    }
                }

                // Update engine with new graph
                let new_graph = idx.graph().clone();

                // Re-embed changed nodes if embedding engine is available
                if let Some(ref emb_engine) = emb_engine_clone {
                    let file_nodes: Vec<_> = new_graph.all_nodes().into_iter()
                        .filter(|n| n.file == rel_path)
                        .collect();
                    let mut eng = engine_clone.lock().await;
                    for node in &file_nodes {
                        let text = format!("{} {}", node.name, node.signature);
                        if let Ok(vec) = emb_engine.embed(&text) {
                            if let Some(vs) = eng.vector_store() {
                                let _ = vs.upsert_vector(&node.file, &node.name, node.id.byte_offset, &vec);
                            }
                        }
                    }
                    eng.update_graph(new_graph.clone());
                } else {
                    let mut eng = engine_clone.lock().await;
                    eng.update_graph(new_graph.clone());
                }

                // Persist to graph store (best effort)
                {
                    let gs = graph_store_clone.lock().await;
                    if let Err(e) = gs.save_graph(&new_graph) {
                        tracing::warn!("Failed to persist graph: {}", e);
                    }
                }
            }
        });

        tracing::info!("File watcher started");
    } else {
        tracing::warn!("Failed to start file watcher, running without live updates");
    }

    // ── Create McpHandler and start StdioServer ──────────────────────
    let handler = Arc::new(McpHandler::new(
        engine,
        indexer,
        memory_store,
        graph_store,
        workspace_root,
    ));
    let server = StdioServer::new(handler);
    server.run().await?;

    Ok(())
}

/// Parse workspace root from command-line args.
/// Supports `--workspace <path>` or defaults to current directory.
fn parse_workspace_root() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();

    for i in 0..args.len() {
        if args[i] == "--workspace" || args[i] == "-w" {
            if let Some(path) = args.get(i + 1) {
                let p = PathBuf::from(path);
                if p.is_dir() {
                    return p.canonicalize().unwrap_or(p);
                } else {
                    eprintln!("Warning: --workspace path '{}' is not a directory, using cwd", path);
                }
            }
        }
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Walk the workspace directory and index all supported files.
/// Returns the number of files successfully indexed.
fn index_workspace(
    root: &PathBuf,
    indexer: &mut Indexer,
    security_filter: &SecurityFilter,
) -> usize {
    let mut count = 0;
    walk_and_index(root, root, indexer, security_filter, &mut count);
    count
}

/// Recursively walk a directory and index files.
fn walk_and_index(
    dir: &PathBuf,
    root: &PathBuf,
    indexer: &mut Indexer,
    security_filter: &SecurityFilter,
    count: &mut usize,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            // Skip excluded directories
            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                if watcher::EXCLUDED_DIRS.contains(&dir_name) {
                    continue;
                }
            }
            walk_and_index(&path, root, indexer, security_filter, count);
        } else if path.is_file() {
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            // Check watcher filter (extension + excluded dirs/patterns)
            if !watcher::should_index_file(&rel_path) {
                continue;
            }

            // Check security filter (.lattice_ignore + default secret patterns)
            if security_filter.is_excluded(&rel_path) {
                continue;
            }

            // Read and index
            if let Ok(content) = std::fs::read_to_string(&path) {
                match indexer.index_file_content(&rel_path, &content) {
                    Ok(()) => *count += 1,
                    Err(e) => {
                        tracing::debug!("Failed to index {}: {}", rel_path, e);
                    }
                }
            }
        }
    }
}
