#![recursion_limit = "256"]

mod adoption_metrics;
mod cli;
mod daemon_settings;
mod disk_budget;
mod doctor;
mod git_intelligence_runtime;
mod health_facts_runtime;
mod hook_adapter;
mod hook_enforcement;
mod hook_session_binding;
mod hook_session_client;
mod hook_session_registry;
mod hook_session_route;
mod hook_shell_changes;
mod hook_workflow_state;
mod index_health;
mod index_work;
mod install;
mod install_project;
mod lifecycle_log;
mod memory_attribution;
mod memory_retention_runtime;
mod proxy;
mod repo_state;
mod resource_budget;
mod rpc;
mod runtime_support;
mod session_digest_consolidation_runtime;
mod socket_server;
mod storage_operator;
mod transport;
mod transport_credentials;
mod trusted_check_runner;
mod vector_sync;
mod verification_producer;
mod watcher;
mod watcher_health;
mod workspace_identity;
mod worktree_base;

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

use index_health::IndexHealth;
use index_work::{IndexReadiness, IndexWorkCoordinator};
use lattice_core::embeddings::{
    verified_shared_embedding_model_path, CachedEmbeddingEngine, EmbeddingEngine,
    EmbeddingObjectCache, EmbeddingProvider,
};
use lattice_core::error::LatticeError;
use lattice_core::events::{
    CompactionConfig, Compactor, EventStore, EventWriter, FlushPolicy, SchedulerHandle,
};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::identity_migration::migrate_identities;
use lattice_core::memory::{MemoryStore, MemoryStoreFailureKind, RepositoryMemoryOwner};
use lattice_core::memory_graph::MemoryMigrator;
use lattice_core::query::QueryEngine;
use lattice_core::storage::{
    CachePolicy, CheckoutLease, GraphStore, IndexSnapshot, IndexSnapshotLoad, SharedVectorIndex,
    StorageRegistry, UsearchVectorIndex, VectorIndex, VectorStore,
};
use lattice_core::workspace::WorkspaceManager;
use rpc::mcp::McpHandler;
use rpc::mcp::{GitIntelligenceSnapshotHandle, HealthFactsSnapshotHandle};
#[cfg(test)]
use runtime_support::build_incremental_index_for_roots;
use runtime_support::{
    background_vector_sync_enabled, build_incremental_index_for_roots_with_cache_budgeted_base,
    load_incremental_manifest, max_warm_graph_bytes, max_warm_graph_files,
    persist_incremental_cache, BaseReuseContext, IncrementalIndexResult, ParsedCacheRuntime,
    WARM_GRAPH_BYTE_LIMIT_ENV, WARM_GRAPH_FILE_LIMIT_ENV,
};
use watcher_health::WatcherHealth;

fn cached_embedding_provider(
    engine: Arc<EmbeddingEngine>,
    object_root: &Path,
    checkout_id: &str,
) -> Arc<dyn EmbeddingProvider> {
    match EmbeddingObjectCache::open(object_root) {
        Ok(cache) => Arc::new(CachedEmbeddingEngine::new(
            engine,
            cache,
            checkout_id.to_string(),
        )),
        Err(error) => {
            tracing::warn!(path = %object_root.display(), %error, "Shared embedding cache unavailable; continuing semantic inference without persisted reuse");
            engine
        }
    }
}

fn main() -> Result<()> {
    lattice_core::storage::managed_sqlite::ManagedSqlite::initialize_process()
        .context("Managed SQLite process initialization failed")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    if hook_adapter::is_hook_adapter_command() {
        hook_adapter::run_from_env().await;
        return Ok(());
    }

    if verification_producer::is_verification_producer_command() {
        std::process::exit(verification_producer::run_from_env().await);
    }

    if storage_operator::is_storage_command() {
        std::process::exit(storage_operator::run_from_env()?);
    }

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
        let request = transport::ProxyRequest {
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
                    serde_json::json!(request.workspace_roots.clone()),
                ),
                ("daemon_addr", serde_json::json!(proxy::daemon_addr())),
            ],
        );
        let result = proxy::run_stdio_proxy(request).await;
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

    std::process::exit(cli::run_usage_or_error());
}

pub(crate) struct WorkspaceRuntime {
    pub(crate) handler: Arc<McpHandler>,
    background_tasks: Vec<JoinHandle<()>>,
    completion_tasks: Vec<JoinHandle<()>>,
    compaction_scheduler: Option<SchedulerHandle>,
    session_capture_retention:
        Option<session_digest_consolidation_runtime::SessionCaptureRetentionHandle>,
    session_digest_consolidation:
        Option<session_digest_consolidation_runtime::SessionDigestConsolidationHandle>,
    periodic_maintenance: Option<PeriodicMaintenanceHandle>,
    runtime_work: Arc<crate::index_work::RuntimeWorkTracker>,
    _checkout_lease: CheckoutLease,
    storage_heartbeat: Option<StorageHeartbeat>,
}

impl WorkspaceRuntime {
    pub(crate) fn begin_work(&self) -> crate::index_work::RuntimeWorkGuard {
        self.runtime_work.begin()
    }

    pub(crate) async fn shutdown(mut self) {
        self.handler.auto_flush_session_state().await;
        if let Some(heartbeat) = self.storage_heartbeat.take() {
            heartbeat.shutdown().await;
        }
        for task in &self.background_tasks {
            task.abort();
        }
        for task in self.background_tasks.drain(..) {
            let _ = task.await;
        }
        // Startup publication is finite and may be inside an uninterruptible
        // blocking SQLite/filesystem operation. Join it cooperatively instead
        // of detaching that writer with `abort`.
        for task in self.completion_tasks.drain(..) {
            let _ = task.await;
        }
        if let Some(scheduler) = self.compaction_scheduler.take() {
            scheduler.shutdown().await;
        }
        if let Some(runtime) = self.session_capture_retention.take() {
            runtime.shutdown().await;
        }
        if let Some(runtime) = self.session_digest_consolidation.take() {
            runtime.shutdown().await;
        }
        if let Some(runtime) = self.periodic_maintenance.take() {
            runtime.shutdown().await;
        }
        // `spawn_blocking` work is not cancelled when its async wrapper is
        // aborted. Keep the checkout lease and its GC exclusion alive until
        // every runtime-owned writer has really returned.
        self.runtime_work.wait_idle().await;
    }
}

pub(crate) async fn build_workspace_runtime(
    workspace_roots: Vec<PathBuf>,
    default_focus_files: Vec<String>,
    default_focus_dirs: Vec<String>,
    index_work: Arc<IndexWorkCoordinator>,
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

    let memory_identity = crate::workspace_identity::WorkspaceIdentity::resolve(&workspace_root)?;
    let lattice_dir = memory_identity.checkout_lattice_dir();
    let cache_dir = memory_identity.checkout_cache_dir();
    std::fs::create_dir_all(&memory_identity.repository_lattice_dir)?;
    std::fs::create_dir_all(&lattice_dir)?;
    let mut storage_registry = StorageRegistry::open(
        &memory_identity.repository_lattice_dir,
        &memory_identity.repository_id,
    )?;
    let checkout_lease = storage_registry.register_and_lease(
        &memory_identity.checkout_id,
        &memory_identity.checkout_root,
        now_epoch_secs(),
    )?;
    if let Err(error) = maintain_checkout_caches(&mut storage_registry) {
        tracing::warn!(%error, "Repository maintenance deferred; indexing remains available");
    }
    // Keep watcher attribution scoped to the handler session for this runtime.
    let session_id = generate_session_id();

    let memories_path = memory_identity.memories_path();
    let parsed_cache_runtime = ParsedCacheRuntime::open(&memory_identity.parsed_cache_path())?;
    let migration = memory_identity.is_git_repository.then_some((
        memory_identity.repository_id.as_str(),
        &memory_identity.proven_repository_identities,
    ));
    let (memory_store, memory_mode) = open_memory_store(&memories_path, migration)?;
    if memory_mode == MemoryStoreMode::Persistent {
        memory_retention_runtime::register(&memories_path)?;
    }
    let vector_index = open_vector_index(&cache_dir);
    let event_store = Arc::new(EventStore::open(&lattice_dir.join("events.db"))?);
    let event_writer = Arc::new(
        EventWriter::new(
            event_store.clone(),
            memory_identity.repository_id.clone(),
            4096,
        )
        .with_flush_policy(FlushPolicy::Batched { interval_ms: 250 }),
    );

    let graph_path = cache_dir.join("graph.db");
    let (graph_store, warm_graph) = open_graph_store_with_warm_graph(
        graph_path.clone(),
        memory_identity.repository_lattice_dir.join("symbol-bodies"),
        memory_identity.checkout_id.clone(),
        workspace_root.clone(),
        Arc::clone(&index_work),
    )
    .await?;
    let (graph, engine) = build_warm_query_engine(warm_graph, vector_index.clone());
    let compaction_graph = Arc::new(std::sync::Mutex::new(Arc::clone(&graph)));
    let engine = Arc::new(Mutex::new(engine));
    let indexer = Arc::new(Mutex::new(Indexer::new(workspace_root.clone())));
    let graph_store = Arc::new(Mutex::new(graph_store));
    let mut background_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut completion_tasks: Vec<JoinHandle<()>> = Vec::new();
    let runtime_work = Arc::new(crate::index_work::RuntimeWorkTracker::default());
    let index_health = Arc::new(IndexHealth::default());

    // Validate every Git store before spawning its worker. A failed schema or
    // active-generation audit must fail workspace construction, not surface
    // later as a detached background error.
    let mut git_refresh_handles = HashMap::new();
    let mut health_refresh_handles = HashMap::new();
    let git_intelligence_snapshots = GitIntelligenceSnapshotHandle::default();
    let health_fact_snapshots = HealthFactsSnapshotHandle::default();
    let mut validated_runtimes = Vec::with_capacity(workspace_roots.len());
    for root in &workspace_roots {
        let identity = crate::workspace_identity::WorkspaceIdentity::resolve(root)?;
        let (git_handle, git_runtime) =
            crate::git_intelligence_runtime::GitIntelligenceRuntime::open(
                root.clone(),
                &graph_path,
                &identity
                    .repository_lattice_dir
                    .join("history-object-cache.db"),
                identity.repository_id.clone(),
                Arc::clone(&index_work),
                git_intelligence_snapshots.clone(),
            )?;
        // Same admission control and the same fail-at-construction rule: an
        // unreadable health-fact generation must fail workspace construction
        // rather than surface later as a detached background error.
        let (health_handle, health_runtime) =
            crate::health_facts_runtime::HealthFactsRuntime::open(
                root.clone(),
                &graph_path,
                identity.repository_id,
                Arc::clone(&engine),
                Arc::clone(&index_work),
                Arc::clone(&index_health),
                is_multi_repo.then(|| repo_name_for_root(root)),
                health_fact_snapshots.clone(),
            )?;
        validated_runtimes.push((
            root.clone(),
            git_handle,
            git_runtime,
            health_handle,
            health_runtime,
        ));
    }

    // No worker is spawned until every repository root and every persistence
    // surface has passed validation. A failure on a later root therefore drops
    // only inert runtime values and cannot detach earlier-root writers.
    for (root, git_handle, git_runtime, health_handle, health_runtime) in validated_runtimes {
        git_refresh_handles.insert(root.clone(), git_handle);
        background_tasks.push(
            git_runtime
                .with_runtime_work_tracker(Arc::clone(&runtime_work))
                .spawn(),
        );
        health_refresh_handles.insert(root, health_handle);
        background_tasks.push(
            health_runtime
                .with_runtime_work_tracker(Arc::clone(&runtime_work))
                .spawn(),
        );
    }
    let compaction_scheduler = start_event_compaction_scheduler(
        &lattice_dir,
        Arc::clone(&event_store),
        Arc::clone(&event_writer),
        Arc::clone(&compaction_graph),
    );
    let storage_heartbeat =
        spawn_storage_heartbeat(storage_registry, memory_identity.checkout_id.clone());

    let workspace_manager: Option<Arc<Mutex<WorkspaceManager>>> = None;
    let embedding_engine: Arc<OnceLock<Arc<dyn EmbeddingProvider>>> = Arc::new(OnceLock::new());
    let embedding_object_root = memory_identity
        .repository_lattice_dir
        .join("embedding-objects");
    let embedding_checkout_id = memory_identity.checkout_id.clone();
    let indexing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let index_readiness = Arc::new(IndexReadiness::default());
    let watcher_health = Arc::new(WatcherHealth::default());
    let repo_state = Arc::new(Mutex::new(crate::repo_state::RepoStateTracker::new(
        &workspace_root,
    )));

    // Establish source coverage before startup indexing takes its scan. An
    // edit after the scan but before watcher registration would otherwise be
    // absent from both the startup generation and the native event stream.
    let mut watcher_registrations = Vec::with_capacity(workspace_roots.len());
    {
        let indexer = Arc::clone(&indexer);
        let graph_store = Arc::clone(&graph_store);
        let workspace_manager = workspace_manager.clone();
        let embedding_engine = Arc::clone(&embedding_engine);
        let vector_index = vector_index.clone();
        let watcher_session_id = session_id.clone();

        for root in workspace_roots.iter().cloned() {
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
                Arc::clone(&index_work),
                Arc::clone(&index_readiness),
                Arc::clone(&watcher_health),
                Arc::clone(&index_health),
                git_refresh_handles.get(&root).cloned(),
                health_refresh_handles.get(&root).cloned(),
                watcher_session_id.clone(),
            )
            .with_parsed_cache(parsed_cache_runtime.clone())
            .with_runtime_work_tracker(Arc::clone(&runtime_work));
            let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                if let Err(e) = watcher.run_with_registration(registered_tx).await {
                    tracing::error!("File watcher failed for {:?}: {}", root, e);
                }
            });
            background_tasks.push(task);
            watcher_registrations.push(registered_rx);
        }
    }
    for registration in watcher_registrations {
        registration
            .await
            .context("file watcher exited before establishing native or polling coverage")?;
    }

    {
        let engine_bg = Arc::clone(&engine);
        let indexer_bg = Arc::clone(&indexer);
        let graph_store_bg = Arc::clone(&graph_store);
        let compaction_graph_bg = Arc::clone(&compaction_graph);
        let embedding_engine_bg = Arc::clone(&embedding_engine);
        let embedding_object_root_bg = embedding_object_root.clone();
        let embedding_checkout_id_bg = embedding_checkout_id.clone();
        let indexing_bg = Arc::clone(&indexing);
        let ws_roots_bg = workspace_roots.clone();
        let ws_root = workspace_root.clone();
        let lattice_dir_bg = lattice_dir.clone();
        let vector_index_bg = vector_index.clone();
        let index_work_bg = Arc::clone(&index_work);
        let index_readiness_bg = Arc::clone(&index_readiness);
        let index_health_bg = Arc::clone(&index_health);
        let parsed_cache_bg = parsed_cache_runtime.clone();
        let runtime_work_bg = Arc::clone(&runtime_work);
        let health_refresh_handles_bg = health_refresh_handles.clone();
        let base_reuse = memory_identity
            .git_common_dir
            .as_ref()
            .filter(|_| !is_multi_repo)
            .map(|git_common_dir| BaseReuseContext {
                repository_id: memory_identity.repository_id.clone(),
                checkout_id: memory_identity.checkout_id.clone(),
                git_common_dir: git_common_dir.clone(),
            });

        let task = tokio::spawn(async move {
            tracing::info!("Background indexing starting for {}...", ws_root.display());
            let index_permit = Arc::new(
                index_work_bg
                    .acquire(ws_root.to_string_lossy().to_string(), "startup")
                    .await
                    .expect("index work coordinator remains open for the process lifetime"),
            );
            let resource_budget = index_work_bg.resource_budget();
            let _ = std::fs::create_dir_all(&lattice_dir_bg);

            let manifest = load_incremental_manifest(&graph_store_bg).await;
            let roots = if is_multi_repo {
                ws_roots_bg.clone()
            } else {
                vec![ws_root.clone()]
            };
            let runtime_guard = runtime_work_bg.begin();
            let child_permit = Arc::clone(&index_permit);
            let incremental = match tokio::task::spawn_blocking(move || {
                let (_runtime_guard, _child_permit) = (runtime_guard, child_permit);
                build_incremental_index_for_roots_with_cache_budgeted_base(
                    &roots,
                    Some(&manifest),
                    HashMap::new(),
                    &parsed_cache_bg,
                    Some(&resource_budget),
                    base_reuse.as_ref(),
                )
            })
            .await
            {
                Ok(Ok(incremental)) => incremental,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "Startup indexing was resource limited; keeping exact filesystem service and reporting partial semantic coverage");
                    indexing_bg.store(false, std::sync::atomic::Ordering::Relaxed);
                    index_health_bg.mark_resource_limited(error.to_string());
                    index_readiness_bg.mark_ready();
                    return;
                }
                Err(error) => {
                    tracing::error!(%error, "Incremental indexing worker failed; keeping the previously published graph");
                    indexing_bg.store(false, std::sync::atomic::Ordering::Relaxed);
                    index_readiness_bg.mark_ready();
                    return;
                }
            };

            let Some(incremental) = persist_incremental_cache(&graph_store_bg, incremental).await
            else {
                indexing_bg.store(false, std::sync::atomic::Ordering::Relaxed);
                index_readiness_bg.mark_ready();
                return;
            };
            let IncrementalIndexResult {
                graph: incremental_graph,
                parsed_files,
                changed_count,
                index_report,
                ..
            } = incremental;
            index_health_bg.replace_from_report(&index_report);
            {
                let mut idx = indexer_bg.lock().await;
                idx.replace_shared_index(incremental_graph, parsed_files);
            }
            let files_indexed = changed_count;
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
                if let Ok(mut graph) = compaction_graph_bg.lock() {
                    *graph = Arc::clone(&new_graph);
                }
                let mut eng = engine_bg.lock().await;
                eng.update_graph_arc(new_graph);
            }

            // A successful startup publication is itself a health-fact input,
            // even when no source file changed relative to the warm manifest.
            // Each repository runtime derives its bounded local file set from
            // this immutable graph snapshot.
            for refresh in health_refresh_handles_bg.values() {
                refresh.request_full();
            }

            if let Some(model_path) = verified_shared_embedding_model_path() {
                match EmbeddingEngine::new(model_path.to_string_lossy().as_ref()) {
                    Ok(emb_engine) => {
                        tracing::info!("ONNX embedding model loaded");
                        let emb = cached_embedding_provider(
                            emb_engine,
                            &embedding_object_root_bg,
                            &embedding_checkout_id_bg,
                        );
                        if let Some(index) = vector_index_bg.as_ref() {
                            match emb.storage_identity().and_then(|identity| match identity {
                                Some(identity) => index
                                    .bind_embedding_identity(&identity)
                                    .map_err(anyhow::Error::new),
                                None => Ok(()),
                            }) {
                                Ok(()) => {}
                                Err(error) => {
                                    tracing::warn!(%error, "Failed to bind semantic index to the loaded model; lexical retrieval remains available");
                                    indexing_bg.store(false, std::sync::atomic::Ordering::Relaxed);
                                    index_readiness_bg.mark_ready();
                                    return;
                                }
                            }
                        }
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
                    Err(e) => tracing::warn!(
                        error = %e,
                        "Semantic retrieval unavailable; continuing with lexical retrieval"
                    ),
                }
            } else {
                tracing::info!(
                    "No valid shared embedding model installed; semantic search disabled and lexical recall remains available"
                );
            }

            indexing_bg.store(false, std::sync::atomic::Ordering::Relaxed);
            index_readiness_bg.mark_ready();
            tracing::info!("Background indexing complete for {}", ws_root.display());
        });
        completion_tasks.push(task);
    }

    let context_cache_path = cache_dir.join("context_handles.json");
    tracing::info!(
        "Creating MCP handler for {} (session: {})",
        workspace_root.display(),
        session_id
    );
    let handler = Arc::new(
        McpHandler::new_with_shared_repo_state(
            engine,
            indexer,
            memory_store,
            graph_store,
            embedding_engine,
            vector_index,
            workspace_root,
            memory_identity.repository_id.clone(),
            memories_path.clone(),
            context_cache_path,
            session_id,
            workspace_manager,
            workspace_roots,
            indexing,
            Some(Arc::clone(&event_writer)),
            default_focus_files,
            default_focus_dirs,
            repo_state,
            index_work,
            watcher_health,
            index_health,
        )
        .with_runtime_work_tracker(Arc::clone(&runtime_work))
        .with_checkout_storage(memory_identity.checkout_id.clone(), parsed_cache_runtime)
        .with_git_intelligence_snapshot_handle(git_intelligence_snapshots)
        .with_health_facts_snapshot_handle(health_fact_snapshots),
    );
    let session_digest_consolidation = session_digest_consolidation_runtime::start(
        memory_identity.repository_id.clone(),
        memory_identity.checkout_id.clone(),
        memory_identity.checkout_root.clone(),
        memories_path.clone(),
        Arc::clone(&event_writer),
    );
    let session_capture_retention = session_digest_consolidation_runtime::start_capture_retention(
        memory_identity.repository_id.clone(),
        memory_identity.checkout_id.clone(),
        memories_path.clone(),
    );
    let periodic_maintenance = {
        let handler = Arc::clone(&handler);
        let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _ = interval.tick() => {}
                }
                let _ = handler.auto_checkpoint_active_states("interval").await;
                if let Err(error) = handler.process_pending_memory_verifications().await {
                    tracing::warn!(%error, "Queued memory verification deferred");
                }
            }
        });
        PeriodicMaintenanceHandle {
            shutdown: Some(shutdown),
            task,
        }
    };
    Ok(WorkspaceRuntime {
        handler,
        background_tasks,
        completion_tasks,
        compaction_scheduler,
        session_capture_retention,
        session_digest_consolidation,
        periodic_maintenance: Some(periodic_maintenance),
        runtime_work,
        _checkout_lease: checkout_lease,
        storage_heartbeat: Some(storage_heartbeat),
    })
}

fn maintain_checkout_caches(registry: &mut StorageRegistry) -> Result<()> {
    crate::disk_budget::register_and_collect(registry, now_epoch_secs())?;
    let policy = CachePolicy::default();
    let now = now_epoch_secs();
    registry.advance_shared_accounting(256)?;
    let inventory = registry.advance_inventory(now, &policy, 256)?;
    if !inventory.historical_derived_artifacts.is_empty() {
        tracing::warn!(
            artifacts = ?inventory.historical_derived_artifacts,
            allocated_bytes = inventory.historical_derived_allocated_bytes,
            "Historical derived files require explicit offline migration; live files were preserved"
        );
    }
    let plan = if inventory.accounting_complete && !inventory.pressure_unknown {
        registry.plan_gc(now, &policy)?
    } else {
        tracing::info!("Repository cache accounting is incomplete; bounded sweep will resume");
        Vec::new()
    };
    if !plan.is_empty() {
        let report = registry.execute_gc(&plan, now, &policy)?;
        tracing::info!(
            moved = report.moved_files,
            deleted = report.deleted_files,
            released_bytes = report.released_bytes,
            "Repository derived-cache maintenance completed"
        );
    }
    // Parse objects become collectible only after the checkout cache has been
    // moved through the registry's durable trash journal.  Bound both the
    // registry enumeration and database deletion; a corrupt reconstructable
    // cache is retained and normal indexing falls back to local parsing.
    match registry.collect_parsed_objects(policy.batch_files) {
        Ok(removed) if removed > 0 => tracing::info!(
            removed,
            "Repository parsed-file cache maintenance completed"
        ),
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "Parsed-file cache maintenance deferred; preserving reconstructable cache")
        }
    }
    match registry.collect_content_objects(policy.batch_files) {
        Ok(report) if report.removed > 0 => tracing::info!(
            removed = report.removed,
            released_bytes = report.released_bytes,
            remaining_candidates = report.remaining_candidates,
            "Repository symbol-body maintenance completed"
        ),
        Ok(_) => {}
        Err(error) if error.to_string().contains("publication is active") => {
            tracing::debug!(%error, "Symbol-body maintenance deferred")
        }
        Err(error) => return Err(error),
    }
    let embeddings = registry.collect_embedding_objects(policy.low_bytes, policy.batch_files)?;
    if embeddings.removed > 0 {
        tracing::info!(
            removed = embeddings.removed,
            released_bytes = embeddings.released_bytes,
            remaining_bytes = embeddings.remaining_bytes,
            remaining_candidates = embeddings.remaining_candidates,
            "Repository embedding-object maintenance completed"
        );
    }
    Ok(())
}

struct StorageHeartbeat(Option<JoinHandle<()>>);

struct PeriodicMaintenanceHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl PeriodicMaintenanceHandle {
    async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

impl Drop for StorageHeartbeat {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl StorageHeartbeat {
    async fn shutdown(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

fn spawn_storage_heartbeat(mut registry: StorageRegistry, checkout_id: String) -> StorageHeartbeat {
    StorageHeartbeat(Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        let mut ticks = 0u8;
        loop {
            interval.tick().await;
            if let Err(error) = registry.heartbeat(&checkout_id, now_epoch_secs(), 0) {
                tracing::error!(checkout_id, %error, "Checkout storage heartbeat failed");
            }
            ticks = ticks.wrapping_add(1);
            if ticks == 15 {
                ticks = 0;
                if let Err(error) = maintain_checkout_caches(&mut registry) {
                    // Another process may own the repository maintenance lock.
                    // Its completed journal is authoritative; the next tick retries.
                    tracing::warn!(checkout_id, %error, "Repository cache maintenance deferred");
                }
            }
        }
    })))
}

fn start_event_compaction_scheduler(
    lattice_dir: &Path,
    event_store: Arc<EventStore>,
    event_writer: Arc<EventWriter>,
    graph: Arc<std::sync::Mutex<Arc<CodeGraph>>>,
) -> Option<SchedulerHandle> {
    let compactor = Compactor::new(
        event_store,
        event_writer,
        graph,
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

enum WarmGraphLoad {
    Snapshot(IndexSnapshot),
    Graph(Arc<CodeGraph>),
}

fn build_warm_query_engine(
    warm_graph: WarmGraphLoad,
    vector_index: Option<SharedVectorIndex>,
) -> (Arc<CodeGraph>, QueryEngine) {
    match warm_graph {
        WarmGraphLoad::Snapshot(snapshot) => {
            let graph = Arc::clone(&snapshot.graph);
            (
                graph,
                QueryEngine::from_index_snapshot(snapshot, vector_index),
            )
        }
        // A graph saved before digest snapshots is still structurally valid. It
        // remains available while startup indexing publishes a fresh generation.
        WarmGraphLoad::Graph(graph) => {
            let engine = QueryEngine::new_shared(Arc::clone(&graph), vector_index);
            (graph, engine)
        }
    }
}

async fn open_graph_store_with_warm_graph(
    graph_path: PathBuf,
    body_objects_path: PathBuf,
    checkout_id: String,
    workspace_root: PathBuf,
    index_work: Arc<IndexWorkCoordinator>,
) -> Result<(GraphStore, WarmGraphLoad)> {
    let _permit = index_work
        .acquire(workspace_root.to_string_lossy().to_string(), "warm_load")
        .await
        .expect("index work coordinator remains open for the process lifetime");
    tokio::task::spawn_blocking(move || {
        let graph_store = GraphStore::open_recovering_with_objects(
            &graph_path,
            &body_objects_path,
            &checkout_id,
        )?;
        if graph_store.recovery() == lattice_core::storage::GraphStoreRecovery::RebuiltCorrupt {
            tracing::warn!(
                graph_path = %graph_path.display(),
                workspace = %workspace_root.display(),
                "Replaced corrupt derived graph database; workspace indexing will rebuild it"
            );
        }
        let warm_graph = if should_warm_load_graph(&graph_store, &workspace_root) {
            match graph_store.load_index_snapshot() {
                Ok(IndexSnapshotLoad::Ready(snapshot)) => WarmGraphLoad::Snapshot(snapshot),
                Ok(IndexSnapshotLoad::DigestCacheMissing { graph }) => {
                    tracing::info!(
                        workspace = %workspace_root.display(),
                        "Persisted graph has no digest snapshot; using graph-only warm fallback"
                    );
                    WarmGraphLoad::Graph(graph)
                }
                Err(error) => {
                    tracing::warn!(
                        workspace = %workspace_root.display(),
                        %error,
                        "Failed to load persisted graph snapshot"
                    );
                    WarmGraphLoad::Graph(Arc::new(CodeGraph::new()))
                }
            }
        } else {
            WarmGraphLoad::Graph(Arc::new(CodeGraph::new()))
        };
        let graph = match &warm_graph {
            WarmGraphLoad::Snapshot(snapshot) => Arc::clone(&snapshot.graph),
            WarmGraphLoad::Graph(graph) => Arc::clone(graph),
        };
        let stats = graph.stats();
        if stats.node_count > 0 {
            tracing::info!(
                workspace = %workspace_root.display(),
                nodes = stats.node_count,
                edges = stats.edge_count,
                files = stats.file_count,
                "Warm-loaded persisted graph"
            );
        }
        Ok((graph_store, warm_graph))
    })
    .await
    .map_err(|error| anyhow::anyhow!("graph warm-load worker failed: {error}"))?
}

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
    Unavailable,
}

fn open_memory_store(
    path: &Path,
    migration: Option<(&str, &std::collections::BTreeSet<String>)>,
) -> Result<(Arc<Mutex<MemoryStore>>, MemoryStoreMode), LatticeError> {
    match try_open_persistent_memory_store(path, migration) {
        Ok(primary) => {
            tracing::info!("Persistent memory store ready: {}", path.display());
            Ok((Arc::new(Mutex::new(primary)), MemoryStoreMode::Persistent))
        }
        Err(initial_err) => unavailable_memory_store(path, initial_err),
    }
}

fn try_open_persistent_memory_store(
    path: &Path,
    migration: Option<(&str, &std::collections::BTreeSet<String>)>,
) -> Result<MemoryStore, LatticeError> {
    let owner = RepositoryMemoryOwner::acquire(path, std::time::Duration::from_secs(5))?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            LatticeError::MemoryStorageAccessDenied("Memory database name is invalid".into())
        })?;
    let primary = owner.open_store(leaf)?;
    if let Some((repository_id, proven)) = migration {
        let report = primary
            .with_connection(|connection| migrate_identities(connection, repository_id, proven))?;
        if report.migrated_memories + report.migrated_dependents + report.migrated_serialized_states
            > 0
        {
            tracing::info!(repository_id, migrated_memories=report.migrated_memories, migrated_dependents=report.migrated_dependents, migrated_serialized_states=report.migrated_serialized_states, before_checksum=%report.before_checksum, after_checksum=%report.after_checksum, "Historical memory identities migrated");
        }
    }
    Ok(primary)
}

#[cfg(test)]
fn memory_store_artifact_paths(path: &Path) -> [PathBuf; 3] {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
}

fn unavailable_memory_store(
    path: &Path,
    error: LatticeError,
) -> Result<(Arc<Mutex<MemoryStore>>, MemoryStoreMode), LatticeError> {
    let kind = match &error {
        LatticeError::MemoryStorageBusy(_) => MemoryStoreFailureKind::Busy,
        LatticeError::MemoryStorageAccessDenied(_) => MemoryStoreFailureKind::AccessDenied,
        LatticeError::MemoryStorageFull(_) => MemoryStoreFailureKind::Full,
        LatticeError::UnsupportedMemorySchema(_) => MemoryStoreFailureKind::UnsupportedSchema,
        LatticeError::CorruptMemoryStorage(_) => MemoryStoreFailureKind::Corrupt,
        _ => MemoryStoreFailureKind::Other,
    };
    let reason = error.to_string();
    tracing::error!("Persistent memory unavailable at {}: {}. Graph service remains available; memory writes will fail until explicit offline recovery.", path.display(), reason);
    let primary = MemoryStore::unavailable(path, kind, reason)?;
    Ok((Arc::new(Mutex::new(primary)), MemoryStoreMode::Unavailable))
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
    let lattice_dir =
        crate::workspace_identity::WorkspaceIdentity::resolve(&workspace)?.repository_lattice_dir;
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

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{
        build_incremental_index_for_roots, build_warm_query_engine, memory_store_artifact_paths,
        open_memory_store, MemoryStoreMode, WarmGraphLoad,
    };
    use lattice_core::error::LatticeError;
    use lattice_core::graph::CodeGraph;
    use lattice_core::memory::{
        Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus,
    };
    use lattice_core::storage::{GraphStore, IndexSnapshotLoad};
    use lattice_core::symbols::{Language, SymbolId, SymbolKind};
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

    fn graph_with_payment_symbol() -> CodeGraph {
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/payments.rs".to_string(),
                name: "charge_card".to_string(),
                byte_offset: 0,
            },
            SymbolKind::Function,
            "charge_card".to_string(),
            "fn charge_card(card: Card) -> Receipt".to_string(),
            "fn charge_card(card: Card) -> Receipt { todo!() }".to_string(),
            "src/payments.rs".to_string(),
            1,
            3,
            true,
            Language::Rust,
        );
        graph
    }

    #[test]
    fn warm_snapshot_hydrates_cached_module_digests_into_query_engine() {
        let store = GraphStore::open_in_memory().expect("in-memory graph store");
        let snapshot = store
            .save_index_snapshot(&graph_with_payment_symbol())
            .expect("save graph and digest snapshot");
        let (_, mut engine) = build_warm_query_engine(WarmGraphLoad::Snapshot(snapshot), None);

        let capsule = engine.query("charge card payments", None, false);
        assert!(
            capsule
                .context
                .iter()
                .any(|node| node.kind == "module_digest"),
            "hydrated snapshots should render their precomputed module digests"
        );
    }

    #[test]
    fn digest_cache_missing_uses_graph_only_warm_fallback() {
        let store = GraphStore::open_in_memory().expect("in-memory graph store");
        let warm_graph = match store.load_index_snapshot().expect("load fresh graph store") {
            IndexSnapshotLoad::DigestCacheMissing { graph } => WarmGraphLoad::Graph(graph),
            IndexSnapshotLoad::Ready(_) => {
                panic!("fresh graph store must not report a digest snapshot")
            }
        };
        let (loaded_graph, mut engine) = build_warm_query_engine(warm_graph, None);

        assert_eq!(loaded_graph.stats().node_count, 0);
        let capsule = engine.query("charge card payments", None, false);
        assert!(
            capsule
                .context
                .iter()
                .all(|node| node.kind != "module_digest"),
            "graph-only fallback must not synthesize digests while serving a query"
        );
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
    fn test_open_memory_store_uses_persistent_store_when_available() {
        let root = unique_temp_path("persistent-root");
        std::fs::create_dir_all(&root).expect("failed to create temp root");
        let db_path = root.join("memories.db");

        let (memory_store, mode) = open_memory_store(&db_path, None).unwrap();
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
    fn test_open_memory_store_migrates_proven_identity_before_open() {
        let root = unique_temp_path("identity-migration-root");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("memories.db");
        let seed = MemoryStore::open(&db_path).unwrap();
        seed.with_connection(|connection| { connection.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed,access_count)VALUES('historical','sentinel','fact','/proven/repo/.git',1,1,0)",[]).unwrap(); Ok(()) }).unwrap();
        drop(seed);
        let repository_id = format!("repo_{}", "d".repeat(64));
        let proven = std::collections::BTreeSet::from(["/proven/repo/.git".to_string()]);
        let (primary, mode) = open_memory_store(&db_path, Some((&repository_id, &proven))).unwrap();
        assert_eq!(mode, MemoryStoreMode::Persistent);
        let migrated = primary
            .try_lock()
            .unwrap()
            .get_by_id("historical")
            .unwrap()
            .unwrap();
        assert_eq!(
            migrated.workspace_id.as_deref(),
            Some(repository_id.as_str())
        );
        drop(primary);
        cleanup_memory_store_artifacts(&db_path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_open_memory_store_rejects_writes_when_persistent_open_fails() {
        let db_path = unique_temp_path("missing-parent")
            .join("missing")
            .join("memories.db");

        let (memory_store, mode) = open_memory_store(&db_path, None).unwrap();

        assert_eq!(mode, MemoryStoreMode::Unavailable);
        assert!(
            !db_path.exists(),
            "fallback path should not create an unusable persistent database"
        );

        let primary_error = memory_store
            .try_lock()
            .expect("memory store lock should be available")
            .store(make_memory("fallback primary write"))
            .expect_err("unavailable primary must reject non-durable writes");
        assert!(matches!(
            primary_error,
            LatticeError::MemoryStorageUnavailable(_)
        ));
    }

    #[test]
    fn test_open_memory_store_preserves_broken_artifacts_without_live_recovery() {
        let root = unique_temp_path("recover-root");
        std::fs::create_dir_all(&root).expect("failed to create temp root");
        let db_path = root.join("memories.db");

        std::fs::create_dir_all(&db_path)
            .expect("failed to create blocking directory at database path");

        let (memory_store, mode) = open_memory_store(&db_path, None).unwrap();

        assert_eq!(mode, MemoryStoreMode::Unavailable);
        assert!(
            db_path.is_dir(),
            "the original artifact must remain untouched"
        );
        assert!(
            !root.join("recovered-memory").exists(),
            "startup must not quarantine live artifacts"
        );
        memory_store
            .try_lock()
            .expect("memory store lock should be available")
            .store(make_memory("recovered persistent write"))
            .expect_err("unavailable store must reject writes");
        drop(memory_store);
        cleanup_memory_store_artifacts(&db_path);
        let _ = std::fs::remove_dir(&root);
    }
}
