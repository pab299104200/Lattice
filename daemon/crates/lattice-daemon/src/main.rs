#![recursion_limit = "256"]

mod adoption_metrics;
mod cli;
mod doctor;
mod lifecycle_log;
mod proxy;
mod repo_state;
mod rpc;
mod runtime_support;
mod socket_server;
mod vector_sync;
mod watcher;
mod watcher_health;

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::events::{
    CompactionConfig, Compactor, EventStore, EventWriter, FlushPolicy, SchedulerHandle,
};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryStore;
use lattice_core::memory_graph::MemoryMigrator;
use lattice_core::parser;
use lattice_core::query::QueryEngine;
use lattice_core::security::SecurityFilter;
use lattice_core::storage::{
    FileIndexEntry, GraphStore, SharedVectorIndex, UsearchVectorIndex, VectorIndex, VectorStore,
    FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use lattice_core::symbols::ParsedFile;
use lattice_core::watcher as core_watcher;
use lattice_core::workspace::WorkspaceManager;
use rpc::mcp::McpHandler;
use rpc::server::StdioServer;
use watcher_health::WatcherHealth;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    if is_memory_migrate_command() {
        return run_memory_migrate_cli();
    }

    if is_doctor_command() {
        let workspace_roots = parse_workspace_roots()?;
        let ok = doctor::run(workspace_roots).await?;
        if !ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    if cli::is_cli_query_command() {
        std::process::exit(cli::run_from_env().await);
    }

    if has_arg("--daemon") {
        tracing::info!("Lattice global daemon starting...");
        lifecycle_log::log_event(
            "daemon",
            "process_start",
            &[(
                "argv",
                serde_json::json!(std::env::args().collect::<Vec<_>>()),
            )],
        );
        let result = socket_server::run_global_daemon().await;
        match &result {
            Ok(()) => lifecycle_log::log_event(
                "daemon",
                "process_exit",
                &[("status", serde_json::json!("ok"))],
            ),
            Err(error) => lifecycle_log::log_event(
                "daemon",
                "process_exit",
                &[
                    ("status", serde_json::json!("error")),
                    ("error", serde_json::json!(error.to_string())),
                ],
            ),
        }
        return result;
    }

    if has_arg("--stdio") {
        let workspace_roots = parse_workspace_roots()?;
        let workspace_root = workspace_roots[0].clone();
        let default_focus = parse_focus_args(&workspace_root);
        let hello = proxy::ProxyHello {
            workspace_roots: workspace_roots
                .iter()
                .map(|root| root.to_string_lossy().to_string())
                .collect(),
            focus_files: default_focus.files,
            focus_dirs: default_focus.dirs,
        };
        tracing::info!("Starting lightweight stdio proxy");
        lifecycle_log::log_event(
            "proxy",
            "process_start",
            &[
                (
                    "workspace_roots",
                    serde_json::json!(hello.workspace_roots.clone()),
                ),
                ("daemon_addr", serde_json::json!(proxy::daemon_addr())),
            ],
        );
        let result = proxy::run_stdio_proxy(hello).await;
        match &result {
            Ok(()) => lifecycle_log::log_event(
                "proxy",
                "process_exit",
                &[("status", serde_json::json!("ok"))],
            ),
            Err(error) => lifecycle_log::log_event(
                "proxy",
                "process_exit",
                &[
                    ("status", serde_json::json!("error")),
                    ("error", serde_json::json!(error.to_string())),
                ],
            ),
        }
        return result;
    }

    if !has_arg("--daemon") && !has_arg("--stdio") {
        std::process::exit(cli::run_usage_or_error());
    }

    tracing::info!("Lattice daemon starting...");

    // ── Parse workspace roots ────────────────────────────────────────
    let workspace_roots = parse_workspace_roots()?;
    let workspace_root = workspace_roots[0].clone();
    let default_focus = parse_focus_args(&workspace_root);
    let is_multi_repo = workspace_roots.len() > 1;
    if is_multi_repo {
        tracing::info!("Multi-repo mode: {} workspaces", workspace_roots.len());
        for r in &workspace_roots {
            tracing::info!("  - {}", r.display());
        }
    }

    // Create .lattice dir for persistent storage
    let lattice_dir = workspace_root.join(".lattice");
    let _ = std::fs::create_dir_all(&lattice_dir);

    // File-backed memory store — observations persist across daemon restarts when available.
    let memories_path = lattice_dir.join("memories.db");
    let (memory_store, ms_for_engine, memory_mode) = open_memory_stores(&memories_path);
    let vector_index = open_vector_index(&lattice_dir);
    let event_store = Arc::new(EventStore::open(&lattice_dir.join("events.db"))?);
    let event_writer = Arc::new(
        EventWriter::new(
            event_store.clone(),
            workspace_root.to_string_lossy().to_string(),
            4096,
        )
        .with_flush_policy(FlushPolicy::Batched { interval_ms: 250 }),
    );

    let graph_path = lattice_dir.join("graph.db");
    let graph_store = match GraphStore::open(&graph_path) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(
                "Failed to open persistent graph store at {}: {}. Falling back to memory-only graph store.",
                graph_path.display(),
                e
            );
            GraphStore::open_in_memory().expect("Failed to create in-memory graph store")
        }
    };
    let graph = match should_warm_load_graph(&graph_store, &workspace_root) {
        false => CodeGraph::new(),
        true => match graph_store.load_graph() {
            Ok(loaded) => {
                let stats = loaded.stats();
                if stats.node_count > 0 {
                    tracing::info!(
                        "Warm-loaded persisted graph: {} nodes, {} edges, {} files",
                        stats.node_count,
                        stats.edge_count,
                        stats.file_count
                    );
                }
                loaded
            }
            Err(e) => {
                tracing::warn!("Failed to load persisted graph: {}", e);
                CodeGraph::new()
            }
        },
    };
    let graph = Arc::new(graph);
    let compaction_graph = Arc::new(std::sync::Mutex::new(Arc::clone(&graph)));
    let engine = QueryEngine::new_shared(
        Arc::clone(&graph),
        vector_index.clone(),
        Some(Arc::new(std::sync::Mutex::new(ms_for_engine))),
    );
    let engine = Arc::new(Mutex::new(engine));
    let indexer = Arc::new(Mutex::new(Indexer::new(workspace_root.clone())));
    let graph_store = Arc::new(Mutex::new(graph_store));
    let compaction_scheduler = start_event_compaction_scheduler(
        &lattice_dir,
        memory_mode,
        &memories_path,
        Arc::clone(&event_store),
        Arc::clone(&event_writer),
        Arc::clone(&compaction_graph),
    );

    // Multi-repo workspace manager (only used when multiple workspaces)
    let workspace_manager: Option<Arc<Mutex<WorkspaceManager>>> = None;

    // OnceLock for EmbeddingEngine — populated in background once model loads
    let embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>> = Arc::new(OnceLock::new());

    // Shared indexing state flag
    let indexing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let watcher_health = Arc::new(WatcherHealth::default());
    let repo_state = Arc::new(Mutex::new(crate::repo_state::RepoStateTracker::new(
        &workspace_root,
    )));

    // ── Spawn background indexing task ────────────────────────────────
    {
        let engine_bg = Arc::clone(&engine);
        let indexer_bg = Arc::clone(&indexer);
        let graph_store_bg = Arc::clone(&graph_store);
        let compaction_graph_bg = Arc::clone(&compaction_graph);
        let embedding_engine_bg = Arc::clone(&embedding_engine);
        let indexing_bg = Arc::clone(&indexing);
        let ws_roots_bg = workspace_roots.clone();
        let ws_root = workspace_root.clone();
        let lattice_dir_bg = ws_root.join(".lattice");
        let vector_index_bg = vector_index.clone();

        tokio::spawn(async move {
            tracing::info!("Background indexing starting...");
            let _ = std::fs::create_dir_all(&lattice_dir_bg);

            let (manifest, parsed_cache) = load_incremental_cache(&graph_store_bg).await;
            publish_cached_parsed_graph_snapshot(
                &parsed_cache,
                Some(&indexer_bg),
                &engine_bg,
                &compaction_graph_bg,
            )
            .await;
            let roots = if is_multi_repo {
                ws_roots_bg.clone()
            } else {
                vec![ws_root.clone()]
            };
            let incremental = tokio::task::spawn_blocking(move || {
                build_incremental_index_for_roots(&roots, Some(&manifest), parsed_cache)
            })
            .await
            .unwrap_or_else(|err| {
                tracing::warn!("Incremental indexing task failed: {}", err);
                IncrementalIndexResult::empty()
            });

            persist_incremental_cache(
                &graph_store_bg,
                &incremental.graph,
                &incremental.file_index,
                &incremental.parsed_files,
            )
            .await;
            {
                let mut idx = indexer_bg.lock().await;
                idx.replace_shared_index(
                    Arc::clone(&incremental.graph),
                    incremental.parsed_files.clone(),
                );
            }
            tracing::info!(
                "Incremental indexing: {} current files, {} parsed/updated, {} removed",
                incremental.file_index.len(),
                incremental.changed_count,
                incremental.removed_count
            );
            let files_indexed = incremental.changed_count;
            tracing::info!("Indexed {} files total", files_indexed);

            // Final save to graph store
            {
                let new_graph = {
                    let idx = indexer_bg.lock().await;
                    idx.graph_arc()
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
                if let Ok(mut graph) = compaction_graph_bg.lock() {
                    *graph = Arc::clone(&new_graph);
                } else {
                    tracing::warn!("Failed to refresh compaction graph snapshot handle");
                }

                let mut eng = engine_bg.lock().await;
                eng.update_graph_arc(new_graph);
            }

            // Try to load ONNX embedding model
            let model_path = lattice_dir_bg.join("models").join("model.onnx");
            if model_path.exists() {
                match EmbeddingEngine::new(model_path.to_string_lossy().as_ref()) {
                    Ok(emb_engine) => {
                        tracing::info!("ONNX embedding model loaded");
                        let emb = Arc::new(emb_engine);
                        let _ = embedding_engine_bg.set(Arc::clone(&emb));

                        if background_vector_sync_enabled() {
                            let graph_snapshot = {
                                let eng = engine_bg.lock().await;
                                eng.graph().clone()
                            };
                            if let Some(index) = vector_index_bg.as_ref() {
                                match crate::vector_sync::sync_full_graph_embeddings(
                                    &graph_snapshot,
                                    emb.as_ref(),
                                    index.as_ref(),
                                ) {
                                    Ok(stats) => {
                                        tracing::info!(
                                            mode = stats.mode,
                                            implementation = stats.implementation,
                                            graph_nodes = stats.graph_nodes,
                                            nodes_considered = stats.nodes_considered,
                                            embedded_nodes = stats.embedded_nodes,
                                            failed_nodes = stats.failed_nodes,
                                            payload_chars_total = stats.payload_chars_total,
                                            payload_chars_avg = stats.payload_chars_avg,
                                            payload_chars_max = stats.payload_chars_max,
                                            elapsed_ms = stats.elapsed_ms as u64,
                                            throughput_nodes_per_sec =
                                                stats.throughput_nodes_per_sec(),
                                            "Background semantic sync complete"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to sync semantic index: {}", e);
                                    }
                                }
                            } else {
                                tracing::info!(
                                    "No vector index configured, semantic search disabled"
                                );
                            }
                        } else {
                            tracing::info!(
                                "Background semantic sync disabled; set LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC=1 to enable"
                            );
                        }
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
        let embedding_engine = Arc::clone(&embedding_engine);
        let vector_index = vector_index.clone();
        let repo_state = Arc::clone(&repo_state);
        let indexing_state = Arc::clone(&indexing);
        let watcher_health = Arc::clone(&watcher_health);

        tokio::spawn(async move {
            for root in workspace_roots {
                let watcher = crate::watcher::FileWatcher::new(
                    root.clone(),
                    is_multi_repo.then(|| repo_name_for_root(&root)),
                    Some(Arc::clone(&indexer)),
                    workspace_manager.clone(),
                    Arc::clone(&graph_store),
                    Arc::clone(&engine),
                    Arc::clone(&embedding_engine),
                    vector_index.clone(),
                    Arc::clone(&repo_state),
                    Arc::clone(&indexing_state),
                    Arc::clone(&watcher_health),
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
    let handler = Arc::new(McpHandler::new_with_shared_repo_state(
        engine,
        indexer,
        memory_store,
        graph_store,
        embedding_engine,
        vector_index,
        workspace_root,
        context_cache_path,
        session_id,
        workspace_manager,
        workspace_roots,
        indexing,
        Some(event_writer),
        default_focus.files,
        default_focus.dirs,
        repo_state,
        watcher_health,
    ));
    tracing::info!("Starting stdio server");
    let server = StdioServer::new(handler);
    server.run().await?;
    if let Some(scheduler) = compaction_scheduler {
        scheduler.shutdown().await;
    }
    tracing::info!("Stdio server exited");

    Ok(())
}

pub(crate) struct WorkspaceRuntime {
    pub(crate) handler: Arc<McpHandler>,
    background_tasks: Vec<JoinHandle<()>>,
    compaction_scheduler: Option<SchedulerHandle>,
}

impl WorkspaceRuntime {
    pub(crate) async fn shutdown(mut self) {
        self.handler.auto_flush_session_state().await;
        for task in self.background_tasks.drain(..) {
            task.abort();
        }
        if let Some(scheduler) = self.compaction_scheduler.take() {
            scheduler.shutdown().await;
        }
    }
}

pub(crate) async fn build_workspace_runtime(
    workspace_roots: Vec<PathBuf>,
    default_focus_files: Vec<String>,
    default_focus_dirs: Vec<String>,
) -> Result<WorkspaceRuntime> {
    let workspace_root = workspace_roots[0].clone();
    let is_multi_repo = workspace_roots.len() > 1;
    if is_multi_repo {
        tracing::info!(
            "Registering multi-repo workspace: {} roots",
            workspace_roots.len()
        );
        for r in &workspace_roots {
            tracing::info!("  - {}", r.display());
        }
    }

    let lattice_dir = workspace_root.join(".lattice");
    let _ = std::fs::create_dir_all(&lattice_dir);

    let memories_path = lattice_dir.join("memories.db");
    let (memory_store, ms_for_engine, memory_mode) = open_memory_stores(&memories_path);
    let vector_index = open_vector_index(&lattice_dir);
    let event_store = Arc::new(EventStore::open(&lattice_dir.join("events.db"))?);
    let event_writer = Arc::new(
        EventWriter::new(
            event_store.clone(),
            workspace_root.to_string_lossy().to_string(),
            4096,
        )
        .with_flush_policy(FlushPolicy::Batched { interval_ms: 250 }),
    );

    let graph_path = lattice_dir.join("graph.db");
    let graph_store = match GraphStore::open(&graph_path) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(
                "Failed to open persistent graph store at {}: {}. Falling back to memory-only graph store.",
                graph_path.display(),
                e
            );
            GraphStore::open_in_memory().expect("Failed to create in-memory graph store")
        }
    };
    let graph = match should_warm_load_graph(&graph_store, &workspace_root) {
        false => CodeGraph::new(),
        true => match graph_store.load_graph() {
            Ok(loaded) => {
                let stats = loaded.stats();
                if stats.node_count > 0 {
                    tracing::info!(
                        "Warm-loaded persisted graph for {}: {} nodes, {} edges, {} files",
                        workspace_root.display(),
                        stats.node_count,
                        stats.edge_count,
                        stats.file_count
                    );
                }
                loaded
            }
            Err(e) => {
                tracing::warn!("Failed to load persisted graph: {}", e);
                CodeGraph::new()
            }
        },
    };
    let graph = Arc::new(graph);
    let compaction_graph = Arc::new(std::sync::Mutex::new(Arc::clone(&graph)));
    let engine = QueryEngine::new_shared(
        Arc::clone(&graph),
        vector_index.clone(),
        Some(Arc::new(std::sync::Mutex::new(ms_for_engine))),
    );
    let engine = Arc::new(Mutex::new(engine));
    let indexer = Arc::new(Mutex::new(Indexer::new(workspace_root.clone())));
    let graph_store = Arc::new(Mutex::new(graph_store));
    let compaction_scheduler = start_event_compaction_scheduler(
        &lattice_dir,
        memory_mode,
        &memories_path,
        Arc::clone(&event_store),
        Arc::clone(&event_writer),
        Arc::clone(&compaction_graph),
    );
    let mut background_tasks: Vec<JoinHandle<()>> = Vec::new();

    let workspace_manager: Option<Arc<Mutex<WorkspaceManager>>> = None;
    let embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>> = Arc::new(OnceLock::new());
    let indexing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let watcher_health = Arc::new(WatcherHealth::default());
    let repo_state = Arc::new(Mutex::new(crate::repo_state::RepoStateTracker::new(
        &workspace_root,
    )));

    {
        let engine_bg = Arc::clone(&engine);
        let indexer_bg = Arc::clone(&indexer);
        let graph_store_bg = Arc::clone(&graph_store);
        let compaction_graph_bg = Arc::clone(&compaction_graph);
        let embedding_engine_bg = Arc::clone(&embedding_engine);
        let indexing_bg = Arc::clone(&indexing);
        let ws_roots_bg = workspace_roots.clone();
        let ws_root = workspace_root.clone();
        let lattice_dir_bg = ws_root.join(".lattice");
        let vector_index_bg = vector_index.clone();

        let task = tokio::spawn(async move {
            tracing::info!("Background indexing starting for {}...", ws_root.display());
            let _ = std::fs::create_dir_all(&lattice_dir_bg);

            let (manifest, parsed_cache) = load_incremental_cache(&graph_store_bg).await;
            publish_cached_parsed_graph_snapshot(
                &parsed_cache,
                Some(&indexer_bg),
                &engine_bg,
                &compaction_graph_bg,
            )
            .await;
            let roots = if is_multi_repo {
                ws_roots_bg.clone()
            } else {
                vec![ws_root.clone()]
            };
            let incremental = tokio::task::spawn_blocking(move || {
                build_incremental_index_for_roots(&roots, Some(&manifest), parsed_cache)
            })
            .await
            .unwrap_or_else(|err| {
                tracing::warn!("Incremental indexing task failed: {}", err);
                IncrementalIndexResult::empty()
            });

            persist_incremental_cache(
                &graph_store_bg,
                &incremental.graph,
                &incremental.file_index,
                &incremental.parsed_files,
            )
            .await;
            {
                let mut idx = indexer_bg.lock().await;
                idx.replace_shared_index(
                    Arc::clone(&incremental.graph),
                    incremental.parsed_files.clone(),
                );
            }
            let files_indexed = incremental.changed_count;
            tracing::info!("Indexed {} files total", files_indexed);

            {
                let new_graph = {
                    let idx = indexer_bg.lock().await;
                    idx.graph_arc()
                };
                let stats = new_graph.stats();
                tracing::info!(
                    "Graph ready for {}: {} nodes, {} edges, {} files",
                    ws_root.display(),
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
                if let Ok(mut graph) = compaction_graph_bg.lock() {
                    *graph = Arc::clone(&new_graph);
                }
                let mut eng = engine_bg.lock().await;
                eng.update_graph_arc(new_graph);
            }

            let model_path = lattice_dir_bg.join("models").join("model.onnx");
            if model_path.exists() {
                match EmbeddingEngine::new(model_path.to_string_lossy().as_ref()) {
                    Ok(emb_engine) => {
                        tracing::info!("ONNX embedding model loaded");
                        let emb = Arc::new(emb_engine);
                        let _ = embedding_engine_bg.set(Arc::clone(&emb));
                        if background_vector_sync_enabled() {
                            let graph_snapshot = {
                                let eng = engine_bg.lock().await;
                                eng.graph().clone()
                            };
                            if let Some(index) = vector_index_bg.as_ref() {
                                if let Err(e) = crate::vector_sync::sync_full_graph_embeddings(
                                    &graph_snapshot,
                                    emb.as_ref(),
                                    index.as_ref(),
                                ) {
                                    tracing::warn!("Failed to sync semantic index: {}", e);
                                }
                            } else {
                                tracing::info!(
                                    "No vector index configured, semantic search disabled"
                                );
                            }
                        } else {
                            tracing::info!(
                                "Background semantic sync disabled for {}; set LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC=1 to enable",
                                ws_root.display()
                            );
                        }
                    }
                    Err(e) => tracing::info!("No ONNX model: {}", e),
                }
            }

            indexing_bg.store(false, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("Background indexing complete for {}", ws_root.display());
        });
        background_tasks.push(task);
    }

    {
        let engine = Arc::clone(&engine);
        let indexer = Arc::clone(&indexer);
        let graph_store = Arc::clone(&graph_store);
        let workspace_manager = workspace_manager.clone();
        let workspace_roots = workspace_roots.clone();
        let embedding_engine = Arc::clone(&embedding_engine);
        let vector_index = vector_index.clone();
        let watcher_health = Arc::clone(&watcher_health);

        for root in workspace_roots {
            let watcher = crate::watcher::FileWatcher::new(
                root.clone(),
                is_multi_repo.then(|| repo_name_for_root(&root)),
                Some(Arc::clone(&indexer)),
                workspace_manager.clone(),
                Arc::clone(&graph_store),
                Arc::clone(&engine),
                Arc::clone(&embedding_engine),
                vector_index.clone(),
                Arc::clone(&repo_state),
                Arc::clone(&indexing),
                Arc::clone(&watcher_health),
            );
            let task = tokio::spawn(async move {
                if let Err(e) = watcher.run().await {
                    tracing::error!("File watcher failed for {:?}: {}", root, e);
                }
            });
            background_tasks.push(task);
        }
    }

    {
        let memory_store_decay = Arc::clone(&memory_store);
        let task = tokio::spawn(async move {
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
        background_tasks.push(task);
    }

    let session_id = generate_session_id();
    let context_cache_path = lattice_dir.join("context_handles.json");
    tracing::info!(
        "Creating MCP handler for {} (session: {})",
        workspace_root.display(),
        session_id
    );
    let handler = Arc::new(McpHandler::new_with_shared_repo_state(
        engine,
        indexer,
        memory_store,
        graph_store,
        embedding_engine,
        vector_index,
        workspace_root,
        context_cache_path,
        session_id,
        workspace_manager,
        workspace_roots,
        indexing,
        Some(event_writer),
        default_focus_files,
        default_focus_dirs,
        repo_state,
        watcher_health,
    ));
    {
        let handler = Arc::clone(&handler);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let _ = handler.auto_checkpoint_active_states("interval").await;
            }
        });
        background_tasks.push(task);
    }
    Ok(WorkspaceRuntime {
        handler,
        background_tasks,
        compaction_scheduler,
    })
}

fn start_event_compaction_scheduler(
    lattice_dir: &Path,
    memory_mode: MemoryStoreMode,
    memories_path: &Path,
    event_store: Arc<EventStore>,
    event_writer: Arc<EventWriter>,
    graph: Arc<std::sync::Mutex<Arc<CodeGraph>>>,
) -> Option<SchedulerHandle> {
    if memory_mode == MemoryStoreMode::InMemoryFallback {
        tracing::warn!(
            "Event compaction scheduler disabled because memory storage is in-memory fallback"
        );
        return None;
    }
    let memory = match MemoryStore::open(memories_path) {
        Ok(store) => Arc::new(std::sync::Mutex::new(store)),
        Err(err) => {
            tracing::error!(
                "Failed to open compaction memory store at {}: {}",
                memories_path.display(),
                err
            );
            return None;
        }
    };
    let compactor = Compactor::new(
        event_store,
        event_writer,
        graph,
        memory,
        event_compaction_config(lattice_dir),
    );
    Some(compactor.spawn_scheduler())
}

fn event_compaction_config(lattice_dir: &Path) -> CompactionConfig {
    let mut config = CompactionConfig::new(lattice_dir.join("snapshots"));
    config.interval = env_duration_secs("LATTICE_EVENT_COMPACTION_INTERVAL_SECS", config.interval);
    config.min_events_since_last = env_u64(
        "LATTICE_EVENT_COMPACTION_MIN_EVENTS",
        config.min_events_since_last,
    );
    config.retain_snapshots = env_usize(
        "LATTICE_EVENT_COMPACTION_RETAIN_SNAPSHOTS",
        config.retain_snapshots,
    );
    config
}

fn env_duration_secs(name: &str, fallback: std::time::Duration) -> std::time::Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(fallback)
}

fn env_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn env_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

pub(crate) fn background_vector_sync_enabled() -> bool {
    matches!(
        std::env::var("LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

const WARM_GRAPH_FILE_LIMIT_ENV: &str = "LATTICE_MAX_WARM_GRAPH_FILES";
const WARM_GRAPH_BYTE_LIMIT_ENV: &str = "LATTICE_MAX_WARM_GRAPH_BYTES";
const DEFAULT_MAX_WARM_GRAPH_BYTES: u64 = 1024 * 1024 * 1024;

fn should_warm_load_graph(graph_store: &GraphStore, workspace_root: &Path) -> bool {
    let max_files = max_warm_graph_files();
    let max_bytes = max_warm_graph_bytes();
    match graph_store.persisted_graph_file_count() {
        Ok(file_count) if file_count > max_files => {
            tracing::warn!(
                workspace = %workspace_root.display(),
                persisted_files = file_count,
                max_files,
                env_var = WARM_GRAPH_FILE_LIMIT_ENV,
                "Skipping persisted graph warm-load because it exceeds the safety limit"
            );
            false
        }
        Ok(_) => match graph_store.persisted_graph_disk_bytes() {
            Ok(Some(byte_count)) if byte_count > max_bytes => {
                tracing::warn!(
                    workspace = %workspace_root.display(),
                    persisted_bytes = byte_count,
                    max_bytes,
                    env_var = WARM_GRAPH_BYTE_LIMIT_ENV,
                    "Skipping persisted graph warm-load because on-disk graph size exceeds the safety limit"
                );
                false
            }
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    workspace = %workspace_root.display(),
                    %error,
                    "Skipping persisted graph warm-load because persisted graph disk size could not be validated"
                );
                false
            }
        },
        Err(error) => {
            tracing::warn!(
                workspace = %workspace_root.display(),
                %error,
                "Skipping persisted graph warm-load because persisted graph size could not be validated"
            );
            false
        }
    }
}

fn max_warm_graph_files() -> usize {
    std::env::var(WARM_GRAPH_FILE_LIMIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(50_000)
}

fn max_warm_graph_bytes() -> u64 {
    std::env::var(WARM_GRAPH_BYTE_LIMIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_WARM_GRAPH_BYTES)
}

fn open_vector_index(lattice_dir: &Path) -> Option<SharedVectorIndex> {
    let sqlite_path = lattice_dir.join("vectors.db");
    let ann_path = lattice_dir.join("vectors.usearch");

    match UsearchVectorIndex::open(sqlite_path.to_string_lossy().as_ref(), ann_path.clone()) {
        Ok(index) => match index.initialize(384).and_then(|_| index.warm()) {
            Ok(()) => {
                tracing::info!(
                    "Semantic search backend ready: {} ({})",
                    index.implementation_name(),
                    ann_path.display()
                );
                Some(Arc::new(index) as SharedVectorIndex)
            }
            Err(err) => {
                tracing::warn!(
                    "Failed to initialize USearch backend at {}: {}. Falling back to SQLite exact search.",
                    ann_path.display(),
                    err
                );
                open_sqlite_vector_fallback(&sqlite_path)
            }
        },
        Err(err) => {
            tracing::warn!(
                "Failed to open USearch backend at {}: {}. Falling back to SQLite exact search.",
                ann_path.display(),
                err
            );
            open_sqlite_vector_fallback(&sqlite_path)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryStoreMode {
    Persistent,
    RecoveredPersistent,
    InMemoryFallback,
}

fn open_memory_stores(path: &Path) -> (Arc<Mutex<MemoryStore>>, MemoryStore, MemoryStoreMode) {
    match try_open_persistent_memory_stores(path) {
        Ok((primary, engine)) => {
            tracing::info!("Persistent memory store ready: {}", path.display());
            (
                Arc::new(Mutex::new(primary)),
                engine,
                MemoryStoreMode::Persistent,
            )
        }
        Err(initial_err) => {
            tracing::warn!(
                "Failed to open persistent memory store at {} ({}). Attempting recovery by quarantining the existing workspace memory artifacts and rebuilding a fresh database.",
                path.display(),
                initial_err
            );

            match recover_persistent_memory_store(path) {
                Ok((primary, engine, quarantine_dir)) => {
                    tracing::warn!(
                        "Recovered persistent memory store at {} by quarantining the previous artifacts under {}. The old memory database is preserved there for manual inspection, and new durable memory writes will use a fresh store.",
                        path.display(),
                        quarantine_dir.display()
                    );
                    (
                        Arc::new(Mutex::new(primary)),
                        engine,
                        MemoryStoreMode::RecoveredPersistent,
                    )
                }
                Err(recovery_err) => fallback_to_in_memory_memory_stores(
                    path,
                    format!("initial open failed: {initial_err}; recovery failed: {recovery_err}"),
                ),
            }
        }
    }
}

fn try_open_persistent_memory_stores(path: &Path) -> Result<(MemoryStore, MemoryStore), String> {
    let primary = MemoryStore::open(path)
        .map_err(|err| format!("primary store initialization failed: {err}"))?;
    let engine = MemoryStore::open(path)
        .map_err(|err| format!("secondary store initialization failed: {err}"))?;
    Ok((primary, engine))
}

fn recover_persistent_memory_store(
    path: &Path,
) -> Result<(MemoryStore, MemoryStore, PathBuf), String> {
    let quarantine_dir = quarantine_memory_store_artifacts(path)?;
    let (primary, engine) = try_open_persistent_memory_stores(path).map_err(|err| {
        format!(
            "quarantined previous artifacts to {} but reopening still failed: {}",
            quarantine_dir.display(),
            err
        )
    })?;
    Ok((primary, engine, quarantine_dir))
}

fn quarantine_memory_store_artifacts(path: &Path) -> Result<PathBuf, String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "memory database path {} has no parent directory",
            path.display()
        )
    })?;
    let recovery_root = parent.join("recovered-memory");
    std::fs::create_dir_all(&recovery_root).map_err(|err| {
        format!(
            "failed to create recovery directory {}: {}",
            recovery_root.display(),
            err
        )
    })?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let quarantine_dir = recovery_root.join(format!("memories-db-{timestamp}"));
    std::fs::create_dir_all(&quarantine_dir).map_err(|err| {
        format!(
            "failed to create quarantine directory {}: {}",
            quarantine_dir.display(),
            err
        )
    })?;

    let mut moved_any = false;
    for artifact in memory_store_artifact_paths(path) {
        if !artifact.exists() {
            continue;
        }

        moved_any = true;
        let file_name = artifact.file_name().ok_or_else(|| {
            format!(
                "memory store artifact path {} has no terminal file name",
                artifact.display()
            )
        })?;
        let target = quarantine_dir.join(file_name);
        std::fs::rename(&artifact, &target).map_err(|err| {
            format!(
                "failed to move {} to {}: {}",
                artifact.display(),
                target.display(),
                err
            )
        })?;
    }

    if !moved_any {
        return Err(format!(
            "no memory store artifacts were present at {} to recover",
            path.display()
        ));
    }

    Ok(quarantine_dir)
}

fn memory_store_artifact_paths(path: &Path) -> [PathBuf; 3] {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
}

fn fallback_to_in_memory_memory_stores(
    path: &Path,
    reason: String,
) -> (Arc<Mutex<MemoryStore>>, MemoryStore, MemoryStoreMode) {
    tracing::warn!(
        "Failed to open persistent memory store at {} ({}). Falling back to in-memory memory for this session; durable memory writes are disabled until the workspace database is repaired.",
        path.display(),
        reason
    );

    let primary = MemoryStore::open_in_memory().expect("Failed to create in-memory memory store");
    let engine =
        MemoryStore::open_in_memory().expect("Failed to create in-memory engine memory store");
    (
        Arc::new(Mutex::new(primary)),
        engine,
        MemoryStoreMode::InMemoryFallback,
    )
}

fn open_sqlite_vector_fallback(path: &Path) -> Option<SharedVectorIndex> {
    match VectorStore::open(path.to_string_lossy().as_ref()) {
        Ok(store) => match store.initialize(384).and_then(|_| store.warm()) {
            Ok(()) => {
                tracing::info!(
                    "Semantic search backend ready: sqlite-exact ({})",
                    path.display()
                );
                Some(Arc::new(store) as SharedVectorIndex)
            }
            Err(err) => {
                tracing::warn!(
                    "Failed to initialize SQLite vector fallback at {}: {}. Semantic search disabled.",
                    path.display(),
                    err
                );
                None
            }
        },
        Err(err) => {
            tracing::warn!(
                "Failed to open SQLite vector fallback at {}: {}. Semantic search disabled.",
                path.display(),
                err
            );
            None
        }
    }
}

/// Parse workspace roots from command-line args.
/// Supports multiple `--workspace <path>` flags. Defaults to current directory.
/// Applies anti-double-indexing: if root A is a parent of root B, B is dropped.
fn parse_workspace_roots() -> Result<Vec<PathBuf>> {
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

    // Anti-double-indexing: remove any root that is a subdirectory of another.
    let roots = deduplicate_roots(roots);
    validate_workspace_roots(&roots)?;
    let current_dir = std::env::current_dir().ok();
    Ok(prefer_current_dir_root(roots, current_dir.as_deref()))
}

fn validate_workspace_roots(roots: &[PathBuf]) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok());
    for root in roots {
        validate_workspace_root(root, home.as_deref())?;
    }
    Ok(())
}

fn validate_workspace_root(root: &Path, home: Option<&Path>) -> Result<()> {
    if is_filesystem_root(root) {
        anyhow::bail!(
            "refusing workspace root `{}`: filesystem root is not a valid Lattice workspace",
            root.display()
        );
    }
    if let Some(home) = home {
        if root == home {
            anyhow::bail!(
                "refusing workspace root `{}`: the user's home directory is not a valid Lattice workspace; choose a project directory instead",
                root.display()
            );
        }
    }
    Ok(())
}

fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none()
}

fn prefer_current_dir_root(mut roots: Vec<PathBuf>, current_dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(current_dir) = current_dir else {
        return roots;
    };
    let current_dir = current_dir
        .canonicalize()
        .unwrap_or_else(|_| current_dir.to_path_buf());
    if let Some(position) = roots.iter().position(|root| current_dir.starts_with(root)) {
        if position != 0 {
            let active_root = roots.remove(position);
            roots.insert(0, active_root);
        }
    }
    roots
}

#[derive(Debug, Default, Clone)]
struct SessionFocus {
    files: Vec<String>,
    dirs: Vec<String>,
}

fn parse_focus_args(workspace_root: &Path) -> SessionFocus {
    let args: Vec<String> = std::env::args().collect();
    let mut focus = SessionFocus::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--focus-file" => {
                if let Some(value) = args.get(i + 1) {
                    if let Some(normalized) = normalize_focus_path(workspace_root, value) {
                        push_unique(&mut focus.files, normalized);
                    }
                    i += 2;
                    continue;
                }
            }
            "--focus-dir" => {
                if let Some(value) = args.get(i + 1) {
                    if let Some(normalized) = normalize_focus_path(workspace_root, value) {
                        push_unique(
                            &mut focus.dirs,
                            normalized.trim_end_matches('/').to_string(),
                        );
                    }
                    i += 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    focus
}

fn normalize_focus_path(workspace_root: &Path, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(trimmed);
    if candidate.is_absolute() {
        let canonical = candidate.canonicalize().unwrap_or(candidate);
        if let Ok(relative) = canonical.strip_prefix(workspace_root) {
            let text = relative.to_string_lossy().replace('\\', "/");
            if !text.is_empty() {
                return Some(text);
            }
        }
        return Some(canonical.to_string_lossy().replace('\\', "/"));
    }
    Some(trimmed.trim_start_matches("./").replace('\\', "/"))
}

fn push_unique(items: &mut Vec<String>, value: String) {
    if !items.iter().any(|existing| existing == &value) {
        items.push(value);
    }
}

fn has_arg(name: &str) -> bool {
    std::env::args().any(|arg| arg == name)
}

fn is_memory_migrate_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("memory-migrate")
}

fn is_doctor_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("doctor")
}

fn run_memory_migrate_cli() -> Result<()> {
    let options = parse_memory_migrate_options()?;
    let source = rusqlite::Connection::open(&options.source_path)?;
    let dest = rusqlite::Connection::open(&options.dest_path)?;
    let migrator = MemoryMigrator::new(
        Arc::new(std::sync::Mutex::new(source)),
        Arc::new(std::sync::Mutex::new(dest)),
        options.batch_size,
        options.dry_run,
    );
    let plan = migrator.plan()?;
    print_migration_plan(&plan);
    let report = migrator.run(&plan)?;
    print_migration_report(&report);
    Ok(())
}

struct MemoryMigrateOptions {
    source_path: PathBuf,
    dest_path: PathBuf,
    batch_size: usize,
    dry_run: bool,
}

fn parse_memory_migrate_options() -> Result<MemoryMigrateOptions> {
    let args: Vec<String> = std::env::args().collect();
    let mut source_path = None;
    let mut dest_path = None;
    let mut batch_size = 500_usize;
    let mut dry_run = false;
    let mut apply = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => dry_run = true,
            "--apply" => apply = true,
            "--source" => {
                source_path = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            "--dest" => {
                dest_path = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            "--batch-size" => {
                batch_size = parse_batch_size(args.get(i + 1))?;
                i += 1;
            }
            other => anyhow::bail!("unknown memory-migrate argument `{other}`"),
        }
        i += 1;
    }
    if dry_run == apply {
        anyhow::bail!("memory-migrate requires exactly one of --dry-run or --apply");
    }
    let workspace = parse_workspace_roots()?.remove(0);
    let lattice_dir = workspace.join(".lattice");
    Ok(MemoryMigrateOptions {
        source_path: source_path.unwrap_or_else(|| lattice_dir.join("memories.db")),
        dest_path: dest_path.unwrap_or_else(|| lattice_dir.join("memory_graph.db")),
        batch_size,
        dry_run,
    })
}

fn parse_batch_size(value: Option<&String>) -> Result<usize> {
    let Some(value) = value else {
        anyhow::bail!("--batch-size requires a positive integer");
    };
    let parsed = value.parse::<usize>()?;
    if parsed == 0 {
        anyhow::bail!("--batch-size must be positive");
    }
    Ok(parsed)
}

fn print_migration_plan(plan: &lattice_core::memory_graph::MigrationPlan) {
    println!("Memory migration plan");
    println!("source_rows={}", plan.source_rows);
    println!("already_migrated_rows={}", plan.already_migrated_rows);
    println!("planned_memories={}", plan.destination_inserts.memories);
    println!("planned_links={}", plan.destination_inserts.memory_links);
    println!(
        "planned_evidence={}",
        plan.destination_inserts.memory_evidence
    );
    println!(
        "planned_accesses={}",
        plan.destination_inserts.memory_accesses
    );
    println!("planned_scores={}", plan.destination_inserts.memory_scores);
    println!("planned_skipped_rows={}", plan.skipped_rows.len());
}

fn print_migration_report(report: &lattice_core::memory_graph::MigrationReport) {
    println!("Memory migration report");
    println!("dry_run={}", report.dry_run);
    println!("migrated_rows={}", report.migrated_rows);
    println!("already_migrated_rows={}", report.already_migrated_rows);
    println!("inserted_memories={}", report.destination_inserts.memories);
    println!("inserted_links={}", report.destination_inserts.memory_links);
    println!(
        "inserted_evidence={}",
        report.destination_inserts.memory_evidence
    );
    println!(
        "inserted_accesses={}",
        report.destination_inserts.memory_accesses
    );
    println!(
        "inserted_scores={}",
        report.destination_inserts.memory_scores
    );
    println!("skipped_rows={}", report.skipped_rows.len());
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

#[derive(Debug, Clone)]
struct SourceFileRecord {
    indexed_path: String,
    rel_path: String,
    root: PathBuf,
    content_hash: Option<String>,
    mtime_ns: i64,
    size_bytes: i64,
}

#[derive(Clone)]
pub(crate) struct IncrementalIndexResult {
    graph: Arc<CodeGraph>,
    parsed_files: HashMap<String, ParsedFile>,
    file_index: Vec<FileIndexEntry>,
    changed_count: usize,
    removed_count: usize,
}

impl IncrementalIndexResult {
    fn empty() -> Self {
        Self {
            graph: Arc::new(CodeGraph::new()),
            parsed_files: HashMap::new(),
            file_index: Vec::new(),
            changed_count: 0,
            removed_count: 0,
        }
    }
}

async fn publish_cached_parsed_graph_snapshot(
    parsed_files: &HashMap<String, ParsedFile>,
    indexer: Option<&Arc<Mutex<Indexer>>>,
    engine: &Arc<Mutex<QueryEngine>>,
    compaction_graph: &Arc<std::sync::Mutex<Arc<CodeGraph>>>,
) -> bool {
    if parsed_files.is_empty() {
        return false;
    }

    {
        let engine = engine.lock().await;
        if engine.graph().stats().node_count > 0 {
            return false;
        }
    }

    let mut cached_indexer = Indexer::new(PathBuf::new());
    cached_indexer.replace_parsed_files(parsed_files.clone());
    let (cached_graph, cached_parsed_files) = cached_indexer.into_parts();
    let cached_graph = Arc::new(cached_graph);
    let stats = cached_graph.stats();
    if stats.node_count == 0 {
        return false;
    }

    if let Some(indexer) = indexer {
        let mut indexer = indexer.lock().await;
        indexer.replace_shared_index(Arc::clone(&cached_graph), cached_parsed_files);
    }

    if let Ok(mut graph) = compaction_graph.lock() {
        *graph = Arc::clone(&cached_graph);
    } else {
        tracing::warn!("Failed to publish cached graph to compaction graph snapshot");
    }

    let mut engine = engine.lock().await;
    if engine.graph().stats().node_count == 0 {
        engine.update_graph_arc(cached_graph);
        tracing::info!(
            "Warm-published cached graph snapshot: {} nodes, {} edges, {} files",
            stats.node_count,
            stats.edge_count,
            stats.file_count
        );
        true
    } else {
        false
    }
}

pub(crate) async fn load_incremental_cache(
    graph_store: &Arc<Mutex<GraphStore>>,
) -> (HashMap<String, FileIndexEntry>, HashMap<String, ParsedFile>) {
    let store = graph_store.lock().await;
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
}

pub(crate) async fn persist_incremental_cache(
    graph_store: &Arc<Mutex<GraphStore>>,
    graph: &CodeGraph,
    file_index: &[FileIndexEntry],
    parsed_files: &HashMap<String, ParsedFile>,
) {
    let store = graph_store.lock().await;
    if let Err(err) = store.save_graph(graph) {
        tracing::warn!("Failed to save graph: {}", err);
    }
    if let Err(err) = store.save_file_index(file_index) {
        tracing::warn!("Failed to save file index manifest: {}", err);
    }
    if let Err(err) = store.save_parsed_files(parsed_files) {
        tracing::warn!("Failed to save cached parsed files: {}", err);
    }
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
    IncrementalIndexResult {
        graph: Arc::new(graph),
        parsed_files,
        file_index,
        changed_count,
        removed_count,
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
        let mut files = collect_indexable_files(root, &security_filter);
        prioritize_indexable_paths(root, &mut files);
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

fn max_cached_parsed_files() -> usize {
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

/// Collect all indexable file paths from a workspace root.
/// Returns paths filtered by SecurityFilter and supported language extensions.
fn collect_indexable_files(root: &PathBuf, security_filter: &SecurityFilter) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(root, root, security_filter, &mut files);
    prioritize_indexable_paths(root, &mut files);
    files
}

pub(crate) fn prioritize_indexable_paths(root: &Path, files: &mut Vec<PathBuf>) {
    files.sort_by(|a, b| {
        let a_rel = a
            .strip_prefix(root)
            .unwrap_or(a.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let b_rel = b
            .strip_prefix(root)
            .unwrap_or(b.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        indexing_priority(&a_rel)
            .cmp(&indexing_priority(&b_rel))
            .then_with(|| a_rel.cmp(&b_rel))
    });
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

#[cfg(test)]
mod tests {
    use super::{
        build_incremental_index_for_roots, memory_store_artifact_paths, open_memory_stores,
        MemoryStoreMode,
    };
    use lattice_core::memory::{Memory, MemoryScope, MemoryType, MemoryVerificationStatus};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("lattice-daemon-{name}-{unique}"))
    }

    fn write_file(path: &std::path::Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("file should have parent"))
            .expect("create parent dir");
        std::fs::write(path, content).expect("write test file");
    }

    fn make_memory(content: &str) -> Memory {
        Memory {
            id: String::new(),
            session_id: "test-session".to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Session,
            confidence: 1.0,
            linked_symbols: Vec::new(),
            linked_files: Vec::new(),
            workspace_id: None,
            branch: None,
            scope_organization_id: None,
            refresh_key: None,
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: MemoryVerificationStatus::Unverified,
        }
    }

    #[test]
    fn prefer_current_dir_root_moves_matching_workspace_first_without_project_hints() {
        let root_a = PathBuf::from("/work/lattice");
        let root_b = PathBuf::from("/work/portal");
        let roots = vec![root_a.clone(), root_b.clone()];

        let reordered = super::prefer_current_dir_root(
            roots,
            Some(PathBuf::from("/work/portal/docs/audit").as_path()),
        );

        assert_eq!(reordered, vec![root_b, root_a]);
    }

    #[test]
    fn validate_workspace_root_rejects_filesystem_root() {
        let err = super::validate_workspace_root(PathBuf::from("/").as_path(), None)
            .expect_err("filesystem root must be rejected");

        assert!(err.to_string().contains("filesystem root"));
    }

    #[test]
    fn validate_workspace_root_rejects_home_directory() {
        let home = PathBuf::from("/home/tester");
        let err = super::validate_workspace_root(home.as_path(), Some(home.as_path()))
            .expect_err("home directory must be rejected");

        assert!(err.to_string().contains("home directory"));
    }

    #[test]
    fn test_incremental_index_reuses_cached_parsed_files_when_hash_unchanged() {
        let root = unique_temp_path("incremental-index");
        write_file(
            &root.join("src/auth.ts"),
            "export function loginUser() { return true; }",
        );

        let first = build_incremental_index_for_roots(&[root.clone()], None, Default::default());
        assert_eq!(first.changed_count, 1);
        assert_eq!(first.graph.stats().file_count, 1);

        let manifest = first
            .file_index
            .iter()
            .cloned()
            .map(|entry| (entry.file.clone(), entry))
            .collect();
        let second =
            build_incremental_index_for_roots(&[root.clone()], Some(&manifest), first.parsed_files);
        assert_eq!(second.changed_count, 0);
        assert_eq!(second.removed_count, 0);
        assert_eq!(second.graph.stats().file_count, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    fn cleanup_memory_store_artifacts(path: &PathBuf) {
        for artifact in memory_store_artifact_paths(path) {
            if artifact.is_dir() {
                let _ = std::fs::remove_dir_all(&artifact);
            } else {
                let _ = std::fs::remove_file(&artifact);
            }
        }
    }

    #[test]
    fn test_open_memory_stores_uses_persistent_store_when_available() {
        let root = unique_temp_path("persistent-root");
        std::fs::create_dir_all(&root).expect("failed to create temp root");
        let db_path = root.join("memories.db");

        let (memory_store, engine_store, mode) = open_memory_stores(&db_path);
        assert_eq!(mode, MemoryStoreMode::Persistent);
        assert!(
            db_path.exists(),
            "persistent store should create the sqlite file"
        );

        let stored_id = memory_store
            .try_lock()
            .expect("memory store lock should be available")
            .store(make_memory("persistent write"))
            .expect("persistent memory store should accept writes");
        drop(memory_store);
        drop(engine_store);

        let reopened = lattice_core::memory::MemoryStore::open(&db_path)
            .expect("reopening persistent memory store should succeed");
        let stored = reopened
            .get_by_id(&stored_id)
            .expect("reopen lookup should succeed");
        assert!(stored.is_some(), "persistent memory should survive reopen");

        cleanup_memory_store_artifacts(&db_path);
        let _ = std::fs::remove_dir(&root);
    }

    #[test]
    fn test_open_memory_stores_falls_back_when_persistent_open_fails() {
        let db_path = unique_temp_path("missing-parent")
            .join("missing")
            .join("memories.db");

        let (memory_store, engine_store, mode) = open_memory_stores(&db_path);

        assert_eq!(mode, MemoryStoreMode::InMemoryFallback);
        assert!(
            !db_path.exists(),
            "fallback path should not create an unusable persistent database"
        );

        memory_store
            .try_lock()
            .expect("memory store lock should be available")
            .store(make_memory("fallback primary write"))
            .expect("primary in-memory fallback should accept writes");
        let stored_id = engine_store
            .store(make_memory("fallback engine write"))
            .expect("engine in-memory fallback should accept writes");
        let stored = engine_store
            .get_by_id(&stored_id)
            .expect("fallback lookup should succeed");
        assert!(stored.is_some(), "fallback memory should be queryable");
    }

    #[test]
    fn test_open_memory_stores_recovers_by_quarantining_broken_artifacts() {
        let root = unique_temp_path("recover-root");
        std::fs::create_dir_all(&root).expect("failed to create temp root");
        let db_path = root.join("memories.db");

        std::fs::create_dir_all(&db_path)
            .expect("failed to create blocking directory at database path");

        let (memory_store, engine_store, mode) = open_memory_stores(&db_path);

        assert_eq!(mode, MemoryStoreMode::RecoveredPersistent);
        assert!(
            db_path.is_file(),
            "recovery should recreate the sqlite database"
        );

        let recovery_root = root.join("recovered-memory");
        let recovery_entries = std::fs::read_dir(&recovery_root)
            .expect("recovery directory should exist after quarantine")
            .collect::<Result<Vec<_>, _>>()
            .expect("recovery directory should be readable");
        assert_eq!(
            recovery_entries.len(),
            1,
            "expected exactly one quarantine directory"
        );
        let quarantined_db = recovery_entries[0].path().join("memories.db");
        assert!(
            quarantined_db.is_dir(),
            "the blocking artifact should be preserved in quarantine"
        );

        let stored_id = memory_store
            .try_lock()
            .expect("memory store lock should be available")
            .store(make_memory("recovered persistent write"))
            .expect("recovered persistent store should accept writes");
        drop(memory_store);
        drop(engine_store);

        let reopened = lattice_core::memory::MemoryStore::open(&db_path)
            .expect("reopening recovered memory store should succeed");
        let stored = reopened
            .get_by_id(&stored_id)
            .expect("reopen lookup should succeed after recovery");
        assert!(
            stored.is_some(),
            "recovered persistent memory should survive reopen"
        );

        cleanup_memory_store_artifacts(&db_path);
        let _ = std::fs::remove_dir_all(&recovery_root);
        let _ = std::fs::remove_dir(&root);
    }
}
