use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, MutexGuard, Semaphore};
use tracing::Instrument;

use lattice_core::consolidation::{
    EpisodeOutcome, EpisodeTemplate, SessionConsolidationConfig, SessionConsolidator,
};
use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::events::{
    BranchRef, EventPage, EventQuery, EventReader, EventWriter, QueryOrder, SessionId,
};
use lattice_core::graph::model::CodeGraph;
use lattice_core::identity::MemoryId;
use lattice_core::indexer::Indexer;
use lattice_core::intelligence::{
    diagnose_failure, expand_context, find_relevant_tests, find_stale_docs, get_backlinks,
    get_docs_capsule, get_outgoing_links, get_repo_playbook, get_working_set_context,
    impact_from_diff, plan_edit, prepare_change, summarize_subsystem, trace_scenario, BundleMode,
    DocsTargetKind, ExpandContextSeed, FailureDiagnosis, MemoryHighlight, PlanEditBundle,
    RepoPlaybook, RulesDetector, ScenarioTraceBundle, SubsystemSummary, TaskBundle,
    WorkingSetContext,
};
use lattice_core::memory::model::MemoryStructuredFields;
use lattice_core::memory::{
    Memory, MemoryClass, MemoryQueryAuthority, MemoryRecallTier, MemoryScope, MemoryStore,
    MemoryStoreRouter, MemoryType, MemoryVerificationStatus,
};
use lattice_core::query::{ContextCapsule, QueryEngine};
use lattice_core::storage::{GraphStore, SharedVectorIndex};
use lattice_core::symbols::stable_file_handle;
use lattice_core::verification::ScopeFilter;
use lattice_core::watcher::should_index_file;
use lattice_core::working_memory::{summarize_state, CheckpointScope, WorkingMemoryState};
use lattice_core::workspace::WorkspaceManager;

use super::context_cache::ContextHandleCache;
use super::event_capture::{EventCapture, ToolOutcome};
use super::memory_v2;
use super::metrics_surface::{detail_payload, MetricsSurface};
use super::server::RequestHandler;
use super::session_metrics::{SessionMetrics, SessionMetricsReport, ToolCallMetadata};
use super::workflow_v2::outcome_capture::WorkflowOutcomeRecorder;
use super::workflow_v2::{
    self, VecEventSink, WorkflowBundle, WorkflowRenderChoice, WorkflowRequest,
};
use super::working_memory_tool;
use crate::adoption_metrics::{
    source_from_arguments, suggested_files_from_tool_result, AdoptionMetricsStore, ToolCallRecord,
};
use crate::index_health::IndexHealth;
use crate::index_work::IndexWorkCoordinator;
use crate::repo_state::{resolve_repo_state, RepoStateTracker, ValidationOutcome};
use crate::runtime_support::{
    background_vector_sync_enabled, build_incremental_index_for_roots, load_incremental_cache,
    max_warm_graph_bytes, max_warm_graph_files, persist_incremental_cache, IncrementalIndexResult,
    WARM_GRAPH_BYTE_LIMIT_ENV, WARM_GRAPH_FILE_LIMIT_ENV,
};
use crate::watcher_health::WatcherHealth;

/// MCP (Model Context Protocol) handler that routes JSON-RPC methods
/// to the appropriate tool implementations.
pub struct McpHandler {
    engine: Arc<Mutex<QueryEngine>>,
    query_jobs: Arc<Semaphore>,
    indexer: Arc<Mutex<Indexer>>,
    memory_store: Arc<Mutex<MemoryStore>>,
    shared_memory: Option<SharedMemoryRuntime>,
    shared_memory_error: Option<String>,
    graph_store: Arc<Mutex<GraphStore>>,
    embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
    vector_index: Option<SharedVectorIndex>,
    workspace_root: PathBuf,
    /// Canonical repository identity used only for durable memory scope. The
    /// checkout root remains the source/graph boundary.
    memory_workspace_id: String,
    session_id: String,
    #[allow(dead_code)]
    workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
    workspace_roots: Vec<PathBuf>,
    indexing: Arc<AtomicBool>,
    index_work: Arc<IndexWorkCoordinator>,
    context_cache: Arc<Mutex<ContextHandleCache>>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    adoption_metrics: Arc<AdoptionMetricsStore>,
    client_name: Arc<Mutex<Option<String>>>,
    event_capture: Option<Arc<EventCapture>>,
    workflow_outcome_recorder: Arc<WorkflowOutcomeRecorder>,
    session_consolidator: Option<Arc<StdMutex<SessionConsolidator>>>,
    working_memory_states: Arc<Mutex<HashMap<String, WorkingMemoryState>>>,
    working_memory_snapshots: Arc<Mutex<HashMap<String, WorkingMemoryState>>>,
    working_memory_checkpoint_hashes: Arc<Mutex<HashMap<String, String>>>,
    verify_explain_reports:
        Arc<Mutex<HashMap<String, memory_v2::verify_explain_memory::ExplainReport>>>,
    default_focus_files: Vec<String>,
    default_focus_dirs: Vec<String>,
    repo_state: Arc<Mutex<RepoStateTracker>>,
    refresh_running: Arc<AtomicBool>,
    watcher_health: Arc<WatcherHealth>,
    index_health: Arc<IndexHealth>,
}

#[derive(Clone)]
struct SharedMemoryRuntime {
    store: Arc<Mutex<MemoryStore>>,
    organization_id: String,
}

#[derive(Debug, Default)]
struct SharedMemoryConfig {
    organization_id: Option<String>,
    shared_store_path: Option<PathBuf>,
}

impl SharedMemoryRuntime {
    /// Shared-memory authority is process configuration only. In particular,
    /// repository files and MCP request arguments never grant organization
    /// access. C1 supplies the canonical repository identity to the handler.
    fn from_environment() -> Result<Option<Self>, String> {
        let config = load_shared_memory_config()?;
        let organization_id = match std::env::var("LATTICE_ORGANIZATION_ID") {
            Ok(value) if !value.trim().is_empty() => Some(value),
            Ok(_) => return Err("LATTICE_ORGANIZATION_ID must not be empty".to_string()),
            Err(std::env::VarError::NotPresent) => config.organization_id,
            Err(error) => return Err(format!("failed to read LATTICE_ORGANIZATION_ID: {error}")),
        };
        let Some(organization_id) = organization_id else {
            return Ok(None);
        };
        let path = match std::env::var_os("LATTICE_SHARED_MEMORY_PATH") {
            Some(value) => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err("LATTICE_SHARED_MEMORY_PATH must be absolute".to_string());
                }
                path
            }
            None => config.shared_store_path.unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".lattice/shared/memories.db"))
                    .unwrap_or_default()
            }),
        };
        if !path.is_absolute() {
            return Err("shared memory path must be absolute".to_string());
        }
        let parent = path.parent().ok_or_else(|| {
            format!(
                "shared memory path {} has no parent directory",
                path.display()
            )
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create shared memory directory {}: {error}",
                parent.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    format!(
                        "failed to protect shared memory directory {}: {error}",
                        parent.display()
                    )
                },
            )?;
        }
        let store = MemoryStore::open(&path).map_err(|error| {
            format!(
                "failed to open shared memory store {}: {error}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
                |error| {
                    format!(
                        "failed to protect shared memory database {}: {error}",
                        path.display()
                    )
                },
            )?;
        }
        Ok(Some(Self {
            store: Arc::new(Mutex::new(store)),
            organization_id,
        }))
    }

    fn query_authority(
        &self,
        repository_id: &str,
        checkout_id: &str,
        branch: Option<String>,
        session_id: &str,
    ) -> Result<MemoryQueryAuthority, String> {
        MemoryQueryAuthority::new(
            repository_id.to_string(),
            checkout_id.to_string(),
            branch,
            session_id.to_string(),
            Some(self.organization_id.clone()),
        )
        .map_err(|error| error.to_string())
    }
}

fn load_shared_memory_config() -> Result<SharedMemoryConfig, String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(SharedMemoryConfig::default());
    };
    let path = PathBuf::from(home).join(".lattice/config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SharedMemoryConfig::default())
        }
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    parse_shared_memory_config(&text)
}

fn parse_shared_memory_config(text: &str) -> Result<SharedMemoryConfig, String> {
    let mut config = SharedMemoryConfig::default();
    let mut in_memory_section = false;
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_memory_section = line == "[memory]";
            continue;
        }
        if !in_memory_section {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err("invalid [memory] configuration line; expected key = value".to_string());
        };
        let key = raw_key.trim();
        let value = raw_value.trim().trim_matches('"').trim_matches('\'');
        if value.is_empty() {
            return Err(format!("[memory].{key} must not be empty"));
        }
        match key {
            "organization_id" => config.organization_id = Some(value.to_string()),
            "shared_store_path" => config.shared_store_path = Some(PathBuf::from(value)),
            _ => {}
        }
    }
    if config
        .shared_store_path
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err("[memory].shared_store_path must be absolute".to_string());
    }
    Ok(config)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedBundleMode {
    Auto,
    Compact,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowBudget {
    Auto,
    Tiny,
    Compact,
    Full,
}

impl WorkflowBudget {
    fn as_str(self) -> &'static str {
        match self {
            WorkflowBudget::Auto => "auto",
            WorkflowBudget::Tiny => "tiny",
            WorkflowBudget::Compact => "compact",
            WorkflowBudget::Full => "full",
        }
    }
}

const TINY_WORKFLOW_TOKEN_CAP: usize = 260;
const COMPACT_WORKFLOW_TOKEN_CAP: usize = 850;
const FULL_WORKFLOW_TOKEN_CAP: usize = 2600;

pub(crate) const AGENT_CONTEXT_MODE_DEFAULT: &str = "auto";
pub(crate) const AGENT_IMPACT_LIMIT_DEFAULT: u64 = 12;
pub(crate) const AGENT_RECALL_MODE_DEFAULT: &str = "search";
pub(crate) const AGENT_STATUS_SCOPE_DEFAULT: &str = "index";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowWireFormat {
    Auto,
    Standard,
    Dense,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowRenderMode {
    Json,
    Markdown,
}

#[derive(Debug, Clone)]
struct WorkflowResponseOptions {
    budget: WorkflowBudget,
    max_tokens: Option<usize>,
    wire_format: WorkflowWireFormat,
    render: WorkflowRenderMode,
}

#[derive(Debug, Clone)]
struct WorkflowRunMetadata {
    delivery_mode: String,
    wire_format: String,
    single_anchor_used: bool,
    _mode_reason: String,
    semantic_fallback_used: bool,
    outcome_memory_reuse_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct SessionPruningProfile {
    prefer_tiny: bool,
    prune_memory_highlights: bool,
    prefer_dense: bool,
}

struct StatusSnapshot {
    stats: lattice_core::graph::GraphStats,
    languages: Option<std::collections::HashMap<String, usize>>,
}

const MAX_CONCURRENT_QUERY_JOBS: usize = 2;

#[derive(Debug, PartialEq, Eq)]
enum QueryJobError {
    Indexing,
    Busy,
    Panicked(String),
}

impl McpHandler {
    /// Create a new McpHandler with all shared state.
    #[allow(dead_code)]
    pub fn new(
        engine: Arc<Mutex<QueryEngine>>,
        indexer: Arc<Mutex<Indexer>>,
        memory_store: Arc<Mutex<MemoryStore>>,
        graph_store: Arc<Mutex<GraphStore>>,
        embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
        vector_index: Option<SharedVectorIndex>,
        workspace_root: PathBuf,
        context_cache_path: PathBuf,
        session_id: String,
        workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
        workspace_roots: Vec<PathBuf>,
        indexing: Arc<AtomicBool>,
        event_writer: Option<Arc<EventWriter>>,
        default_focus_files: Vec<String>,
        default_focus_dirs: Vec<String>,
    ) -> Self {
        let repo_state = Arc::new(Mutex::new(RepoStateTracker::new(&workspace_root)));
        let memory_workspace_id = workspace_root.to_string_lossy().to_string();
        let memory_db_path = workspace_root.join(".lattice").join("memories.db");
        Self::new_with_shared_repo_state(
            engine,
            indexer,
            memory_store,
            graph_store,
            embedding_engine,
            vector_index,
            workspace_root,
            memory_workspace_id,
            memory_db_path,
            context_cache_path,
            session_id,
            workspace_manager,
            workspace_roots,
            indexing,
            event_writer,
            default_focus_files,
            default_focus_dirs,
            repo_state,
            IndexWorkCoordinator::from_env(),
            Arc::new(WatcherHealth::default()),
            Arc::new(IndexHealth::default()),
        )
    }

    pub(crate) fn new_with_shared_repo_state(
        engine: Arc<Mutex<QueryEngine>>,
        indexer: Arc<Mutex<Indexer>>,
        memory_store: Arc<Mutex<MemoryStore>>,
        graph_store: Arc<Mutex<GraphStore>>,
        embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
        vector_index: Option<SharedVectorIndex>,
        workspace_root: PathBuf,
        memory_workspace_id: String,
        memory_db_path: PathBuf,
        context_cache_path: PathBuf,
        session_id: String,
        workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
        workspace_roots: Vec<PathBuf>,
        indexing: Arc<AtomicBool>,
        event_writer: Option<Arc<EventWriter>>,
        default_focus_files: Vec<String>,
        default_focus_dirs: Vec<String>,
        repo_state: Arc<Mutex<RepoStateTracker>>,
        index_work: Arc<IndexWorkCoordinator>,
        watcher_health: Arc<WatcherHealth>,
        index_health: Arc<IndexHealth>,
    ) -> Self {
        let workspace_id = memory_workspace_id.clone();
        let (shared_memory, shared_memory_error) = match SharedMemoryRuntime::from_environment() {
            Ok(shared_memory) => (shared_memory, None),
            Err(error) => {
                tracing::error!(%error, "organization memory disabled due to invalid daemon configuration");
                (None, Some(error))
            }
        };
        let refresh_running = Arc::new(AtomicBool::new(false));
        let branch = resolve_repo_state(&workspace_root)
            .and_then(|snapshot| snapshot.head_ref)
            .and_then(|head_ref| {
                head_ref
                    .strip_prefix("refs/heads/")
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        let event_capture = event_writer.as_ref().and_then(|writer| {
            match EventCapture::new(
                writer.clone(),
                workspace_id,
                branch,
                SessionId {
                    value: session_id.clone(),
                },
            ) {
                Ok(capture) => Some(Arc::new(capture)),
                Err(error) => {
                    tracing::warn!(%error, "event capture disabled for session");
                    None
                }
            }
        });
        let session_consolidator = event_writer.as_ref().and_then(|writer| {
            if !memory_db_path.exists() {
                return None;
            }
            match SessionConsolidator::open(
                writer.store(),
                &memory_db_path,
                SessionConsolidationConfig::default(),
            ) {
                Ok(consolidator) => Some(Arc::new(StdMutex::new(consolidator))),
                Err(error) => {
                    tracing::warn!(%error, "session consolidation disabled for workspace");
                    None
                }
            }
        });
        Self {
            engine,
            query_jobs: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERY_JOBS)),
            indexer,
            memory_store,
            shared_memory,
            shared_memory_error,
            graph_store,
            embedding_engine,
            vector_index,
            workspace_root: workspace_root.clone(),
            memory_workspace_id,
            session_id,
            workspace_manager,
            workspace_roots,
            indexing,
            index_work,
            context_cache: Arc::new(Mutex::new(ContextHandleCache::new_with_persistence(
                context_cache_path,
            ))),
            session_metrics: Arc::new(Mutex::new(SessionMetrics::new())),
            adoption_metrics: Arc::new(AdoptionMetricsStore::new(&workspace_root)),
            client_name: Arc::new(Mutex::new(None)),
            event_capture,
            workflow_outcome_recorder: Arc::new(WorkflowOutcomeRecorder::new()),
            session_consolidator,
            working_memory_states: Arc::new(Mutex::new(HashMap::new())),
            working_memory_snapshots: Arc::new(Mutex::new(HashMap::new())),
            working_memory_checkpoint_hashes: Arc::new(Mutex::new(HashMap::new())),
            verify_explain_reports: Arc::new(Mutex::new(HashMap::new())),
            default_focus_files,
            default_focus_dirs,
            repo_state,
            refresh_running,
            watcher_health,
            index_health,
        }
    }

    fn embed_query_for_fallback(&self, query: &str) -> Option<Vec<f32>> {
        self.embedding_engine
            .get()
            .and_then(|eng| eng.embed(query).ok())
    }

    fn is_indexing(&self) -> bool {
        self.indexing.load(Ordering::Relaxed)
            || self
                .index_work
                .workspace_is_busy(&self.workspace_root.to_string_lossy())
    }

    async fn validate_repo_epoch_for_graph_reads(&self) -> ValidationOutcome {
        let outcome = {
            let mut repo_state = self.repo_state.lock().await;
            repo_state.validate(&self.workspace_root)
        };
        if outcome != ValidationOutcome::Fresh {
            self.indexing.store(true, Ordering::Relaxed);
            self.spawn_workspace_refresh_if_needed();
        }
        outcome
    }

    fn spawn_workspace_refresh_if_needed(&self) {
        if self
            .refresh_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let workspace_roots = self.workspace_roots.clone();
        let engine = Arc::clone(&self.engine);
        let indexer = Arc::clone(&self.indexer);
        let graph_store = Arc::clone(&self.graph_store);
        let workspace_manager = self.workspace_manager.clone();
        let embedding_engine = Arc::clone(&self.embedding_engine);
        let vector_index = self.vector_index.clone();
        let repo_state = Arc::clone(&self.repo_state);
        let indexing = Arc::clone(&self.indexing);
        let refresh_running = Arc::clone(&self.refresh_running);
        let index_work = Arc::clone(&self.index_work);
        let index_health = Arc::clone(&self.index_health);
        let workspace_key = self.workspace_root.to_string_lossy().to_string();

        tokio::spawn(async move {
            loop {
                let _index_permit = index_work
                    .acquire(workspace_key.clone(), "workspace_refresh")
                    .await
                    .expect("index work coordinator remains open for the process lifetime");
                let target_epoch = {
                    let repo_state = repo_state.lock().await;
                    repo_state.current_epoch()
                };

                let (manifest, parsed_cache) = load_incremental_cache(&graph_store).await;
                let roots = workspace_roots.clone();
                let incremental = match tokio::task::spawn_blocking(move || {
                    build_incremental_index_for_roots(&roots, Some(&manifest), parsed_cache)
                })
                .await
                {
                    Ok(incremental) => incremental,
                    Err(error) => {
                        tracing::error!(%error, "Workspace refresh indexing worker failed; keeping the previously published graph");
                        break;
                    }
                };

                let Some(incremental) = persist_incremental_cache(&graph_store, incremental).await
                else {
                    break;
                };
                let IncrementalIndexResult {
                    graph: incremental_graph,
                    parsed_files,
                    index_report,
                    ..
                } = incremental;
                index_health.replace_from_report(&index_report);
                let new_graph = if let Some(manager) = &workspace_manager {
                    {
                        let mut manager = manager.lock().await;
                        for root in &workspace_roots {
                            let repo_name = crate::repo_name_for_root(root);
                            if let Err(error) = manager.add_repo(repo_name.clone(), root.clone()) {
                                tracing::warn!(
                                    "Failed to add repo {} during workspace refresh: {}",
                                    repo_name,
                                    error
                                );
                                continue;
                            }
                            let repo_prefix = format!("{}/", repo_name);
                            let repo_files: HashMap<String, lattice_core::symbols::ParsedFile> =
                                parsed_files
                                    .iter()
                                    .filter(|(file, _)| file.starts_with(&repo_prefix))
                                    .map(|(file, parsed)| (file.clone(), parsed.clone()))
                                    .collect();
                            manager.replace_repo_parsed_files(&repo_name, repo_files);
                        }
                        manager.detect_cross_repo_edges();
                    }
                    let manager = manager.lock().await;
                    Arc::new(manager.unified_graph())
                } else {
                    {
                        let mut indexer = indexer.lock().await;
                        indexer.replace_shared_index(Arc::clone(&incremental_graph), parsed_files);
                    }
                    incremental_graph
                };

                let publish_allowed = {
                    let repo_state = repo_state.lock().await;
                    repo_state.can_publish_epoch(target_epoch)
                };
                if !publish_allowed {
                    continue;
                }

                {
                    let mut engine = engine.lock().await;
                    engine.update_graph_arc(Arc::clone(&new_graph));
                }

                if background_vector_sync_enabled() {
                    if let (Some(embedding_engine), Some(vector_index)) =
                        (embedding_engine.get(), vector_index.as_ref())
                    {
                        let graph_for_sync = Arc::clone(&new_graph);
                        let embedding_for_sync = Arc::clone(embedding_engine);
                        let vector_for_sync = Arc::clone(vector_index);
                        match tokio::task::spawn_blocking(move || {
                            crate::vector_sync::sync_full_graph_embeddings(
                                &graph_for_sync,
                                embedding_for_sync.as_ref(),
                                vector_for_sync.as_ref(),
                            )
                        })
                        .await
                        {
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => tracing::warn!(
                                "Failed to refresh semantic index after workspace refresh: {}",
                                error
                            ),
                            Err(error) => tracing::warn!(
                                "Workspace refresh semantic sync worker failed: {}",
                                error
                            ),
                        }
                    }
                }

                {
                    let mut repo_state = repo_state.lock().await;
                    repo_state.mark_published_epoch(target_epoch);
                }
                indexing.store(false, Ordering::Relaxed);
                break;
            }
            refresh_running.store(false, Ordering::Release);
            let still_switching = {
                let repo_state = repo_state.lock().await;
                repo_state.branch_switching()
            };
            if still_switching {
                indexing.store(true, Ordering::Relaxed);
            }
        });
    }

    async fn workflow_repo_state_placeholder(
        &self,
        tool_name: &str,
        query: &str,
        render: WorkflowRenderMode,
    ) -> Option<Value> {
        match self.validate_repo_epoch_for_graph_reads().await {
            ValidationOutcome::Fresh => None,
            ValidationOutcome::BranchSwitch => Some(wrap_workflow_tool_result(
                indexing_workflow_response(tool_name, query, "branch_switch"),
                render,
            )),
            ValidationOutcome::WorkspaceChange => Some(wrap_workflow_tool_result(
                indexing_workflow_response(tool_name, query, "workspace_change"),
                render,
            )),
        }
    }

    async fn current_repo_epoch(&self) -> u64 {
        let repo_state = self.repo_state.lock().await;
        repo_state.indexed_epoch()
    }

    fn current_memory_scope_filter(&self) -> ScopeFilter {
        let branch = current_git_branch(&self.workspace_root).map(|name| BranchRef { name });
        ScopeFilter::new(self.memory_workspace_id.clone(), branch, None)
            .for_session(self.session_id.clone())
    }

    async fn lock_query_engine_for_workflow(&self) -> Result<MutexGuard<'_, QueryEngine>, ()> {
        if self.is_indexing() {
            if let Ok(engine) = self.engine.try_lock() {
                return Ok(engine);
            }
            return tokio::time::timeout(std::time::Duration::from_millis(75), self.engine.lock())
                .await
                .map_err(|_| ());
        }

        Ok(self.engine.lock().await)
    }

    async fn query_engine_snapshot_for_workflow(&self) -> Result<QueryEngine, QueryJobError> {
        let mut engine = self
            .lock_query_engine_for_workflow()
            .await
            .map_err(|()| QueryJobError::Indexing)?;
        if self.is_indexing()
            && workflow_graph_is_empty(engine.graph())
            && !self.promote_live_graph_for_workflow(&mut engine)
        {
            return Err(QueryJobError::Indexing);
        }
        Ok(engine.clone())
    }

    async fn run_query_job<T, F>(&self, job: F) -> Result<T, QueryJobError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = Arc::clone(&self.query_jobs)
            .try_acquire_owned()
            .map_err(|_| QueryJobError::Busy)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job()
        })
        .await
        .map_err(|error| QueryJobError::Panicked(error.to_string()))
    }

    fn query_job_error_response(
        &self,
        tool_name: &str,
        query: &str,
        render: WorkflowRenderMode,
        error: QueryJobError,
    ) -> Result<Value, (i32, String)> {
        match error {
            QueryJobError::Indexing => Ok(wrap_workflow_tool_result(
                indexing_workflow_response(tool_name, query, "indexing"),
                render,
            )),
            QueryJobError::Busy => Ok(wrap_workflow_tool_result(
                busy_query_workflow_response(tool_name, query),
                render,
            )),
            QueryJobError::Panicked(message) => Err((
                -32603,
                format!("{tool_name} query worker failed: {message}"),
            )),
        }
    }

    fn promote_live_graph_for_workflow(&self, engine: &mut QueryEngine) -> bool {
        if engine.graph().stats().node_count > 0 {
            return true;
        }
        if !self.is_indexing() {
            return false;
        }

        if let Some(wm) = &self.workspace_manager {
            if let Ok(wm) = wm.try_lock() {
                let graph = wm.unified_graph();
                if graph.stats().node_count > 0 {
                    engine.update_graph(graph);
                    return true;
                }
            }
        }

        if let Ok(indexer) = self.indexer.try_lock() {
            let graph = indexer.graph().clone();
            if graph.stats().node_count > 0 {
                engine.update_graph(graph);
                return true;
            }
        }

        false
    }

    // ── MCP Protocol Methods ──────────────────────────────────────────

    async fn remember_client_info(&self, params: &Value) {
        let Some(name) = params
            .get("clientInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            return;
        };
        let mut client = self.client_name.lock().await;
        *client = Some(name.to_string());
    }

    fn handle_initialize(&self) -> Value {
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": "lattice",
                "version": env!("CARGO_PKG_VERSION")
            }
        })
    }

    fn try_live_status_snapshot(&self, include_languages: bool) -> Option<StatusSnapshot> {
        if let Some(wm) = &self.workspace_manager {
            if let Ok(wm) = wm.try_lock() {
                let repo_stats = wm.repo_stats();
                return Some(StatusSnapshot {
                    stats: lattice_core::graph::GraphStats {
                        node_count: repo_stats.iter().map(|s| s.node_count).sum(),
                        edge_count: repo_stats.iter().map(|s| s.edge_count).sum(),
                        file_count: repo_stats.iter().map(|s| s.file_count).sum(),
                    },
                    // Avoid building a merged graph while indexing. Multi-repo status must
                    // stay cheap even when one repo has a large transient graph.
                    languages: None,
                });
            }
        }

        if let Ok(indexer) = self.indexer.try_lock() {
            return Some(status_snapshot_from_graph(
                indexer.graph(),
                include_languages,
            ));
        }

        None
    }

    async fn current_status_snapshot(&self, include_languages: bool) -> StatusSnapshot {
        if self.is_indexing() {
            if let Some(snapshot) = self.try_live_status_snapshot(include_languages) {
                return snapshot;
            }
        }

        if let Ok(engine) = self.engine.try_lock() {
            return status_snapshot_from_graph(engine.graph(), include_languages);
        }

        let engine = self.engine.lock().await;
        status_snapshot_from_graph(engine.graph(), include_languages)
    }

    fn handle_agent_tools_list(&self) -> Value {
        json!({
            "tools": [
                {
                    "name": "context",
                    "description": "Finds the relevant code, docs, rules, or prior handle for a task before you know exact strings — the working set grep cannot rank. Do not use for exact literal lookup.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "query": { "type": "string", "description": "Natural language query for retrieval modes." },
                            "mode": {
                                "type": "string",
                                "description": "Context mode.",
                                "enum": ["auto", "focused", "subsystem", "docs", "skeleton", "working_set", "rules", "expand", "repo"],
                                "default": AGENT_CONTEXT_MODE_DEFAULT
                            },
                            "files": { "type": "array", "items": { "type": "string" } },
                            "symbols": { "type": "array", "items": { "type": "string" } },
                            "file": { "type": "string", "description": "File for skeleton mode." },
                            "handle": { "type": "string", "description": "Context handle for expand mode." },
                            "focus": { "type": "string", "description": "Expansion focus for expand mode." },
                            "max_tokens": { "type": "integer" },
                            "budget": { "type": "string", "enum": ["tiny", "compact", "full"] },
                            "render": { "type": "string", "enum": ["json", "markdown"], "default": "markdown" },
                            "wire_format": { "type": "string", "enum": ["standard", "dense"] }
                        }
                    }
                },
                {
                    "name": "prepare_change",
                    "description": "Builds the edit plan, likely files, tests, risks, and memory for a change — the implementation map grep cannot assemble. Call before non-trivial fixes, features, or refactors.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "task": { "type": "string", "description": "Natural language task." },
                            "query": { "type": "string", "description": "Alias for task." },
                            "mode": {
                                "type": "string",
                                "enum": ["prepare", "plan_edit", "trace", "auto", "compact", "full"],
                                "default": "prepare"
                            },
                            "entry_files": { "type": "array", "items": { "type": "string" } },
                            "entry_symbols": { "type": "array", "items": { "type": "string" } },
                            "budget": { "type": "string", "enum": ["tiny", "compact", "full"] },
                            "max_tokens": { "type": "integer" },
                            "render": { "type": "string", "enum": ["json", "markdown"], "default": "markdown" },
                            "wire_format": { "type": "string", "enum": ["standard", "dense"] }
                        },
                        "required": ["task"]
                    }
                },
                {
                    "name": "impact",
                    "description": "Returns every symbol, file, and test affected by changing a target — the blast radius grep cannot compute. Call before multi-file or non-obvious changes.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "target": { "description": "Symbol/file/diff target. May be a string or object.", "oneOf": [{ "type": "string" }, { "type": "object" }] },
                            "direction": { "type": "string", "enum": ["dependents", "dependencies", "both", "diff", "tests"], "default": "dependents" },
                            "include_tests": { "type": "boolean", "default": true },
                            "name": { "type": "string" },
                            "file": { "type": "string" },
                            "diff": { "type": "string" },
                            "files": { "type": "array", "items": { "type": "string" } },
                            "symbols": { "type": "array", "items": { "type": "string" } },
                            "hops": { "type": "integer", "default": 3 },
                            "limit": { "type": "integer", "default": AGENT_IMPACT_LIMIT_DEFAULT },
                            "budget": { "type": "string", "enum": ["tiny", "compact", "full"] },
                            "max_tokens": { "type": "integer" },
                            "render": { "type": "string", "enum": ["json", "markdown"], "default": "markdown" },
                            "wire_format": { "type": "string", "enum": ["standard", "dense"] }
                        }
                    }
                },
                {
                    "name": "diagnose",
                    "description": "Maps compiler, test, and runtime failure text to likely culprit code and tests — the failure path grep cannot infer. Call before opening files from a stack trace.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "failure_text": { "type": "string" },
                            "input": { "type": "string", "description": "Alias for failure_text." },
                            "context_files": { "type": "array", "items": { "type": "string" } },
                            "kind": { "type": "string" },
                            "budget": { "type": "string", "enum": ["tiny", "compact", "full"] },
                            "mode": { "type": "string", "enum": ["auto", "compact", "full"], "default": "auto" },
                            "max_tokens": { "type": "integer" },
                            "render": { "type": "string", "enum": ["json", "markdown"], "default": "markdown" },
                            "wire_format": { "type": "string", "enum": ["standard", "dense"] }
                        },
                        "required": ["failure_text"]
                    }
                },
                {
                    "name": "search",
                    "description": "Searches symbols, call paths, and docs links using graph identity — the structural match grep cannot provide. Use rg for exact text.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "query": { "type": "string" },
                            "kind": { "type": "string", "enum": ["symbol", "flow", "links", "symbol_detail"], "default": "symbol" },
                            "name": { "type": "string" },
                            "file": { "type": "string" },
                            "from": { "type": "string" },
                            "to": { "type": "string" },
                            "from_file": { "type": "string" },
                            "to_file": { "type": "string" },
                            "target": { "type": "string" },
                            "direction": { "type": "string", "enum": ["backlinks", "outgoing"], "default": "backlinks" },
                            "limit": { "type": "integer" },
                            "detail": { "type": "string", "enum": ["summary", "full"] },
                            "max_depth": { "type": "integer" }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "remember",
                    "description": "Stores durable task memory or workflow outcomes for future sessions — the cross-session recall grep cannot create. Use only for claims worth reusing.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "content": { "type": "string" },
                            "kind": { "type": "string", "enum": ["quick", "durable", "outcome"], "default": "quick" },
                            "task": { "type": "string" },
                            "summary": { "type": "string" },
                            "status": { "type": "string", "enum": ["success", "failure"] },
                            "scope": { "type": "string" },
                            "confidence": { "type": "number" },
                            "confidence_reason": { "type": "string" },
                            "freshness_policy": { "type": "string" },
                            "memory_class": { "type": "string" },
                            "linked_files": { "type": "array", "items": { "type": "string" } },
                            "linked_symbols": { "type": "array", "items": { "type": "string" } },
                            "linked_docs": { "type": "array", "items": { "type": "string" } },
                            "linked_tests": { "type": "array", "items": { "type": "string" } },
                            "files": { "type": "array", "items": { "type": "string" } },
                            "symbols": { "type": "array", "items": { "type": "string" } },
                            "tests": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["content"]
                    }
                },
                {
                    "name": "recall",
                    "description": "Retrieves and verifies prior task memory with trust signals — the historical context grep cannot recover. Treat retrieved memory as recall until verified.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "query": { "type": "string" },
                            "mode": { "type": "string", "enum": ["search", "task", "verify"], "default": AGENT_RECALL_MODE_DEFAULT },
                            "task_id": { "type": "string" },
                            "task_statement": { "type": "string" },
                            "memory_id": { "type": "string" },
                            "limit": { "type": "integer" },
                            "budget_tokens": { "type": "integer" },
                            "focus_files": { "type": "array", "items": { "type": "string" } },
                            "focus_dirs": { "type": "array", "items": { "type": "string" } },
                            "intent_hint": { "type": "string" },
                            "render_mode": { "type": "string", "enum": ["compact", "full", "diagnostic"] }
                        }
                    }
                },
                {
                    "name": "status",
                    "description": "Reports indexing, stale-doc, stale-memory, and conflict health — the operational state grep cannot see. Call when results look incomplete or memory may be stale.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "_lattice_client": { "type": "string", "description": "Optional client identity for adoption metrics." },
                            "_lattice_channel": { "type": "string", "description": "Optional channel identity for adoption metrics." },
                            "scope": { "type": "string", "enum": ["index", "docs", "memory", "conflicts"], "default": AGENT_STATUS_SCOPE_DEFAULT },
                            "query": { "type": "string" },
                            "files": { "type": "array", "items": { "type": "string" } },
                            "symbols": { "type": "array", "items": { "type": "string" } },
                            "anchor": { "type": "string" },
                            "limit": { "type": "integer" },
                            "cursor": { "type": "integer" },
                            "render_mode": { "type": "string", "enum": ["compact", "full", "diagnostic"] }
                        }
                    }
                }
            ]
        })
    }

    async fn handle_agent_tools_call(&self, params: &Value) -> Result<Value, (i32, String)> {
        let tool_name = params["name"]
            .as_str()
            .ok_or((-32602, "Missing tool name".to_string()))?;
        let arguments = &params["arguments"];
        let span = tracing::info_span!("tool", name = tool_name);
        let tool_called_event = self.capture_tool_called(tool_name, arguments);
        let started = Instant::now();

        let result = match tokio::time::timeout(Duration::from_secs(5), async {
            match tool_name {
                "context" => self.tool_agent_context(arguments).await,
                "prepare_change" => self.tool_agent_prepare_change(arguments).await,
                "impact" => self.tool_agent_impact(arguments).await,
                "diagnose" => self.tool_agent_diagnose(arguments).await,
                "search" => self.tool_agent_search(arguments).await,
                "remember" => self.tool_agent_remember(arguments).await,
                "recall" => self.tool_agent_recall(arguments).await,
                "status" => self.tool_agent_status(arguments).await,
                _ => Err((-32602, format!("Unknown tool: {}", tool_name))),
            }
        })
        .instrument(span)
        .await
        {
            Ok(result) => result,
            Err(_) => Ok(wrap_tool_result(json!({
                "partial": true,
                "timeout": true,
                "tool": tool_name,
                "message": "server-side 5s cap reached; returning a bounded partial response",
            }))),
        };

        if let Ok(ref value) = result {
            self.record_tool_metrics(tool_name, value).await;
        }
        self.record_adoption_tool_call(
            tool_name,
            arguments,
            result.as_ref().ok(),
            started.elapsed(),
        )
            .await;
        self.capture_tool_result(tool_name, arguments, &result, tool_called_event)
            .await;

        result
    }

    async fn tool_agent_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let mode = args["mode"].as_str().unwrap_or(AGENT_CONTEXT_MODE_DEFAULT);
        let mut routed = clone_object_value(args);
        match mode {
            "focused" => {
                set_string(&mut routed, "mode", "focused");
                self.tool_query_context(&routed).await
            }
            "subsystem" => self.tool_summarize_subsystem(&routed).await,
            "docs" => self.tool_get_docs_capsule(&routed).await,
            "skeleton" => {
                if routed.get("file").is_none() {
                    if let Some(file) = first_string(args, &["target", "query"]) {
                        set_value(&mut routed, "file", Value::String(file));
                    }
                }
                self.tool_get_file_context(&routed).await
            }
            "working_set" => self.tool_get_working_set_context(&routed).await,
            "rules" => self.tool_get_project_rules(&routed).await,
            "expand" => self.tool_expand_context(&routed).await,
            "repo" => self.tool_get_repo_playbook(&routed).await,
            "auto" | _ => {
                set_string(&mut routed, "mode", "full");
                self.tool_query_context(&routed).await
            }
        }
    }

    async fn tool_agent_prepare_change(&self, args: &Value) -> Result<Value, (i32, String)> {
        let route_mode = args["mode"].as_str().unwrap_or("prepare");
        let mut routed = clone_object_value(args);
        copy_first_string(&mut routed, args, &["query", "task"], "query");
        match route_mode {
            "plan_edit" => self.tool_plan_edit(&routed).await,
            "trace" => {
                copy_first_string(
                    &mut routed,
                    args,
                    &["scenario", "task", "query"],
                    "scenario",
                );
                self.tool_trace_scenario(&routed).await
            }
            "compact" | "full" => {
                set_string(&mut routed, "mode", route_mode);
                self.tool_prepare_change(&routed).await
            }
            "prepare" | "auto" | _ => {
                remove_key(&mut routed, "mode");
                self.tool_prepare_change(&routed).await
            }
        }
    }

    async fn tool_agent_impact(&self, args: &Value) -> Result<Value, (i32, String)> {
        let mut routed = clone_object_value(args);
        normalize_target_fields(&mut routed, args);
        let direction = args["direction"].as_str().unwrap_or("dependents");
        if routed.get("diff").and_then(Value::as_str).is_some() || direction == "diff" {
            return self.tool_impact_from_diff(&routed).await;
        }
        if direction == "tests" {
            return self.tool_find_relevant_tests(&routed).await;
        }

        let impact = if routed["name"].as_str().is_none() && routed["file"].as_str().is_some() {
            self.tool_file_impact(&routed, direction).await?
        } else {
            match direction {
                "dependencies" => self.tool_get_dependencies(&routed).await?,
                "both" => json!({
                    "dependents": self.tool_get_dependents(&routed).await?,
                    "dependencies": self.tool_get_dependencies(&routed).await?
                }),
                _ => self.tool_blast_radius(&routed).await?,
            }
        };
        if args["include_tests"].as_bool().unwrap_or(true) {
            let tests = self.tool_find_relevant_tests(&routed).await?;
            Ok(wrap_tool_result(json!({
                "impact": unwrap_tool_text_json(&impact).unwrap_or(impact),
                "tests": unwrap_tool_text_json(&tests).unwrap_or(tests)
            })))
        } else {
            Ok(impact)
        }
    }

    async fn tool_file_impact(
        &self,
        args: &Value,
        direction: &str,
    ) -> Result<Value, (i32, String)> {
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;
        let hops = (args["hops"].as_u64().unwrap_or(3) as usize).min(10);
        let symbol_limit =
            (args["limit"].as_u64().unwrap_or(AGENT_IMPACT_LIMIT_DEFAULT) as usize).min(50);
        let relation_limit = 25usize;

        let engine = self.engine.lock().await;
        let file_nodes = engine.file_symbols(file);
        let mut affected_files = HashSet::new();
        let mut symbol_impacts = Vec::new();

        for node in file_nodes.iter().take(symbol_limit) {
            let dependents = if direction == "dependencies" {
                Vec::new()
            } else {
                engine.graph().get_transitive_dependents(&node.id, hops)
            };
            let dependencies = if direction == "dependents" {
                Vec::new()
            } else {
                engine
                    .graph()
                    .get_dependencies(&node.id)
                    .into_iter()
                    .map(|(dep, _)| dep.clone())
                    .collect::<Vec<_>>()
            };

            let dependent_values: Vec<Value> = dependents
                .iter()
                .take(relation_limit)
                .map(|dep| {
                    affected_files.insert(dep.file.clone());
                    json!({
                        "s": dep.name,
                        "k": dep.kind.short_code(),
                        "f": dep.file,
                        "l": dep.line,
                    })
                })
                .collect();
            let dependency_values: Vec<Value> = dependencies
                .iter()
                .take(relation_limit)
                .map(|dep| {
                    affected_files.insert(dep.file.clone());
                    json!({
                        "s": dep.name,
                        "k": dep.kind.short_code(),
                        "f": dep.file,
                        "l": dep.line,
                    })
                })
                .collect();

            let mut item = json!({
                "symbol": node.name,
                "kind": node.kind.short_code(),
                "line": node.line,
            });
            if direction != "dependencies" {
                set_value(
                    &mut item,
                    "dependents",
                    json!({
                        "items": dependent_values,
                        "count": dependents.len(),
                        "truncated": dependents.len() > relation_limit,
                    }),
                );
            }
            if direction != "dependents" {
                set_value(
                    &mut item,
                    "dependencies",
                    json!({
                        "items": dependency_values,
                        "count": dependencies.len(),
                        "truncated": dependencies.len() > relation_limit,
                    }),
                );
            }
            symbol_impacts.push(item);
        }

        let mut files: Vec<String> = affected_files.into_iter().collect();
        files.sort();

        Ok(wrap_tool_result(json!({
            "file": file,
            "direction": direction,
            "hops": hops,
            "symbols": symbol_impacts,
            "symbol_count": file_nodes.len(),
            "symbols_truncated": file_nodes.len() > symbol_limit,
            "files": files,
            "count": files.len(),
        })))
    }

    async fn tool_agent_diagnose(&self, args: &Value) -> Result<Value, (i32, String)> {
        let mut routed = clone_object_value(args);
        copy_first_string(&mut routed, args, &["input", "failure_text"], "input");
        self.tool_diagnose_failure(&routed).await
    }

    async fn tool_agent_search(&self, args: &Value) -> Result<Value, (i32, String)> {
        let kind = args["kind"].as_str().unwrap_or("symbol");
        let mut routed = clone_object_value(args);
        match kind {
            "flow" => self.tool_search_logic_flow(&routed).await,
            "links" => {
                if routed.get("target").is_none() {
                    copy_first_string(&mut routed, args, &["target", "query"], "target");
                }
                if args["direction"].as_str() == Some("outgoing") {
                    self.tool_get_outgoing_links(&routed).await
                } else {
                    self.tool_get_backlinks(&routed).await
                }
            }
            "symbol_detail" => {
                copy_first_string(&mut routed, args, &["name", "query"], "name");
                self.tool_get_symbol(&routed).await
            }
            "symbol" | _ => {
                copy_first_string(&mut routed, args, &["pattern", "query"], "pattern");
                self.tool_search_symbols(&routed).await
            }
        }
    }

    async fn tool_agent_remember(&self, args: &Value) -> Result<Value, (i32, String)> {
        match args["kind"].as_str().unwrap_or("quick") {
            "durable" => self.tool_save_memory_v2(args).await,
            "outcome" => self.tool_record_workflow_outcome(args).await,
            "quick" | _ => self.tool_save_quick_memory_v2(args).await,
        }
    }

    async fn tool_agent_recall(&self, args: &Value) -> Result<Value, (i32, String)> {
        match args["mode"].as_str().unwrap_or(AGENT_RECALL_MODE_DEFAULT) {
            "task" => self.tool_get_task_memory_v2(args).await,
            "verify" => self.tool_verify_explain_memory(args).await,
            "search" | _ => self.tool_search_memory(args).await,
        }
    }

    async fn tool_agent_status(&self, args: &Value) -> Result<Value, (i32, String)> {
        match args["scope"].as_str().unwrap_or(AGENT_STATUS_SCOPE_DEFAULT) {
            "docs" => self.tool_find_stale_docs(args).await,
            "memory" => self.tool_list_stale_memories(args).await,
            "conflicts" => self.tool_list_memory_conflicts(args).await,
            "index" | _ => self.tool_index_status(args).await,
        }
    }

    fn handle_tools_list(&self) -> Value {
        let mut result = json!({
            "tools": [
                {
                    "name": "get_context_capsule",
                    "description": "First discovery tool when you do not yet know which files matter. Returns the most relevant source pivots plus nearby symbols, along with a reusable context_handle and suggested_expand target, so you can avoid broad file reads.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language query describing what you need context for"
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'full' (default, multiple pivots + context) or 'focused' (max 1 pivot, max 5 context, minimal budget)",
                                "enum": ["full", "focused"],
                                "default": "full"
                            },
                            "render": {
                                "type": "string",
                                "description": "Result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "prepare_change",
                    "description": "First workflow tool for fix/add/refactor tasks once you know the area. Returns likely edit files, symbols, tests, risks, and reusable memory in one bundle.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language task such as 'fix login timeout' or 'add OAuth refresh'"
                            },
                            "entry_files": {
                                "type": "array",
                                "description": "Optional files to bias the change plan toward",
                                "items": { "type": "string" }
                            },
                            "entry_symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias the change plan toward",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "plan_edit",
                    "description": "Patch-oriented planning bundle that returns likely edit files, candidate spans, affected callers/dependencies, relevant docs, and recommended tests in one assistant-facing plan.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language task such as 'fix login timeout' or 'add OAuth refresh'"
                            },
                            "entry_files": {
                                "type": "array",
                                "description": "Optional files to bias the edit plan toward",
                                "items": { "type": "string" }
                            },
                            "entry_symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias the edit plan toward",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "trace_scenario",
                    "description": "Scenario-focused debugging bundle that traces likely execution paths, guards, side effects, and failure branches from a behavior description.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "scenario": {
                                "type": "string",
                                "description": "Behavior description such as 'why does login fail after refresh'"
                            },
                            "entry_files": {
                                "type": "array",
                                "description": "Optional files to bias scenario tracing toward",
                                "items": { "type": "string" }
                            },
                            "entry_symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias scenario tracing toward",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": ["scenario"]
                    }
                },
                {
                    "name": "find_relevant_tests",
                    "description": "Find tests that are most relevant to a set of files, symbols, or a diff.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "files": {
                                "type": "array",
                                "description": "Optional source files to anchor test selection",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols to anchor test selection",
                                "items": { "type": "string" }
                            },
                            "diff": {
                                "type": "string",
                                "description": "Optional unified diff text; file paths will be extracted from it"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum tests to return (default: 8)",
                                "default": 8
                            }
                        }
                    }
                },
                {
                    "name": "impact_from_diff",
                    "description": "First review tool for a local diff. Summarizes changed symbols, downstream impact, risks, review checklist, and relevant tests.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "diff": {
                                "type": "string",
                                "description": "Unified diff text to analyze"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional extra files to bias impact and test selection",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional extra symbols to bias impact and test selection",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            },
                            "hops": {
                                "type": "integer",
                                "description": "Dependent traversal depth (default: 2)",
                                "default": 2
                            }
                        },
                        "required": ["diff"]
                    }
                },
                {
                    "name": "get_working_set_context",
                    "description": "Use when several files are already open or known, not as the first discovery call. Compresses the working set into one bundle of files, symbols, tests, and memory.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Optional task hint used to bias nearby symbols and memory recall"
                            },
                            "files": {
                                "type": "array",
                                "description": "Files already in the active working set, such as open editors or recently changed files",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Focused symbols already in the working set",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "summarize_subsystem",
                    "description": "Summary-first map for an unfamiliar subsystem. Returns key files, symbols, tests, and durable memory without loading full source.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language subsystem or domain you want summarized"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional files to anchor the subsystem summary",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols to anchor the subsystem summary",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_repo_playbook",
                    "description": "Repo-wide startup summary of architecture, conventions, high-signal files, and durable patterns.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "get_docs_capsule",
                    "description": "Return the most relevant Markdown docs and sections for a query, plus related code symbols mentioned from those docs.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language question or topic to find docs for"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional source files to bias the doc ranking toward",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias the doc ranking toward",
                                "items": { "type": "string" }
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum docs or sections to return (default: 6)",
                                "default": 6
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_backlinks",
                    "description": "Return inbound Markdown references to a symbol, file, document, or section.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "target": {
                                "type": "string",
                                "description": "Target symbol, file path, doc path, or section reference such as docs/guide.md#Setup"
                            },
                            "kind": {
                                "type": "string",
                                "description": "Optional target kind hint",
                                "enum": ["auto", "file", "symbol", "doc", "section"],
                                "default": "auto"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum backlinks to return (default: 12)",
                                "default": 12
                            }
                        },
                        "required": ["target"]
                    }
                },
                {
                    "name": "get_outgoing_links",
                    "description": "Return outgoing Markdown links and code mentions from a document, section, or file target.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "target": {
                                "type": "string",
                                "description": "Target doc path, section reference, symbol, or file path"
                            },
                            "kind": {
                                "type": "string",
                                "description": "Optional target kind hint",
                                "enum": ["auto", "file", "symbol", "doc", "section"],
                                "default": "auto"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum outgoing links to return (default: 12)",
                                "default": 12
                            }
                        },
                        "required": ["target"]
                    }
                },
                {
                    "name": "find_stale_docs",
                    "description": "Find Markdown docs and sections that likely need review because they mention changed symbols, changed files, or changed docs.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "files": {
                                "type": "array",
                                "description": "Changed file paths to check docs against",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Changed symbol names to check docs against",
                                "items": { "type": "string" }
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum stale docs or sections to return (default: 12)",
                                "default": 12
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "diagnose_failure",
                    "description": "First failure tool for compiler errors, failing tests, and stack traces. Turns raw failure text into suspects, related code, tests, and next steps.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Raw error text, stack trace, failing test output, or compiler diagnostic"
                            },
                            "kind": {
                                "type": "string",
                                "description": "Optional hint such as 'compiler', 'test', or 'runtime'"
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: bounded Markdown summary by default, or structured JSON on request.",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": ["input"]
                    }
                },
                {
                    "name": "record_workflow_outcome",
                    "description": "Distill a successful or failed coding outcome into durable repo or branch memory for future agent workflows.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task": {
                                "type": "string",
                                "description": "Short task label such as 'fix login timeout'"
                            },
                            "status": {
                                "type": "string",
                                "description": "Outcome status",
                                "enum": ["success", "failure"],
                                "default": "success"
                            },
                            "summary": {
                                "type": "string",
                                "description": "Optional terse summary of what worked or failed"
                            },
                            "context_handle": {
                                "type": "string",
                                "description": "Optional workflow context handle to inherit files, symbols, tests, and task intent"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional source files changed or confirmed relevant",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols that proved relevant",
                                "items": { "type": "string" }
                            },
                            "tests": {
                                "type": "array",
                                "description": "Optional tests that verified the outcome",
                                "items": { "type": "string" }
                            },
                            "dry_run": {
                                "type": "boolean",
                                "description": "When true, compute the durable memory content, refresh key, identifiers, and scope without writing memory. Use for live MCP verification probes.",
                                "default": false
                            }
                        },
                        "required": ["task"]
                    }
                },
                {
                    "name": "expand_context",
                    "description": "Follow-up to a result with a context_handle, including get_context_capsule and the workflow tools. Expands one suggested file, symbol, test, or memory target without repeating the broad search.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "handle": {
                                "type": "string",
                                "description": "A context handle returned by a prior result such as get_context_capsule, prepare_change, plan_edit, trace_scenario, or get_working_set_context"
                            },
                            "focus": {
                                "type": "string",
                                "description": "Target to expand, such as symbol_id:{...}, file_id:src/auth.ts, symbol:loginUser, file:src/auth.ts, test:tests/auth.test.ts, or memory:0"
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Approximate maximum response size budget (default: 1200)",
                                "default": 1200
                            }
                        },
                        "required": ["handle", "focus"]
                    }
                },
                {
                    "name": "get_symbol",
                    "description": "Get detailed information about a specific symbol by name and file.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            },
                            "detail": {
                                "type": "string",
                                "description": "Detail level: 'summary' (default) or 'full' (includes source, end_line, is_exported, dep lists)",
                                "enum": ["summary", "full"],
                                "default": "summary"
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "get_dependents",
                    "description": "Get all symbols that depend on the given symbol (incoming edges).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "get_dependencies",
                    "description": "Get all symbols that the given symbol depends on (outgoing edges).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "get_impact_graph",
                    "description": "What breaks if a symbol changes — all transitive dependents up to N hops.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            },
                            "hops": {
                                "type": "integer",
                                "description": "Number of hops to traverse (default: 3)",
                                "default": 3
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "search_symbols",
                    "description": "Search for symbols by name pattern across the code graph.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "Substring to search for in symbol names (case-insensitive)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results (default: 20)",
                                "default": 20
                            },
                            "detail": {
                                "type": "string",
                                "description": "Detail level: 'summary' (default) or 'full' (adds kind, exported, signature)",
                                "enum": ["summary", "full"],
                                "default": "summary"
                            }
                        },
                        "required": ["pattern"]
                    }
                },
                {
                    "name": "get_skeleton",
                    "description": "Token-efficient file structure view — symbols, kinds, and dependent counts.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "File path to get context for"
                            }
                        },
                        "required": ["file"]
                    }
                },
                {
                    "name": "search_memory",
                    "description": "Search all sessions for memories matching a query.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Query to search memories with"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of memories to return (default: 10)",
                                "default": 10
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "search_logic_flow",
                    "description": "Execution paths between functions — finds call chains from one symbol to another.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "from": {
                                "type": "string",
                                "description": "Source symbol name"
                            },
                            "to": {
                                "type": "string",
                                "description": "Target symbol name"
                            },
                            "from_file": {
                                "type": "string",
                                "description": "Optional file path to disambiguate source symbol"
                            },
                            "to_file": {
                                "type": "string",
                                "description": "Optional file path to disambiguate target symbol"
                            },
                            "max_depth": {
                                "type": "integer",
                                "description": "Maximum path depth (default: 5)",
                                "default": 5
                            }
                        },
                        "required": ["from", "to"]
                    }
                },
                {
                    "name": "submit_lsp_edges",
                    "description": "Submit high-confidence edges from LSP call hierarchy to enrich the graph.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "edges": {
                                "type": "array",
                                "description": "Array of edges to add",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "from_name": { "type": "string" },
                                        "from_file": { "type": "string" },
                                        "to_name": { "type": "string" },
                                        "to_file": { "type": "string" },
                                        "kind": { "type": "string", "default": "Calls" }
                                    },
                                    "required": ["from_name", "from_file", "to_name", "to_file"]
                                }
                            }
                        },
                        "required": ["edges"]
                    }
                },
                {
                    "name": "workspace_setup",
                    "description": "Get workspace conventions, language breakdown, and recommended configuration.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "format": {
                                "type": "string",
                                "description": "Output format: 'json' or 'markdown' (default: 'markdown')",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "index_status",
                    "description": "Get current indexing status, graph stats, and language breakdown.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                },
                {
                    "name": "get_session_metrics",
                    "description": "Inspect live assistant-session workflow metrics such as tool-call counts, payload sizes, handle reuse, and automatic memory writes.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                },
                {
                    "name": "get_project_rules",
                    "description": "Get project-specific rules and conventions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                },
                working_memory_tool::tool_definition(),
                {
                    "name": "list_stale_memories",
                    "description": "List stale memories that likely need review or refresh.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Optional keyword filter applied to memory content, linked symbols, and linked files"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results to return (default: 50, max: 200)",
                                "default": 50
                            }
                        },
                        "required": []
                    }
                }
            ]
        });
        if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
            tools.extend([
                memory_v2::consolidate_session::tool_definition(),
                memory_v2::get_memory_metrics::tool_definition(),
                memory_v2::get_event_trace::tool_definition(),
                memory_v2::get_task_memory::tool_definition(),
                memory_v2::save_quick_memory::tool_definition(),
                memory_v2::save_memory::tool_definition(),
                memory_v2::propose_memory_evolution::tool_definition(),
                memory_v2::verify_explain_memory::tool_definition(),
                memory_v2::list_memory_conflicts::tool_definition(),
            ]);
        }
        result
    }

    async fn handle_tools_call(&self, params: &Value) -> Result<Value, (i32, String)> {
        let tool_name = params["name"]
            .as_str()
            .ok_or((-32602, "Missing tool name".to_string()))?;
        let arguments = &params["arguments"];
        let span = tracing::info_span!("tool", name = tool_name);
        let tool_called_event = self.capture_tool_called(tool_name, arguments);
        let started = Instant::now();

        let result = async {
            match tool_name {
                "get_context_capsule" => self.tool_query_context(arguments).await,
                "prepare_change" => self.tool_prepare_change(arguments).await,
                "plan_edit" => self.tool_plan_edit(arguments).await,
                "trace_scenario" => self.tool_trace_scenario(arguments).await,
                "find_relevant_tests" => self.tool_find_relevant_tests(arguments).await,
                "impact_from_diff" => self.tool_impact_from_diff(arguments).await,
                "get_working_set_context" => self.tool_get_working_set_context(arguments).await,
                "summarize_subsystem" => self.tool_summarize_subsystem(arguments).await,
                "get_repo_playbook" => self.tool_get_repo_playbook(arguments).await,
                "get_docs_capsule" => self.tool_get_docs_capsule(arguments).await,
                "get_backlinks" => self.tool_get_backlinks(arguments).await,
                "get_outgoing_links" => self.tool_get_outgoing_links(arguments).await,
                "find_stale_docs" => self.tool_find_stale_docs(arguments).await,
                "diagnose_failure" => self.tool_diagnose_failure(arguments).await,
                "record_workflow_outcome" => self.tool_record_workflow_outcome(arguments).await,
                "expand_context" => self.tool_expand_context(arguments).await,
                "get_symbol" => self.tool_get_symbol(arguments).await,
                "get_dependents" => self.tool_get_dependents(arguments).await,
                "get_dependencies" => self.tool_get_dependencies(arguments).await,
                "get_impact_graph" => self.tool_blast_radius(arguments).await,
                "search_symbols" => self.tool_search_symbols(arguments).await,
                "get_skeleton" => self.tool_get_file_context(arguments).await,
                "search_memory" => self.tool_search_memory(arguments).await,
                "list_stale_memories" => self.tool_list_stale_memories(arguments).await,
                "search_logic_flow" => self.tool_search_logic_flow(arguments).await,
                "submit_lsp_edges" => self.tool_submit_lsp_edges(arguments).await,
                "workspace_setup" => self.tool_workspace_setup(arguments).await,
                "index_status" => self.tool_index_status(arguments).await,
                "get_session_metrics" => self.tool_get_session_metrics(arguments).await,
                "get_project_rules" => self.tool_get_project_rules(arguments).await,
                "inspect_working_memory" => self.tool_inspect_working_memory(arguments).await,
                "consolidate_session" => self.tool_consolidate_session_v2(arguments).await,
                "get_memory_metrics" => self.tool_get_memory_metrics_v2(arguments).await,
                "get_event_trace" => self.tool_get_event_trace_v2(arguments).await,
                "get_task_memory" => self.tool_get_task_memory_v2(arguments).await,
                "save_quick_memory" => self.tool_save_quick_memory_v2(arguments).await,
                "save_memory" => self.tool_save_memory_v2(arguments).await,
                "propose_memory_evolution" => {
                    self.tool_propose_memory_evolution_v2(arguments).await
                }
                "verify_explain_memory" => self.tool_verify_explain_memory(arguments).await,
                "list_memory_conflicts" => self.tool_list_memory_conflicts(arguments).await,
                _ => Err((-32602, format!("Unknown tool: {}", tool_name))),
            }
        }
        .instrument(span)
        .await;

        if let Ok(ref value) = result {
            if tool_name != "get_session_metrics" {
                self.record_tool_metrics(tool_name, value).await;
            }
        }
        if tool_name != "get_session_metrics" {
            self.record_adoption_tool_call(
                tool_name,
                arguments,
                result.as_ref().ok(),
                started.elapsed(),
            )
                .await;
        }
        self.capture_tool_result(tool_name, arguments, &result, tool_called_event)
            .await;

        result
    }

    fn capture_tool_called(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Option<lattice_core::identity::EventId> {
        let capture = self.event_capture.as_ref()?;
        if let Err(error) = capture.ensure_task_started("MCP tool-call session") {
            tracing::warn!(tool = tool_name, %error, "failed to capture task start event");
        }
        match capture.record_tool_called(tool_name, arguments) {
            Ok(event_id) => Some(event_id),
            Err(error) => {
                tracing::warn!(tool = tool_name, %error, "failed to capture tool call event");
                None
            }
        }
    }

    async fn capture_tool_result(
        &self,
        tool_name: &str,
        arguments: &Value,
        result: &Result<Value, (i32, String)>,
        tool_called_event: Option<lattice_core::identity::EventId>,
    ) {
        let Some(capture) = self.event_capture.as_ref() else {
            return;
        };
        let Some(parent) = tool_called_event else {
            return;
        };
        let outcome = match result {
            Ok(value) => ToolOutcome::Success(value.clone()),
            Err((code, message)) => ToolOutcome::Error {
                code: *code,
                message: message.clone(),
            },
        };
        let tool_result = match capture.record_tool_result(tool_name, &outcome, parent) {
            Ok(event_id) => event_id,
            Err(error) => {
                tracing::warn!(tool = tool_name, %error, "failed to capture tool result event");
                return;
            }
        };
        if !WorkflowOutcomeRecorder::should_record(tool_name) {
            if let Err(error) = capture.record_workflow_events(tool_name, &outcome, tool_result) {
                tracing::warn!(tool = tool_name, %error, "failed to capture workflow outcome events");
            }
            return;
        }

        let mut metrics = self.session_metrics.lock().await;
        if let Err(error) = self.workflow_outcome_recorder.record(
            capture,
            &mut metrics,
            tool_name,
            arguments,
            result,
            &tool_result,
        ) {
            tracing::warn!(tool = tool_name, %error, "failed to capture workflow outcome events");
        }
    }

    // ── Tool Implementations ──────────────────────────────────────────

    async fn tool_query_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let render_choice = WorkflowRenderChoice::from_mode_str(args["mode"].as_str());
        let response_options = parse_workflow_response_options(args)?;
        if let Some(response) = self
            .workflow_repo_state_placeholder("get_context_capsule", query, response_options.render)
            .await
        {
            return Ok(response);
        }

        let mut engine = match self.query_engine_snapshot_for_workflow().await {
            Ok(engine) => engine,
            Err(error) => {
                return self.query_job_error_response(
                    "get_context_capsule",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let query_owned = query.to_string();
        let workspace_root = self.workspace_root.to_string_lossy().to_string();
        let (mut bundle, seed) = match self
            .run_query_job(move || {
                let capsule = engine.query(
                    &query_owned,
                    None,
                    matches!(render_choice, WorkflowRenderChoice::Focused),
                );
                let request = workflow_v2::WorkflowRequest {
                    input: query_owned,
                    entry_files: Vec::new(),
                    entry_symbols: Vec::new(),
                    render_mode: format!("{:?}", render_choice).to_lowercase(),
                };
                let bundle = workflow_v2::context_capsule::build_bundle(
                    engine.graph(),
                    &workspace_root,
                    &request,
                    &capsule,
                    render_choice,
                );
                let seed = workflow_v2::build_expand_seed(&bundle);
                (bundle, seed)
            })
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return self.query_job_error_response(
                    "get_context_capsule",
                    query,
                    response_options.render,
                    error,
                )
            }
        };

        let handle = self.store_context_handle("get_context_capsule", seed).await;
        self.enrich_workflow_bundle_relevance(
            "get_context_capsule",
            &handle.legacy_handle,
            None,
            &mut bundle,
        )
        .await;
        let mut value = serde_json::to_value(&bundle)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, &handle.legacy_handle, "get_context_capsule");

        let metadata = WorkflowRunMetadata {
            delivery_mode: format!("{:?}", render_choice).to_lowercase(),
            wire_format: "standard".to_string(),
            single_anchor_used: false,
            _mode_reason: "context capsule defaults to bounded first-pass retrieval".to_string(),
            semantic_fallback_used: false,
            outcome_memory_reuse_count: 0,
        };

        self.finalize_workflow_value("get_context_capsule", value, &metadata, &response_options)
            .await
    }

    async fn tool_prepare_change(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args)?;
        let entry_files = merge_unique_strings(
            merge_unique_strings(
                parse_string_array(args, "entry_files"),
                extract_workspace_file_references(query, &self.workspace_root),
            ),
            self.default_focus_files.clone(),
        );
        let entry_symbols = parse_string_array(args, "entry_symbols");
        if let Some(response) = self
            .workflow_repo_state_placeholder("prepare_change", query, response_options.render)
            .await
        {
            return Ok(response);
        }

        let engine = match self.query_engine_snapshot_for_workflow().await {
            Ok(engine) => engine,
            Err(error) => {
                return self.query_job_error_response(
                    "prepare_change",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let query_owned = query.to_string();
        let mut query_engine = engine.clone();
        let (mut capsule, project_rules) = match self
            .run_query_job(move || {
                let project_rules = detect_project_rules(query_engine.graph());
                let capsule = query_engine.query(&query_owned, None, false);
                (capsule, project_rules)
            })
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return self.query_job_error_response(
                    "prepare_change",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let mut semantic_fallback_used = false;
        if !self.is_indexing()
            && should_try_prepare_change_semantic_fallback(&capsule, &entry_files, &entry_symbols)
        {
            if let Some(embedding) = self.embed_query_for_fallback(query) {
                let mut semantic_engine = engine.clone();
                let query_owned = query.to_string();
                let semantic_capsule = match self
                    .run_query_job(move || {
                        semantic_engine.query(&query_owned, Some(embedding.as_slice()), false)
                    })
                    .await
                {
                    Ok(capsule) => capsule,
                    Err(error) => {
                        return self.query_job_error_response(
                            "prepare_change",
                            query,
                            response_options.render,
                            error,
                        )
                    }
                };
                if prepare_change_capsule_quality(&semantic_capsule, &entry_files, &entry_symbols)
                    > prepare_change_capsule_quality(&capsule, &entry_files, &entry_symbols)
                {
                    capsule = semantic_capsule;
                    semantic_fallback_used = true;
                }
            }
        }

        capsule.memories = self
            .augment_memory_values_with_playbooks(
                query,
                &entry_files,
                &entry_symbols,
                capsule.memories,
                5,
            )
            .await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&capsule.memories);
        let entry_files_for_bundle = entry_files.clone();
        let entry_symbols_for_bundle = entry_symbols.clone();
        let capsule_for_bundle = capsule.clone();
        let (bundle, delivery_mode, mode_reason) = match self
            .run_query_job(move || {
                let compact_bundle = prepare_change(
                    engine.graph(),
                    &capsule_for_bundle,
                    &entry_files_for_bundle,
                    &entry_symbols_for_bundle,
                    &project_rules,
                    BundleMode::Compact,
                );
                let (delivery_mode, mode_reason) =
                    select_task_bundle_mode(requested_mode, &compact_bundle);
                let bundle = if matches!(delivery_mode, BundleMode::Full) {
                    prepare_change(
                        engine.graph(),
                        &capsule_for_bundle,
                        &entry_files_for_bundle,
                        &entry_symbols_for_bundle,
                        &project_rules,
                        BundleMode::Full,
                    )
                } else {
                    compact_bundle
                };
                (bundle, delivery_mode, mode_reason)
            })
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return self.query_job_error_response(
                    "prepare_change",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let metadata = WorkflowRunMetadata {
            delivery_mode: delivery_mode.as_str().to_string(),
            wire_format: "standard".to_string(),
            single_anchor_used: false,
            _mode_reason: mode_reason,
            semantic_fallback_used,
            outcome_memory_reuse_count,
        };
        let handle = self
            .store_context_handle("prepare_change", seed_from_task_bundle(&bundle))
            .await;
        let request = WorkflowRequest {
            input: query.to_string(),
            entry_files: entry_files.clone(),
            entry_symbols: entry_symbols.clone(),
            render_mode: metadata.delivery_mode.clone(),
        };
        let mut sink = VecEventSink::default();
        let mut workflow_bundle = workflow_v2::prepare_change::run(
            &self.workspace_root.to_string_lossy(),
            &request,
            &bundle,
            &capsule,
            &mut sink,
        );
        self.enrich_workflow_bundle_relevance(
            "prepare_change",
            &handle.legacy_handle,
            None,
            &mut workflow_bundle,
        )
        .await;

        self.serialize_workflow_with_context_handle(
            "prepare_change",
            workflow_bundle,
            &handle.legacy_handle,
            "prepare_change",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_plan_edit(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args)?;
        let entry_files = merge_unique_strings(
            parse_string_array(args, "entry_files"),
            self.default_focus_files.clone(),
        );
        let entry_symbols = parse_string_array(args, "entry_symbols");
        if let Some(response) = self
            .workflow_repo_state_placeholder("plan_edit", query, response_options.render)
            .await
        {
            return Ok(response);
        }

        let engine = match self.query_engine_snapshot_for_workflow().await {
            Ok(engine) => engine,
            Err(error) => {
                return self.query_job_error_response(
                    "plan_edit",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let query_owned = query.to_string();
        let mut query_engine = engine.clone();
        let (mut capsule, project_rules) = match self
            .run_query_job(move || {
                let project_rules = detect_project_rules(query_engine.graph());
                let capsule = query_engine.query(&query_owned, None, false);
                (capsule, project_rules)
            })
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return self.query_job_error_response(
                    "plan_edit",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let mut semantic_fallback_used = false;
        if !self.is_indexing()
            && should_try_prepare_change_semantic_fallback(&capsule, &entry_files, &entry_symbols)
        {
            if let Some(embedding) = self.embed_query_for_fallback(query) {
                let mut semantic_engine = engine.clone();
                let query_owned = query.to_string();
                let semantic_capsule = match self
                    .run_query_job(move || {
                        semantic_engine.query(&query_owned, Some(embedding.as_slice()), false)
                    })
                    .await
                {
                    Ok(capsule) => capsule,
                    Err(error) => {
                        return self.query_job_error_response(
                            "plan_edit",
                            query,
                            response_options.render,
                            error,
                        )
                    }
                };
                if prepare_change_capsule_quality(&semantic_capsule, &entry_files, &entry_symbols)
                    > prepare_change_capsule_quality(&capsule, &entry_files, &entry_symbols)
                {
                    capsule = semantic_capsule;
                    semantic_fallback_used = true;
                }
            }
        }

        capsule.memories = self
            .augment_memory_values_with_playbooks(
                query,
                &entry_files,
                &entry_symbols,
                capsule.memories,
                5,
            )
            .await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&capsule.memories);

        let entry_files_for_bundle = entry_files.clone();
        let entry_symbols_for_bundle = entry_symbols.clone();
        let capsule_for_bundle = capsule.clone();
        let (bundle, delivery_mode, mode_reason) = match self
            .run_query_job(move || {
                let compact_bundle = plan_edit(
                    engine.graph(),
                    &capsule_for_bundle,
                    &entry_files_for_bundle,
                    &entry_symbols_for_bundle,
                    &project_rules,
                    BundleMode::Compact,
                );
                let (delivery_mode, mode_reason) =
                    select_plan_edit_mode(requested_mode, &compact_bundle);
                let bundle = if matches!(delivery_mode, BundleMode::Full) {
                    plan_edit(
                        engine.graph(),
                        &capsule_for_bundle,
                        &entry_files_for_bundle,
                        &entry_symbols_for_bundle,
                        &project_rules,
                        BundleMode::Full,
                    )
                } else {
                    compact_bundle
                };

                (bundle, delivery_mode, mode_reason)
            })
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return self.query_job_error_response(
                    "plan_edit",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let metadata = WorkflowRunMetadata {
            delivery_mode: delivery_mode.as_str().to_string(),
            wire_format: "standard".to_string(),
            single_anchor_used: false,
            _mode_reason: mode_reason,
            semantic_fallback_used,
            outcome_memory_reuse_count,
        };
        let handle = self
            .store_context_handle("plan_edit", seed_from_plan_edit_bundle(&bundle))
            .await;
        let request = WorkflowRequest {
            input: query.to_string(),
            entry_files: entry_files.clone(),
            entry_symbols: entry_symbols.clone(),
            render_mode: metadata.delivery_mode.clone(),
        };
        let mut sink = VecEventSink::default();
        let mut workflow_bundle = workflow_v2::plan_edit::run(
            &self.workspace_root.to_string_lossy(),
            &request,
            &bundle,
            &capsule,
            &mut sink,
        );
        self.enrich_workflow_bundle_relevance(
            "plan_edit",
            &handle.legacy_handle,
            None,
            &mut workflow_bundle,
        )
        .await;

        self.serialize_workflow_with_context_handle(
            "plan_edit",
            workflow_bundle,
            &handle.legacy_handle,
            "plan_edit",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_trace_scenario(&self, args: &Value) -> Result<Value, (i32, String)> {
        let scenario = args["scenario"]
            .as_str()
            .or_else(|| args["query"].as_str())
            .ok_or((-32602, "Missing required parameter: scenario".to_string()))?;
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args)?;
        let entry_files = parse_string_array(args, "entry_files");
        let entry_symbols = parse_string_array(args, "entry_symbols");
        if let Some(response) = self
            .workflow_repo_state_placeholder("trace_scenario", scenario, response_options.render)
            .await
        {
            return Ok(response);
        }

        let (bundle, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_bundle = trace_scenario(
                engine.graph(),
                scenario,
                &entry_files,
                &entry_symbols,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_trace_scenario_mode(requested_mode, &compact_bundle);
            let bundle = if matches!(delivery_mode, BundleMode::Full) {
                trace_scenario(
                    engine.graph(),
                    scenario,
                    &entry_files,
                    &entry_symbols,
                    &project_rules,
                    BundleMode::Full,
                )
            } else {
                compact_bundle
            };

            (
                bundle,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count: 0,
                },
            )
        };

        let handle = self
            .store_context_handle("trace_scenario", seed_from_trace_scenario_bundle(&bundle))
            .await;
        let request = WorkflowRequest {
            input: scenario.to_string(),
            entry_files: entry_files.clone(),
            entry_symbols: entry_symbols.clone(),
            render_mode: metadata.delivery_mode.clone(),
        };
        let mut sink = VecEventSink::default();
        let mut workflow_bundle = workflow_v2::trace_scenario::run(
            &self.workspace_root.to_string_lossy(),
            &request,
            &bundle,
            &[],
            &mut sink,
        );
        self.enrich_workflow_bundle_relevance(
            "trace_scenario",
            &handle.legacy_handle,
            None,
            &mut workflow_bundle,
        )
        .await;

        self.serialize_workflow_with_context_handle(
            "trace_scenario",
            workflow_bundle,
            &handle.legacy_handle,
            "trace_scenario",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_find_relevant_tests(&self, args: &Value) -> Result<Value, (i32, String)> {
        let files = merge_unique_strings(
            parse_string_array(args, "files"),
            self.default_focus_files.clone(),
        );
        let symbols = parse_string_array(args, "symbols");
        let diff = args["diff"].as_str();
        let limit = (args["limit"].as_u64().unwrap_or(8) as usize).min(50);
        let render_choice = WorkflowRenderChoice::from_mode_str(args["mode"].as_str());
        let response_options = parse_workflow_response_options(args)?;
        if let Some(response) = self
            .workflow_repo_state_placeholder(
                "find_relevant_tests",
                diff.unwrap_or("relevant tests"),
                response_options.render,
            )
            .await
        {
            return Ok(response);
        }

        let mut bundle = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let report = find_relevant_tests(
                engine.graph(),
                &files,
                &symbols,
                diff,
                &project_rules,
                limit,
            );
            let request = workflow_v2::WorkflowRequest {
                input: diff.unwrap_or("relevant tests").to_string(),
                entry_files: files.clone(),
                entry_symbols: symbols.clone(),
                render_mode: format!("{:?}", render_choice).to_lowercase(),
            };
            workflow_v2::relevant_tests::build_bundle(
                &self.workspace_root.to_string_lossy(),
                &request,
                &report,
                render_choice,
            )
        };
        let handle = self
            .store_context_handle(
                "find_relevant_tests",
                workflow_v2::build_expand_seed(&bundle),
            )
            .await;
        self.enrich_workflow_bundle_relevance(
            "find_relevant_tests",
            &handle.legacy_handle,
            None,
            &mut bundle,
        )
        .await;

        self.serialize_workflow_with_context_handle(
            "find_relevant_tests",
            bundle,
            &handle.legacy_handle,
            "find_relevant_tests",
            &WorkflowRunMetadata {
                delivery_mode: format!("{:?}", render_choice).to_lowercase(),
                wire_format: "standard".to_string(),
                single_anchor_used: false,
                _mode_reason: "ranked tests default to compact verification guidance".to_string(),
                semantic_fallback_used: false,
                outcome_memory_reuse_count: 0,
            },
            &response_options,
        )
        .await
    }

    async fn tool_impact_from_diff(&self, args: &Value) -> Result<Value, (i32, String)> {
        let diff = args["diff"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: diff".to_string()))?;
        let files = merge_unique_strings(
            parse_string_array(args, "files"),
            self.default_focus_files.clone(),
        );
        let symbols = parse_string_array(args, "symbols");
        let render_choice = WorkflowRenderChoice::from_mode_str(args["mode"].as_str());
        let response_options = parse_workflow_response_options(args)?;
        let hops = (args["hops"].as_u64().unwrap_or(2) as usize).min(5);
        if let Some(response) = self
            .workflow_repo_state_placeholder("impact_from_diff", diff, response_options.render)
            .await
        {
            return Ok(response);
        }

        let (bundle, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let report = impact_from_diff(
                engine.graph(),
                diff,
                &files,
                &symbols,
                &project_rules,
                if matches!(
                    render_choice,
                    WorkflowRenderChoice::Full | WorkflowRenderChoice::Diagnostic
                ) {
                    BundleMode::Full
                } else {
                    BundleMode::Compact
                },
                hops,
            );
            let request = workflow_v2::WorkflowRequest {
                input: diff.to_string(),
                entry_files: files.clone(),
                entry_symbols: symbols.clone(),
                render_mode: format!("{:?}", render_choice).to_lowercase(),
            };
            let bundle = workflow_v2::impact_from_diff::build_bundle(
                engine.graph(),
                &self.workspace_root.to_string_lossy(),
                &request,
                &report,
                render_choice,
            );

            (
                bundle,
                WorkflowRunMetadata {
                    delivery_mode: format!("{:?}", render_choice).to_lowercase(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: "bounded diff impact traversal".to_string(),
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count: 0,
                },
            )
        };
        let handle = self
            .store_context_handle("impact_from_diff", workflow_v2::build_expand_seed(&bundle))
            .await;
        let mut bundle = bundle;
        self.enrich_workflow_bundle_relevance(
            "impact_from_diff",
            &handle.legacy_handle,
            None,
            &mut bundle,
        )
        .await;

        self.serialize_workflow_with_context_handle(
            "impact_from_diff",
            bundle,
            &handle.legacy_handle,
            "impact_from_diff",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_get_working_set_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"].as_str();
        let files = merge_unique_strings(
            parse_string_array(args, "files"),
            self.default_focus_files.clone(),
        );
        let symbols = parse_string_array(args, "symbols");
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args)?;
        let memory_limit = if matches!(requested_mode, RequestedBundleMode::Full) {
            8
        } else {
            5
        };
        if let Some(response) = self
            .workflow_repo_state_placeholder(
                "get_working_set_context",
                query.unwrap_or("working set"),
                response_options.render,
            )
            .await
        {
            return Ok(response);
        }
        let memory_query = build_memory_query(query, &files, &symbols);

        let memories = self
            .augment_memory_values_with_playbooks(
                query.unwrap_or(memory_query.as_deref().unwrap_or("working set")),
                &files,
                &symbols,
                self.load_relevant_memory_values(query, &files, &symbols, memory_limit)
                    .await?,
                memory_limit,
            )
            .await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        let (report, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_report = get_working_set_context(
                engine.graph(),
                &files,
                &symbols,
                query,
                &memories,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_working_set_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                get_working_set_context(
                    engine.graph(),
                    &files,
                    &symbols,
                    query,
                    &memories,
                    &project_rules,
                    BundleMode::Full,
                )
            } else {
                compact_report
            };

            (
                report,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count,
                },
            )
        };
        let handle = self
            .store_context_handle(
                "get_working_set_context",
                seed_from_working_set_context(&report),
            )
            .await;

        self.serialize_workflow_with_context_handle(
            "get_working_set_context",
            report,
            &handle.legacy_handle,
            "get_working_set_context",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_summarize_subsystem(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let mut files = parse_string_array(args, "files");
        let mut symbols = parse_string_array(args, "symbols");
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args)?;
        let memory_limit = if matches!(requested_mode, RequestedBundleMode::Full) {
            6
        } else {
            4
        };
        if let Some(response) = self
            .workflow_repo_state_placeholder("summarize_subsystem", query, response_options.render)
            .await
        {
            return Ok(response);
        }
        let mut memories = self
            .augment_memory_values_with_playbooks(
                query,
                &files,
                &symbols,
                self.load_relevant_memory_values(Some(query), &files, &symbols, memory_limit)
                    .await?,
                memory_limit,
            )
            .await?;

        let engine = match self.query_engine_snapshot_for_workflow().await {
            Ok(engine) => engine,
            Err(error) => {
                return self.query_job_error_response(
                    "summarize_subsystem",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let compact_engine = engine.clone();
        let compact_query = query.to_string();
        let compact_files = files.clone();
        let compact_symbols = symbols.clone();
        let compact_memories = memories.clone();
        let mut compact_report = match self
            .run_query_job(move || {
                let project_rules = detect_project_rules(compact_engine.graph());
                summarize_subsystem(
                    compact_engine.graph(),
                    &compact_query,
                    &compact_files,
                    &compact_symbols,
                    &compact_memories,
                    &project_rules,
                    BundleMode::Compact,
                )
            })
            .await
        {
            Ok(report) => report,
            Err(error) => {
                return self.query_job_error_response(
                    "summarize_subsystem",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let mut semantic_fallback_used = false;

        if !self.is_indexing()
            && should_try_subsystem_semantic_fallback(&compact_report, &files, &symbols)
        {
            if let Some(embedding) = self.embed_query_for_fallback(query) {
                let mut semantic_engine = engine.clone();
                let semantic_query = query.to_string();
                let semantic_capsule = match self
                    .run_query_job(move || {
                        semantic_engine.query(&semantic_query, Some(embedding.as_slice()), false)
                    })
                    .await
                {
                    Ok(capsule) => capsule,
                    Err(error) => {
                        return self.query_job_error_response(
                            "summarize_subsystem",
                            query,
                            response_options.render,
                            error,
                        )
                    }
                };
                let candidate_files = merge_anchor_files_from_capsule(&files, &semantic_capsule);
                let candidate_symbols =
                    merge_anchor_symbols_from_capsule(&symbols, &semantic_capsule);

                if candidate_files != files || candidate_symbols != symbols {
                    let candidate_memories = self
                        .augment_memory_values_with_playbooks(
                            query,
                            &candidate_files,
                            &candidate_symbols,
                            self.load_relevant_memory_values(
                                Some(query),
                                &candidate_files,
                                &candidate_symbols,
                                memory_limit,
                            )
                            .await?,
                            memory_limit,
                        )
                        .await?;
                    let candidate_engine = engine.clone();
                    let candidate_query = query.to_string();
                    let candidate_files_for_report = candidate_files.clone();
                    let candidate_symbols_for_report = candidate_symbols.clone();
                    let candidate_memories_for_report = candidate_memories.clone();
                    let candidate_report = match self
                        .run_query_job(move || {
                            let project_rules = detect_project_rules(candidate_engine.graph());
                            summarize_subsystem(
                                candidate_engine.graph(),
                                &candidate_query,
                                &candidate_files_for_report,
                                &candidate_symbols_for_report,
                                &candidate_memories_for_report,
                                &project_rules,
                                BundleMode::Compact,
                            )
                        })
                        .await
                    {
                        Ok(report) => report,
                        Err(error) => {
                            return self.query_job_error_response(
                                "summarize_subsystem",
                                query,
                                response_options.render,
                                error,
                            )
                        }
                    };

                    if subsystem_summary_quality(&candidate_report)
                        > subsystem_summary_quality(&compact_report)
                    {
                        files = candidate_files;
                        symbols = candidate_symbols;
                        memories = candidate_memories;
                        compact_report = candidate_report;
                        semantic_fallback_used = true;
                    }
                }
            }
        }

        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        let final_query = query.to_string();
        let final_files = files.clone();
        let final_symbols = symbols.clone();
        let final_memories = memories.clone();
        let (report, delivery_mode, mode_reason) = match self
            .run_query_job(move || {
                let project_rules = detect_project_rules(engine.graph());
                let (delivery_mode, mode_reason) =
                    select_subsystem_summary_mode(requested_mode, &compact_report);
                let report = if matches!(delivery_mode, BundleMode::Full) {
                    summarize_subsystem(
                        engine.graph(),
                        &final_query,
                        &final_files,
                        &final_symbols,
                        &final_memories,
                        &project_rules,
                        BundleMode::Full,
                    )
                } else {
                    compact_report
                };

                (report, delivery_mode, mode_reason)
            })
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return self.query_job_error_response(
                    "summarize_subsystem",
                    query,
                    response_options.render,
                    error,
                )
            }
        };
        let metadata = WorkflowRunMetadata {
            delivery_mode: delivery_mode.as_str().to_string(),
            wire_format: "standard".to_string(),
            single_anchor_used: false,
            _mode_reason: mode_reason,
            semantic_fallback_used,
            outcome_memory_reuse_count,
        };
        let handle = self
            .store_context_handle("summarize_subsystem", seed_from_subsystem_summary(&report))
            .await;

        let playbook_memory = self
            .auto_upsert_playbook_memory(
                format!(
                    "subsystem_playbook::{}",
                    stable_refresh_key(query, &files, &symbols)
                ),
                summarize_subsystem_memory_content(&report),
                report
                    .key_files
                    .iter()
                    .map(|item| item.file.clone())
                    .collect(),
                report
                    .key_symbols
                    .iter()
                    .map(|item| item.symbol.clone())
                    .collect(),
                Some(query.to_string()),
                true,
            )
            .await?;

        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, &handle.legacy_handle, "summarize_subsystem");
        attach_playbook_memory(&mut value, playbook_memory);
        self.finalize_workflow_value("summarize_subsystem", value, &metadata, &response_options)
            .await
    }

    async fn tool_get_repo_playbook(&self, args: &Value) -> Result<Value, (i32, String)> {
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args)?;
        let memory_limit = if matches!(requested_mode, RequestedBundleMode::Full) {
            8
        } else {
            5
        };
        if let Some(response) = self
            .workflow_repo_state_placeholder(
                "get_repo_playbook",
                "repo playbook",
                response_options.render,
            )
            .await
        {
            return Ok(response);
        }
        let memories = self.load_durable_memory_values(memory_limit).await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        let (report, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_report = get_repo_playbook(
                engine.graph(),
                &memories,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_repo_playbook_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                get_repo_playbook(engine.graph(), &memories, &project_rules, BundleMode::Full)
            } else {
                compact_report
            };

            (
                report,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count,
                },
            )
        };
        let handle = self
            .store_context_handle("get_repo_playbook", seed_from_repo_playbook(&report))
            .await;

        let playbook_memory = self
            .auto_upsert_playbook_memory(
                "repo_playbook".to_string(),
                summarize_repo_playbook_memory_content(&report),
                report
                    .key_files
                    .iter()
                    .map(|item| item.file.clone())
                    .collect(),
                report
                    .notable_symbols
                    .iter()
                    .map(|item| item.symbol.clone())
                    .collect(),
                Some("repo playbook".to_string()),
                false,
            )
            .await?;

        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, &handle.legacy_handle, "get_repo_playbook");
        attach_playbook_memory(&mut value, playbook_memory);
        self.finalize_workflow_value("get_repo_playbook", value, &metadata, &response_options)
            .await
    }

    async fn tool_get_docs_capsule(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let files = parse_string_array(args, "files");
        let symbols = parse_string_array(args, "symbols");
        let limit = (args["limit"].as_u64().unwrap_or(6) as usize).clamp(1, 20);
        let render_choice = WorkflowRenderChoice::from_mode_str(args["mode"].as_str());
        let response_options = parse_workflow_response_options(args)?;
        if let Some(response) = self
            .workflow_repo_state_placeholder("get_docs_capsule", query, response_options.render)
            .await
        {
            return Ok(response);
        }

        let mut bundle = {
            let engine = self.engine.lock().await;
            let report = get_docs_capsule(engine.graph(), query, &files, &symbols, limit);
            let request = workflow_v2::WorkflowRequest {
                input: query.to_string(),
                entry_files: files.clone(),
                entry_symbols: symbols.clone(),
                render_mode: format!("{:?}", render_choice).to_lowercase(),
            };
            workflow_v2::docs_capsule::build_bundle(
                &self.workspace_root.to_string_lossy(),
                &request,
                &report,
                render_choice,
            )
        };
        let handle = self
            .store_context_handle("get_docs_capsule", workflow_v2::build_expand_seed(&bundle))
            .await;
        self.enrich_workflow_bundle_relevance(
            "get_docs_capsule",
            &handle.legacy_handle,
            None,
            &mut bundle,
        )
        .await;

        self.serialize_workflow_with_context_handle(
            "get_docs_capsule",
            bundle,
            &handle.legacy_handle,
            "get_docs_capsule",
            &WorkflowRunMetadata {
                delivery_mode: format!("{:?}", render_choice).to_lowercase(),
                wire_format: "standard".to_string(),
                single_anchor_used: false,
                _mode_reason: "docs capsules prefer authoritative markdown pivots".to_string(),
                semantic_fallback_used: false,
                outcome_memory_reuse_count: 0,
            },
            &response_options,
        )
        .await
    }

    async fn tool_get_backlinks(&self, args: &Value) -> Result<Value, (i32, String)> {
        let target = args["target"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: target".to_string()))?;
        let kind = DocsTargetKind::from_str(args["kind"].as_str());
        let limit = (args["limit"].as_u64().unwrap_or(12) as usize).clamp(1, 50);

        let engine = self.engine.lock().await;
        let report = get_backlinks(engine.graph(), target, kind, limit).ok_or((
            -32602,
            format!("Unable to resolve backlinks target: {}", target),
        ))?;
        let value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_get_outgoing_links(&self, args: &Value) -> Result<Value, (i32, String)> {
        let target = args["target"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: target".to_string()))?;
        let kind = DocsTargetKind::from_str(args["kind"].as_str());
        let limit = (args["limit"].as_u64().unwrap_or(12) as usize).clamp(1, 50);

        let engine = self.engine.lock().await;
        let report = get_outgoing_links(engine.graph(), target, kind, limit).ok_or((
            -32602,
            format!("Unable to resolve outgoing-links target: {}", target),
        ))?;
        let value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_find_stale_docs(&self, args: &Value) -> Result<Value, (i32, String)> {
        let files = parse_string_array(args, "files");
        let symbols = parse_string_array(args, "symbols");
        let limit = (args["limit"].as_u64().unwrap_or(12) as usize).clamp(1, 50);

        let engine = self.engine.lock().await;
        let report = find_stale_docs(engine.graph(), &files, &symbols, limit);
        let value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_diagnose_failure(&self, args: &Value) -> Result<Value, (i32, String)> {
        let input = args["input"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: input".to_string()))?;
        let kind = args["kind"].as_str();
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args)?;
        if let Some(response) = self
            .workflow_repo_state_placeholder("diagnose_failure", input, response_options.render)
            .await
        {
            return Ok(response);
        }

        let (mut report, metadata_mode_reason) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_report = diagnose_failure(
                engine.graph(),
                input,
                kind,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_failure_diagnosis_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                diagnose_failure(
                    engine.graph(),
                    input,
                    kind,
                    &project_rules,
                    BundleMode::Full,
                )
            } else {
                compact_report
            };
            (report, (delivery_mode, mode_reason))
        };

        let memories = self
            .augment_memory_values_with_playbooks(
                input,
                &report.extracted_files,
                &report.extracted_symbols,
                self.load_relevant_memory_values(
                    Some(input),
                    &report.extracted_files,
                    &report.extracted_symbols,
                    4,
                )
                .await?,
                4,
            )
            .await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        report.memory_highlights = report_memory_highlights(&memories, 1);
        report.overview =
            build_failure_overview_value(&report.overview, report.memory_highlights.first());
        let metadata = WorkflowRunMetadata {
            delivery_mode: metadata_mode_reason.0.as_str().to_string(),
            wire_format: "standard".to_string(),
            single_anchor_used: false,
            _mode_reason: metadata_mode_reason.1,
            semantic_fallback_used: false,
            outcome_memory_reuse_count,
        };
        let handle = self
            .store_context_handle("diagnose_failure", seed_from_failure_diagnosis(&report))
            .await;
        let request = WorkflowRequest {
            input: input.to_string(),
            entry_files: report.extracted_files.clone(),
            entry_symbols: report.extracted_symbols.clone(),
            render_mode: metadata.delivery_mode.clone(),
        };
        let mut sink = VecEventSink::default();
        let mut workflow_bundle = workflow_v2::diagnose_failure::run(
            &self.workspace_root.to_string_lossy(),
            &request,
            &report,
            &memories,
            &mut sink,
        );
        self.enrich_workflow_bundle_relevance(
            "diagnose_failure",
            &handle.legacy_handle,
            None,
            &mut workflow_bundle,
        )
        .await;

        self.serialize_workflow_with_context_handle(
            "diagnose_failure",
            workflow_bundle,
            &handle.legacy_handle,
            "diagnose_failure",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_record_workflow_outcome(&self, args: &Value) -> Result<Value, (i32, String)> {
        let task = args["task"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: task".to_string()))?;
        let status = args["status"].as_str().unwrap_or("success");
        let summary = args["summary"]
            .as_str()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty());
        let mut files = parse_string_array(args, "files");
        let mut symbols = parse_string_array(args, "symbols");
        let mut tests = parse_string_array(args, "tests");
        let context_handle = args["context_handle"].as_str();
        let dry_run = args["dry_run"].as_bool().unwrap_or(false);

        let inherited_query = if let Some(handle) = context_handle {
            let cached = {
                let mut cache = self.context_cache.lock().await;
                cache.get(handle).ok_or((
                    -32602,
                    format!("Unknown or expired context handle: {}", handle),
                ))?
            };
            files.extend(cached.seed.files);
            symbols.extend(cached.seed.symbols);
            tests.extend(cached.seed.tests);
            cached.seed.query
        } else {
            None
        };

        files.retain(|file| is_queryable_workflow_file(file));
        tests.retain(|file| !file.trim().is_empty());
        dedupe_string_values(&mut files);
        dedupe_string_values(&mut symbols);
        dedupe_string_values(&mut tests);

        let source_query =
            combined_workflow_source_query(task, summary, inherited_query.as_deref());
        let mut refresh_inputs = files.clone();
        if let Some(source_query) = source_query.as_ref() {
            refresh_inputs.push(source_query.clone());
        }
        let refresh_key = format!(
            "workflow_outcome::{}",
            stable_refresh_key(task, &refresh_inputs, &symbols)
        );
        let mut content =
            summarize_workflow_outcome_content(task, status, summary, &files, &symbols, &tests);
        let identifiers =
            workflow_outcome_identifiers(task, summary, source_query.as_deref(), &files);
        if !identifiers.is_empty() {
            content.push_str(&format!(" Identifiers: {}.", identifiers.join(", ")));
        }
        let workspace_id = self.memory_workspace_id.clone();
        let branch = current_git_branch(&self.workspace_root);
        let scope = if branch.is_some() {
            MemoryScope::Branch
        } else {
            MemoryScope::Repo
        };

        if dry_run {
            return Ok(wrap_tool_result(json!({
                "status": "dry_run",
                "would_store": true,
                "scope": scope.as_str(),
                "workspace_id": workspace_id,
                "branch": branch,
                "refresh_key": refresh_key,
                "content": content,
                "identifiers": identifiers,
                "files": files,
                "symbols": symbols,
                "tests": tests,
                "source_query": source_query
            })));
        }

        let store = self.memory_store.lock().await;
        let existing = store
            .find_by_refresh_key(&refresh_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to find workflow outcome memory: {}", e),
                )
            })?;

        let value = if let Some(existing) = existing {
            let refreshed = store
                .refresh_memory(
                    &existing.id,
                    Some(&content),
                    Some(MemoryType::Pattern),
                    Some(scope.clone()),
                    Some(&symbols),
                    Some(&files),
                    Some(&workspace_id),
                    branch.as_deref(),
                    Some(&refresh_key),
                    source_query.as_deref(),
                    Some(if status == "success" { 0.96 } else { 0.72 }),
                )
                .map_err(|e| {
                    (
                        -32603,
                        format!("Failed to refresh workflow outcome memory: {}", e),
                    )
                })?;
            json!({
                "status": "refreshed",
                "id": refreshed.id,
                "scope": refreshed.scope.as_str(),
                "refresh_key": refresh_key,
                "files": files,
                "symbols": symbols,
                "tests": tests,
            })
        } else {
            let id = store
                .store(Memory {
                    id: String::new(),
                    session_id: self.session_id.clone(),
                    content,
                    memory_type: MemoryType::Pattern,
                    scope: scope.clone(),
                    confidence: if status == "success" { 0.96 } else { 0.72 },
                    linked_symbols: symbols.clone(),
                    linked_files: files.clone(),
                    workspace_id: Some(workspace_id),
                    branch: branch.clone(),
                    scope_organization_id: None,
                    refresh_key: Some(refresh_key.clone()),
                    source_query,
                    created_at: 0,
                    last_accessed: 0,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .map_err(|e| {
                    (
                        -32603,
                        format!("Failed to store workflow outcome memory: {}", e),
                    )
                })?;
            json!({
                "status": "stored",
                "id": id,
                "scope": scope.as_str(),
                "refresh_key": refresh_key,
                "files": files,
                "symbols": symbols,
                "tests": tests,
            })
        };
        drop(store);
        self.record_auto_memory_write(1).await;
        self.record_outcome_pattern_write(1).await;
        self.trigger_session_consolidation(task, status);

        Ok(wrap_tool_result(value))
    }

    fn trigger_session_consolidation(&self, task: &str, status: &str) {
        let Some(consolidator) = self.session_consolidator.as_ref().cloned() else {
            return;
        };
        let workspace_id = self.memory_workspace_id.clone();
        let task_id = task.to_string();
        let outcome = match status {
            "failure" | "failed" => EpisodeOutcome::Failure,
            "abandoned" => EpisodeOutcome::Abandoned,
            _ => EpisodeOutcome::Success,
        };
        tokio::task::spawn_blocking(move || {
            let mut consolidator = match consolidator.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    tracing::warn!("session consolidation lock was poisoned");
                    return;
                }
            };
            if let Err(error) = consolidator.on_task_complete(
                &workspace_id,
                &lattice_core::events::TaskId {
                    value: task_id.clone(),
                },
                outcome,
            ) {
                tracing::warn!(task_id = task_id.as_str(), %error, "session consolidation failed");
            }
        });
    }

    #[allow(dead_code)]
    pub(crate) async fn auto_checkpoint_active_states(
        &self,
        reason: &str,
    ) -> Result<usize, (i32, String)> {
        let states = self.working_memory_states.lock().await.clone();
        let mut checkpointed = 0usize;
        for (task_id, state) in states {
            if self
                .checkpoint_working_memory_state(&task_id, &state, reason, false)
                .await?
                .is_some()
            {
                checkpointed += 1;
            }
        }
        Ok(checkpointed)
    }

    #[allow(dead_code)]
    pub(crate) async fn auto_flush_session_state(&self) {
        let _ = self.auto_checkpoint_active_states("shutdown").await;
        let task_ids: Vec<String> = self
            .working_memory_states
            .lock()
            .await
            .keys()
            .cloned()
            .collect();
        for task_id in task_ids {
            self.trigger_session_consolidation(&task_id, "success");
        }
    }

    async fn tool_expand_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let handle = args["handle"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: handle".to_string()))?;
        let focus = args["focus"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: focus".to_string()))?;
        let max_tokens = (args["max_tokens"].as_u64().unwrap_or(1200) as usize).clamp(200, 4000);

        let cached = {
            let mut cache = self.context_cache.lock().await;
            cache.get(handle).ok_or((
                -32602,
                format!("Unknown or expired context handle: {}", handle),
            ))?
        };

        if self.validate_repo_epoch_for_graph_reads().await != ValidationOutcome::Fresh {
            return Err((
                -32001,
                format!(
                    "Context handle {} is stale because the workspace trust epoch changed; retry the parent workflow after refresh completes.",
                    handle
                ),
            ));
        }
        if cached.repo_epoch != self.current_repo_epoch().await {
            return Err((
                -32001,
                format!(
                    "Context handle {} was created for repo epoch {} and is stale for the current workspace state.",
                    handle, cached.repo_epoch
                ),
            ));
        }

        let engine = self.engine.lock().await;
        let report = expand_context(engine.graph(), &cached.seed, focus, max_tokens);

        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, handle, &cached.origin);
        Ok(wrap_tool_result(value))
    }

    async fn store_context_handle(
        &self,
        origin: &str,
        seed: ExpandContextSeed,
    ) -> super::context_cache::HandleRecord {
        let repo_epoch = self.current_repo_epoch().await;
        let mut cache = self.context_cache.lock().await;
        cache.insert(
            origin,
            seed,
            &self.workspace_root.to_string_lossy(),
            &self.session_id,
            repo_epoch,
        )
    }

    async fn serialize_workflow_with_context_handle<T: serde::Serialize>(
        &self,
        tool_name: &str,
        report: T,
        handle: &str,
        origin: &str,
        metadata: &WorkflowRunMetadata,
        response_options: &WorkflowResponseOptions,
    ) -> Result<Value, (i32, String)> {
        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, handle, origin);
        self.finalize_workflow_value(tool_name, value, metadata, response_options)
            .await
    }

    async fn finalize_workflow_value(
        &self,
        tool_name: &str,
        mut value: Value,
        metadata: &WorkflowRunMetadata,
        response_options: &WorkflowResponseOptions,
    ) -> Result<Value, (i32, String)> {
        let pruning_profile = self.session_pruning_profile().await;
        let mut metadata = metadata.clone();
        let mut budget = select_workflow_budget(
            tool_name,
            &value,
            &metadata,
            response_options,
            pruning_profile,
        );
        apply_workflow_budget(
            tool_name,
            &mut value,
            budget,
            pruning_profile,
            &mut metadata,
        );
        attach_agent_retrieval_guidance(tool_name, &mut value);

        let effective_token_cap = response_options
            .max_tokens
            .unwrap_or_else(|| default_workflow_token_cap(budget));
        let mut truncated = approx_value_tokens(&value) > effective_token_cap;
        if truncated {
            if !matches!(budget, WorkflowBudget::Tiny) {
                budget = WorkflowBudget::Tiny;
                apply_workflow_budget(
                    tool_name,
                    &mut value,
                    budget,
                    pruning_profile,
                    &mut metadata,
                );
                attach_agent_retrieval_guidance(tool_name, &mut value);
            }
            trim_value_for_token_budget(&mut value, effective_token_cap);
        }

        let mut wire_format = select_workflow_wire_format(
            response_options,
            pruning_profile,
            budget,
            approx_value_tokens(&value),
        );
        if approx_value_tokens(&value) > effective_token_cap {
            wire_format = WorkflowWireFormat::Dense;
        }

        if matches!(wire_format, WorkflowWireFormat::Dense) {
            let dense_preview = densify_workflow_value(value.clone());
            if approx_value_tokens(&dense_preview) <= effective_token_cap {
                value = dense_preview;
            }
        }

        if approx_value_tokens(&value) > effective_token_cap {
            trim_value_for_token_budget(&mut value, effective_token_cap);
            truncated = true;
        }

        metadata.wire_format = match wire_format {
            WorkflowWireFormat::Dense => "dense".to_string(),
            _ => "standard".to_string(),
        };
        attach_workflow_metadata(&mut value, &metadata);
        attach_workflow_budget_metadata(&mut value, budget, effective_token_cap, truncated);

        if matches!(wire_format, WorkflowWireFormat::Dense) {
            value = densify_workflow_value(value);
        }

        Ok(wrap_workflow_tool_result(value, response_options.render))
    }

    async fn session_pruning_profile(&self) -> SessionPruningProfile {
        let metrics = self.session_metrics.lock().await;
        derive_session_pruning_profile(&metrics.snapshot())
    }

    async fn load_relevant_memory_values(
        &self,
        query: Option<&str>,
        files: &[String],
        symbols: &[String],
        limit: usize,
    ) -> Result<Vec<Value>, (i32, String)> {
        let memory_query = build_memory_query(query, files, symbols);
        let store = self.memory_store.lock().await;
        let branch = current_git_branch(&self.workspace_root);
        let scope_filter = self.current_memory_scope_filter();

        let scoped_memories = store
            .list_all_scoped(&scope_filter)
            .map_err(|e| (-32603, format!("Failed to load scoped memories: {}", e)))?;
        let current: Vec<_> = scoped_memories
            .iter()
            .filter(|memory| memory.session_id == self.session_id)
            .take(limit.min(3))
            .cloned()
            .collect();
        let mut values = serialize_memory_values(&store, &current, true)?;

        if values.len() < limit {
            let remaining = limit.saturating_sub(values.len());
            if let Some(ref keyword) = memory_query {
                let previous = store
                    .query(Some(keyword), remaining, &scope_filter)
                    .map_err(|e| (-32603, format!("Failed to search scoped memories: {}", e)))?;
                let previous: Vec<_> = previous
                    .into_iter()
                    .filter(|memory| memory.session_id != self.session_id)
                    .collect();
                values.extend(serialize_memory_values(&store, &previous, true)?);
            }
        }

        sort_memory_values_for_recall(&mut values, branch.as_deref());
        dedupe_memory_values(&mut values);
        Ok(values)
    }

    async fn load_durable_memory_values(&self, limit: usize) -> Result<Vec<Value>, (i32, String)> {
        let workspace_id = self.memory_workspace_id.clone();
        let store = self.memory_store.lock().await;
        let branch = current_git_branch(&self.workspace_root);
        let scope_filter = self.current_memory_scope_filter();
        let mut memories = store
            .list_all_scoped(&scope_filter)
            .map_err(|e| (-32603, format!("Failed to list scoped memories: {}", e)))?;

        memories.retain(|memory| {
            !memory.is_stale
                && memory.scope != MemoryScope::Session
                && memory.workspace_id.as_deref() == Some(workspace_id.as_str())
        });
        memories.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.access_count.cmp(&a.access_count))
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        memories.truncate(limit.max(1).saturating_mul(8));

        let mut values = serialize_memory_values(&store, &memories, true)?;
        sort_memory_values_for_recall(&mut values, branch.as_deref());
        dedupe_memory_values(&mut values);
        values.truncate(limit.max(1));
        Ok(values)
    }

    async fn augment_memory_values_with_playbooks(
        &self,
        query: &str,
        files: &[String],
        symbols: &[String],
        mut values: Vec<Value>,
        limit: usize,
    ) -> Result<Vec<Value>, (i32, String)> {
        let playbooks = self
            .load_playbook_memory_values(query, files, symbols)
            .await?;
        let outcomes = self
            .load_outcome_memory_values(query, files, symbols)
            .await?;
        let branch = current_git_branch(&self.workspace_root);
        values.splice(0..0, playbooks);
        values.splice(0..0, outcomes);
        sort_memory_values_for_recall(&mut values, branch.as_deref());
        dedupe_memory_values(&mut values);
        values.truncate(limit.max(1));
        Ok(values)
    }

    async fn load_playbook_memory_values(
        &self,
        query: &str,
        files: &[String],
        symbols: &[String],
    ) -> Result<Vec<Value>, (i32, String)> {
        let workspace_id = self.memory_workspace_id.clone();
        let branch = current_git_branch(&self.workspace_root);
        let subsystem_key = format!(
            "subsystem_playbook::{}",
            stable_refresh_key(query, files, symbols)
        );

        let store = self.memory_store.lock().await;
        let mut values = Vec::new();

        if let Some(memory) = store
            .find_by_refresh_key("repo_playbook", Some(&workspace_id), None)
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to load repo playbook memory: {}", e),
                )
            })?
        {
            values.push(serialize_memory_value(&store, &memory, true)?);
        }

        if let Some(memory) = store
            .find_by_refresh_key(&subsystem_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to load subsystem playbook memory: {}", e),
                )
            })?
        {
            values.push(serialize_memory_value(&store, &memory, true)?);
        }

        sort_memory_values_for_recall(&mut values, branch.as_deref());
        Ok(values)
    }

    async fn load_outcome_memory_values(
        &self,
        query: &str,
        files: &[String],
        symbols: &[String],
    ) -> Result<Vec<Value>, (i32, String)> {
        let workspace_id = self.memory_workspace_id.clone();
        let branch = current_git_branch(&self.workspace_root);
        let refresh_key = format!(
            "workflow_outcome::{}",
            stable_refresh_key(query, files, symbols)
        );

        let store = self.memory_store.lock().await;
        let mut values = Vec::new();

        if let Some(memory) = store
            .find_by_refresh_key(&refresh_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to load workflow outcome memory: {}", e),
                )
            })?
        {
            values.push(serialize_memory_value(&store, &memory, true)?);
        }

        if branch.is_some() {
            if let Some(memory) = store
                .find_by_refresh_key(&refresh_key, Some(&workspace_id), None)
                .map_err(|e| (-32603, format!("Failed to load repo outcome memory: {}", e)))?
            {
                values.push(serialize_memory_value(&store, &memory, true)?);
            }
        }

        if values.len() < 2 {
            if let Some(keyword) = build_memory_query(Some(query), files, symbols) {
                let scope_filter = self.current_memory_scope_filter();
                let mut searched = store.query(Some(&keyword), 2, &scope_filter).map_err(|e| {
                    (
                        -32603,
                        format!("Failed to search scoped outcome memories: {}", e),
                    )
                })?;
                searched.retain(|memory| {
                    memory.session_id != self.session_id
                        && memory.workspace_id.as_deref() == Some(workspace_id.as_str())
                        && memory
                            .refresh_key
                            .as_deref()
                            .map(|key| key.starts_with("workflow_outcome::"))
                            .unwrap_or(false)
                });
                values.extend(serialize_memory_values(&store, &searched, true)?);
            }
        }

        sort_memory_values_for_recall(&mut values, branch.as_deref());
        dedupe_memory_values(&mut values);
        values.truncate(2);
        Ok(values)
    }

    async fn auto_upsert_playbook_memory(
        &self,
        refresh_key: String,
        content: String,
        linked_files: Vec<String>,
        linked_symbols: Vec<String>,
        source_query: Option<String>,
        prefer_branch_scope: bool,
    ) -> Result<Value, (i32, String)> {
        let workspace_id = self.memory_workspace_id.clone();
        let branch = current_git_branch(&self.workspace_root);
        let scope = if prefer_branch_scope && branch.is_some() {
            MemoryScope::Branch
        } else {
            MemoryScope::Repo
        };
        let scoped_branch = if scope == MemoryScope::Branch {
            branch.clone()
        } else {
            None
        };

        let store = self.memory_store.lock().await;
        let existing = store
            .find_by_refresh_key(&refresh_key, Some(&workspace_id), scoped_branch.as_deref())
            .map_err(|e| (-32603, format!("Failed to find playbook memory: {}", e)))?;

        let result = if let Some(existing) = existing {
            let refreshed = store
                .refresh_memory(
                    &existing.id,
                    Some(&content),
                    Some(MemoryType::Pattern),
                    Some(scope.clone()),
                    Some(&linked_symbols),
                    Some(&linked_files),
                    Some(&workspace_id),
                    scoped_branch.as_deref(),
                    Some(&refresh_key),
                    source_query.as_deref(),
                    Some(0.95),
                )
                .map_err(|e| (-32603, format!("Failed to refresh playbook memory: {}", e)))?;
            json!({
                "id": refreshed.id,
                "status": "refreshed",
                "scope": refreshed.scope.as_str(),
                "refresh_key": refresh_key
            })
        } else {
            let id = store
                .store(Memory {
                    id: String::new(),
                    session_id: self.session_id.clone(),
                    content,
                    memory_type: MemoryType::Pattern,
                    scope: scope.clone(),
                    confidence: 0.95,
                    linked_symbols,
                    linked_files,
                    workspace_id: Some(workspace_id),
                    branch: scoped_branch.clone(),
                    scope_organization_id: None,
                    refresh_key: Some(refresh_key.clone()),
                    source_query,
                    created_at: 0,
                    last_accessed: 0,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .map_err(|e| (-32603, format!("Failed to store playbook memory: {}", e)))?;
            json!({
                "id": id,
                "status": "stored",
                "scope": scope.as_str(),
                "refresh_key": refresh_key
            })
        };
        drop(store);
        self.record_auto_memory_write(1).await;

        Ok(result)
    }

    async fn record_auto_memory_write(&self, count: usize) {
        let mut metrics = self.session_metrics.lock().await;
        metrics.record_auto_memory_write(count);
    }

    async fn record_outcome_pattern_write(&self, count: usize) {
        let mut metrics = self.session_metrics.lock().await;
        metrics.record_outcome_pattern_write(count);
    }

    async fn record_tool_metrics(&self, tool_name: &str, value: &Value) {
        let (payload_bytes, approx_tokens, context_handle, context_origin, metadata) =
            extract_wrapped_tool_metrics(value);
        let mut metrics = self.session_metrics.lock().await;
        metrics.record_tool_call(
            tool_name,
            payload_bytes,
            approx_tokens,
            context_handle.as_deref(),
            context_origin.as_deref(),
            metadata,
        );
    }

    async fn record_adoption_tool_call(
        &self,
        tool_name: &str,
        arguments: &Value,
        result: Option<&Value>,
        elapsed: Duration,
    ) {
        let default_client = self
            .client_name
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "mcp".to_string());
        let source = source_from_arguments(arguments, &default_client, "mcp");
        let latency_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        let suggested_files = result
            .map(|value| suggested_files_from_tool_result(tool_name, value))
            .unwrap_or_default();
        if let Err(error) = self.adoption_metrics.record(ToolCallRecord {
            session_id: self.session_id.clone(),
            client: source.client,
            channel: source.channel,
            tool: tool_name.to_string(),
            latency_ms,
            suggested_files,
        }) {
            tracing::warn!(%error, tool = tool_name, "failed to record adoption metrics");
        }
    }

    async fn tool_get_symbol(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;
        let detail = args["detail"].as_str().unwrap_or("summary");

        let engine = self.engine.lock().await;

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dependencies = engine.graph().get_dependencies(&n.id);

                let mut result = json!({
                    "symbol": n.name,
                    "kind": n.kind.short_code(),
                    "file": n.file,
                    "line": n.line,
                    "signature": n.signature,
                    "dependents": dependents.len(),
                    "dependencies": dependencies.len()
                });

                if detail == "full" {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert("source".to_string(), json!(n.body));
                        obj.insert("end_line".to_string(), json!(n.end_line));
                        obj.insert("is_exported".to_string(), json!(n.is_exported));
                        obj.insert(
                            "dep_list".to_string(),
                            json!(dependents
                                .iter()
                                .map(|(dep, edge)| json!({
                                    "s": dep.name, "f": dep.file, "e": edge.short_code()
                                }))
                                .collect::<Vec<_>>()),
                        );
                        obj.insert(
                            "deps_list".to_string(),
                            json!(dependencies
                                .iter()
                                .map(|(dep, edge)| json!({
                                    "s": dep.name, "f": dep.file, "e": edge.short_code()
                                }))
                                .collect::<Vec<_>>()),
                        );
                    }
                }

                Ok(wrap_tool_result(result))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_get_dependents(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dep_values: Vec<Value> = dependents
                    .iter()
                    .map(|(dep, edge)| {
                        json!({
                            "s": dep.name,
                            "k": dep.kind.short_code(),
                            "f": dep.file,
                            "l": dep.line,
                            "e": edge.short_code()
                        })
                    })
                    .collect();

                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "dependents": dep_values,
                    "count": dep_values.len()
                })))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_get_dependencies(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependencies = engine.graph().get_dependencies(&n.id);
                let dep_values: Vec<Value> = dependencies
                    .iter()
                    .map(|(dep, edge)| {
                        json!({
                            "s": dep.name,
                            "k": dep.kind.short_code(),
                            "f": dep.file,
                            "l": dep.line,
                            "e": edge.short_code()
                        })
                    })
                    .collect();

                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "dependencies": dep_values,
                    "count": dep_values.len()
                })))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_blast_radius(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;
        let hops = (args["hops"].as_u64().unwrap_or(3) as usize).min(10);

        let engine = self.engine.lock().await;

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let affected = engine.graph().get_transitive_dependents(&n.id, hops);
                let affected_files: HashSet<&str> =
                    affected.iter().map(|a| a.file.as_str()).collect();

                let affected_values: Vec<Value> = affected
                    .iter()
                    .map(|a| {
                        json!({
                            "s": a.name,
                            "k": a.kind.short_code(),
                            "f": a.file,
                            "l": a.line
                        })
                    })
                    .collect();

                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "hops": hops,
                    "affected": affected_values,
                    "files": affected_files.into_iter().collect::<Vec<_>>(),
                    "count": affected_values.len()
                })))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_search_symbols(&self, args: &Value) -> Result<Value, (i32, String)> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: pattern".to_string()))?;
        let limit = (args["limit"].as_u64().unwrap_or(20) as usize).min(200);
        let detail = args["detail"].as_str().unwrap_or("summary");

        let engine = self.engine.lock().await;
        let pattern_lower = pattern.to_lowercase();
        let pattern_terms = normalized_search_terms(pattern);

        let mut matches = engine
            .graph()
            .all_nodes()
            .into_iter()
            .filter_map(|node| {
                search_node_match(&pattern_lower, &pattern_terms, &node.name, &node.file)
                    .map(|(score, reason)| (node, score, reason))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left, left_score, _), (right, right_score, _)| {
            right_score
                .cmp(left_score)
                .then_with(|| left.file.cmp(&right.file))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.line.cmp(&right.line))
        });

        let results: Vec<Value> = matches
            .into_iter()
            .take(limit)
            .map(|(n, _, match_reason)| {
                let mut obj = json!({
                    "symbol": n.name,
                    "file": n.file,
                    "line": n.line,
                    "kind": n.kind.short_code(),
                    "match_reason": match_reason,
                });
                if detail == "full" {
                    if let Some(m) = obj.as_object_mut() {
                        m.insert("exported".to_string(), json!(n.is_exported));
                        m.insert("signature".to_string(), json!(n.signature));
                    }
                }
                obj
            })
            .collect();

        Ok(wrap_tool_result(json!({
            "pattern": pattern,
            "results": results,
            "count": results.len()
        })))
    }

    async fn tool_get_file_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;

        // Direct graph lookup: all symbols in the file
        let file_nodes = engine.file_symbols(file);
        let symbols: Vec<Value> = file_nodes
            .iter()
            .map(|n| {
                let dep_count = engine.graph().get_dependents(&n.id).len();
                json!({
                    "symbol": n.name,
                    "kind": n.kind.short_code(),
                    "line": n.line,
                    "exported": n.is_exported,
                    "dependents": dep_count
                })
            })
            .collect();

        Ok(wrap_tool_result(json!({
            "file": file,
            "symbols": symbols,
            "count": symbols.len()
        })))
    }

    async fn tool_search_memory(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let limit = (args["limit"].as_u64().unwrap_or(10) as usize).min(100);

        if self.shared_memory.is_some() {
            return self.tool_search_memory_merged(query, limit).await;
        }

        let store = self.memory_store.lock().await;
        let scope_filter = self.current_memory_scope_filter();
        let workspace_id = self.memory_workspace_id.clone();
        let fts_memories = store
            .query(Some(query), limit.saturating_mul(4).max(20), &scope_filter)
            .map_err(|e| (-32603, format!("Failed to search scoped memories: {}", e)))?;
        let all_memories = store
            .list_all_scoped(&scope_filter)
            .map_err(|e| (-32603, format!("Failed to list scoped memories: {}", e)))?;
        let exact_terms = memory_v2::get_task_memory::structured_query_terms(query, None);
        let mut durable_exact_term_counts: HashMap<String, usize> =
            exact_terms.iter().map(|term| (term.clone(), 0)).collect();
        let mut candidates_by_id: HashMap<String, Memory> = HashMap::new();
        for memory in fts_memories.into_iter().chain(all_memories) {
            if memory
                .workspace_id
                .as_deref()
                .is_some_and(|memory_workspace| memory_workspace != workspace_id)
            {
                continue;
            }
            candidates_by_id.entry(memory.id.clone()).or_insert(memory);
        }

        let mut candidate_text_by_id = HashMap::new();
        for memory in candidates_by_id.values() {
            let fields = store
                .get_structured_fields(&memory.id)
                .map_err(|e| (-32603, format!("Failed to load memory fields: {}", e)))?
                .unwrap_or_default();
            let text = memory_v2::get_task_memory::durable_memory_search_text(memory, &fields);
            for term in &exact_terms {
                if text.to_ascii_lowercase().contains(term) {
                    *durable_exact_term_counts.entry(term.clone()).or_insert(0) += 1;
                }
            }
            candidate_text_by_id.insert(memory.id.clone(), (text, fields));
        }

        let mut ranked = Vec::new();
        for memory in candidates_by_id.into_values() {
            let (text, fields) = candidate_text_by_id
                .remove(&memory.id)
                .unwrap_or_else(|| (memory.content.clone(), MemoryStructuredFields::default()));
            let matched_terms = memory_v2::get_task_memory::matching_query_terms(&text, query);
            if matched_terms.is_empty() {
                continue;
            }
            let matched_exact_terms: Vec<String> = matched_terms
                .iter()
                .filter(|term| memory_v2::get_task_memory::is_structured_remediation_token(term))
                .cloned()
                .collect();
            if !exact_terms.is_empty() && matched_exact_terms.is_empty() {
                continue;
            }
            let mut score = memory_v2::get_task_memory::search_match_score(&matched_terms)
                + (memory.confidence * 100.0).round() as i64;
            if !exact_terms.is_empty() && matched_exact_terms.len() == exact_terms.len() {
                score += 2500;
            }
            score += match fields.verification_status {
                MemoryVerificationStatus::Verified => 600,
                MemoryVerificationStatus::InReview => 200,
                MemoryVerificationStatus::Unverified => 0,
                MemoryVerificationStatus::Superseded => -2500,
                MemoryVerificationStatus::Contradicted => -3500,
                MemoryVerificationStatus::Stale => -3000,
                MemoryVerificationStatus::Expired | MemoryVerificationStatus::Invalidated => -4000,
            };
            if memory.is_stale {
                score -= 3000;
            }
            if memory_current_state_warning(&memory, &self.workspace_root).is_some() {
                score -= 1800;
            }
            ranked.push((memory, matched_terms, score, fields.verification_status));
        }
        let matched_exact_terms: Vec<String> = ranked
            .iter()
            .flat_map(|(_, matched_terms, _, _)| matched_terms.iter())
            .filter(|term| memory_v2::get_task_memory::is_structured_remediation_token(term))
            .fold(Vec::new(), |mut terms, term| {
                if !terms.contains(term) {
                    terms.push(term.clone());
                }
                terms
            });
        let unmatched_exact_terms: Vec<String> = exact_terms
            .iter()
            .filter(|term| !matched_exact_terms.contains(term))
            .cloned()
            .collect();
        let exact_term_status = if exact_terms.is_empty() {
            "not_requested"
        } else if unmatched_exact_terms.is_empty() {
            "matched"
        } else if matched_exact_terms.is_empty()
            && durable_exact_term_counts.values().all(|count| *count == 0)
        {
            "absent_from_durable_memory"
        } else {
            "partially_matched"
        };
        ranked.sort_by(|left, right| {
            right
                .2
                .cmp(&left.2)
                .then_with(|| right.0.created_at.cmp(&left.0.created_at))
        });
        ranked.truncate(limit);
        let diagnostics: Vec<Value> = ranked
            .iter()
            .map(|(memory, matched_terms, score, verification_status)| {
                json!({
                    "memory_id": memory.id,
                    "workspace": memory.workspace_id,
                    "matched_terms": matched_terms,
                    "matched_exact_terms": matched_terms
                        .iter()
                        .filter(|term| memory_v2::get_task_memory::is_structured_remediation_token(term))
                        .collect::<Vec<_>>(),
                    "score": score,
                    "verification_status": verification_status.as_str(),
                    "is_stale": memory.is_stale
                })
            })
            .collect();
        let memories: Vec<Memory> = ranked.into_iter().map(|(memory, _, _, _)| memory).collect();
        let mut memory_values = serialize_memory_values(&store, &memories, true)?;
        annotate_memory_freshness_values(&mut memory_values, &memories, &self.workspace_root);

        Ok(wrap_tool_result(json!({
            "query": query,
            "memories": memory_values,
            "count": memory_values.len(),
            "diagnostics": {
                "workspace": workspace_id,
                "exact_term_rerank": true,
                "query_exact_terms": exact_terms,
                "matched_exact_terms": matched_exact_terms,
                "unmatched_exact_terms": unmatched_exact_terms,
                "durable_exact_term_counts": durable_exact_term_counts,
                "exact_term_status": exact_term_status,
                "matches": diagnostics
            }
        })))
    }

    async fn tool_search_memory_merged(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Value, (i32, String)> {
        let runtime = self.shared_memory.as_ref().expect("checked above");
        let authority = runtime
            .query_authority(
                &self.memory_workspace_id,
                &self.workspace_root.to_string_lossy(),
                current_git_branch(&self.workspace_root),
                &self.session_id,
            )
            .map_err(|error| (-32602, error))?;
        let repository_store = self.memory_store.lock().await;
        let shared_store = runtime.store.lock().await;
        let router = MemoryStoreRouter::new(&repository_store, Some(&shared_store), authority)
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to initialize shared memory router: {error}"),
                )
            })?;
        let recalled = router
            .recall(Some(query), limit)
            .map_err(|error| (-32603, format!("Failed to recall scoped memory: {error}")))?;
        let mut values = Vec::with_capacity(recalled.len());
        for result in recalled {
            let store = match result.source_tier {
                MemoryRecallTier::Repository => &*repository_store,
                MemoryRecallTier::Organization => &*shared_store,
            };
            let mut value = serialize_memory_value(store, &result.memory, true)?;
            annotate_shared_memory_value(
                &mut value,
                &result.memory_id.encoded(),
                match result.source_tier {
                    MemoryRecallTier::Repository => "repository",
                    MemoryRecallTier::Organization => "organization",
                },
                result.cross_repo,
                result.origin_repository_id.as_deref(),
                result.origin_checkout_id.as_deref(),
                result.effective_verification_status.as_str(),
                &result.trust_reason,
            );
            if let Some(object) = value.as_object_mut() {
                object.insert("assertion_key".to_string(), json!(result.assertion_key));
                object.insert(
                    "origin_verification_status".to_string(),
                    json!(result.origin_verification_status.as_str()),
                );
            }
            values.push(value);
        }
        Ok(wrap_tool_result(json!({
            "query": query,
            "memories": values,
            "count": values.len(),
            "diagnostics": {
                "repository_id": self.memory_workspace_id,
                "organization_id": runtime.organization_id,
                "search_tiers": ["repository", "organization"],
                "partial": false
            }
        })))
    }

    async fn tool_list_stale_memories(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"].as_str();
        let limit = (args["limit"].as_u64().unwrap_or(50) as usize).min(200);

        let store = self.memory_store.lock().await;
        let memories = store
            .list_stale(query, limit)
            .map_err(|e| (-32603, format!("Failed to list stale memories: {}", e)))?;

        let entries = serialize_memory_values(&store, &memories, true)?;

        Ok(wrap_tool_result(json!({
            "count": entries.len(),
            "query": query,
            "memories": entries
        })))
    }

    async fn tool_submit_lsp_edges(&self, args: &Value) -> Result<Value, (i32, String)> {
        let edges = args["edges"].as_array().ok_or((
            -32602,
            "Missing required parameter: edges (array)".to_string(),
        ))?;

        let mut added = 0usize;
        let mut skipped = 0usize;
        let mut skip_reasons: Vec<Value> = Vec::new();

        if self.workspace_manager.is_some() {
            let new_graph = {
                let mut engine = self.engine.lock().await;
                for edge in edges {
                    apply_lsp_edge_to_graph(
                        engine.graph_mut(),
                        edge,
                        &mut added,
                        &mut skipped,
                        &mut skip_reasons,
                    );
                }
                std::sync::Arc::new(engine.graph().clone())
            };

            let graph_store = self.graph_store.lock().await;
            let _ = graph_store.save_graph(&new_graph);
        } else {
            let new_graph = {
                let mut indexer = self.indexer.lock().await;
                for edge in edges {
                    apply_lsp_edge_to_graph(
                        indexer.graph_mut(),
                        edge,
                        &mut added,
                        &mut skipped,
                        &mut skip_reasons,
                    );
                }
                indexer.graph_arc()
            };

            {
                let graph_store = self.graph_store.lock().await;
                let _ = graph_store.save_graph(&new_graph);
            }

            let mut engine = self.engine.lock().await;
            engine.update_graph_arc(new_graph);
        }

        Ok(wrap_tool_result(json!({
            "added": added,
            "skipped": skipped,
            "skip_reasons": skip_reasons
        })))
    }

    async fn tool_workspace_setup(&self, args: &Value) -> Result<Value, (i32, String)> {
        let format = args["format"].as_str().unwrap_or("markdown");

        let engine = self.engine.lock().await;
        let all_nodes = engine.graph().all_nodes();
        let stats = engine.graph().stats();

        // Collect unique files and language breakdown
        let mut file_set = HashSet::new();
        let mut lang_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for node in &all_nodes {
            file_set.insert(node.file.clone());
            *lang_counts
                .entry(format!("{:?}", node.language))
                .or_insert(0) += 1;
        }
        let files: Vec<String> = file_set.into_iter().collect();

        // Detect project rules
        let detector = lattice_core::intelligence::RulesDetector::new();
        let rules = detector.detect_rules(&files);

        if format == "markdown" {
            let mut md = String::new();
            md.push_str(&format!("# Workspace Setup\n\n"));
            md.push_str(&format!(
                "**Files:** {} | **Symbols:** {} | **Edges:** {}\n\n",
                stats.file_count, stats.node_count, stats.edge_count
            ));
            md.push_str("## Languages\n\n");
            for (lang, count) in &lang_counts {
                md.push_str(&format!("- {}: {} symbols\n", lang, count));
            }
            md.push_str("\n## Detected Conventions\n\n");
            for rule in &rules {
                md.push_str(&format!(
                    "- {} (confidence: {:.0}%, {} occurrences)\n",
                    rule.description,
                    rule.confidence * 100.0,
                    rule.occurrences
                ));
            }
            Ok(wrap_tool_result(json!({ "markdown": md })))
        } else {
            let rule_values: Vec<Value> = rules
                .iter()
                .map(|r| {
                    json!({
                        "description": r.description,
                        "confidence": r.confidence,
                        "occurrences": r.occurrences,
                    })
                })
                .collect();

            Ok(wrap_tool_result(json!({
                "files": stats.file_count,
                "symbols": stats.node_count,
                "edges": stats.edge_count,
                "languages": lang_counts,
                "rules": rule_values
            })))
        }
    }

    async fn tool_index_status(&self, _args: &Value) -> Result<Value, (i32, String)> {
        let is_indexing = self.is_indexing();
        let snapshot = self.current_status_snapshot(true).await;
        let index_work = self.index_work.snapshot();
        let file_limit = max_warm_graph_files();
        let byte_limit = max_warm_graph_bytes();
        let (
            persisted_files,
            persisted_bytes,
            graph_storage_state,
            warm_load_error,
            graph_storage_diagnostic,
        ) = if let Ok(graph_store) = self.graph_store.try_lock() {
            let persisted_files = graph_store.persisted_graph_file_count();
            let persisted_bytes = graph_store.persisted_graph_disk_bytes();
            match (persisted_files, persisted_bytes) {
                (Ok(files), Ok(bytes)) => (
                    Some(files),
                    bytes,
                    graph_store.recovery().as_str(),
                    None,
                    None,
                ),
                (Err(error), _) => (
                    None,
                    None,
                    "unhealthy",
                    Some(error.to_string()),
                    Some(error.to_string()),
                ),
                (_, Err(error)) => (
                    None,
                    None,
                    "unhealthy",
                    Some(error.to_string()),
                    Some(error.to_string()),
                ),
            }
        } else {
            (
                None,
                None,
                "busy",
                None,
                Some("graph store is publishing an index snapshot".to_string()),
            )
        };
        let file_limit_hit = persisted_files.is_some_and(|count| count > file_limit);
        let byte_limit_hit = persisted_bytes.is_some_and(|bytes| bytes > byte_limit);
        let warm_load_skipped = warm_load_error.is_some() || file_limit_hit || byte_limit_hit;
        let warm_load_skip_reason = if warm_load_error.is_some() {
            Some("validation_error")
        } else if file_limit_hit {
            Some("file_limit")
        } else if byte_limit_hit {
            Some("byte_limit")
        } else {
            None
        };
        let effective_files = persisted_files.unwrap_or(snapshot.stats.file_count);
        let watcher_health = self.watcher_health.snapshot();
        let index_health = self.index_health.snapshot(10);

        let mut result = json!({
            "status": if is_indexing { "indexing" } else { "ready" },
            "version": env!("CARGO_PKG_VERSION"),
            "workspace": self.workspace_root.to_string_lossy(),
            "workspace_role": "shard",
            "workspace_field_meaning": "shard_workspace",
            "request_workspace": self.workspace_root.to_string_lossy(),
            "nodes": snapshot.stats.node_count,
            "edges": snapshot.stats.edge_count,
            "files": snapshot.stats.file_count,
            "languages": snapshot.languages.unwrap_or_default(),
            "warm_load_skipped": warm_load_skipped,
            "warm_load_skip_reason": warm_load_skip_reason,
            "persisted_files": persisted_files,
            "graph_storage_state": graph_storage_state,
            "limit": file_limit,
            "env_var": WARM_GRAPH_FILE_LIMIT_ENV,
            "persisted_bytes": persisted_bytes,
            "byte_limit": byte_limit,
            "byte_env_var": WARM_GRAPH_BYTE_LIMIT_ENV,
            "effective_files": effective_files,
            "index_work": index_work,
            "watch_degraded": watcher_health.watch_degraded,
            "watch_degraded_reason": watcher_health.reason,
            "watch_poll_interval_secs": watcher_health.polling_interval_secs,
            "watch_last_poll_epoch_secs": watcher_health.last_poll_epoch_secs,
            "is_partial": index_health.is_partial,
            "parse_failures": index_health.parse_failures,
            "failed_files": index_health.failed_files
        });
        if let Some(error) = warm_load_error {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("warm_load_error".to_string(), json!(error));
            }
        }
        if let Some(diagnostic) = graph_storage_diagnostic {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("graph_storage_diagnostic".to_string(), json!(diagnostic));
            }
        }

        // Add multi-repo info if applicable
        if self.workspace_roots.len() > 1 {
            let obj = match result.as_object_mut() {
                Some(o) => o,
                None => return Ok(result),
            };
            let roots: Vec<String> = self
                .workspace_roots
                .iter()
                .map(|r| r.to_string_lossy().to_string())
                .collect();
            obj.insert("workspaces".to_string(), json!(roots));
            obj.insert("multi_repo".to_string(), json!(true));

            if let Some(wm) = &self.workspace_manager {
                if let Ok(wm) = wm.try_lock() {
                    let repo_stats: Vec<Value> = wm
                        .repo_stats()
                        .iter()
                        .map(|s| {
                            json!({
                                "name": s.name,
                                "files": s.file_count,
                                "nodes": s.node_count,
                                "edges": s.edge_count
                            })
                        })
                        .collect();
                    obj.insert("repos".to_string(), json!(repo_stats));
                } else {
                    obj.insert("repos_pending".to_string(), json!(true));
                }
            }
        }

        Ok(wrap_tool_result(result))
    }

    async fn tool_get_session_metrics(&self, _args: &Value) -> Result<Value, (i32, String)> {
        let metrics = self.session_metrics.lock().await;
        let report = metrics.snapshot();
        serde_json::to_value(&report)
            .map(|value| wrap_tool_result(value))
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))
    }

    async fn tool_get_project_rules(&self, _args: &Value) -> Result<Value, (i32, String)> {
        let engine = self.engine.lock().await;
        let all_nodes = engine.graph().all_nodes();

        // Collect unique file paths from the graph
        let files: Vec<String> = {
            let mut file_set = HashSet::new();
            for node in &all_nodes {
                file_set.insert(node.file.clone());
            }
            file_set.into_iter().collect()
        };

        let detector = lattice_core::intelligence::RulesDetector::new();
        let rules = detector.detect_rules(&files);

        let rule_values: Vec<Value> = rules
            .iter()
            .map(|r| {
                json!({
                    "description": r.description,
                    "confidence": r.confidence,
                    "occurrences": r.occurrences,
                    "example_files": r.example_files
                })
            })
            .collect();

        Ok(wrap_tool_result(json!({
            "status": "ok",
            "rules": rule_values,
            "count": rule_values.len()
        })))
    }

    async fn tool_inspect_working_memory(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed = working_memory_tool::parse_args(args).map_err(|message| (-32602, message))?;
        let (state, checkpoint_id) = self
            .load_working_memory_state(&parsed.task_id)
            .await?
            .ok_or_else(|| {
                (
                    -32004,
                    format!(
                        "No active or checkpointed working memory state exists for task `{}`",
                        parsed.task_id
                    ),
                )
            })?;
        let expansion_handle = self
            .store_working_memory_snapshot(&parsed.task_id, &state)
            .await;
        let value = match parsed.mode {
            working_memory_tool::InspectWorkingMemoryMode::Compact => {
                working_memory_tool::compact_response_value(
                    parsed.task_id,
                    checkpoint_id,
                    expansion_handle,
                    summarize_state(&state),
                )
            }
            working_memory_tool::InspectWorkingMemoryMode::Diagnostic => {
                let mut diagnostic_state = state.clone();
                if !parsed.include_excluded {
                    diagnostic_state.excluded_memories.clear();
                }
                working_memory_tool::diagnostic_response_value(
                    parsed.task_id,
                    checkpoint_id,
                    expansion_handle,
                    diagnostic_state,
                )
            }
        }
        .map_err(|message| (-32603, message))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_get_task_memory_v2(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed =
            memory_v2::get_task_memory::parse_args(args).map_err(|message| (-32602, message))?;
        let effective_focus_files =
            merge_unique_strings(parsed.focus_files.clone(), self.default_focus_files.clone());
        let effective_focus_dirs =
            merge_unique_strings(parsed.focus_dirs.clone(), self.default_focus_dirs.clone());
        let (mut state, checkpoint_id) =
            match self.load_working_memory_state(&parsed.task_id).await? {
                Some(existing) => existing,
                None => {
                    let created = self.build_implicit_working_memory_state(
                        &parsed.task_id,
                        parsed.task_statement.as_deref(),
                        parsed.intent_hint.as_deref(),
                        &effective_focus_files,
                    );
                    self.working_memory_states
                        .lock()
                        .await
                        .insert(parsed.task_id.clone(), created.clone());
                    (created, None)
                }
            };
        self.apply_task_focus_to_state(&mut state, &effective_focus_files);
        self.working_memory_states
            .lock()
            .await
            .insert(parsed.task_id.clone(), state.clone());
        let _ = self
            .checkpoint_working_memory_state(&parsed.task_id, &state, "access", false)
            .await?;
        let scope_filter = self.current_memory_scope_filter();
        let store = self.memory_store.lock().await;
        let candidates = store
            .list_all_scoped(&scope_filter)
            .map_err(|error| (-32603, format!("Failed to list scoped memories: {error}")))?;
        let ranked = memory_v2::get_task_memory::rank_memories(
            &store,
            &self.workspace_root.to_string_lossy(),
            &state,
            candidates,
            parsed.intent_hint.as_deref(),
            &effective_focus_files,
            &effective_focus_dirs,
            current_git_branch(&self.workspace_root).as_deref(),
        )
        .map_err(|error| (-32603, error))?;
        let clipped = memory_v2::get_task_memory::clip_to_budget(&ranked, parsed.budget_tokens);
        let bundle = memory_v2::get_task_memory::build_bundle(
            &store,
            &self.workspace_root.to_string_lossy(),
            parsed.task_id,
            checkpoint_id,
            &state,
            clipped,
        )
        .map_err(|error| (-32603, error))?;
        if let Some(capture) = &self.event_capture {
            let ids: Vec<_> = bundle
                .memories
                .iter()
                .map(|memory| MemoryId {
                    workspace_id: self.memory_workspace_id.clone(),
                    ulid: memory.id.clone(),
                })
                .collect();
            let reasons: Vec<String> = bundle
                .memories
                .iter()
                .map(|memory| memory.inclusion_reason.clone())
                .collect();
            if let Err(error) = capture.record_memory_retrieved(&ids, &reasons) {
                tracing::warn!(tool = "get_task_memory", %error, "failed to capture memory retrieval");
            }
        }
        serde_json::to_value(&bundle)
            .map(wrap_tool_result)
            .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn tool_save_quick_memory_v2(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed =
            memory_v2::save_quick_memory::parse_args(args).map_err(|message| (-32602, message))?;
        memory_v2::save_quick_memory::validate_args(&parsed)
            .map_err(|message| (-32602, message))?;
        let workspace_id = self.memory_workspace_id.clone();
        let task_state = if let Some(task_id) = parsed.task_id.as_deref() {
            self.load_working_memory_state(task_id)
                .await?
                .map(|(state, _)| state)
        } else {
            None
        };
        let save_args = memory_v2::save_quick_memory::build_save_memory_args(
            parsed,
            task_state.as_ref(),
            &workspace_id,
            &self.default_focus_files,
            &self.default_focus_dirs,
        );
        memory_v2::save_memory::validate_args(&save_args).map_err(|message| (-32602, message))?;
        let (memory, structured) =
            memory_v2::save_memory::build_memory(&self.session_id, &workspace_id, &save_args);
        let store = self.memory_store.lock().await;
        let memory_id = store
            .store(memory)
            .map_err(|error| (-32603, format!("Failed to store memory: {error}")))?;
        store
            .update_structured_fields(&memory_id, &structured)
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to persist structured memory fields: {error}"),
                )
            })?;
        let verification_job_id = store
            .enqueue_verification_job(&workspace_id, &memory_id)
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to enqueue verification job: {error}"),
                )
            })?;
        let response = memory_v2::save_memory::build_response(
            &store,
            &workspace_id,
            &memory_id,
            verification_job_id,
        )
        .map_err(|error| (-32603, error))?;
        if let Some(capture) = &self.event_capture {
            let memory_identity = MemoryId {
                workspace_id: workspace_id.clone(),
                ulid: memory_id.clone(),
            };
            if let Err(error) = capture.record_memory_created(memory_identity, &[]) {
                tracing::warn!(tool = "save_quick_memory", %error, "failed to capture memory creation");
            }
        }
        serde_json::to_value(&response)
            .map(wrap_tool_result)
            .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn tool_save_memory_v2(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed =
            memory_v2::save_memory::parse_args(args).map_err(|message| (-32602, message))?;
        memory_v2::save_memory::validate_args(&parsed).map_err(|message| (-32602, message))?;
        let workspace_id = self.memory_workspace_id.clone();
        let (memory, structured) =
            memory_v2::save_memory::build_memory(&self.session_id, &workspace_id, &parsed);
        if matches!(
            parsed.scope,
            memory_v2::save_memory::MemoryScopeArg::Organization
        ) {
            let runtime = self.shared_memory.as_ref().ok_or_else(|| {
                (
                    -32602,
                    self.shared_memory_error.clone().unwrap_or_else(|| {
                        "organization memory requires trusted daemon configuration: set LATTICE_ORGANIZATION_ID and optional LATTICE_SHARED_MEMORY_PATH".to_string()
                    }),
                )
            })?;
            let authority = runtime
                .query_authority(
                    &self.memory_workspace_id,
                    &self.workspace_root.to_string_lossy(),
                    current_git_branch(&self.workspace_root),
                    &self.session_id,
                )
                .map_err(|error| (-32602, error))?;
            let repository_store = self.memory_store.lock().await;
            let shared_store = runtime.store.lock().await;
            let router = MemoryStoreRouter::new(&repository_store, Some(&shared_store), authority)
                .map_err(|error| {
                    (
                        -32603,
                        format!("Failed to initialize shared memory router: {error}"),
                    )
                })?;
            let qualified_id = router
                .remember(memory, &structured, parsed.organization_id.as_deref())
                .map_err(|error| {
                    (
                        -32602,
                        format!("Failed to store organization memory: {error}"),
                    )
                })?;
            let verification_job_id = shared_store
                .enqueue_verification_job(&self.memory_workspace_id, &qualified_id.local_id)
                .map_err(|error| {
                    (
                        -32603,
                        format!("Failed to enqueue organization memory verification: {error}"),
                    )
                })?;
            let response = memory_v2::save_memory::build_response(
                &shared_store,
                &self.memory_workspace_id,
                &qualified_id.local_id,
                verification_job_id,
            )
            .map_err(|error| (-32603, error))?;
            let mut value = serde_json::to_value(response)
                .map_err(|error| (-32603, format!("Serialization error: {error}")))?;
            annotate_shared_memory_value(
                &mut value,
                &qualified_id.encoded(),
                "organization",
                false,
                Some(self.memory_workspace_id.as_str()),
                Some(self.workspace_root.to_string_lossy().as_ref()),
                "unverified",
                "organization memory is unverified until evidence is verified for this repository",
            );
            return Ok(wrap_tool_result(value));
        }
        let store = self.memory_store.lock().await;
        let memory_id = store
            .store(memory)
            .map_err(|error| (-32603, format!("Failed to store memory: {error}")))?;
        store
            .update_structured_fields(&memory_id, &structured)
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to persist structured memory fields: {error}"),
                )
            })?;
        let verification_job_id = store
            .enqueue_verification_job(&workspace_id, &memory_id)
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to enqueue verification job: {error}"),
                )
            })?;
        let response = memory_v2::save_memory::build_response(
            &store,
            &workspace_id,
            &memory_id,
            verification_job_id,
        )
        .map_err(|error| (-32603, error))?;
        if let Some(capture) = &self.event_capture {
            let memory_identity = MemoryId {
                workspace_id: workspace_id.clone(),
                ulid: memory_id.clone(),
            };
            if let Err(error) = capture.record_memory_created(memory_identity, &[]) {
                tracing::warn!(tool = "save_memory", %error, "failed to capture memory creation");
            }
        }
        serde_json::to_value(&response)
            .map(wrap_tool_result)
            .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn tool_propose_memory_evolution_v2(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed = memory_v2::propose_memory_evolution::parse_args(args)
            .map_err(|message| (-32602, message))?;
        memory_v2::propose_memory_evolution::validate_args(&parsed)
            .map_err(|message| (-32602, message))?;
        let workspace_id = self.memory_workspace_id.clone();
        let store = self.memory_store.lock().await;
        match parsed.action {
            memory_v2::EvolutionAction::Propose => {
                let proposal = memory_v2::propose_memory_evolution::build_proposal(
                    &workspace_id,
                    &store,
                    &parsed,
                )
                .map_err(|error| (-32603, error))?;
                let workspace_for_persist = workspace_id.clone();
                let proposal_kind = proposal.proposal_kind;
                let proposal_id = proposal.proposal_id.clone();
                let prior_state = proposal.prior_state.clone();
                let proposed_state = proposal.proposed_state.clone();
                store
                    .with_connection(|conn| {
                        lattice_core::consolidation::persist_pending_proposal(
                            conn,
                            &workspace_for_persist,
                            proposal_kind.as_str(),
                            lattice_core::consolidation::ConsolidationJobMode::SynchronousPostTask,
                            &proposal,
                        )
                        .map(|_| ())
                    })
                    .map_err(|error| (-32603, format!("Failed to persist proposal: {error}")))?;
                let response = memory_v2::EvolutionProposal {
                    proposal_id,
                    action: parsed.action,
                    source_memory_id: parsed.memory_id,
                    proposal_kind: proposal_kind.as_str().to_string(),
                    decision: "pending".to_string(),
                    prior_state,
                    proposed_state,
                };
                serde_json::to_value(&response)
                    .map(wrap_tool_result)
                    .map_err(|error| (-32603, format!("Serialization error: {error}")))
            }
            memory_v2::EvolutionAction::Apply | memory_v2::EvolutionAction::Reject => {
                let proposal_id = parsed
                    .proposal_id
                    .as_deref()
                    .ok_or((-32602, "Missing proposal_id".to_string()))?;
                let capture = self.event_capture.as_ref().ok_or((
                    -32603,
                    "Event capture is not configured for this session".to_string(),
                ))?;
                let writer = capture.writer();
                let decided_by = parsed.decided_by.as_deref().unwrap_or("assistant");
                store
                    .with_connection(|conn| {
                        let proposal = lattice_core::consolidation::ConsolidationProposal::load(
                            conn,
                            proposal_id,
                        )?
                        .ok_or_else(|| {
                            lattice_core::LatticeError::Storage(format!(
                                "Proposal `{proposal_id}` was not found"
                            ))
                        })?;
                        match parsed.action {
                            memory_v2::EvolutionAction::Apply => {
                                proposal.apply(
                                    conn,
                                    &store,
                                    writer.as_ref(),
                                    decided_by,
                                    parsed.reason.as_deref(),
                                )?;
                            }
                            memory_v2::EvolutionAction::Reject => {
                                proposal.reject(
                                    conn,
                                    writer.as_ref(),
                                    decided_by,
                                    parsed.reason.as_deref(),
                                )?;
                            }
                            memory_v2::EvolutionAction::Propose => {}
                        }
                        Ok(())
                    })
                    .map_err(|error| (-32603, format!("Failed to decide proposal: {error}")))?;
                let record = store
                    .with_connection(|conn| {
                        lattice_core::consolidation::ConsolidationProposal::load_record(
                            conn,
                            proposal_id,
                        )
                        .map_err(Into::into)
                        .and_then(|value| {
                            value.ok_or_else(|| {
                                lattice_core::LatticeError::Storage(format!(
                                    "Proposal `{proposal_id}` was not found after decision"
                                ))
                            })
                        })
                    })
                    .map_err(|error| (-32603, format!("Failed to reload proposal: {error}")))?;
                let response = memory_v2::EvolutionProposal {
                    proposal_id: record.proposal_id.clone(),
                    action: parsed.action,
                    source_memory_id: record.target_memory_id.clone(),
                    proposal_kind: record.proposal_kind.as_str().to_string(),
                    decision: record.decision.as_str().to_string(),
                    prior_state: record.prior_state.clone(),
                    proposed_state: record.proposed_state.clone(),
                };
                serde_json::to_value(&response)
                    .map(wrap_tool_result)
                    .map_err(|error| (-32603, format!("Serialization error: {error}")))
            }
        }
    }

    async fn tool_consolidate_session_v2(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed = memory_v2::consolidate_session::parse_args(args)
            .map_err(|message| (-32602, message))?;
        memory_v2::consolidate_session::validate_args(&parsed)
            .map_err(|message| (-32602, message))?;
        let render_mode = parsed.render_mode.unwrap_or_default();
        let mode = parsed.mode.unwrap_or_default();
        let workspace_id = self.memory_workspace_id.clone();
        let capture = self.event_capture.as_ref().ok_or((
            -32603,
            "Event capture is not configured for this session".to_string(),
        ))?;
        let reader = EventReader::new(capture.writer().store());
        let events = reader
            .execute(
                EventQuery::new()
                    .session(parsed.session_id.clone())
                    .workspace(workspace_id.clone())
                    .order(QueryOrder::OldestFirst)
                    .limit(1_000),
            )
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to read session event trace: {error}"),
                )
            })?;
        let slices = memory_v2::consolidate_session::group_task_slices(&events);
        let mut store = self.memory_store.lock().await;
        let proposals =
            persist_session_consolidation_proposals(&workspace_id, &mut store, &slices, mode)?;
        emit_consolidation_proposal_events(capture, &workspace_id, &proposals)?;
        tracing::info!(
            tool = "consolidate_session",
            scope = parsed.session_id.as_str(),
            proposal_count = proposals.len(),
            "session consolidation proposals generated"
        );
        serde_json::to_value(build_consolidation_report(
            parsed,
            mode,
            render_mode,
            proposals,
        ))
        .map(wrap_tool_result)
        .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn tool_get_memory_metrics_v2(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed =
            memory_v2::get_memory_metrics::parse_args(args).map_err(|message| (-32602, message))?;
        let render_mode = parsed.render_mode.unwrap_or_default();
        let scope = parsed
            .scope
            .unwrap_or(memory_v2::get_memory_metrics::MetricScopeKind::Session);
        let events = self
            .load_metric_scope_events(scope, parsed.time_range.as_ref())
            .await?;
        let collector = lattice_core::metrics::MetricsCollector::new().with_events(events.clone());
        let session_snapshot = self.session_metrics.lock().await.snapshot();
        let surface = MetricsSurface::new(
            self.workspace_root.to_string_lossy(),
            collector,
            Some(session_snapshot),
        );
        let metric_scope = self.build_metric_scope(scope, parsed.time_range.clone());
        let signals = surface.collect(
            metric_scope,
            &memory_v2::get_memory_metrics::requested_signals(&parsed),
        );
        let incomplete = signals.iter().any(|signal| signal.incomplete)
            || signals
                .iter()
                .any(|signal| signal.source == lattice_core::metrics::MetricSource::SessionMetrics);
        let notes = build_metric_snapshot_notes(&signals);
        tracing::info!(
            tool = "get_memory_metrics",
            scope = ?scope,
            event_count = events.len(),
            "memory metrics snapshot generated"
        );
        serde_json::to_value(memory_v2::get_memory_metrics::MetricSnapshot {
            scope,
            render_mode,
            signals,
            incomplete,
            notes,
        })
        .map(wrap_tool_result)
        .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn tool_get_event_trace_v2(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed =
            memory_v2::get_event_trace::parse_args(args).map_err(|message| (-32602, message))?;
        memory_v2::get_event_trace::validate_args(&parsed).map_err(|message| (-32602, message))?;
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        if parsed
            .workspace_id
            .as_deref()
            .is_some_and(|value| value != workspace_id)
        {
            return Err((
                -32011,
                format!(
                    "Event trace scope `{}` is outside the active workspace",
                    parsed.workspace_id.unwrap_or_default()
                ),
            ));
        }
        let capture = self.event_capture.as_ref().ok_or((
            -32603,
            "Event capture is not configured for this session".to_string(),
        ))?;
        let render_mode = parsed.render_mode.unwrap_or_default();
        let limit = parsed.limit.unwrap_or(25).min(100);
        let reader = EventReader::new(capture.writer().store());
        let query = build_event_trace_query(
            &parsed,
            &workspace_id,
            current_git_branch(&self.workspace_root).as_deref(),
            limit,
        );
        let cursor = parsed
            .cursor
            .as_deref()
            .map(memory_v2::get_event_trace::decode_cursor)
            .transpose()
            .map_err(|message| (-32602, message))?;
        let EventPage {
            events,
            next_cursor_row_id,
        } = reader
            .execute_page(query, cursor)
            .map_err(|error| (-32603, format!("Failed to load event trace page: {error}")))?;
        tracing::info!(
            tool = "get_event_trace",
            scope = event_trace_scope_kind(&parsed),
            event_count = events.len(),
            "event trace page generated"
        );
        serde_json::to_value(build_event_trace_page(
            parsed,
            render_mode,
            events,
            next_cursor_row_id,
            limit,
        ))
        .map(wrap_tool_result)
        .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn load_metric_scope_events(
        &self,
        scope: memory_v2::get_memory_metrics::MetricScopeKind,
        time_range: Option<&memory_v2::get_memory_metrics::MetricTimeRange>,
    ) -> Result<Vec<lattice_core::events::EventEnvelope>, (i32, String)> {
        let capture = self.event_capture.as_ref().ok_or((
            -32603,
            "Event capture is not configured for this session".to_string(),
        ))?;
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        let mut query = match scope {
            memory_v2::get_memory_metrics::MetricScopeKind::Session => EventQuery::new()
                .session(self.session_id.clone())
                .workspace(workspace_id),
            memory_v2::get_memory_metrics::MetricScopeKind::Branch
            | memory_v2::get_memory_metrics::MetricScopeKind::Repo => {
                let branch =
                    current_git_branch(&self.workspace_root).unwrap_or_else(|| "main".to_string());
                EventQuery::new().workspace(workspace_id).branch(branch)
            }
            memory_v2::get_memory_metrics::MetricScopeKind::User
            | memory_v2::get_memory_metrics::MetricScopeKind::Organization => {
                return Ok(Vec::new());
            }
        }
        .order(QueryOrder::OldestFirst)
        .limit(1_000);
        if let Some(range) = time_range {
            if let Some(since) = range.since {
                query = query.after(since);
            }
            if let Some(until) = range.until {
                query = query.before(until);
            }
        }
        EventReader::new(capture.writer().store())
            .execute(query)
            .map_err(|error| (-32603, format!("Failed to read metric events: {error}")))
    }

    fn build_metric_scope(
        &self,
        scope: memory_v2::get_memory_metrics::MetricScopeKind,
        time_range: Option<memory_v2::get_memory_metrics::MetricTimeRange>,
    ) -> lattice_core::metrics::MetricScope {
        let scope = match scope {
            memory_v2::get_memory_metrics::MetricScopeKind::Session => {
                lattice_core::metrics::MetricScope::session(self.session_id.clone())
            }
            memory_v2::get_memory_metrics::MetricScopeKind::Branch => {
                let branch =
                    current_git_branch(&self.workspace_root).unwrap_or_else(|| "main".to_string());
                lattice_core::metrics::MetricScope::branch(
                    self.workspace_root.to_string_lossy(),
                    branch,
                )
            }
            memory_v2::get_memory_metrics::MetricScopeKind::Repo => {
                lattice_core::metrics::MetricScope::repo(self.workspace_root.to_string_lossy())
            }
            memory_v2::get_memory_metrics::MetricScopeKind::User => {
                let mut scope =
                    lattice_core::metrics::MetricScope::session(self.session_id.clone());
                scope.kind = lattice_core::metrics::MetricScopeKind::User;
                scope.session_id = None;
                scope
            }
            memory_v2::get_memory_metrics::MetricScopeKind::Organization => {
                let mut scope =
                    lattice_core::metrics::MetricScope::session(self.session_id.clone());
                scope.kind = lattice_core::metrics::MetricScopeKind::Organization;
                scope.session_id = None;
                scope
            }
        };
        if let Some(range) = time_range {
            return scope.with_time_range(range.since, range.until);
        }
        scope
    }

    async fn enrich_workflow_bundle_relevance(
        &self,
        tool_name: &str,
        call_id: &str,
        request_scope: Option<String>,
        bundle: &mut WorkflowBundle,
    ) {
        let session_snapshot = self.session_metrics.lock().await.snapshot();
        let mut surface = MetricsSurface::new(
            self.workspace_root.to_string_lossy(),
            lattice_core::metrics::MetricsCollector::new(),
            Some(session_snapshot),
        );
        let report = surface.build_call_relevance_report(
            call_id.to_string(),
            tool_name,
            request_scope,
            bundle,
        );
        surface.record_call(report.clone());
        let Ok(report) = surface.collect_for_call(call_id, tool_name) else {
            return;
        };
        bundle.workflow_record.excluded_high_scoring_candidates = report
            .excluded_high_scoring_candidates
            .iter()
            .map(|candidate| format!("{}: {}", candidate.label, candidate.rejection_reason))
            .collect();
        let compact_mode = matches!(
            bundle.render_choice.mode.as_str(),
            "compact" | "focused" | "tiny"
        );

        for (pivot, detail) in bundle.ranked_pivots.iter_mut().zip(report.pivots.iter()) {
            let focus = format!("memory:{}", detail.pivot_key);
            let seed = ExpandContextSeed {
                query: Some(detail.label.clone()),
                files: pivot.file.clone().into_iter().collect(),
                symbols: pivot.symbol.clone().into_iter().collect(),
                tests: Vec::new(),
                memories: vec![detail_payload(
                    &detail.label,
                    &detail.pivot_key,
                    "pivot",
                    &detail.inclusion_reason,
                    &detail.breakdown,
                )],
            };
            let handle = self.store_context_handle("relevance_detail", seed).await;
            pivot.relevance_detail_handle = Some(handle.legacy_handle);
            pivot.relevance_detail_focus = Some(focus);
            if compact_mode {
                pivot.relevance_summary =
                    Some(surface.summarize_for_compact_mode(&detail.breakdown));
                pivot.relevance_breakdown = None;
            } else {
                pivot.relevance_summary = None;
                pivot.relevance_breakdown = Some(detail.breakdown.clone());
            }
        }

        for (memory, detail) in bundle
            .memory_highlights
            .iter_mut()
            .zip(report.memories.iter())
        {
            let focus = format!("memory:{}", detail.memory_key);
            let seed = ExpandContextSeed {
                query: Some(memory.content.clone()),
                files: Vec::new(),
                symbols: Vec::new(),
                tests: Vec::new(),
                memories: vec![detail_payload(
                    &memory.content,
                    &detail.memory_key,
                    "memory",
                    &detail.inclusion_reason,
                    &detail.breakdown,
                )],
            };
            let handle = self.store_context_handle("relevance_detail", seed).await;
            memory.relevance_detail_handle = Some(handle.legacy_handle);
            memory.relevance_detail_focus = Some(focus);
            if compact_mode {
                memory.relevance_summary =
                    Some(surface.summarize_for_compact_mode(&detail.breakdown));
                memory.relevance_breakdown = None;
            } else {
                memory.relevance_summary = None;
                memory.relevance_breakdown = Some(detail.breakdown.clone());
            }
        }
    }

    async fn tool_verify_explain_memory(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed = memory_v2::verify_explain_memory::parse_args(args)
            .map_err(|message| (-32602, message))?;
        let scope_filter = self.current_memory_scope_filter();
        let store = self.memory_store.lock().await;
        let indexer = self.indexer.lock().await;
        let graph_store = self.graph_store.lock().await;
        let mut reports = self.verify_explain_reports.lock().await;
        let execution = memory_v2::verify_explain_memory::execute(
            &store,
            &indexer,
            &graph_store,
            &self.workspace_root,
            &scope_filter,
            &mut reports,
            parsed.clone(),
        )
        .map_err(|message| (-32603, message))?;
        drop(reports);
        drop(graph_store);
        drop(indexer);
        drop(store);

        let handle = self
            .store_context_handle("verify_explain_memory", execution.expansion_seed.clone())
            .await;
        let response = memory_v2::verify_explain_memory::render_response(
            &execution.report,
            parsed.render_mode,
            handle.legacy_handle,
        );
        tracing::info!(
            tool = "verify_explain_memory",
            memory_id = execution.memory_id.ulid.as_str(),
            scope = execution.memory_id.workspace_id.as_str(),
            status = execution.report.status.as_str(),
            check_count = response.checks.len(),
            "memory verification completed"
        );
        if let Some(capture) = &self.event_capture {
            let reason = vec![format!(
                "verify_explain_memory:{}",
                execution.report.status.as_str()
            )];
            if let Err(error) =
                capture.record_memory_retrieved(&[execution.memory_id.clone()], &reason)
            {
                tracing::warn!(tool = "verify_explain_memory", %error, "failed to capture memory retrieval");
            }
            if execution.prior_status == lattice_core::verification::VerificationStatus::Verified
                && execution.report.status
                    != lattice_core::verification::VerificationStatus::Verified
            {
                if let Err(error) = capture.record_memory_invalidated(
                    execution.memory_id.clone(),
                    execution
                        .report
                        .summary_lines
                        .first()
                        .map(String::as_str)
                        .unwrap_or("verification status changed away from verified"),
                ) {
                    tracing::warn!(tool = "verify_explain_memory", %error, "failed to capture invalidation");
                }
            }
        }
        serde_json::to_value(&response)
            .map(wrap_tool_result)
            .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn tool_list_memory_conflicts(&self, args: &Value) -> Result<Value, (i32, String)> {
        let parsed = memory_v2::list_memory_conflicts::parse_args(args)
            .map_err(|message| (-32602, message))?;
        let scope_filter = self.current_memory_scope_filter();
        let store = self.memory_store.lock().await;
        let execution = memory_v2::list_memory_conflicts::execute(&store, &scope_filter, parsed)
            .map_err(|message| (-32603, message))?;
        tracing::info!(
            tool = "list_memory_conflicts",
            scope = scope_filter.workspace_id.as_str(),
            status = "ok",
            check_count = execution.response.conflicts.len(),
            "memory conflicts listed"
        );
        if let Some(capture) = &self.event_capture {
            let reasons =
                vec!["list_memory_conflicts".to_string(); execution.surfaced_memory_ids.len()];
            if let Err(error) =
                capture.record_memory_retrieved(&execution.surfaced_memory_ids, &reasons)
            {
                tracing::warn!(tool = "list_memory_conflicts", %error, "failed to capture conflict retrieval");
            }
        }
        serde_json::to_value(&execution.response)
            .map(wrap_tool_result)
            .map_err(|error| (-32603, format!("Serialization error: {error}")))
    }

    async fn load_working_memory_state(
        &self,
        task_id: &str,
    ) -> Result<Option<(WorkingMemoryState, Option<i64>)>, (i32, String)> {
        if let Some(state) = self
            .working_memory_states
            .lock()
            .await
            .get(task_id)
            .cloned()
        {
            return Ok(Some((state, None)));
        }

        let scope = CheckpointScope::new(
            self.workspace_root.to_string_lossy().to_string(),
            self.session_id.clone(),
            task_id.to_string(),
        );
        let checkpoint = self
            .memory_store
            .lock()
            .await
            .load_latest_working_memory_checkpoint(&scope)
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to load working memory checkpoint for `{task_id}`: {error}"),
                )
            })?;
        if let Some((checkpoint_id, state)) = checkpoint {
            self.working_memory_states
                .lock()
                .await
                .insert(task_id.to_string(), state.clone());
            return Ok(Some((state, Some(checkpoint_id))));
        }
        Ok(None)
    }

    fn build_implicit_working_memory_state(
        &self,
        task_id: &str,
        task_statement: Option<&str>,
        intent_hint: Option<&str>,
        focus_files: &[String],
    ) -> WorkingMemoryState {
        let statement = task_statement
            .filter(|value| !value.trim().is_empty())
            .or(intent_hint.filter(|value| !value.trim().is_empty()))
            .unwrap_or(task_id);
        let mut state = WorkingMemoryState::new(statement);
        self.apply_task_focus_to_state(&mut state, focus_files);
        state
    }

    fn apply_task_focus_to_state(&self, state: &mut WorkingMemoryState, focus_files: &[String]) {
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        for path in focus_files {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                continue;
            }
            state.active_files.insert(lattice_core::identity::FileId {
                workspace_id: workspace_id.clone(),
                repo_relative_path: trimmed.to_string(),
                content_hash: "focus".to_string(),
            });
        }
    }

    async fn store_working_memory_snapshot(
        &self,
        task_id: &str,
        state: &WorkingMemoryState,
    ) -> String {
        let handle = self
            .store_context_handle(
                "inspect_working_memory",
                ExpandContextSeed {
                    query: Some(format!("inspect working memory {task_id}")),
                    files: Vec::new(),
                    symbols: Vec::new(),
                    tests: Vec::new(),
                    memories: Vec::new(),
                },
            )
            .await;
        self.working_memory_snapshots
            .lock()
            .await
            .insert(handle.legacy_handle.clone(), state.clone());
        handle.legacy_handle
    }

    async fn checkpoint_working_memory_state(
        &self,
        task_id: &str,
        state: &WorkingMemoryState,
        reason: &str,
        force: bool,
    ) -> Result<Option<i64>, (i32, String)> {
        let state_hash = serde_json::to_string(state).map_err(|error| {
            (
                -32603,
                format!("Failed to serialize working memory state: {error}"),
            )
        })?;
        if !force {
            let hashes = self.working_memory_checkpoint_hashes.lock().await;
            if hashes.get(task_id) == Some(&state_hash) {
                return Ok(None);
            }
        }
        let scope = CheckpointScope::new(
            self.workspace_root.to_string_lossy().to_string(),
            self.session_id.clone(),
            task_id.to_string(),
        );
        let checkpoint_id = self
            .memory_store
            .lock()
            .await
            .save_working_memory_checkpoint_for_scope(state, &format!("auto:{reason}"), &scope)
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to save working memory checkpoint for `{task_id}`: {error}"),
                )
            })?;
        self.working_memory_checkpoint_hashes
            .lock()
            .await
            .insert(task_id.to_string(), state_hash);
        Ok(Some(checkpoint_id))
    }

    #[cfg(test)]
    pub(crate) async fn remember_working_memory_state_for_test(
        &self,
        task_id: &str,
        state: WorkingMemoryState,
    ) {
        self.working_memory_states
            .lock()
            .await
            .insert(task_id.to_string(), state);
    }

    #[cfg(test)]
    pub(crate) async fn resolve_working_memory_snapshot_for_test(
        &self,
        handle: &str,
    ) -> Option<WorkingMemoryState> {
        self.working_memory_snapshots
            .lock()
            .await
            .get(handle)
            .cloned()
    }

    async fn tool_search_logic_flow(&self, args: &Value) -> Result<Value, (i32, String)> {
        let from_name = args["from"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: from".to_string()))?;
        let to_name = args["to"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: to".to_string()))?;
        let from_file = args["from_file"].as_str();
        let to_file = args["to_file"].as_str();
        let max_depth = (args["max_depth"].as_u64().unwrap_or(5) as usize).min(15);

        let engine = self.engine.lock().await;
        let all_nodes = engine.graph().all_nodes();

        // Find source symbol — supports exact, qualified (Class.method), and suffix matching
        let from_node = find_symbol_fuzzy(&all_nodes, from_name, from_file);
        // Find target symbol
        let to_node = find_symbol_fuzzy(&all_nodes, to_name, to_file);

        let (from_node, to_node) = match (from_node, to_node) {
            (Some(f), Some(t)) => (f, t),
            (None, _) => {
                return Ok(wrap_tool_result(json!({
                    "error": format!("Source symbol '{}' not found", from_name)
                })))
            }
            (_, None) => {
                return Ok(wrap_tool_result(json!({
                    "error": format!("Target symbol '{}' not found", to_name)
                })))
            }
        };

        let paths = engine
            .graph()
            .find_call_paths(&from_node.id, &to_node.id, max_depth, 10);

        let path_values: Vec<Value> = paths
            .iter()
            .map(|path| {
                json!(path
                    .iter()
                    .map(|n| json!({
                        "s": n.name,
                        "f": n.file,
                        "l": n.line,
                        "k": n.kind.short_code()
                    }))
                    .collect::<Vec<_>>())
            })
            .collect();

        Ok(wrap_tool_result(json!({
            "from": from_name,
            "to": to_name,
            "paths": path_values,
            "count": path_values.len()
        })))
    }

    // ── Daemon Method Handlers ────────────────────────────────────────

    /// Handle `lattice/file_symbols` — symbols in a specific file with dependent counts.
    async fn handle_file_symbols(&self, params: &Value) -> Result<Value, (i32, String)> {
        let file = params["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;
        let mut file_nodes = engine.file_symbols(file);
        file_nodes.sort_by(|left, right| {
            left.line
                .cmp(&right.line)
                .then_with(|| left.name.cmp(&right.name))
        });

        let symbols: Vec<Value> = file_nodes
            .iter()
            .map(|n| {
                let dependents = engine.graph().get_dependents(&n.id);
                let dependent_files: HashSet<&str> =
                    dependents.iter().map(|(d, _)| d.file.as_str()).collect();
                let dependent_file_count = dependent_files.len();

                json!({
                    "name": n.name,
                    "line": n.line,
                    "character": 0,
                    "kind": n.kind.short_code(),
                    "dependentCount": dependents.len(),
                    "dependentFileCount": dependent_file_count,
                    "fileCount": dependent_file_count
                })
            })
            .collect();

        Ok(json!({ "symbols": symbols }))
    }

    /// Handle `lattice/symbol_info` — detailed info about a single symbol.
    async fn handle_symbol_info(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = params["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;
        let _line = params["line"].as_u64().unwrap_or(0);

        let engine = self.engine.lock().await;
        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let cross_repo_count = 0usize; // Cross-repo is 0 for single workspace

                // Top 3 callers: dependents that call this symbol
                let top_callers: Vec<String> = dependents
                    .iter()
                    .take(3)
                    .map(|(d, _)| format!("{}:{}", d.file, d.name))
                    .collect();

                // Hotspot score from edit_count
                let hotspot = n.edit_count as f64;

                Ok(json!({
                    "name": n.name,
                    "dependentCount": dependents.len(),
                    "crossRepoCount": cross_repo_count,
                    "topCallers": top_callers,
                    "hotspot": hotspot,
                    "lastModified": n.last_modified.to_string()
                }))
            }
            None => Err((-32602, format!("Symbol '{}' not found in '{}'", name, file))),
        }
    }

    /// Handle `lattice/dependents` — list of dependent symbols.
    async fn handle_dependents(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = params["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;
        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dep_values: Vec<Value> = dependents
                    .iter()
                    .map(|(dep, edge)| {
                        json!({
                            "name": dep.name,
                            "file": dep.file,
                            "line": dep.line,
                            "edge": format!("{:?}", edge)
                        })
                    })
                    .collect();

                Ok(json!(dep_values))
            }
            None => Err((-32602, format!("Symbol '{}' not found in '{}'", name, file))),
        }
    }

    /// Handle `lattice/clear_memory` or `lattice/clear` — clear all memories.
    async fn handle_clear_memory(&self) -> Result<Value, (i32, String)> {
        let store = self.memory_store.lock().await;
        let cleared = store
            .clear_all()
            .map_err(|e| (-32603, format!("Failed to clear memories: {}", e)))?;

        Ok(json!({
            "status": "ok",
            "cleared": cleared
        }))
    }

    /// Handle `lattice/reindex` — spawn background re-scan, return immediately.
    async fn handle_reindex(&self) -> Result<Value, (i32, String)> {
        if self
            .refresh_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(json!({
                "status": "already_running",
                "message": "A workspace refresh is already running or queued"
            }));
        }
        let workspace_roots = self.workspace_roots.clone();
        let indexer = Arc::clone(&self.indexer);
        let engine = Arc::clone(&self.engine);
        let graph_store = Arc::clone(&self.graph_store);
        let indexing = Arc::clone(&self.indexing);
        let embedding_engine = Arc::clone(&self.embedding_engine);
        let vector_index = self.vector_index.clone();
        let index_work = Arc::clone(&self.index_work);
        let refresh_running = Arc::clone(&self.refresh_running);
        let index_health = Arc::clone(&self.index_health);
        let workspace_key = self.workspace_root.to_string_lossy().to_string();

        indexing.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            let _index_permit = index_work
                .acquire(workspace_key, "explicit_reindex")
                .await
                .expect("index work coordinator remains open for the process lifetime");
            let (manifest, parsed_cache) = load_incremental_cache(&graph_store).await;
            let roots = workspace_roots.clone();
            let incremental = match tokio::task::spawn_blocking(move || {
                build_incremental_index_for_roots(&roots, Some(&manifest), parsed_cache)
            })
            .await
            {
                Ok(incremental) => incremental,
                Err(error) => {
                    tracing::error!(%error, "Reindex worker failed; keeping the previously published graph");
                    indexing.store(false, Ordering::Relaxed);
                    refresh_running.store(false, Ordering::Release);
                    return;
                }
            };

            let Some(incremental) = persist_incremental_cache(&graph_store, incremental).await
            else {
                indexing.store(false, Ordering::Relaxed);
                refresh_running.store(false, Ordering::Release);
                return;
            };
            let IncrementalIndexResult {
                graph: new_graph,
                parsed_files,
                file_index,
                index_report,
                ..
            } = incremental;
            index_health.replace_from_report(&index_report);
            {
                let mut idx = indexer.lock().await;
                idx.replace_shared_index(Arc::clone(&new_graph), parsed_files);
            }

            let mut eng = engine.lock().await;
            eng.update_graph_arc(std::sync::Arc::clone(&new_graph));
            drop(eng);

            if let (Some(embedding_engine), Some(vector_index)) =
                (embedding_engine.get(), vector_index.as_ref())
            {
                let graph_for_sync = Arc::clone(&new_graph);
                let embedding_for_sync = Arc::clone(embedding_engine);
                let vector_for_sync = Arc::clone(vector_index);
                match tokio::task::spawn_blocking(move || {
                    crate::vector_sync::sync_full_graph_embeddings(
                        &graph_for_sync,
                        embedding_for_sync.as_ref(),
                        vector_for_sync.as_ref(),
                    )
                })
                .await
                {
                    Ok(Ok(stats)) => tracing::info!(
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
                        throughput_nodes_per_sec = stats.throughput_nodes_per_sec(),
                        "Reindex semantic sync complete"
                    ),
                    Ok(Err(err)) => {
                        tracing::warn!("Reindex graph updated but semantic sync failed: {}", err)
                    }
                    Err(err) => tracing::warn!("Reindex semantic sync worker failed: {}", err),
                }
            }

            indexing.store(false, Ordering::Relaxed);
            refresh_running.store(false, Ordering::Release);
            tracing::info!(
                "Reindex complete: {} files indexed, {} errors",
                file_index.len(),
                0
            );
        });

        Ok(json!({
            "status": "started",
            "message": "Re-index started in background"
        }))
    }
}

fn normalized_search_terms(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .filter_map(|term| {
            let term = term.trim().to_ascii_lowercase();
            (!term.is_empty()).then_some(term)
        })
        .collect()
}

fn search_node_match(
    pattern_lower: &str,
    pattern_terms: &[String],
    name: &str,
    file: &str,
) -> Option<(u8, &'static str)> {
    let name_lower = name.to_ascii_lowercase();
    let file_lower = file.to_ascii_lowercase();
    if name_lower == pattern_lower {
        return Some((5, "exact_name"));
    }
    if file_lower == pattern_lower || file_lower.ends_with(&format!("/{pattern_lower}")) {
        return Some((5, "exact_file"));
    }
    if name_lower.contains(pattern_lower) {
        return Some((4, "name_substring"));
    }
    if file_lower.contains(pattern_lower) {
        return Some((3, "file_substring"));
    }
    if !pattern_terms.is_empty()
        && pattern_terms
            .iter()
            .all(|term| name_lower.contains(term) || file_lower.contains(term))
    {
        return Some((2, "all_terms"));
    }
    None
}

fn status_snapshot_from_graph(graph: &CodeGraph, include_languages: bool) -> StatusSnapshot {
    let languages = include_languages.then(|| {
        let mut counts = std::collections::HashMap::new();
        for node in graph.all_nodes() {
            *counts.entry(format!("{:?}", node.language)).or_insert(0) += 1;
        }
        counts
    });

    StatusSnapshot {
        stats: graph.stats(),
        languages,
    }
}

fn indexing_workflow_response(tool_name: &str, query: &str, reason: &str) -> Value {
    let overview = if reason == "branch_switch" {
        "The workspace branch changed and the graph is being refreshed; retry shortly or use rg for exact literal lookup."
    } else if reason == "workspace_change" {
        "The workspace changed substantially and the graph is being refreshed; retry shortly or use rg for exact literal lookup."
    } else {
        "Indexing is still in progress and the graph is temporarily busy; retry shortly or use rg for exact literal lookup."
    };
    json!({
        "query": query,
        "overview": overview,
        "indexing": true,
        "reason": reason,
        "primary_files": [],
        "symbols": [],
        "tests": [],
        "rationale": [
            if reason == "branch_switch" {
                format!("{tool_name} returned a bounded response because the workspace branch changed and the previous graph may point at the wrong branch.")
            } else if reason == "workspace_change" {
                format!("{tool_name} returned a bounded response because the workspace changed substantially and the previous graph may be stale.")
            } else {
                format!("{tool_name} returned a bounded indexing response instead of waiting on the graph lock.")
            }
        ],
        "suggested_expand": {
            "focus": "index_status",
            "reason": if reason == "branch_switch" {
                "Check index_status, then retry once the branch-refresh publish completes."
            } else if reason == "workspace_change" {
                "Check index_status, then retry once the workspace-refresh publish completes."
            } else {
                "Check index_status, then retry the workflow once the graph is ready."
            }
        }
    })
}

fn busy_query_workflow_response(tool_name: &str, query: &str) -> Value {
    json!({
        "query": query,
        "overview": "The bounded query workers are busy; retry shortly. Status and exact structural search remain available.",
        "partial": true,
        "reason": "query_capacity",
        "primary_files": [],
        "symbols": [],
        "tests": [],
        "rationale": [
            format!("{tool_name} returned immediately instead of creating unbounded CPU work or blocking latency-sensitive daemon operations.")
        ],
        "suggested_expand": {
            "focus": "retry",
            "reason": "Retry after an active context query completes."
        }
    })
}

fn workflow_graph_is_empty(graph: &CodeGraph) -> bool {
    graph.stats().node_count == 0
}

fn apply_lsp_edge_to_graph(
    graph: &mut lattice_core::graph::CodeGraph,
    edge: &Value,
    added: &mut usize,
    skipped: &mut usize,
    skip_reasons: &mut Vec<Value>,
) {
    let from_name = edge["from_name"].as_str().unwrap_or("");
    let from_file = edge["from_file"].as_str().unwrap_or("");
    let to_name = edge["to_name"].as_str().unwrap_or("");
    let to_file = edge["to_file"].as_str().unwrap_or("");
    let kind_str = edge["kind"].as_str().unwrap_or("Calls");

    let edge_kind = match kind_str {
        "Calls" | "C" => lattice_core::graph::model::EdgeKind::Calls,
        "Imports" | "I" => lattice_core::graph::model::EdgeKind::Imports,
        "TypeRef" | "T" => lattice_core::graph::model::EdgeKind::TypeRef,
        "Implements" | "M" => lattice_core::graph::model::EdgeKind::Implements,
        "Extends" | "E" => lattice_core::graph::model::EdgeKind::Extends,
        _ => lattice_core::graph::model::EdgeKind::Calls,
    };

    let from_id = graph
        .all_nodes()
        .iter()
        .find(|n| n.name == from_name && n.file == from_file)
        .map(|n| n.id.clone());
    let to_id = graph
        .all_nodes()
        .iter()
        .find(|n| n.name == to_name && n.file == to_file)
        .map(|n| n.id.clone());

    match (from_id, to_id) {
        (Some(fid), Some(tid)) => {
            graph.add_edge(&fid, &tid, edge_kind);
            *added += 1;
        }
        (None, None) => {
            skip_reasons.push(json!({
                "from": from_name, "to": to_name,
                "reason": format!("both '{}::{}' and '{}::{}' not found in graph", from_file, from_name, to_file, to_name)
            }));
            *skipped += 1;
        }
        (None, Some(_)) => {
            skip_reasons.push(json!({
                "from": from_name, "to": to_name,
                "reason": format!("source '{}::{}' not found in graph", from_file, from_name)
            }));
            *skipped += 1;
        }
        (Some(_), None) => {
            skip_reasons.push(json!({
                "from": from_name, "to": to_name,
                "reason": format!("target '{}::{}' not found in graph", to_file, to_name)
            }));
            *skipped += 1;
        }
    }
}

#[async_trait::async_trait]
impl RequestHandler for McpHandler {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)> {
        match method {
            "initialize" => {
                self.remember_client_info(&params).await;
                Ok(self.handle_initialize())
            }
            "tools/list" => Ok(self.handle_agent_tools_list()),
            "tools/call" => self.handle_agent_tools_call(&params).await,
            "lattice/tools/list_all" => Ok(self.handle_tools_list()),
            "lattice/tool_call" | "lattice/tools/call" => self.handle_tools_call(&params).await,
            "ping" => Ok(json!({})),
            "lattice/status" => {
                let is_indexing = self.is_indexing();
                let snapshot = self.current_status_snapshot(false).await;
                Ok(json!({
                    "status": if is_indexing { "indexing" } else { "ready" },
                    "version": env!("CARGO_PKG_VERSION"),
                    "workspace": self.workspace_root.to_string_lossy(),
                    "nodes": snapshot.stats.node_count,
                    "edges": snapshot.stats.edge_count,
                    "files": snapshot.stats.file_count,
                    "index_work": self.index_work.snapshot()
                }))
            }
            "lattice/reindex" => self.handle_reindex().await,
            "lattice/file_symbols" => self.handle_file_symbols(&params).await,
            "lattice/symbol_info" => self.handle_symbol_info(&params).await,
            "lattice/dependents" => self.handle_dependents(&params).await,
            "lattice/clear_memory" | "lattice/clear" => self.handle_clear_memory().await,
            // MCP notifications — acknowledge silently
            "notifications/initialized"
            | "notifications/cancelled"
            | "notifications/progress"
            | "notifications/roots/list_changed" => Ok(json!({})),
            _ => Err((-32601, format!("Method not found: {}", method))),
        }
    }
}

// ── Helper Functions ──────────────────────────────────────────────────

/// Find a symbol by name with fuzzy matching.
/// Supports:
///   1. Exact match: "TokenBlacklist.is_blacklisted"
///   2. Suffix match: "is_blacklisted" matches "TokenBlacklist.is_blacklisted"
///   3. Unqualified match: "is_blacklisted" matches the method name part
/// Optional file filter narrows results.
fn find_symbol_fuzzy<'a>(
    nodes: &'a [&lattice_core::graph::model::GraphNode],
    name: &str,
    file_filter: Option<&str>,
) -> Option<&'a lattice_core::graph::model::GraphNode> {
    let matches_file =
        |n: &&lattice_core::graph::model::GraphNode| file_filter.map_or(true, |f| n.file == f);

    // 1. Exact match
    if let Some(node) = nodes.iter().find(|n| n.name == name && matches_file(n)) {
        return Some(node);
    }

    // 2. Suffix match: "is_blacklisted" matches "TokenBlacklist.is_blacklisted"
    let dot_suffix = format!(".{}", name);
    if let Some(node) = nodes
        .iter()
        .find(|n| n.name.ends_with(&dot_suffix) && matches_file(n))
    {
        return Some(node);
    }

    // 3. Case-insensitive exact match
    let name_lower = name.to_lowercase();
    if let Some(node) = nodes
        .iter()
        .find(|n| n.name.to_lowercase() == name_lower && matches_file(n))
    {
        return Some(node);
    }

    // 4. Case-insensitive suffix match
    let dot_suffix_lower = format!(".{}", name_lower);
    nodes
        .iter()
        .find(|n| n.name.to_lowercase().ends_with(&dot_suffix_lower) && matches_file(n))
        .copied()
}

fn parse_string_array(args: &Value, key: &str) -> Vec<String> {
    args[key]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn clone_object_value(value: &Value) -> Value {
    value
        .as_object()
        .map(|object| Value::Object(object.clone()))
        .unwrap_or_else(|| json!({}))
}

fn set_value(value: &mut Value, key: &str, item: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_string(), item);
    }
}

fn set_string(value: &mut Value, key: &str, item: &str) {
    set_value(value, key, Value::String(item.to_string()));
}

fn remove_key(value: &mut Value, key: &str) {
    if let Some(object) = value.as_object_mut() {
        object.remove(key);
    }
}

fn first_string(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| args[*key].as_str().map(ToString::to_string))
}

fn copy_first_string(value: &mut Value, args: &Value, keys: &[&str], destination: &str) {
    if value[destination].as_str().is_some() {
        return;
    }
    if let Some(item) = first_string(args, keys) {
        set_string(value, destination, &item);
    }
}

fn normalize_target_fields(value: &mut Value, args: &Value) {
    match &args["target"] {
        Value::Object(target) => {
            for key in ["name", "file", "diff"] {
                if value[key].is_null() {
                    if let Some(item) = target.get(key).cloned() {
                        set_value(value, key, item);
                    }
                }
            }
            if value["files"].is_null() {
                if let Some(item) = target.get("files").cloned() {
                    set_value(value, "files", item);
                }
            }
            if value["symbols"].is_null() {
                if let Some(item) = target.get("symbols").cloned() {
                    set_value(value, "symbols", item);
                }
            }
        }
        Value::String(target) => {
            if value["name"].is_null() && !target.contains('\n') && !target.contains("diff --git") {
                set_string(value, "name", target);
                if value["symbols"].is_null() {
                    set_value(value, "symbols", json!([target]));
                }
            }
            if value["diff"].is_null() && (target.contains('\n') || target.contains("diff --git")) {
                set_string(value, "diff", target);
            }
        }
        _ => {}
    }
    if !value["file"].is_null() && value["files"].is_null() {
        if let Some(file) = value["file"].as_str() {
            set_value(value, "files", json!([file]));
        }
    }
    if !value["name"].is_null() && value["symbols"].is_null() {
        if let Some(name) = value["name"].as_str() {
            set_value(value, "symbols", json!([name]));
        }
    }
}

fn unwrap_tool_text_json(value: &Value) -> Option<Value> {
    value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str(text).ok())
}

fn parse_requested_bundle_mode(args: &Value) -> RequestedBundleMode {
    match args["mode"].as_str().unwrap_or("auto") {
        "full" => RequestedBundleMode::Full,
        "compact" => RequestedBundleMode::Compact,
        _ => RequestedBundleMode::Auto,
    }
}

fn parse_workflow_response_options(args: &Value) -> Result<WorkflowResponseOptions, (i32, String)> {
    let budget = match args["budget"].as_str() {
        Some("tiny") => WorkflowBudget::Tiny,
        Some("compact") => WorkflowBudget::Compact,
        Some("full") => WorkflowBudget::Full,
        _ => WorkflowBudget::Auto,
    };
    let wire_format = match args["wire_format"].as_str() {
        Some("dense") => WorkflowWireFormat::Dense,
        Some("standard") => WorkflowWireFormat::Standard,
        _ => WorkflowWireFormat::Auto,
    };
    let render = match args["render"].as_str() {
        Some("json") => WorkflowRenderMode::Json,
        Some("markdown") | None => WorkflowRenderMode::Markdown,
        Some(render) => {
            return Err((
                -32602,
                format!("Invalid render value: {render}. Expected 'markdown' or 'json'."),
            ));
        }
    };
    let max_tokens = args["max_tokens"]
        .as_u64()
        .map(|value| (value as usize).clamp(80, 4000));

    Ok(WorkflowResponseOptions {
        budget,
        max_tokens,
        wire_format,
        render,
    })
}

fn select_task_bundle_mode(
    requested: RequestedBundleMode,
    compact: &TaskBundle,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = task_bundle_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because likely edit anchors were already strong".to_string(),
                )
            }
        }
    }
}

fn select_plan_edit_mode(
    requested: RequestedBundleMode,
    compact: &PlanEditBundle,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = plan_edit_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the edit plan already mapped to concrete patch anchors"
                        .to_string(),
                )
            }
        }
    }
}

fn select_trace_scenario_mode(
    requested: RequestedBundleMode,
    compact: &ScenarioTraceBundle,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = trace_scenario_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the scenario trace already had concrete execution anchors"
                        .to_string(),
                )
            }
        }
    }
}

fn select_working_set_mode(
    requested: RequestedBundleMode,
    compact: &WorkingSetContext,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = working_set_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the working set already had enough local context"
                        .to_string(),
                )
            }
        }
    }
}

fn select_subsystem_summary_mode(
    requested: RequestedBundleMode,
    compact: &SubsystemSummary,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = subsystem_summary_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the subsystem map already had enough coverage"
                        .to_string(),
                )
            }
        }
    }
}

fn select_repo_playbook_mode(
    requested: RequestedBundleMode,
    compact: &RepoPlaybook,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = repo_playbook_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because architecture and conventions were already well covered"
                        .to_string(),
                )
            }
        }
    }
}

fn select_failure_diagnosis_mode(
    requested: RequestedBundleMode,
    compact: &FailureDiagnosis,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = failure_diagnosis_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because likely culprits and next steps were already clear"
                        .to_string(),
                )
            }
        }
    }
}

fn task_bundle_widen_reason(bundle: &TaskBundle) -> Option<&'static str> {
    let high_primary = bundle
        .primary_files
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();
    let high_symbols = bundle
        .symbols
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();

    if bundle.primary_files.is_empty() {
        Some("no primary edit files were identified")
    } else if high_primary == 0 && high_symbols == 0 {
        Some("the likely edit area is still low-confidence")
    } else if bundle.tests.is_empty() && bundle.primary_files.len() <= 1 && bundle.symbols.len() < 2
    {
        Some("supporting symbols and tests were still sparse")
    } else {
        None
    }
}

fn plan_edit_widen_reason(bundle: &PlanEditBundle) -> Option<&'static str> {
    let high_edit_files = bundle
        .edit_files
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();
    let high_symbols = bundle
        .symbols
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();

    if bundle.edit_files.is_empty() {
        Some("no likely edit files were identified")
    } else if high_edit_files == 0 && high_symbols == 0 {
        Some("edit anchors were still low-confidence")
    } else if bundle.candidate_spans.is_empty()
        && bundle.affected_callers.is_empty()
        && bundle.affected_dependencies.is_empty()
    {
        Some("candidate spans and downstream impact were too sparse")
    } else if bundle.tests.is_empty()
        && bundle.relevant_docs.is_empty()
        && bundle.stale_doc_signals.is_empty()
    {
        Some("test and documentation guidance were still sparse")
    } else {
        None
    }
}

fn trace_scenario_widen_reason(bundle: &ScenarioTraceBundle) -> Option<&'static str> {
    let high_entrypoints = bundle
        .likely_entrypoints
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();
    let high_paths = bundle
        .execution_path
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();

    if bundle.likely_entrypoints.is_empty() {
        Some("no likely scenario entrypoints were identified")
    } else if high_entrypoints == 0 && high_paths == 0 {
        Some("entrypoint and path confidence were still low")
    } else if bundle.execution_path.is_empty() && bundle.plausible_paths.is_empty() {
        Some("execution path candidates were still sparse")
    } else if bundle.guards.is_empty()
        && bundle.side_effects.is_empty()
        && bundle.failure_branches.is_empty()
    {
        Some("guard, side-effect, and failure signals were still sparse")
    } else if bundle.tests.is_empty() && bundle.relevant_docs.is_empty() {
        Some("tests and docs guidance were still sparse")
    } else {
        None
    }
}

fn working_set_widen_reason(report: &WorkingSetContext) -> Option<&'static str> {
    if report.files.is_empty() {
        Some("the working set did not produce any anchored files")
    } else if report.files.len() == 1
        && report.active_symbols.len() + report.nearby_symbols.len() < 3
        && report.tests.is_empty()
    {
        Some("the working set was still too thin to avoid additional lookups")
    } else {
        None
    }
}

fn subsystem_summary_widen_reason(report: &SubsystemSummary) -> Option<&'static str> {
    if report.key_files.len() < 2 {
        Some("the compact summary surfaced too few key files")
    } else if report.key_symbols.is_empty() && report.tests.is_empty() && report.memories.is_empty()
    {
        Some("the compact summary lacked symbol, test, and memory coverage")
    } else {
        None
    }
}

fn repo_playbook_widen_reason(report: &RepoPlaybook) -> Option<&'static str> {
    if report.key_files.len() < 2 {
        Some("the compact playbook surfaced too few anchor files")
    } else if report.notable_symbols.is_empty() && report.conventions.len() < 2 {
        Some("the compact playbook lacked notable symbols and conventions")
    } else {
        None
    }
}

fn failure_diagnosis_widen_reason(report: &FailureDiagnosis) -> Option<&'static str> {
    let top_suspect_high = report
        .suspects
        .first()
        .map(|item| item.confidence_band == "high")
        .unwrap_or(false);

    if report.suspects.is_empty() {
        Some("the compact diagnosis did not identify concrete suspects")
    } else if !top_suspect_high && report.tests.is_empty() {
        Some("the diagnosis was not yet strong enough to anchor the next move")
    } else if report.likely_causes.is_empty() && report.related_symbols.len() < 2 {
        Some("the diagnosis lacked enough cause or nearby-context detail")
    } else {
        None
    }
}

fn should_try_prepare_change_semantic_fallback(
    capsule: &ContextCapsule,
    entry_files: &[String],
    entry_symbols: &[String],
) -> bool {
    if capsule.pivots.is_empty() {
        return true;
    }

    if capsule.stats.seed_count <= 1 && capsule.context.len() < 3 {
        return true;
    }

    if !entry_files.is_empty()
        && !capsule
            .pivots
            .iter()
            .any(|item| entry_files.iter().any(|file| file == &item.file))
        && !capsule
            .context
            .iter()
            .any(|item| entry_files.iter().any(|file| file == &item.file))
    {
        return true;
    }

    !entry_symbols.is_empty()
        && !capsule.pivots.iter().any(|pivot| {
            entry_symbols
                .iter()
                .any(|symbol| symbol_matches_hint(&pivot.symbol, symbol))
        })
}

fn merge_unique_strings(primary: Vec<String>, secondary: Vec<String>) -> Vec<String> {
    let mut merged = Vec::new();
    for value in primary.into_iter().chain(secondary) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !merged.iter().any(|existing: &String| existing == trimmed) {
            merged.push(trimmed.to_string());
        }
    }
    merged
}

fn extract_workspace_file_references(query: &str, workspace_root: &Path) -> Vec<String> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut files = Vec::new();
    for token in query.split_whitespace() {
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
            )
        });
        let token = token.strip_prefix("file:").unwrap_or(token);
        let token = token.split('#').next().unwrap_or(token);
        if token.is_empty()
            || (!token.contains('/')
                && !token.contains('\\')
                && Path::new(token).extension().is_none())
        {
            continue;
        }
        let candidate = Path::new(token);
        let absolute = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            canonical_root.join(candidate)
        };
        let Ok(canonical) = absolute.canonicalize() else {
            continue;
        };
        if !canonical.is_file() {
            continue;
        }
        let Ok(relative) = canonical.strip_prefix(&canonical_root) else {
            continue;
        };
        let normalized = relative.to_string_lossy().replace('\\', "/");
        if !files.iter().any(|existing| existing == &normalized) {
            files.push(normalized);
        }
        if files.len() == 16 {
            break;
        }
    }
    files
}

fn prepare_change_capsule_quality(
    capsule: &ContextCapsule,
    entry_files: &[String],
    entry_symbols: &[String],
) -> usize {
    let pivot_file_hits = capsule
        .pivots
        .iter()
        .filter(|pivot| entry_files.iter().any(|file| file == &pivot.file))
        .count();
    let pivot_symbol_hits = capsule
        .pivots
        .iter()
        .filter(|pivot| {
            entry_symbols
                .iter()
                .any(|symbol| symbol_matches_hint(&pivot.symbol, symbol))
        })
        .count();

    capsule.pivots.len() * 4
        + capsule.context.len() * 2
        + capsule.stats.seed_count.min(4)
        + pivot_file_hits * 4
        + pivot_symbol_hits * 3
}

fn should_try_subsystem_semantic_fallback(
    report: &SubsystemSummary,
    files: &[String],
    symbols: &[String],
) -> bool {
    report.key_files.len() < 2
        || (report.key_symbols.is_empty() && !files.is_empty())
        || (report.key_symbols.len() < 2 && !symbols.is_empty())
}

fn subsystem_summary_quality(report: &SubsystemSummary) -> usize {
    report.key_files.len() * 4
        + report.key_symbols.len() * 3
        + report.tests.len() * 2
        + report.memories.len()
        + report
            .key_files
            .iter()
            .filter(|item| item.confidence_band == "high")
            .count()
}

fn merge_anchor_files_from_capsule(existing: &[String], capsule: &ContextCapsule) -> Vec<String> {
    let mut files = existing.to_vec();
    files.extend(
        capsule
            .pivots
            .iter()
            .map(|pivot| pivot.file.clone())
            .chain(capsule.context.iter().map(|context| context.file.clone()))
            .filter(|file| is_queryable_workflow_file(file)),
    );
    dedupe_string_values(&mut files);
    files.truncate(4);
    files
}

fn merge_anchor_symbols_from_capsule(existing: &[String], capsule: &ContextCapsule) -> Vec<String> {
    let mut symbols = existing.to_vec();
    symbols.extend(
        capsule
            .pivots
            .iter()
            .map(|pivot| pivot.symbol.clone())
            .chain(capsule.context.iter().map(|context| context.symbol.clone())),
    );
    dedupe_string_values(&mut symbols);
    symbols.truncate(6);
    symbols
}

fn is_queryable_workflow_file(file: &str) -> bool {
    !file.trim().is_empty() && should_index_file(file)
}

fn dedupe_string_values(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn symbol_matches_hint(candidate: &str, hint: &str) -> bool {
    candidate == hint || candidate.ends_with(&format!(".{}", hint))
}

fn count_outcome_memory_reuse(values: &[Value]) -> usize {
    values
        .iter()
        .filter(|value| {
            value
                .get("refresh_key")
                .and_then(|item| item.as_str())
                .map(|item| item.starts_with("workflow_outcome::"))
                .unwrap_or(false)
        })
        .count()
}

fn approx_value_tokens(value: &Value) -> usize {
    let bytes = serde_json::to_string(value)
        .map(|serialized| serialized.len())
        .unwrap_or_else(|_| value.to_string().len());
    (bytes / 4).max(1)
}

fn derive_session_pruning_profile(report: &SessionMetricsReport) -> SessionPruningProfile {
    SessionPruningProfile {
        prefer_tiny: report.workflow_tool_calls >= 6
            && report.follow_up_avoidance_rate >= 0.6
            && report.compact_to_expand_rate <= 0.35,
        prune_memory_highlights: report.workflow_tool_calls >= 6
            && report.outcome_memory_reuse_count == 0,
        prefer_dense: report.workflow_tool_calls >= 6
            && report.average_payload_tokens_per_tool >= 300,
    }
}

fn select_workflow_budget(
    tool_name: &str,
    value: &Value,
    metadata: &WorkflowRunMetadata,
    response_options: &WorkflowResponseOptions,
    pruning_profile: SessionPruningProfile,
) -> WorkflowBudget {
    let mut budget = match response_options.budget {
        WorkflowBudget::Tiny => WorkflowBudget::Tiny,
        WorkflowBudget::Compact => WorkflowBudget::Compact,
        WorkflowBudget::Full => WorkflowBudget::Full,
        WorkflowBudget::Auto => {
            if metadata.delivery_mode == "full" {
                WorkflowBudget::Full
            } else if workflow_is_high_confidence(tool_name, value) || pruning_profile.prefer_tiny {
                WorkflowBudget::Tiny
            } else {
                WorkflowBudget::Compact
            }
        }
    };

    if let Some(max_tokens) = response_options.max_tokens {
        if max_tokens <= 220 {
            budget = WorkflowBudget::Tiny;
        } else if max_tokens <= 500 && matches!(budget, WorkflowBudget::Full) {
            budget = WorkflowBudget::Compact;
        }
    }

    budget
}

fn select_workflow_wire_format(
    response_options: &WorkflowResponseOptions,
    pruning_profile: SessionPruningProfile,
    budget: WorkflowBudget,
    approx_tokens: usize,
) -> WorkflowWireFormat {
    match response_options.wire_format {
        WorkflowWireFormat::Dense => WorkflowWireFormat::Dense,
        WorkflowWireFormat::Standard => WorkflowWireFormat::Standard,
        WorkflowWireFormat::Auto => {
            if matches!(budget, WorkflowBudget::Tiny)
                && (pruning_profile.prefer_dense || approx_tokens > 260)
            {
                WorkflowWireFormat::Dense
            } else {
                WorkflowWireFormat::Standard
            }
        }
    }
}

fn default_workflow_token_cap(budget: WorkflowBudget) -> usize {
    match budget {
        WorkflowBudget::Tiny => TINY_WORKFLOW_TOKEN_CAP,
        WorkflowBudget::Auto | WorkflowBudget::Compact => COMPACT_WORKFLOW_TOKEN_CAP,
        WorkflowBudget::Full => FULL_WORKFLOW_TOKEN_CAP,
    }
}

fn apply_workflow_budget(
    tool_name: &str,
    value: &mut Value,
    budget: WorkflowBudget,
    pruning_profile: SessionPruningProfile,
    metadata: &mut WorkflowRunMetadata,
) {
    match budget {
        WorkflowBudget::Full => {}
        WorkflowBudget::Compact => {
            metadata.delivery_mode = "compact".to_string();
            apply_compact_workflow_pruning(value, pruning_profile);
        }
        WorkflowBudget::Tiny => {
            metadata.delivery_mode = "tiny".to_string();
            apply_compact_workflow_pruning(value, pruning_profile);
            let used_single_anchor = if workflow_is_high_confidence(tool_name, value) {
                apply_single_anchor_mode(tool_name, value, pruning_profile)
            } else {
                apply_tiny_workflow_pruning(tool_name, value, pruning_profile);
                false
            };
            metadata.single_anchor_used |= used_single_anchor;
        }
        WorkflowBudget::Auto => {}
    }
}

fn apply_compact_workflow_pruning(value: &mut Value, pruning_profile: SessionPruningProfile) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    object.remove("stats");
    object.remove("memories");
    object.remove("playbook_memory");
    truncate_array_field(object, "rationale", 0);
    truncate_array_field(object, "matched_rules", 1);
    truncate_array_field(object, "test_gaps", 1);

    if pruning_profile.prune_memory_highlights {
        truncate_array_field(object, "memory_highlights", 0);
        truncate_array_field(object, "durable_patterns", 0);
        truncate_array_field(object, "memories", 0);
    } else {
        truncate_array_field(object, "memory_highlights", 1);
        truncate_array_field(object, "durable_patterns", 1);
        shorten_memory_entries(object, "memory_highlights", 56);
        shorten_memory_entries(object, "durable_patterns", 56);
    }

    truncate_string_field(object, "overview", 128);
    truncate_array_strings_field(object, "likely_causes", 2, 72);
    truncate_array_strings_field(object, "next_steps", 2, 72);
    truncate_array_strings_field(object, "architecture", 2, 72);
    truncate_array_strings_field(object, "conventions", 2, 72);

    truncate_array_field(object, "pivots", 5);
    truncate_array_field(object, "context", 5);
    strip_array_object_field(object, "pivots", "source");
    shorten_array_object_string_field(object, "pivots", "reason", 96);
    shorten_array_object_string_field(object, "context", "relationship", 96);
}

fn apply_tiny_workflow_pruning(
    tool_name: &str,
    value: &mut Value,
    pruning_profile: SessionPruningProfile,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    truncate_string_field(object, "overview", 88);
    truncate_array_field(
        object,
        "memory_highlights",
        if pruning_profile.prune_memory_highlights {
            0
        } else {
            1
        },
    );
    shorten_memory_entries(object, "memory_highlights", 44);
    object.remove("playbook_memory");

    match tool_name {
        "prepare_change" => {
            truncate_array_field(object, "primary_files", 1);
            truncate_array_field(object, "secondary_files", 0);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "risks", 0);
        }
        "plan_edit" => {
            truncate_array_field(object, "edit_files", 1);
            truncate_array_field(object, "supporting_files", 0);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "candidate_spans", 1);
            truncate_array_field(object, "affected_callers", 1);
            truncate_array_field(object, "affected_dependencies", 0);
            truncate_array_field(object, "relevant_docs", 1);
            truncate_array_strings_field(object, "stale_doc_signals", 1, 72);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "risks", 0);
        }
        "trace_scenario" => {
            truncate_array_field(object, "likely_entrypoints", 1);
            truncate_array_field(object, "plausible_entrypoints", 0);
            truncate_array_field(object, "execution_path", 1);
            truncate_array_field(object, "plausible_paths", 0);
            truncate_array_field(object, "guards", 1);
            truncate_array_field(object, "side_effects", 1);
            truncate_array_field(object, "failure_branches", 1);
            truncate_array_field(object, "relevant_docs", 1);
            truncate_array_field(object, "tests", 1);
        }
        "impact_from_diff" => {
            truncate_array_field(object, "changed_files", 1);
            truncate_array_field(object, "changed_symbols", 1);
            truncate_array_field(object, "affected_symbols", 1);
            truncate_array_field(object, "review_checklist", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "risks", 0);
        }
        "get_working_set_context" => {
            truncate_array_field(object, "files", 1);
            truncate_array_field(object, "active_symbols", 1);
            truncate_array_field(object, "nearby_symbols", 1);
            truncate_array_field(object, "tests", 1);
        }
        "summarize_subsystem" => {
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "key_symbols", 1);
            truncate_array_field(object, "tests", 1);
        }
        "get_repo_playbook" => {
            truncate_array_field(object, "architecture", 1);
            truncate_array_field(object, "conventions", 1);
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "notable_symbols", 1);
            truncate_array_field(object, "durable_patterns", 1);
            shorten_memory_entries(object, "durable_patterns", 44);
        }
        "diagnose_failure" => {
            truncate_array_field(object, "extracted_files", 1);
            truncate_array_field(object, "extracted_symbols", 1);
            truncate_array_field(object, "suspects", 1);
            truncate_array_field(object, "related_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "likely_causes", 1);
            truncate_array_field(object, "next_steps", 1);
        }
        "get_context_capsule" => {
            truncate_array_field(object, "pivots", 3);
            truncate_array_field(object, "context", 3);
            strip_array_object_field(object, "pivots", "source");
            truncate_array_field(object, "memories", 0);
        }
        _ => {}
    }

    ensure_suggested_expand(tool_name, object);
}

fn apply_single_anchor_mode(
    tool_name: &str,
    value: &mut Value,
    pruning_profile: SessionPruningProfile,
) -> bool {
    apply_tiny_workflow_pruning(tool_name, value, pruning_profile);

    let Some(object) = value.as_object_mut() else {
        return false;
    };

    match tool_name {
        "prepare_change" => {
            truncate_array_field(object, "primary_files", 1);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "plan_edit" => {
            truncate_array_field(object, "edit_files", 1);
            truncate_array_field(object, "candidate_spans", 1);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "affected_callers", 0);
            truncate_array_field(object, "affected_dependencies", 0);
            truncate_array_field(object, "relevant_docs", 0);
            truncate_array_strings_field(object, "stale_doc_signals", 0, 72);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "trace_scenario" => {
            truncate_array_field(object, "likely_entrypoints", 1);
            truncate_array_field(object, "execution_path", 1);
            truncate_array_field(object, "guards", 1);
            truncate_array_field(object, "side_effects", 0);
            truncate_array_field(object, "failure_branches", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "plausible_entrypoints", 0);
            truncate_array_field(object, "plausible_paths", 0);
            truncate_array_field(object, "relevant_docs", 0);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "impact_from_diff" => {
            truncate_array_field(object, "changed_files", 1);
            truncate_array_field(object, "changed_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "review_checklist", 1);
            truncate_array_field(object, "affected_symbols", 0);
        }
        "get_working_set_context" => {
            truncate_array_field(object, "files", 1);
            truncate_array_field(object, "active_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "nearby_symbols", 0);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "summarize_subsystem" => {
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "key_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "memories", 0);
        }
        "get_repo_playbook" => {
            truncate_array_field(object, "architecture", 1);
            truncate_array_field(object, "conventions", 1);
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "notable_symbols", 1);
            truncate_array_field(object, "durable_patterns", 0);
        }
        "diagnose_failure" => {
            truncate_array_field(object, "suspects", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "likely_causes", 1);
            truncate_array_field(object, "next_steps", 1);
            truncate_array_field(object, "related_symbols", 0);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "get_context_capsule" => {
            truncate_array_field(object, "pivots", 1);
            truncate_array_field(object, "context", 2);
            strip_array_object_field(object, "pivots", "source");
            truncate_array_field(object, "memories", 0);
        }
        _ => return false,
    }

    ensure_suggested_expand(tool_name, object);
    true
}

fn workflow_is_high_confidence(tool_name: &str, value: &Value) -> bool {
    match tool_name {
        "prepare_change" => {
            first_confidence_band(value, "primary_files") == Some("high")
                || first_confidence_band(value, "symbols") == Some("high")
        }
        "plan_edit" => {
            first_confidence_band(value, "edit_files") == Some("high")
                || first_confidence_band(value, "candidate_spans") == Some("high")
                || array_len(value, "candidate_spans") == 1
        }
        "trace_scenario" => {
            first_confidence_band(value, "likely_entrypoints") == Some("high")
                || first_confidence_band(value, "execution_path") == Some("high")
                || array_len(value, "execution_path") == 1
        }
        "impact_from_diff" => {
            first_confidence_band(value, "tests") == Some("high")
                || array_len(value, "changed_symbols") == 1
                || array_len(value, "changed_files") == 1
        }
        "get_working_set_context" => {
            first_confidence_band(value, "files") == Some("high")
                || first_confidence_band(value, "active_symbols") == Some("high")
        }
        "summarize_subsystem" => {
            first_confidence_band(value, "key_files") == Some("high")
                || first_confidence_band(value, "key_symbols") == Some("high")
        }
        "get_repo_playbook" => {
            first_confidence_band(value, "key_files") == Some("high")
                || first_confidence_band(value, "notable_symbols") == Some("high")
        }
        "diagnose_failure" => first_confidence_band(value, "suspects") == Some("high"),
        _ => false,
    }
}

fn ensure_suggested_expand(tool_name: &str, object: &mut serde_json::Map<String, Value>) {
    if object.contains_key("suggested_expand") {
        return;
    }

    let suggestion = match tool_name {
        "prepare_change" => first_symbol_focus(object, "symbols", "top change anchor")
            .or_else(|| first_file_focus(object, "primary_files", "top file")),
        "plan_edit" => first_symbol_focus(object, "candidate_spans", "top candidate edit span")
            .or_else(|| first_symbol_focus(object, "symbols", "top edit symbol"))
            .or_else(|| first_file_focus(object, "candidate_spans", "top candidate edit span"))
            .or_else(|| first_file_focus(object, "edit_files", "top edit file")),
        "trace_scenario" => first_symbol_focus(
            object,
            "likely_entrypoints",
            "top likely scenario entrypoint",
        )
        .or_else(|| first_trace_path_focus(object, "execution_path", "top execution path segment"))
        .or_else(|| first_symbol_focus(object, "guards", "top guard signal"))
        .or_else(|| first_symbol_focus(object, "failure_branches", "top failure branch"))
        .or_else(|| first_symbol_focus(object, "side_effects", "top side effect signal"))
        .or_else(|| first_file_focus(object, "likely_entrypoints", "top likely scenario file"))
        .or_else(|| first_trace_path_focus(object, "plausible_paths", "top plausible path")),
        "impact_from_diff" => first_symbol_focus(object, "changed_symbols", "changed symbol")
            .or_else(|| first_file_focus(object, "changed_files", "changed file")),
        "get_working_set_context" => first_symbol_focus(object, "active_symbols", "active symbol")
            .or_else(|| first_file_focus(object, "files", "top file")),
        "summarize_subsystem" => first_symbol_focus(object, "key_symbols", "key symbol")
            .or_else(|| first_file_focus(object, "key_files", "key file")),
        "get_repo_playbook" => first_symbol_focus(object, "notable_symbols", "notable symbol")
            .or_else(|| first_file_focus(object, "key_files", "key file")),
        "diagnose_failure" => first_symbol_focus(object, "suspects", "top suspect")
            .or_else(|| first_file_focus(object, "extracted_files", "failure file")),
        _ => None,
    };

    if let Some(suggested_expand) = suggestion {
        object.insert("suggested_expand".to_string(), suggested_expand);
    }
}

fn first_file_focus(
    object: &serde_json::Map<String, Value>,
    key: &str,
    reason: &str,
) -> Option<Value> {
    let first = object.get(key)?.as_array()?.first()?;
    let file = if first.is_string() {
        first.as_str()?.to_string()
    } else {
        first.get("file")?.as_str()?.to_string()
    };

    Some(json!({
        "focus": stable_file_focus_value(&file),
        "reason": reason,
    }))
}

fn first_symbol_focus(
    object: &serde_json::Map<String, Value>,
    key: &str,
    reason: &str,
) -> Option<Value> {
    let first = object.get(key)?.as_array()?.first()?;
    let focus = first
        .get("symbol_handle")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .or_else(|| {
            first
                .get("symbol")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| format!("symbol:{}", item))
        })?;

    Some(json!({
        "focus": focus,
        "reason": reason,
    }))
}

fn first_trace_path_focus(
    object: &serde_json::Map<String, Value>,
    key: &str,
    reason: &str,
) -> Option<Value> {
    let first = object.get(key)?.as_array()?.first()?;
    let focus = first
        .get("to_symbol_handle")
        .or_else(|| first.get("from_symbol_handle"))
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .or_else(|| {
            first
                .get("to_symbol")
                .or_else(|| first.get("from_symbol"))
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| format!("symbol:{}", item))
        })
        .or_else(|| {
            first
                .get("to_file")
                .or_else(|| first.get("from_file"))
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(stable_file_focus_value)
        })?;

    Some(json!({
        "focus": focus,
        "reason": reason,
    }))
}

fn stable_file_focus_value(file: &str) -> String {
    let trimmed = file.trim();
    if trimmed.starts_with("file_id:") {
        return trimmed.to_string();
    }
    let normalized = file
        .strip_prefix("file:")
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or(trimmed)
        .trim();
    stable_file_handle(normalized)
}

fn attach_agent_retrieval_guidance(_tool_name: &str, value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let next_action = workflow_next_action(object);
    object.remove("agent_retrieval_contract");
    if let Some(next_action) = next_action {
        object.insert(
            "agent_retrieval_contract".to_string(),
            json!({ "next_action": next_action }),
        );
    }
}

fn workflow_next_action(object: &serde_json::Map<String, Value>) -> Option<String> {
    let existing = object
        .get("agent_retrieval_contract")
        .and_then(Value::as_object)
        .and_then(|contract| contract.get("next_action"))
        .and_then(Value::as_str);
    let suggested_expand = object
        .get("suggested_expand")
        .and_then(Value::as_object)
        .and_then(|suggestion| suggestion.get("focus"))
        .and_then(Value::as_str)
        .map(|focus| format!("Expand `{focus}`."));
    let next_step = object
        .get("next_steps")
        .and_then(Value::as_array)
        .and_then(|steps| steps.first())
        .and_then(Value::as_str);

    existing
        .or(suggested_expand.as_deref())
        .or(next_step)
        .map(str::trim)
        .filter(|action| !action.is_empty())
        .map(|action| truncate_text_value(action, 96))
}

fn attach_workflow_budget_metadata(
    value: &mut Value,
    budget: WorkflowBudget,
    max_tokens: usize,
    truncated: bool,
) {
    let approx_tokens = approx_value_tokens(value);
    if let Some(object) = value.as_object_mut() {
        object.insert("budget".to_string(), json!(budget.as_str()));
        object.insert("budget_max_tokens".to_string(), json!(max_tokens));
        object.insert("approx_tokens".to_string(), json!(approx_tokens));
        object.insert(
            "truncated".to_string(),
            json!(truncated || approx_tokens > max_tokens),
        );
    }
}

fn trim_value_for_token_budget(value: &mut Value, max_tokens: usize) {
    if approx_value_tokens(value) <= max_tokens {
        return;
    }

    let Some(object) = value.as_object_mut() else {
        return;
    };

    object.remove("rationale");
    object.remove("matched_rules");
    object.remove("test_gaps");
    object.remove("playbook_memory");
    truncate_array_field(object, "memory_highlights", 0);
    truncate_array_field(object, "durable_patterns", 0);
    truncate_array_field(object, "secondary_files", 0);
    truncate_array_field(object, "related_symbols", 0);
    truncate_array_field(object, "affected_symbols", 1);
    truncate_array_field(object, "risks", 0);
    truncate_array_field(object, "review_checklist", 1);
    truncate_array_field(object, "tests", 1);
    truncate_array_field(object, "symbols", 1);
    truncate_array_field(object, "suspects", 1);
    truncate_array_field(object, "primary_files", 1);
    truncate_array_field(object, "edit_files", 1);
    truncate_array_field(object, "supporting_files", 0);
    truncate_array_field(object, "candidate_spans", 1);
    truncate_array_field(object, "affected_callers", 0);
    truncate_array_field(object, "affected_dependencies", 0);
    truncate_array_field(object, "likely_entrypoints", 1);
    truncate_array_field(object, "plausible_entrypoints", 0);
    truncate_array_field(object, "execution_path", 1);
    truncate_array_field(object, "plausible_paths", 0);
    truncate_array_field(object, "guards", 1);
    truncate_array_field(object, "side_effects", 0);
    truncate_array_field(object, "failure_branches", 1);
    truncate_array_field(object, "relevant_docs", 0);
    truncate_array_strings_field(object, "stale_doc_signals", 0, 72);
    truncate_array_field(object, "changed_files", 1);
    truncate_array_field(object, "changed_symbols", 1);
    truncate_array_field(object, "pivots", 1);
    truncate_array_field(object, "context", 2);
    strip_array_object_field(object, "pivots", "source");
    truncate_array_field(object, "files", 1);
    truncate_array_field(object, "key_files", 1);
    truncate_array_field(object, "key_symbols", 1);
    truncate_array_field(object, "architecture", 1);
    truncate_array_field(object, "conventions", 1);
    truncate_array_field(object, "next_steps", 1);
    truncate_array_field(object, "likely_causes", 1);
    truncate_string_field(object, "overview", 72);
}

fn densify_workflow_value(value: Value) -> Value {
    match value {
        Value::Array(items) => {
            Value::Array(items.into_iter().map(densify_workflow_value).collect())
        }
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (dense_key(&key).to_string(), densify_workflow_value(value)))
                .collect(),
        ),
        other => other,
    }
}

fn dense_key(key: &str) -> &str {
    match key {
        "overview" => "ov",
        "query" => "q",
        "scenario" => "sn",
        "intent" => "i",
        "primary_files" => "pf",
        "secondary_files" => "sf",
        "edit_files" => "efi",
        "supporting_files" => "sfi",
        "likely_entrypoints" => "le",
        "plausible_entrypoints" => "pe",
        "execution_path" => "ep",
        "plausible_paths" => "pp",
        "guards" => "gd",
        "side_effects" => "sx",
        "failure_branches" => "fb",
        "candidate_spans" => "ps",
        "affected_callers" => "ac",
        "affected_dependencies" => "ad",
        "relevant_docs" => "rd",
        "stale_doc_signals" => "sd",
        "files" => "fs",
        "symbols" => "sy",
        "tests" => "ts",
        "test_gaps" => "tg",
        "matched_rules" => "mr",
        "memory_highlights" => "mh",
        "memories" => "mm",
        "risks" => "rk",
        "rationale" => "ra",
        "changed_files" => "cf",
        "changed_symbols" => "cs",
        "affected_symbols" => "af",
        "review_checklist" => "rc",
        "active_symbols" => "as",
        "nearby_symbols" => "ny",
        "key_files" => "kf",
        "key_symbols" => "ks",
        "notable_symbols" => "no",
        "architecture" => "ar",
        "conventions" => "cv",
        "durable_patterns" => "dp",
        "extracted_files" => "ef",
        "extracted_symbols" => "es",
        "suspects" => "su",
        "related_symbols" => "ry",
        "likely_causes" => "lc",
        "next_steps" => "nx",
        "suggested_expand" => "x",
        "context_handle" => "h",
        "context_origin" => "o",
        "delivery_mode" => "dm",
        "wire_format" => "wf",
        "single_anchor_used" => "sa",
        "semantic_fallback_used" => "se",
        "outcome_memory_reuse_count" => "or",
        "agent_retrieval_contract" => "arc",
        "next_action" => "na",
        "budget" => "bg",
        "budget_max_tokens" => "bmt",
        "approx_tokens" => "apt",
        "truncated" => "tr",
        "playbook_memory" => "pm",
        "focus" => "fo",
        "reason" => "r",
        "file" => "f",
        "symbol" => "s",
        "score" => "sc",
        "confidence" => "cf",
        "confidence_band" => "cb",
        "assertion_type" => "at",
        "verification_status" => "vs",
        "confidence_reason" => "cr",
        "supersedes_memory_id" => "smi",
        "superseded_by_memory_id" => "sbi",
        "contradicts_memory_ids" => "cms",
        "contradicted_by_memory_ids" => "cbi",
        "freshness_policy" => "fp",
        "freshness_policy_detail" => "fd",
        "provenance" => "pv",
        "evidence" => "ev",
        "source" => "src",
        "reference" => "rf",
        "captured_at" => "cat",
        "detail" => "dt",
        "note" => "nt",
        "reasons" => "rs",
        "kind" => "k",
        "line" => "ln",
        "line_span" => "ls",
        "start_line" => "sl",
        "end_line" => "el",
        "from_symbol" => "frs",
        "from_symbol_handle" => "frh",
        "from_kind" => "frk",
        "from_file" => "frf",
        "from_line" => "frl",
        "to_symbol" => "tos",
        "to_symbol_handle" => "toh",
        "to_kind" => "tok",
        "to_file" => "tof",
        "to_line" => "tol",
        "signal_type" => "sgt",
        "relationship" => "rp",
        "role" => "ro",
        "level" => "lv",
        "message" => "m",
        "matched_files" => "mf",
        "matched_symbols" => "ms",
        "impact_count" => "ic",
        "summary" => "sm",
        "why" => "w",
        "content" => "ct",
        "memory_type" => "mt",
        "scope" => "sp",
        "is_stale" => "st",
        "status" => "stt",
        "added_lines" => "al",
        "removed_lines" => "rl",
        "hunk_count" => "hc",
        "change_kind" => "ck",
        "via" => "v",
        _ => key,
    }
}

fn truncate_array_field(object: &mut serde_json::Map<String, Value>, key: &str, limit: usize) {
    let remove = match object.get_mut(key) {
        Some(Value::Array(items)) => {
            if limit == 0 {
                true
            } else {
                items.truncate(limit);
                items.is_empty()
            }
        }
        _ => false,
    };

    if remove {
        object.remove(key);
    }
}

fn truncate_string_field(object: &mut serde_json::Map<String, Value>, key: &str, limit: usize) {
    if let Some(Value::String(text)) = object.get_mut(key) {
        *text = truncate_text_value(text, limit);
    }
}

fn truncate_array_strings_field(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    limit: usize,
    text_limit: usize,
) {
    let remove = match object.get_mut(key) {
        Some(Value::Array(items)) => {
            for item in items.iter_mut() {
                if let Some(text) = item.as_str() {
                    *item = Value::String(truncate_text_value(text, text_limit));
                }
            }
            if limit == 0 {
                true
            } else {
                items.truncate(limit);
                items.is_empty()
            }
        }
        _ => false,
    };

    if remove {
        object.remove(key);
    }
}

fn strip_array_object_field(object: &mut serde_json::Map<String, Value>, key: &str, field: &str) {
    if let Some(Value::Array(items)) = object.get_mut(key) {
        for item in items {
            if let Some(entry) = item.as_object_mut() {
                entry.remove(field);
            }
        }
    }
}

fn shorten_array_object_string_field(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    field: &str,
    limit: usize,
) {
    if let Some(Value::Array(items)) = object.get_mut(key) {
        for item in items {
            if let Some(entry) = item.as_object_mut() {
                if let Some(text) = entry.get(field).and_then(|value| value.as_str()) {
                    entry.insert(
                        field.to_string(),
                        Value::String(truncate_text_value(text, limit)),
                    );
                }
            }
        }
    }
}

fn shorten_memory_entries(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    text_limit: usize,
) {
    if let Some(Value::Array(items)) = object.get_mut(key) {
        for item in items.iter_mut() {
            if let Some(entry) = item.as_object_mut() {
                if let Some(content) = entry.get("content").and_then(|value| value.as_str()) {
                    entry.insert(
                        "content".to_string(),
                        Value::String(truncate_text_value(content, text_limit)),
                    );
                } else if let Some(content) = entry.get("ct").and_then(|value| value.as_str()) {
                    entry.insert(
                        "ct".to_string(),
                        Value::String(truncate_text_value(content, text_limit)),
                    );
                }
            }
        }
    }
}

fn truncate_text_value(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        value.to_string()
    } else {
        let cutoff = limit.saturating_sub(3);
        format!("{}...", &value[..cutoff])
    }
}

fn first_confidence_band<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_array()?
        .first()?
        .get("confidence_band")?
        .as_str()
}

fn array_len(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(|item| item.as_array())
        .map(|items| items.len())
        .unwrap_or(0)
}

fn attach_workflow_metadata(value: &mut Value, metadata: &WorkflowRunMetadata) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "delivery_mode".to_string(),
            json!(metadata.delivery_mode.as_str()),
        );
        if metadata.wire_format != "standard" {
            object.insert(
                "wire_format".to_string(),
                json!(metadata.wire_format.as_str()),
            );
        }
        if metadata.single_anchor_used {
            object.insert("single_anchor_used".to_string(), json!(true));
        }
        if metadata.semantic_fallback_used {
            object.insert("semantic_fallback_used".to_string(), json!(true));
        }
        if metadata.outcome_memory_reuse_count > 0 {
            object.insert(
                "outcome_memory_reuse_count".to_string(),
                json!(metadata.outcome_memory_reuse_count),
            );
        }
    }
}

fn summarize_workflow_outcome_content(
    task: &str,
    status: &str,
    summary: Option<&str>,
    files: &[String],
    symbols: &[String],
    tests: &[String],
) -> String {
    let mut parts = vec![format!("Workflow outcome for '{}': {}.", task, status)];

    if let Some(summary) = summary {
        parts.push(format!("Summary: {}.", summary.trim_end_matches('.')));
    }
    if !files.is_empty() {
        parts.push(format!(
            "Files: {}.",
            files.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !symbols.is_empty() {
        parts.push(format!(
            "Symbols: {}.",
            symbols
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !tests.is_empty() {
        parts.push(format!(
            "Tests: {}.",
            tests.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    parts.join(" ")
}

fn combined_workflow_source_query(
    task: &str,
    summary: Option<&str>,
    inherited_query: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(summary) = summary.map(str::trim).filter(|value| !value.is_empty()) {
        parts.push(summary.to_string());
    }
    if let Some(inherited_query) = inherited_query
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        parts.push(inherited_query.to_string());
    }
    if !task.trim().is_empty() {
        parts.push(task.trim().to_string());
    }
    dedupe_string_values(&mut parts);
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn workflow_outcome_identifiers(
    task: &str,
    summary: Option<&str>,
    source_query: Option<&str>,
    files: &[String],
) -> Vec<String> {
    let mut terms = Vec::new();
    for source in std::iter::once(task)
        .chain(summary.into_iter())
        .chain(source_query.into_iter())
        .chain(files.iter().map(String::as_str))
    {
        for term in memory_v2::get_task_memory::structured_query_terms(source, None) {
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
    }
    terms
}

fn memory_to_value(
    memory: &Memory,
    structured_fields: Option<&MemoryStructuredFields>,
    include_session_id: bool,
) -> Value {
    // Memory retrieval paths historically emitted `type`; keep `memory_class`
    // as the canonical alias on every serialized record so retrieval callers
    // can consume one stable typed-memory shape.
    let memory_class = MemoryClass::from_memory_type(&memory.memory_type);
    let mut value = json!({
        "id": memory.id,
        "content": memory.content,
        "type": memory.memory_type.as_str(),
        "memory_class": memory_class.as_str(),
        "scope": memory.scope.as_str(),
        "confidence": memory.confidence,
        "linked_symbols": memory.linked_symbols,
        "linked_files": memory.linked_files,
        "workspace_id": memory.workspace_id,
        "branch": memory.branch,
        "refresh_key": memory.refresh_key,
        "source_query": memory.source_query,
        "created_at": memory.created_at,
        "last_accessed": memory.last_accessed,
        "access_count": memory.access_count,
        "is_stale": memory.is_stale,
        "stale_reason": memory.stale_reason,
    });

    if let Some(object) = value.as_object_mut() {
        if include_session_id {
            object.insert("session_id".to_string(), json!(memory.session_id));
        }

        if let Some(fields) = structured_fields {
            object.insert(
                "assertion_type".to_string(),
                json!(fields.assertion_type.as_str()),
            );
            object.insert(
                "verification_status".to_string(),
                json!(fields.verification_status.as_str()),
            );
            object.insert(
                "confidence_reason".to_string(),
                json!(fields.confidence_reason),
            );
            object.insert(
                "supersedes_memory_id".to_string(),
                json!(fields.supersedes_memory_id),
            );
            object.insert(
                "superseded_by_memory_id".to_string(),
                json!(fields.superseded_by_memory_id),
            );
            object.insert(
                "contradicts_memory_ids".to_string(),
                json!(fields.contradicts_memory_ids),
            );
            object.insert(
                "contradicted_by_memory_ids".to_string(),
                json!(fields.contradicted_by_memory_ids),
            );
            object.insert(
                "freshness_policy".to_string(),
                json!(fields.freshness_policy.as_str()),
            );
            object.insert(
                "freshness_policy_detail".to_string(),
                json!(fields.freshness_policy_detail),
            );
            object.insert("provenance".to_string(), json!(fields.provenance));
            object.insert("evidence".to_string(), json!(fields.evidence));
        }
    }

    value
}

fn serialize_memory_value(
    store: &MemoryStore,
    memory: &Memory,
    include_session_id: bool,
) -> Result<Value, (i32, String)> {
    let structured_fields = store.get_structured_fields(&memory.id).map_err(|e| {
        (
            -32603,
            format!("Failed to load structured memory fields: {}", e),
        )
    })?;
    Ok(memory_to_value(
        memory,
        structured_fields.as_ref(),
        include_session_id,
    ))
}

fn serialize_memory_values(
    store: &MemoryStore,
    memories: &[Memory],
    include_session_id: bool,
) -> Result<Vec<Value>, (i32, String)> {
    memories
        .iter()
        .map(|memory| serialize_memory_value(store, memory, include_session_id))
        .collect()
}

/// Attach the fields that identify a record's owning memory authority. This is
/// deliberately done at the RPC boundary so a shared record is never rendered
/// as if its local SQLite id belonged to the querying repository.
fn annotate_shared_memory_value(
    value: &mut Value,
    qualified_id: &str,
    source_tier: &str,
    cross_repo: bool,
    origin_repository_id: Option<&str>,
    origin_checkout_id: Option<&str>,
    effective_verification_status: &str,
    trust_reason: &str,
) {
    let annotate = |object: &mut serde_json::Map<String, Value>| {
        object.insert("id".to_string(), json!(qualified_id));
        object.insert("memory_id".to_string(), json!(qualified_id));
        object.insert("source_tier".to_string(), json!(source_tier));
        object.insert("cross_repo".to_string(), json!(cross_repo));
        object.insert(
            "origin_repository_id".to_string(),
            json!(origin_repository_id),
        );
        object.insert("origin_checkout_id".to_string(), json!(origin_checkout_id));
        object.insert(
            "effective_verification_status".to_string(),
            json!(effective_verification_status),
        );
        object.insert("trust_reason".to_string(), json!(trust_reason));
        if cross_repo {
            object.insert("verification_status".to_string(), json!("unverified"));
            object.insert("trust_status".to_string(), json!("advisory"));
        }
    };
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("memory_id") {
            object.insert("memory_id".to_string(), json!(qualified_id));
        }
        annotate(object);
        if let Some(memory) = object.get_mut("memory").and_then(Value::as_object_mut) {
            annotate(memory);
        }
    }
}

fn annotate_memory_freshness_values(
    values: &mut [Value],
    memories: &[Memory],
    workspace_root: &Path,
) {
    for (value, memory) in values.iter_mut().zip(memories) {
        let Some(warning) = memory_current_state_warning(memory, workspace_root) else {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "trust_status".to_string(),
                    json!(memory_trust_status(memory, None)),
                );
                object.insert(
                    "trust_reason".to_string(),
                    json!(memory_trust_reason(memory, None)),
                );
                if let Some(conflict) = memory_workspace_conflict(memory) {
                    object.insert("workspace_conflict".to_string(), conflict);
                }
                if let Some(diagnostic) = memory_workspace_path_diagnostic(memory) {
                    object.insert("workspace_path_diagnostic".to_string(), diagnostic);
                }
            }
            continue;
        };
        if let Some(object) = value.as_object_mut() {
            object.insert("freshness_warning".to_string(), warning);
            object.insert(
                "trust_status".to_string(),
                json!(memory_trust_status(memory, Some("freshness_warning"))),
            );
            object.insert(
                "trust_reason".to_string(),
                json!(memory_trust_reason(memory, Some("freshness_warning"))),
            );
            if let Some(conflict) = memory_workspace_conflict(memory) {
                object.insert("workspace_conflict".to_string(), conflict);
            }
            if let Some(diagnostic) = memory_workspace_path_diagnostic(memory) {
                object.insert("workspace_path_diagnostic".to_string(), diagnostic);
            }
        }
    }
}

fn memory_trust_status(memory: &Memory, warning: Option<&str>) -> &'static str {
    if memory.is_stale
        || matches!(
            memory.verification_status,
            MemoryVerificationStatus::Stale
                | MemoryVerificationStatus::Superseded
                | MemoryVerificationStatus::Contradicted
                | MemoryVerificationStatus::Expired
                | MemoryVerificationStatus::Invalidated
        )
    {
        "stale"
    } else if warning.is_some()
        || matches!(
            memory.verification_status,
            MemoryVerificationStatus::Unverified | MemoryVerificationStatus::InReview
        )
    {
        "advisory"
    } else {
        "trusted"
    }
}

fn memory_trust_reason(memory: &Memory, warning: Option<&str>) -> &'static str {
    if warning.is_some() {
        "freshness_warning"
    } else if memory.is_stale {
        "marked_stale"
    } else {
        match memory.verification_status {
            MemoryVerificationStatus::Verified => "verified",
            MemoryVerificationStatus::InReview => "verification_in_review",
            MemoryVerificationStatus::Unverified => "unverified",
            MemoryVerificationStatus::Stale => "verification_stale",
            MemoryVerificationStatus::Superseded => "superseded",
            MemoryVerificationStatus::Contradicted => "contradicted",
            MemoryVerificationStatus::Expired => "expired",
            MemoryVerificationStatus::Invalidated => "invalidated",
        }
    }
}

fn memory_workspace_conflict(memory: &Memory) -> Option<Value> {
    let workspace = memory.workspace_id.as_deref()?;
    let mut conflicting_paths = Vec::new();
    for path in memory
        .linked_files
        .iter()
        .filter(|file| file.starts_with('/'))
    {
        if !Path::new(path).starts_with(workspace) {
            conflicting_paths.push(path.clone());
        }
    }
    if conflicting_paths.is_empty() {
        return None;
    }
    Some(json!({
        "kind": "linked_absolute_path_outside_memory_workspace",
        "workspace_id": workspace,
        "conflicting_paths": conflicting_paths.into_iter().take(8).collect::<Vec<_>>(),
        "reason": "Memory workspace provenance does not contain one or more linked absolute file paths; consider an auditable repair, supersession, or invalidation proposal."
    }))
}

fn memory_workspace_path_diagnostic(memory: &Memory) -> Option<Value> {
    memory.workspace_id.as_deref()?;
    if memory.linked_files.is_empty()
        || memory.linked_files.iter().any(|file| file.starts_with('/'))
    {
        return None;
    }
    Some(json!({
        "kind": "workspace_unverifiable_from_relative_paths",
        "relative_paths": memory.linked_files.iter().take(8).collect::<Vec<_>>(),
        "reason": "Memory provenance cannot be cross-checked against workspace ownership because linked files are relative paths only."
    }))
}

fn memory_current_state_warning(memory: &Memory, workspace_root: &Path) -> Option<Value> {
    if memory.is_stale
        || matches!(
            memory.verification_status,
            MemoryVerificationStatus::Verified
                | MemoryVerificationStatus::Stale
                | MemoryVerificationStatus::Superseded
                | MemoryVerificationStatus::Contradicted
                | MemoryVerificationStatus::Expired
                | MemoryVerificationStatus::Invalidated
        )
    {
        return None;
    }

    let searchable_text = format!(
        "{} {} {}",
        memory.content,
        memory.source_query.as_deref().unwrap_or_default(),
        memory.refresh_key.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let failure_claim = ["blocked", "failure", "failed", "partial", "remain"]
        .iter()
        .any(|term| searchable_text.contains(term));
    if !failure_claim {
        return None;
    }

    let mut paths = Vec::new();
    for file in &memory.linked_files {
        let path = PathBuf::from(file);
        paths.push(if path.is_absolute() {
            path
        } else {
            workspace_root.join(path)
        });
    }
    for path in extract_absolute_paths(&memory.content)
        .into_iter()
        .chain(extract_absolute_paths(
            memory.source_query.as_deref().unwrap_or_default(),
        ))
    {
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }

    let changed_paths: Vec<String> = paths
        .into_iter()
        .filter_map(|path| {
            let modified = path.metadata().ok()?.modified().ok()?;
            let modified = modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            (modified > memory.created_at).then(|| path.to_string_lossy().to_string())
        })
        .take(8)
        .collect();
    if changed_paths.is_empty() {
        return None;
    }

    Some(json!({
        "kind": "referenced_files_changed_after_memory",
        "reason": "This unverified memory contains failure/blocker language and at least one referenced local file was modified after the memory was recorded; treat it as advisory until verified or superseded.",
        "changed_paths": changed_paths,
    }))
}

fn extract_absolute_paths(text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | ';' | ':' | ')' | '(' | '[' | ']' | '{' | '}' | '"' | '\''
            )
        });
        if !token.starts_with('/') {
            continue;
        }
        let path = PathBuf::from(token);
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }
    paths
}

fn memory_verification_status_for_recall(value: &Value) -> &str {
    value
        .get("verification_status")
        .and_then(|item| item.as_str())
        .or_else(|| {
            if value
                .get("is_stale")
                .and_then(|item| item.as_bool())
                .unwrap_or(false)
            {
                Some("stale")
            } else {
                None
            }
        })
        .unwrap_or("unverified")
}

fn memory_assertion_type_for_recall(value: &Value) -> &str {
    value
        .get("assertion_type")
        .and_then(|item| item.as_str())
        .or_else(|| {
            value
                .get("refresh_key")
                .and_then(|item| item.as_str())
                .filter(|key| key.starts_with("workflow_outcome::"))
                .map(|_| "workflow_outcome")
        })
        .or_else(|| {
            value
                .get("type")
                .or_else(|| value.get("memory_type"))
                .and_then(|item| item.as_str())
        })
        .unwrap_or("observation")
}

fn memory_verification_rank(value: &Value) -> i32 {
    match memory_verification_status_for_recall(value) {
        "verified" => 6,
        "in_review" => 5,
        "unverified" => 4,
        "superseded" => 2,
        "contradicted" => 1,
        "stale" => 0,
        _ => 3,
    }
}

fn memory_scope_rank(value: &Value, preferred_branch: Option<&str>) -> i32 {
    match value
        .get("scope")
        .and_then(|item| item.as_str())
        .unwrap_or("session")
    {
        "branch" => {
            if preferred_branch.is_some()
                && value.get("branch").and_then(|item| item.as_str()) == preferred_branch
            {
                4
            } else {
                2
            }
        }
        "repo" => 3,
        "session" => 1,
        _ => 0,
    }
}

fn memory_assertion_rank(value: &Value) -> i32 {
    match memory_assertion_type_for_recall(value) {
        "workflow_outcome" => 4,
        "constraint" => 3,
        "decision" | "pattern" | "anti_pattern" => 2,
        "exploration" => 1,
        _ => 0,
    }
}

fn memory_is_weaker(value: &Value) -> bool {
    matches!(
        memory_verification_status_for_recall(value),
        "stale" | "contradicted" | "superseded"
    ) || value
        .get("superseded_by_memory_id")
        .map(|item| !item.is_null())
        .unwrap_or(false)
        || value
            .get("contradicted_by_memory_ids")
            .and_then(|item| item.as_array())
            .map(|items| !items.is_empty())
            .unwrap_or(false)
        || value
            .get("is_stale")
            .and_then(|item| item.as_bool())
            .unwrap_or(false)
}

fn memory_recall_priority(
    value: &Value,
    preferred_branch: Option<&str>,
) -> (i32, i32, i32, i32, i64, u64, u64) {
    let assertion_type = memory_assertion_type_for_recall(value);
    let is_workflow_outcome = assertion_type == "workflow_outcome"
        || value
            .get("refresh_key")
            .and_then(|item| item.as_str())
            .map(|key| key.starts_with("workflow_outcome::"))
            .unwrap_or(false);
    let confidence = (value
        .get("confidence")
        .and_then(|item| item.as_f64())
        .unwrap_or(0.0)
        * 1000.0)
        .round() as i64;

    (
        memory_verification_rank(value),
        if is_workflow_outcome { 1 } else { 0 },
        if is_workflow_outcome {
            memory_scope_rank(value, preferred_branch)
        } else {
            0
        },
        (memory_assertion_rank(value) * 2)
            + memory_scope_rank(value, preferred_branch)
            + if memory_is_weaker(value) { 0 } else { 1 },
        confidence,
        value
            .get("access_count")
            .and_then(|item| item.as_u64())
            .unwrap_or(0),
        value
            .get("created_at")
            .and_then(|item| item.as_u64())
            .unwrap_or(0),
    )
}

fn sort_memory_values_for_recall(values: &mut [Value], preferred_branch: Option<&str>) {
    values.sort_by(|left, right| {
        memory_recall_priority(right, preferred_branch)
            .cmp(&memory_recall_priority(left, preferred_branch))
    });
}

fn build_memory_query(query: Option<&str>, files: &[String], symbols: &[String]) -> Option<String> {
    let mut terms = Vec::new();

    if let Some(query) = query {
        terms.extend(extract_search_terms(query, 4));
    }

    if terms.is_empty() {
        for symbol in symbols.iter().take(2) {
            terms.extend(extract_search_terms(symbol, 1));
        }
    }

    if terms.is_empty() {
        for file in files.iter().take(2) {
            terms.extend(extract_search_terms(file, 1));
        }
    }

    if terms.is_empty() {
        None
    } else {
        terms.sort();
        terms.dedup();
        Some(terms.join(" "))
    }
}

fn extract_search_terms(value: &str, limit: usize) -> Vec<String> {
    let mut terms = Vec::new();
    for part in value.split(|c: char| !c.is_alphanumeric()) {
        if part.len() < 3 {
            continue;
        }
        let normalized = part.to_lowercase();
        if terms.iter().any(|existing| existing == &normalized) {
            continue;
        }
        terms.push(normalized);
        if terms.len() >= limit {
            break;
        }
    }
    terms
}

fn seed_from_task_bundle(bundle: &TaskBundle) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in bundle
        .primary_files
        .iter()
        .map(|item| item.file.as_str())
        .chain(bundle.secondary_files.iter().map(|item| item.file.as_str()))
    {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &bundle.symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(bundle.query.clone()),
        files,
        symbols,
        tests: bundle.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&bundle.memories, &bundle.memory_highlights),
    }
}

fn seed_from_plan_edit_bundle(bundle: &PlanEditBundle) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in bundle
        .edit_files
        .iter()
        .map(|item| item.file.as_str())
        .chain(
            bundle
                .supporting_files
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(bundle.candidate_spans.iter().map(|item| item.file.as_str()))
        .chain(
            bundle
                .affected_callers
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(
            bundle
                .affected_dependencies
                .iter()
                .map(|item| item.file.as_str()),
        )
    {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &bundle.symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for span in &bundle.candidate_spans {
        push_seed_symbol(
            &mut symbols,
            span.symbol_handle.as_deref(),
            Some(span.symbol.as_str()),
        );
    }
    for impact in &bundle.affected_callers {
        push_seed_symbol(
            &mut symbols,
            impact.symbol_handle.as_deref(),
            Some(impact.symbol.as_str()),
        );
    }
    for impact in &bundle.affected_dependencies {
        push_seed_symbol(
            &mut symbols,
            impact.symbol_handle.as_deref(),
            Some(impact.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(bundle.query.clone()),
        files,
        symbols,
        tests: bundle.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&bundle.memories, &bundle.memory_highlights),
    }
}

fn seed_from_trace_scenario_bundle(bundle: &ScenarioTraceBundle) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in bundle
        .likely_entrypoints
        .iter()
        .map(|item| item.file.as_str())
        .chain(
            bundle
                .plausible_entrypoints
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(bundle.guards.iter().map(|item| item.file.as_str()))
        .chain(bundle.side_effects.iter().map(|item| item.file.as_str()))
        .chain(
            bundle
                .failure_branches
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(bundle.relevant_docs.iter().map(|item| item.file.as_str()))
    {
        push_seed_file(&mut files, file);
    }
    for segment in &bundle.execution_path {
        push_seed_file(&mut files, &segment.from_file);
        push_seed_file(&mut files, &segment.to_file);
    }
    for segment in &bundle.plausible_paths {
        push_seed_file(&mut files, &segment.from_file);
        push_seed_file(&mut files, &segment.to_file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in bundle
        .likely_entrypoints
        .iter()
        .chain(bundle.plausible_entrypoints.iter())
    {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for segment in bundle
        .execution_path
        .iter()
        .chain(bundle.plausible_paths.iter())
    {
        push_seed_symbol(
            &mut symbols,
            segment.from_symbol_handle.as_deref(),
            Some(segment.from_symbol.as_str()),
        );
        push_seed_symbol(
            &mut symbols,
            segment.to_symbol_handle.as_deref(),
            Some(segment.to_symbol.as_str()),
        );
    }
    for signal in bundle
        .guards
        .iter()
        .chain(bundle.side_effects.iter())
        .chain(bundle.failure_branches.iter())
    {
        push_seed_symbol(
            &mut symbols,
            signal.symbol_handle.as_deref(),
            Some(signal.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(bundle.scenario.clone()),
        files,
        symbols,
        tests: bundle.tests.iter().map(|item| item.file.clone()).collect(),
        memories: Vec::new(),
    }
}

fn seed_from_working_set_context(report: &WorkingSetContext) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in report.files.iter().map(|item| item.file.as_str()) {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.active_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for symbol in &report.nearby_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: report.query.clone(),
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&report.memories, &report.memory_highlights),
    }
}

fn seed_from_failure_diagnosis(report: &FailureDiagnosis) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in report
        .extracted_files
        .iter()
        .map(|item| item.as_str())
        .chain(report.suspects.iter().map(|item| item.file.as_str()))
    {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.extracted_symbols {
        push_seed_symbol(&mut symbols, None, Some(symbol));
    }
    for symbol in &report.suspects {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for symbol in &report.related_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(report.kind.clone()),
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: report
            .memory_highlights
            .iter()
            .map(|memory| {
                json!({
                    "content": memory.content,
                    "type": memory.memory_type,
                    "scope": memory.scope,
                    "is_stale": memory.is_stale
                })
            })
            .collect(),
    }
}

fn seed_from_subsystem_summary(report: &SubsystemSummary) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in report.key_files.iter().map(|item| item.file.as_str()) {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.key_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(report.query.clone()),
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: report
            .memories
            .iter()
            .map(|memory| {
                json!({
                    "content": memory.content,
                    "type": memory.memory_type,
                    "scope": memory.scope,
                    "is_stale": memory.is_stale
                })
            })
            .collect(),
    }
}

fn seed_from_repo_playbook(report: &RepoPlaybook) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in report.key_files.iter().map(|item| item.file.as_str()) {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.notable_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(report.overview.clone()),
        files,
        symbols,
        tests: Vec::new(),
        memories: report
            .durable_patterns
            .iter()
            .map(|memory| {
                json!({
                    "content": memory.content,
                    "type": memory.memory_type,
                    "scope": memory.scope,
                    "is_stale": memory.is_stale
                })
            })
            .collect(),
    }
}

fn attach_context_handle(value: &mut Value, handle: &str, origin: &str) {
    if let Some(object) = value.as_object_mut() {
        object.insert("context_handle".to_string(), json!(handle));
        object.insert("context_origin".to_string(), json!(origin));
    }
}

fn push_seed_file(files: &mut Vec<String>, file: &str) {
    let trimmed = file.trim();
    if trimmed.is_empty() {
        return;
    }
    files.push(stable_file_focus_value(trimmed));
    files.push(trimmed.to_string());
}

fn push_seed_symbol(
    symbols: &mut Vec<String>,
    symbol_handle: Option<&str>,
    symbol_name: Option<&str>,
) {
    if let Some(handle) = symbol_handle.map(str::trim).filter(|item| !item.is_empty()) {
        symbols.push(handle.to_string());
    }
    if let Some(name) = symbol_name.map(str::trim).filter(|item| !item.is_empty()) {
        symbols.push(name.to_string());
    }
}

fn normalize_seed_values(values: &mut Vec<String>) {
    values.retain(|item| !item.trim().is_empty());
    values.sort();
    values.dedup();
}

fn attach_playbook_memory(value: &mut Value, playbook_memory: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert("playbook_memory".to_string(), playbook_memory);
    }
}

fn extract_wrapped_tool_metrics(
    value: &Value,
) -> (
    usize,
    usize,
    Option<String>,
    Option<String>,
    ToolCallMetadata,
) {
    let texts: Vec<&str> = value["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(|inner| inner.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let payload_bytes = texts.iter().map(|text| text.len()).sum();
    let approx_tokens = payload_bytes / 4;
    let parsed = texts
        .iter()
        .find_map(|text| parse_wrapped_tool_payload(text));

    let context_handle = parsed
        .as_ref()
        .and_then(|inner| inner.get("context_handle").or_else(|| inner.get("h")))
        .and_then(|item| item.as_str())
        .map(|item| item.to_string());
    let context_origin = parsed
        .as_ref()
        .and_then(|inner| inner.get("context_origin").or_else(|| inner.get("o")))
        .and_then(|item| item.as_str())
        .map(|item| item.to_string());
    let metadata = ToolCallMetadata {
        delivery_mode: parsed
            .as_ref()
            .and_then(|inner| inner.get("delivery_mode").or_else(|| inner.get("dm")))
            .and_then(|item| item.as_str())
            .map(|item| item.to_string()),
        wire_format: parsed
            .as_ref()
            .and_then(|inner| inner.get("wire_format").or_else(|| inner.get("wf")))
            .and_then(|item| item.as_str())
            .map(|item| item.to_string()),
        single_anchor_used: parsed
            .as_ref()
            .and_then(|inner| inner.get("single_anchor_used").or_else(|| inner.get("sa")))
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
        suggested_expand_focus: parsed
            .as_ref()
            .and_then(|inner| inner.get("suggested_expand").or_else(|| inner.get("x")))
            .and_then(|item| item.get("focus").or_else(|| item.get("fo")))
            .and_then(|item| item.as_str())
            .map(|item| item.to_string()),
        semantic_fallback_used: parsed
            .as_ref()
            .and_then(|inner| {
                inner
                    .get("semantic_fallback_used")
                    .or_else(|| inner.get("se"))
            })
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
        outcome_memory_reuse_count: parsed
            .as_ref()
            .and_then(|inner| {
                inner
                    .get("outcome_memory_reuse_count")
                    .or_else(|| inner.get("or"))
            })
            .and_then(|item| item.as_u64())
            .unwrap_or(0) as usize,
    };

    (
        payload_bytes,
        approx_tokens,
        context_handle,
        context_origin,
        metadata,
    )
}

fn parse_wrapped_tool_payload(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .or_else(|| {
            text.split_once("\n\nStructured payload:\n")
                .and_then(|(_, payload)| serde_json::from_str::<Value>(payload).ok())
        })
}

fn dedupe_memory_values(values: &mut Vec<Value>) {
    let mut seen = HashSet::new();
    values.retain(|value| {
        let key = value
            .get("id")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string())
            .unwrap_or_else(|| value.to_string());
        seen.insert(key)
    });
}

fn report_memory_highlights(values: &[Value], limit: usize) -> Vec<MemoryHighlight> {
    values
        .iter()
        .filter_map(|value| {
            let content = value.get("content")?.as_str()?;
            Some(MemoryHighlight {
                content: truncate_memory_snippet(content, 120),
                memory_type: value
                    .get("type")
                    .or_else(|| value.get("memory_type"))
                    .and_then(|item| item.as_str())
                    .unwrap_or("observation")
                    .to_string(),
                scope: value
                    .get("scope")
                    .and_then(|item| item.as_str())
                    .unwrap_or("session")
                    .to_string(),
                is_stale: value
                    .get("is_stale")
                    .and_then(|item| item.as_bool())
                    .unwrap_or(false),
                assertion_type: value
                    .get("assertion_type")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                verification_status: value
                    .get("verification_status")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                confidence_reason: value
                    .get("confidence_reason")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                freshness_policy: value
                    .get("freshness_policy")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                freshness_policy_detail: value
                    .get("freshness_policy_detail")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
            })
        })
        .take(limit.max(1))
        .collect()
}

fn build_failure_overview_value(base: &str, memory: Option<&MemoryHighlight>) -> String {
    if let Some(memory) = memory {
        return format!(
            "{} Consider {}.",
            base.trim_end_matches('.'),
            memory_reference_phrase(memory)
        );
    }
    base.to_string()
}

fn memory_reference_phrase(memory: &MemoryHighlight) -> String {
    format!("prior {} {}", memory.scope, memory.memory_type)
}

fn memory_seed_values(memories: &[Value], highlights: &[MemoryHighlight]) -> Vec<Value> {
    if !memories.is_empty() {
        return memories.to_vec();
    }

    highlights
        .iter()
        .map(|memory| {
            json!({
                "content": memory.content,
                "type": memory.memory_type,
                "scope": memory.scope,
                "is_stale": memory.is_stale
            })
        })
        .collect()
}

fn truncate_memory_snippet(content: &str, limit: usize) -> String {
    let mut output = String::new();
    for ch in content.chars().take(limit) {
        output.push(ch);
    }
    if content.chars().count() > limit {
        output.push_str("...");
    }
    output
}

fn summarize_subsystem_memory_content(report: &SubsystemSummary) -> String {
    let file_list = report
        .key_files
        .iter()
        .map(|item| item.file.clone())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ");
    let symbol_list = report
        .key_symbols
        .iter()
        .map(|item| item.symbol.clone())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "Subsystem playbook for '{}': {} Key files: {}. Key symbols: {}.",
        report.query, report.overview, file_list, symbol_list
    )
}

fn summarize_repo_playbook_memory_content(report: &RepoPlaybook) -> String {
    let file_list = report
        .key_files
        .iter()
        .map(|item| item.file.clone())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ");
    let conventions = report
        .conventions
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");

    format!(
        "Repo playbook: {} High-signal files: {}. Conventions: {}.",
        report.overview, file_list, conventions
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_failure_overview_value, count_outcome_memory_reuse, extract_wrapped_tool_metrics,
        memory_seed_values, parse_wrapped_tool_payload, report_memory_highlights,
        parse_shared_memory_config, parse_workflow_response_options,
        seed_from_plan_edit_bundle, seed_from_task_bundle, seed_from_trace_scenario_bundle,
        stable_refresh_key, summarize_workflow_outcome_content, workflow_outcome_identifiers,
        wrap_tool_result, wrap_workflow_tool_result, McpHandler, QueryJobError, RequestHandler,
        SharedMemoryRuntime, WorkflowRenderMode, FULL_WORKFLOW_TOKEN_CAP,
    };
    use lattice_core::graph::CodeGraph;
    use lattice_core::indexer::Indexer;
    use lattice_core::intelligence::ExpandContextSeed;
    use lattice_core::intelligence::{
        EditSpanRecommendation, FileRecommendation, PlanEditBundle, ScenarioPathSegment,
        ScenarioSignal, ScenarioTraceBundle, SymbolRecommendation, TaskBundle,
    };
    use lattice_core::memory::model::{
        Memory, MemoryAssertionType, MemoryEvidence, MemoryFreshnessPolicy, MemoryProvenance,
        MemoryStructuredFields, MemoryVerificationStatus,
    };
    use lattice_core::memory::{MemoryScope, MemoryStore, MemoryType};
    use lattice_core::query::QueryEngine;
    use lattice_core::query::QueryIntent;
    use lattice_core::storage::GraphStore;
    use lattice_core::symbols::SymbolId;
    use lattice_core::symbols::{Language, SymbolKind};
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::{oneshot, Mutex};

    #[test]
    fn test_report_memory_highlights_truncates_content() {
        let long_content = "repo-playbook ".repeat(40);
        let highlights = report_memory_highlights(
            &[json!({
                "content": long_content,
                "type": "pattern",
                "scope": "repo",
                "is_stale": false
            })],
            3,
        );

        assert_eq!(highlights.len(), 1);
        assert!(highlights[0].content.len() <= 123);
        assert!(highlights[0].content.ends_with("..."));
    }

    #[test]
    fn test_build_failure_overview_value_uses_short_memory_reference() {
        let highlights = report_memory_highlights(
            &[json!({
                "content": "durable-note ".repeat(30),
                "type": "pattern",
                "scope": "repo",
                "is_stale": false
            })],
            1,
        );

        let overview = build_failure_overview_value("test diagnosis.", highlights.first());
        assert!(overview.contains("Consider prior repo pattern."));
        assert!(overview.len() < 80);
    }

    #[test]
    fn test_memory_seed_values_falls_back_to_highlights() {
        let seeded = memory_seed_values(
            &[],
            &[super::MemoryHighlight {
                content: "prior repo pattern".to_string(),
                memory_type: "pattern".to_string(),
                scope: "repo".to_string(),
                is_stale: false,
                assertion_type: None,
                verification_status: None,
                confidence_reason: None,
                freshness_policy: None,
                freshness_policy_detail: None,
            }],
        );

        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0]["content"], "prior repo pattern");
    }

    #[test]
    fn test_seed_from_task_bundle_includes_stable_and_legacy_handles() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let bundle = TaskBundle {
            query: "Fix login timeout".to_string(),
            intent: QueryIntent::FixBug,
            overview: "Likely edit: auth login".to_string(),
            suggested_expand: None,
            primary_files: vec![FileRecommendation {
                file: "src/auth.ts".to_string(),
                score: 9.1,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
                reasons: vec!["entry file".to_string()],
            }],
            secondary_files: Vec::new(),
            symbols: vec![SymbolRecommendation {
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 12,
                role: "pivot".to_string(),
                score: 8.4,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
            }],
            tests: Vec::new(),
            test_gaps: Vec::new(),
            matched_rules: Vec::new(),
            memories: Vec::new(),
            memory_highlights: Vec::new(),
            risks: Vec::new(),
            rationale: Vec::new(),
            stats: None,
        };

        let seed = seed_from_task_bundle(&bundle);
        assert!(
            seed.files.contains(&"src/auth.ts".to_string()),
            "expected legacy file seed for backward compatibility: {:?}",
            seed.files
        );
        assert!(
            seed.files.contains(&"file_id:src/auth.ts".to_string()),
            "expected stable file handle in seed: {:?}",
            seed.files
        );
        assert!(
            seed.symbols.contains(&"loginUser".to_string()),
            "expected legacy symbol seed for backward compatibility: {:?}",
            seed.symbols
        );
        assert!(
            seed.symbols.contains(&symbol_handle),
            "expected stable symbol handle in seed: {:?}",
            seed.symbols
        );
    }

    #[test]
    fn test_seed_from_plan_edit_bundle_includes_stable_and_legacy_handles() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let bundle = PlanEditBundle {
            query: "Fix login timeout".to_string(),
            intent: QueryIntent::FixBug,
            overview: "Patch the auth flow and verify callers.".to_string(),
            suggested_expand: None,
            edit_files: vec![FileRecommendation {
                file: "src/auth.ts".to_string(),
                score: 9.1,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
                reasons: vec!["entry file".to_string()],
            }],
            supporting_files: Vec::new(),
            symbols: vec![SymbolRecommendation {
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 12,
                role: "pivot".to_string(),
                score: 8.4,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
            }],
            candidate_spans: vec![EditSpanRecommendation {
                file: "src/auth.ts".to_string(),
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                line_span: "12-32".to_string(),
                start_line: 12,
                end_line: 32,
                reason: "Primary auth branch".to_string(),
                confidence_band: "high".to_string(),
            }],
            affected_callers: Vec::new(),
            affected_dependencies: Vec::new(),
            relevant_docs: Vec::new(),
            stale_doc_signals: Vec::new(),
            tests: Vec::new(),
            test_gaps: Vec::new(),
            matched_rules: Vec::new(),
            memories: Vec::new(),
            memory_highlights: Vec::new(),
            risks: Vec::new(),
            rationale: Vec::new(),
            stats: None,
        };

        let seed = seed_from_plan_edit_bundle(&bundle);
        assert!(
            seed.files.contains(&"src/auth.ts".to_string()),
            "expected legacy file seed for backward compatibility: {:?}",
            seed.files
        );
        assert!(
            seed.files.contains(&"file_id:src/auth.ts".to_string()),
            "expected stable file handle in seed: {:?}",
            seed.files
        );
        assert!(
            seed.symbols.contains(&"loginUser".to_string()),
            "expected legacy symbol seed for backward compatibility: {:?}",
            seed.symbols
        );
        assert!(
            seed.symbols.contains(&symbol_handle),
            "expected stable symbol handle in seed: {:?}",
            seed.symbols
        );
    }

    #[test]
    fn test_seed_from_trace_scenario_bundle_includes_stable_and_legacy_handles() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let next_symbol_handle = SymbolId {
            file: "src/session.ts".to_string(),
            name: "refreshSession".to_string(),
            byte_offset: 88,
        }
        .stable_handle();
        let bundle = ScenarioTraceBundle {
            scenario: "why does login fail after refresh".to_string(),
            intent: QueryIntent::Explore,
            overview: "Trace login through refresh edge.".to_string(),
            suggested_expand: None,
            likely_entrypoints: vec![SymbolRecommendation {
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 12,
                role: "entrypoint".to_string(),
                score: 9.2,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
            }],
            plausible_entrypoints: Vec::new(),
            execution_path: vec![ScenarioPathSegment {
                from_symbol: "loginUser".to_string(),
                from_symbol_handle: Some(symbol_handle.clone()),
                from_kind: "fn".to_string(),
                from_file: "src/auth.ts".to_string(),
                from_line: 12,
                to_symbol: "refreshSession".to_string(),
                to_symbol_handle: Some(next_symbol_handle),
                to_kind: "fn".to_string(),
                to_file: "src/session.ts".to_string(),
                to_line: 44,
                relationship: "calls".to_string(),
                score: 8.8,
                confidence_band: "high".to_string(),
                rationale: vec!["edge".to_string()],
            }],
            plausible_paths: Vec::new(),
            guards: vec![ScenarioSignal {
                signal_type: "guard".to_string(),
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 15,
                summary: "validate credentials".to_string(),
                score: 7.2,
                confidence_band: "medium".to_string(),
            }],
            side_effects: Vec::new(),
            failure_branches: Vec::new(),
            relevant_docs: Vec::new(),
            tests: Vec::new(),
            test_gaps: Vec::new(),
            matched_rules: Vec::new(),
            rationale: Vec::new(),
            stats: None,
        };

        let seed = seed_from_trace_scenario_bundle(&bundle);
        assert!(
            seed.files.contains(&"src/auth.ts".to_string()),
            "expected legacy file seed for backward compatibility: {:?}",
            seed.files
        );
        assert!(
            seed.files.contains(&"file_id:src/auth.ts".to_string()),
            "expected stable file handle in seed: {:?}",
            seed.files
        );
        assert!(
            seed.symbols.contains(&"loginUser".to_string()),
            "expected legacy symbol seed for backward compatibility: {:?}",
            seed.symbols
        );
        assert!(
            seed.symbols.contains(&symbol_handle),
            "expected stable symbol handle in seed: {:?}",
            seed.symbols
        );
    }

    #[test]
    fn test_extract_wrapped_tool_metrics_reads_workflow_metadata() {
        let wrapped = wrap_tool_result(json!({
            "context_handle": "ctx-7",
            "context_origin": "prepare_change",
            "delivery_mode": "compact",
            "mode_reason": "kept compact",
            "semantic_fallback_used": true,
            "outcome_memory_reuse_count": 2,
            "suggested_expand": {
                "focus": "file:src/auth.ts",
                "reason": "top file"
            }
        }));

        let (_, _, handle, origin, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(handle.as_deref(), Some("ctx-7"));
        assert_eq!(origin.as_deref(), Some("prepare_change"));
        assert_eq!(metadata.delivery_mode.as_deref(), Some("compact"));
        assert_eq!(metadata.wire_format, None);
        assert!(!metadata.single_anchor_used);
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("file:src/auth.ts")
        );
        assert!(metadata.semantic_fallback_used);
        assert_eq!(metadata.outcome_memory_reuse_count, 2);
    }

    #[test]
    fn test_extract_wrapped_tool_metrics_reads_dense_workflow_metadata() {
        let wrapped = wrap_tool_result(json!({
            "h": "ctx-8",
            "o": "diagnose_failure",
            "dm": "tiny",
            "wf": "dense",
            "sa": true,
            "se": true,
            "or": 1,
            "x": {
                "fo": "symbol:_verify_org_access",
                "r": "top suspect"
            }
        }));

        let (_, _, handle, origin, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(handle.as_deref(), Some("ctx-8"));
        assert_eq!(origin.as_deref(), Some("diagnose_failure"));
        assert_eq!(metadata.delivery_mode.as_deref(), Some("tiny"));
        assert_eq!(metadata.wire_format.as_deref(), Some("dense"));
        assert!(metadata.single_anchor_used);
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("symbol:_verify_org_access")
        );
        assert!(metadata.semantic_fallback_used);
        assert_eq!(metadata.outcome_memory_reuse_count, 1);
    }

    #[test]
    fn test_wrap_workflow_tool_result_markdown_returns_only_summary() {
        let wrapped = wrap_workflow_tool_result(
            json!({
                "overview": "Likely edit: auth. focus loginUser.",
                "context_handle": "ctx-9",
                "context_origin": "prepare_change",
                "delivery_mode": "compact",
                "primary_files": [
                    { "file": "src/auth.ts" }
                ],
                "symbols": [
                    { "symbol": "loginUser" }
                ],
                "suggested_expand": {
                    "focus": "file:src/auth.ts",
                    "reason": "top file"
                }
            }),
            WorkflowRenderMode::Markdown,
        );

        let text = wrapped["content"][0]["text"]
            .as_str()
            .expect("expected text payload");
        assert!(text.contains("### Summary"));
        assert!(text.contains("- Overview: Likely edit: auth. focus loginUser."));
        assert!(text.contains("- Top file: `src/auth.ts`"));
        assert!(text.contains("- Top symbol: `loginUser`"));
        assert!(!text.contains("### Structured Payload"));
        assert!(!text.contains("```json"));
        assert!(!text.contains("lattice-metrics"));
    }

    #[test]
    fn test_wrap_workflow_tool_result_summarizes_context_capsule_payload() {
        let wrapped = wrap_workflow_tool_result(
            json!({
                "query": "how does auth login work",
                "intent": "Explore",
                "pivots": [
                    {
                        "file": "src/auth.ts",
                        "symbol": "loginUser",
                        "line": 12,
                        "kind": "fn",
                        "source": "fn loginUser() {}",
                        "score": 9.8,
                        "reason": "keyword"
                    }
                ],
                "context": [
                    {
                        "file": "src/session.ts",
                        "symbol": "validateSession",
                        "line": 44,
                        "kind": "fn",
                        "skeleton": "fn validateSession(...)",
                        "relationship": "dependency",
                        "score": 5.1
                    }
                ],
                "context_handle": "ctx-11",
                "context_origin": "get_context_capsule",
                "suggested_expand": {
                    "focus": "symbol:loginUser",
                    "reason": "Expand the lead pivot to inspect nearby code and relationships."
                }
            }),
            WorkflowRenderMode::Markdown,
        );

        let text = wrapped["content"][0]["text"]
            .as_str()
            .expect("expected text payload");
        assert!(text.contains("### Summary"));
        assert!(text.contains("- Query: how does auth login work"));
        assert!(text.contains("- Top file: `src/auth.ts`"));
        assert!(text.contains("- Top symbol: `loginUser`"));
        assert!(!text.contains("- Suggested expand:"));
        assert!(!text.contains("### Structured Payload"));
    }

    #[test]
    fn test_wrap_workflow_tool_result_prefers_symbol_handle_in_suggested_expand() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let mut object = json!({
            "primary_files": [
                { "file": "src/auth.ts" }
            ],
            "symbols": [
                { "symbol": "loginUser", "symbol_handle": symbol_handle }
            ]
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("prepare_change", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        let expected = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn test_plan_edit_suggested_expand_prefers_candidate_span_handle() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let mut object = json!({
            "edit_files": [
                { "file": "src/auth.ts" }
            ],
            "candidate_spans": [
                { "file": "src/auth.ts", "symbol": "loginUser", "symbol_handle": symbol_handle }
            ]
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("plan_edit", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        let expected = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn test_trace_scenario_suggested_expand_uses_execution_path_handle() {
        let symbol_handle = SymbolId {
            file: "src/session.ts".to_string(),
            name: "refreshSession".to_string(),
            byte_offset: 88,
        }
        .stable_handle();
        let mut object = json!({
            "execution_path": [
                {
                    "from_symbol": "loginUser",
                    "from_file": "src/auth.ts",
                    "to_symbol": "refreshSession",
                    "to_symbol_handle": symbol_handle,
                    "to_file": "src/session.ts"
                }
            ]
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("trace_scenario", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        let expected = SymbolId {
            file: "src/session.ts".to_string(),
            name: "refreshSession".to_string(),
            byte_offset: 88,
        }
        .stable_handle();
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn test_wrap_workflow_tool_result_uses_stable_file_handle_when_symbol_missing() {
        let mut object = json!({
            "primary_files": [
                { "file": "src/auth.ts" }
            ],
            "symbols": []
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("prepare_change", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("file_id:src/auth.ts")
        );
    }

    #[test]
    fn test_wrap_workflow_tool_result_markdown_omits_telemetry_comment() {
        let wrapped = wrap_workflow_tool_result(
            json!({
                "overview": "Likely edit: auth. focus loginUser.",
                "context_handle": "ctx-10",
                "context_origin": "prepare_change",
                "delivery_mode": "compact",
                "primary_files": [
                    { "file": "src/auth.ts" }
                ],
                "symbols": [
                    { "symbol": "loginUser" }
                ],
                "suggested_expand": {
                    "focus": "file:src/auth.ts",
                    "reason": "top file"
                }
            }),
            WorkflowRenderMode::Markdown,
        );

        let text = wrapped["content"][0]["text"]
            .as_str()
            .expect("expected text payload");
        assert!(text.contains("### Summary"));
        assert!(text.contains("- Overview: Likely edit: auth. focus loginUser."));
        assert!(!text.contains("### Structured Payload"));
        assert!(!text.contains("<!-- lattice-metrics: "));
    }

    #[test]
    fn workflow_render_defaults_to_markdown_and_rejects_hybrid() {
        let defaults = parse_workflow_response_options(&json!({}))
            .expect("missing render must select markdown");
        assert_eq!(defaults.render, WorkflowRenderMode::Markdown);

        let error = parse_workflow_response_options(&json!({"render": "hybrid"}))
            .expect_err("hybrid rendering must be rejected");
        assert_eq!(error.0, -32602);
        assert!(error.1.contains("Invalid render value: hybrid"));
    }

    #[test]
    fn workflow_json_render_is_exactly_the_structured_payload() {
        let wrapped = wrap_workflow_tool_result(
            json!({"overview": "structured", "context_handle": "ctx-json"}),
            WorkflowRenderMode::Json,
        );
        let text = wrapped["content"][0]["text"]
            .as_str()
            .expect("JSON render text");
        let payload: Value = serde_json::from_str(text).expect("valid JSON payload");
        assert_eq!(payload["context_handle"].as_str(), Some("ctx-json"));
        assert!(!text.contains("### Summary"));
        assert!(!text.contains("lattice-metrics"));
    }

    #[test]
    fn test_count_outcome_memory_reuse_only_counts_workflow_outcomes() {
        let count = count_outcome_memory_reuse(&[
            json!({"refresh_key": "workflow_outcome::cert-tenant"}),
            json!({"refresh_key": "repo_playbook"}),
            json!({"refresh_key": "workflow_outcome::login-timeout"}),
        ]);
        assert_eq!(count, 2);
    }

    #[test]
    fn test_summarize_workflow_outcome_content_stays_compact() {
        let content = summarize_workflow_outcome_content(
            "fix certificate tenant isolation",
            "success",
            Some("narrowed access checks and updated tests"),
            &[
                "routers/certificates.py".to_string(),
                "models/certificates.py".to_string(),
            ],
            &["_verify_org_access".to_string()],
            &["tests/test_certificate_tenant_isolation.py".to_string()],
        );

        assert!(content.contains("fix certificate tenant isolation"));
        assert!(content.contains("routers/certificates.py"));
        assert!(content.len() < 260);
    }

    #[test]
    fn workflow_outcome_identity_terms_survive_inherited_context() {
        let inherited =
            "Portal remediation /home/pete/cadres/portal/docs/audit/2026-05-17-2248-remediation-run IU-0031 PX-0040";
        let source_query = super::combined_workflow_source_query(
            "governance contract remediation",
            Some("done"),
            Some(inherited),
        )
        .expect("source query");
        let identifiers = workflow_outcome_identifiers(
            "governance contract remediation",
            Some("done"),
            Some(&source_query),
            &[],
        );

        assert_eq!(identifiers, vec!["iu-0031", "px-0040"]);
        let refresh_key =
            stable_refresh_key("governance contract remediation", &[source_query], &[]);
        assert!(refresh_key.contains("iu-0031"));
        assert!(refresh_key.contains("px-0040"));
        assert!(
            !refresh_key.contains("home"),
            "absolute path fragments must not pollute workflow outcome identity"
        );
    }

    #[test]
    fn memory_conflict_and_trust_status_are_machine_readable() {
        let memory = Memory {
            id: "memory-1".to_string(),
            session_id: "session".to_string(),
            content: "PX-0033 remains blocked".to_string(),
            memory_type: MemoryType::Pattern,
            scope: MemoryScope::Repo,
            confidence: 0.7,
            linked_symbols: Vec::new(),
            linked_files: vec!["/home/pete/cadres/portal/src/file.ts".to_string()],
            workspace_id: Some("/home/pete/cadres/rmm".to_string()),
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
        };

        assert_eq!(super::memory_trust_status(&memory, None), "advisory");
        assert_eq!(super::memory_trust_reason(&memory, None), "unverified");
        let conflict = super::memory_workspace_conflict(&memory).expect("workspace conflict");
        assert_eq!(
            conflict["kind"].as_str(),
            Some("linked_absolute_path_outside_memory_workspace")
        );

        let mut relative_memory = memory;
        relative_memory.linked_files = vec!["docs/audit/remediation.md".to_string()];
        let diagnostic =
            super::memory_workspace_path_diagnostic(&relative_memory).expect("path diagnostic");
        assert_eq!(
            diagnostic["kind"].as_str(),
            Some("workspace_unverifiable_from_relative_paths")
        );
    }

    fn unique_test_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    fn build_memory_test_handler(
        session_id: &str,
    ) -> (McpHandler, Arc<Mutex<MemoryStore>>, PathBuf) {
        let workspace_root = unique_test_path("lattice-mcp-memory");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");
        let memory_store = Arc::new(Mutex::new(
            MemoryStore::open_in_memory().expect("memory store"),
        ));
        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            memory_store.clone(),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path,
            session_id.to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            None,
            Vec::new(),
            Vec::new(),
        );

        (handler, memory_store, workspace_root)
    }

    #[test]
    fn shared_memory_config_accepts_only_absolute_store_paths() {
        let config = parse_shared_memory_config(
            "[memory]\norganization_id = \"cadres\"\nshared_store_path = \"/var/tmp/cadres-memories.db\"\n",
        )
        .expect("valid shared memory config");
        assert_eq!(config.organization_id.as_deref(), Some("cadres"));
        assert_eq!(
            config.shared_store_path.as_deref(),
            Some(std::path::Path::new("/var/tmp/cadres-memories.db"))
        );
        assert!(parse_shared_memory_config("[memory]\nshared_store_path = \"relative.db\"\n")
            .unwrap_err()
            .contains("absolute"));
    }

    #[tokio::test]
    async fn organization_memory_routes_to_shared_store_and_is_advisory_cross_repo() {
        let shared_store = Arc::new(Mutex::new(
            MemoryStore::open_in_memory().expect("shared memory store"),
        ));
        let (mut handler_a, repository_a, workspace_a) = build_memory_test_handler("shared-a");
        handler_a.shared_memory = Some(SharedMemoryRuntime {
            store: shared_store.clone(),
            organization_id: "cadres".to_string(),
        });
        handler_a.memory_workspace_id = "repo-a".to_string();
        let save = handler_a
            .tool_save_memory_v2(&json!({
                "content": "All Cadres services use a shared deployment policy.",
                "memory_class": "constraint",
                "scope": "organization",
                "confidence": 0.9,
                "confidence_reason": "operator policy",
                "freshness_policy": "manual_review"
            }))
            .await
            .expect("organization memory saves");
        let save_payload: Value = serde_json::from_str(
            save["content"][0]["text"]
                .as_str()
                .expect("wrapped save response"),
        )
        .expect("save JSON");
        assert!(save_payload["memory_id"]
            .as_str()
            .expect("qualified id")
            .starts_with("organization:cadres:"));
        assert!(repository_a
            .lock()
            .await
            .query_unscoped_admin(None, 10)
            .expect("repository query")
            .is_empty());

        let (mut handler_b, _, workspace_b) = build_memory_test_handler("shared-b");
        handler_b.shared_memory = Some(SharedMemoryRuntime {
            store: shared_store,
            organization_id: "cadres".to_string(),
        });
        handler_b.memory_workspace_id = "repo-b".to_string();
        let search = handler_b
            .tool_search_memory(&json!({"query": "shared deployment", "limit": 10}))
            .await
            .expect("merged organization recall");
        let search_payload: Value = serde_json::from_str(
            search["content"][0]["text"]
                .as_str()
                .expect("wrapped search response"),
        )
        .expect("search JSON");
        let memory = &search_payload["memories"][0];
        assert_eq!(memory["source_tier"], "organization");
        assert_eq!(memory["cross_repo"], true);
        assert_eq!(memory["effective_verification_status"], "unverified");
        assert_eq!(memory["trust_status"], "advisory");

        let _ = std::fs::remove_dir_all(workspace_a);
        let _ = std::fs::remove_dir_all(workspace_b);
    }

    #[tokio::test]
    async fn status_remains_available_while_query_worker_is_blocked() {
        let (handler, _, workspace_root) = build_memory_test_handler("query-status-isolation");
        let handler = Arc::new(handler);
        let snapshot = handler
            .query_engine_snapshot_for_workflow()
            .await
            .expect("query snapshot");
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let worker_handler = Arc::clone(&handler);
        let worker = tokio::spawn(async move {
            worker_handler
                .run_query_job(move || {
                    started_tx.send(()).expect("signal query start");
                    release_rx.blocking_recv().expect("release query");
                    snapshot.graph().stats()
                })
                .await
        });

        started_rx.await.expect("query worker started");
        tokio::time::timeout(
            Duration::from_millis(250),
            handler.tool_agent_status(&json!({})),
        )
        .await
        .expect("status must not wait for query worker")
        .expect("status response");
        release_tx.send(()).expect("release query worker");
        worker.await.expect("worker join").expect("query job");
        std::fs::remove_dir_all(workspace_root).expect("remove temp workspace");
    }

    #[tokio::test]
    async fn query_workers_reject_excess_work_without_queueing() {
        let (handler, _, workspace_root) = build_memory_test_handler("query-capacity");
        let handler = Arc::new(handler);
        let mut releases = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..super::MAX_CONCURRENT_QUERY_JOBS {
            let (started_tx, started_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let worker_handler = Arc::clone(&handler);
            workers.push(tokio::spawn(async move {
                worker_handler
                    .run_query_job(move || {
                        started_tx.send(()).expect("signal query start");
                        release_rx.blocking_recv().expect("release query");
                    })
                    .await
            }));
            started_rx.await.expect("query worker started");
            releases.push(release_tx);
        }

        assert_eq!(handler.run_query_job(|| ()).await, Err(QueryJobError::Busy));
        for release in releases {
            release.send(()).expect("release query worker");
        }
        for worker in workers {
            worker.await.expect("worker join").expect("query job");
        }
        std::fs::remove_dir_all(workspace_root).expect("remove temp workspace");
    }

    #[tokio::test]
    async fn query_capacity_response_is_truthful_and_machine_readable() {
        let (handler, _, workspace_root) = build_memory_test_handler("query-capacity-response");
        let response = handler
            .query_job_error_response(
                "prepare_change",
                "change auth",
                WorkflowRenderMode::Json,
                QueryJobError::Busy,
            )
            .expect("bounded capacity response");
        let text = response["content"][0]["text"]
            .as_str()
            .expect("wrapped tool text");
        let payload = parse_wrapped_tool_payload(text).expect("structured payload");

        assert_eq!(payload["partial"], true);
        assert_eq!(payload["reason"], "query_capacity");
        assert_eq!(payload["primary_files"], json!([]));
        std::fs::remove_dir_all(workspace_root).expect("remove temp workspace");
    }

    #[test]
    fn extract_workspace_file_references_finds_root_and_nested_files_only() {
        let workspace_root = unique_test_path("lattice-explicit-file-references");
        std::fs::create_dir_all(workspace_root.join("docs")).expect("create docs");
        std::fs::write(workspace_root.join("AGENTS.md"), "# Instructions\n").expect("write agents");
        std::fs::write(workspace_root.join("docs/manual.md"), "# Manual\n").expect("write manual");

        let files = super::extract_workspace_file_references(
            "Update AGENTS.md and `docs/manual.md`; ignore missing.md and /etc/hosts",
            &workspace_root,
        );

        assert_eq!(files, vec!["AGENTS.md", "docs/manual.md"]);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn structural_search_matches_root_files_and_punctuated_doc_headings() {
        let file_match = super::search_node_match(
            "agents.md",
            &super::normalized_search_terms("AGENTS.md"),
            "Cadres Meridian — Codex Instructions",
            "AGENTS.md",
        );
        let heading_match = super::search_node_match(
            "product north star frictionless deterministic it",
            &super::normalized_search_terms("Product North Star Frictionless Deterministic IT"),
            "Product North Star: Frictionless, Deterministic IT",
            "AGENTS.md",
        );

        assert_eq!(file_match, Some((5, "exact_file")));
        assert_eq!(heading_match, Some((2, "all_terms")));
    }

    #[tokio::test]
    async fn test_index_status_uses_live_indexer_snapshot_while_indexing() {
        let (handler, _memory_store, workspace_root) =
            build_memory_test_handler("session-index-status-live-indexer");

        {
            let mut indexer = handler.indexer.lock().await;
            indexer
                .index_file_content(
                    "src/status.ts",
                    r#"
export function greet(name: string): string {
    return `hello ${name}`;
}
"#,
                )
                .expect("index test file");
        }
        handler.indexing.store(true, Ordering::Relaxed);

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "index_status",
                "arguments": {}
            }),
        )
        .await
        .expect("index_status tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped index_status response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");

        assert_eq!(payload["status"].as_str(), Some("indexing"));
        assert_eq!(payload["files"].as_u64(), Some(1));
        assert!(payload["nodes"].as_u64().unwrap_or_default() >= 1);
        assert!(payload["edges"].as_u64().is_some());
        assert_eq!(payload["warm_load_skipped"].as_bool(), Some(false));
        assert!(payload["warm_load_skip_reason"].is_null());
        assert_eq!(payload["persisted_files"].as_u64(), Some(0));
        assert_eq!(payload["graph_storage_state"], "healthy");
        assert_eq!(payload["index_work"]["state"], "idle");
        assert_eq!(payload["index_work"]["capacity"], 1);
        assert!(payload["limit"].as_u64().unwrap_or_default() > 0);
        assert_eq!(
            payload["env_var"].as_str(),
            Some("LATTICE_MAX_WARM_GRAPH_FILES")
        );
        assert!(payload["persisted_bytes"].is_null());
        assert!(payload["byte_limit"].as_u64().unwrap_or_default() > 0);
        assert_eq!(
            payload["byte_env_var"].as_str(),
            Some("LATTICE_MAX_WARM_GRAPH_BYTES")
        );
        assert_eq!(payload["effective_files"].as_u64(), Some(0));
        assert_eq!(payload["watch_degraded"].as_bool(), Some(false));
        assert_eq!(
            payload["languages"]["TypeScript"].as_u64(),
            Some(1),
            "expected language counts to come from live indexer snapshot: {payload:?}"
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn index_status_stays_responsive_while_graph_store_is_publishing() {
        let (handler, _memory_store, workspace_root) =
            build_memory_test_handler("index-status-store-busy");
        let _store_guard = handler.graph_store.lock().await;

        let response = tokio::time::timeout(
            Duration::from_millis(100),
            handler.tool_index_status(&json!({})),
        )
        .await
        .expect("status must not wait for graph persistence")
        .expect("status response");
        let payload = parse_wrapped_tool_payload(
            response["content"][0]["text"]
                .as_str()
                .expect("wrapped status text"),
        )
        .expect("status payload");

        assert_eq!(payload["graph_storage_state"], "busy");
        assert_eq!(payload["warm_load_skipped"], false);
        assert_eq!(
            payload["graph_storage_diagnostic"],
            "graph store is publishing an index snapshot"
        );
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn repeated_reindex_request_coalesces_while_refresh_is_running() {
        let (handler, _memory_store, workspace_root) =
            build_memory_test_handler("reindex-coalescing");

        let first = handler
            .handle_reindex()
            .await
            .expect("first reindex request");
        let second = handler
            .handle_reindex()
            .await
            .expect("repeated reindex request");

        assert_eq!(first["status"], "started");
        assert_eq!(second["status"], "already_running");
        tokio::time::timeout(Duration::from_secs(2), async {
            while handler.refresh_running.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background reindex should finish");
        assert!(!handler.indexing.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_index_status_reports_degraded_watcher_health() {
        let (handler, _memory_store, workspace_root) =
            build_memory_test_handler("session-index-status-watch-degraded");
        handler
            .watcher_health
            .mark_degraded("forced watch setup failure", 30);
        handler.watcher_health.mark_poll();

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "index_status",
                "arguments": {}
            }),
        )
        .await
        .expect("index_status tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped index_status response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");

        assert_eq!(payload["watch_degraded"].as_bool(), Some(true));
        assert_eq!(
            payload["watch_degraded_reason"].as_str(),
            Some("forced watch setup failure")
        );
        assert_eq!(payload["watch_poll_interval_secs"].as_u64(), Some(30));
        assert!(payload["watch_last_poll_epoch_secs"].as_u64().is_some());

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn index_status_surfaces_bounded_partial_index_failures() {
        let (handler, _memory_store, workspace_root) =
            build_memory_test_handler("session-index-status-partial");
        handler
            .index_health
            .replace_from_report(&lattice_core::indexer::BatchIndexReport {
                requested_count: 12,
                indexed_count: 0,
                is_partial: true,
                indexed_files: Vec::new(),
                removed_files: Vec::new(),
                failures: (0..12)
                    .map(|index| lattice_core::indexer::IndexFailure {
                        file: format!("src/{:02}.rs", 11 - index),
                        kind: lattice_core::indexer::IndexFailureKind::ParseError,
                        message: "invalid source".to_string(),
                    })
                    .collect(),
            });

        let response = handler
            .tool_index_status(&json!({}))
            .await
            .expect("index status response");
        let payload = parse_wrapped_tool_payload(
            response["content"][0]["text"]
                .as_str()
                .expect("wrapped status text"),
        )
        .expect("status payload");

        assert_eq!(payload["is_partial"], true);
        assert_eq!(payload["parse_failures"], 12);
        let failed_files = payload["failed_files"]
            .as_array()
            .expect("failed files array");
        assert_eq!(failed_files.len(), 10);
        assert_eq!(failed_files[0], "src/00.rs");
        assert_eq!(failed_files[9], "src/09.rs");

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_file_symbols_returns_extension_compatible_fields() {
        use lattice_core::graph::builder::GraphBuilder;
        use lattice_core::parser::parse_file;

        let (handler, _memory_store, workspace_root) =
            build_memory_test_handler("session-file-symbols-contract");

        let source = r#"export function greet(name: string): string {
    return `hello ${name}`;
}"#;
        let caller = r#"import { greet } from "./status";

export function sendGreeting(): string {
    return greet("team");
}"#;

        let mut builder = GraphBuilder::new();
        builder.add_file(parse_file("src/status.ts", source).expect("parse source"));
        builder.add_file(parse_file("src/caller.ts", caller).expect("parse caller"));

        {
            let mut engine = handler.engine.lock().await;
            engine.update_graph(builder.build());
        }

        let payload = RequestHandler::handle(
            &handler,
            "lattice/file_symbols",
            json!({
                "file": "src/status.ts"
            }),
        )
        .await
        .expect("file_symbols should succeed");

        let symbols = payload["symbols"]
            .as_array()
            .expect("expected symbols array");
        let greet = symbols
            .iter()
            .find(|value| value["name"].as_str() == Some("greet"))
            .expect("expected greet symbol");

        assert_eq!(greet["line"].as_u64(), Some(1));
        assert_eq!(greet["character"].as_u64(), Some(0));
        assert_eq!(greet["kind"].as_str(), Some("fn"));
        assert_eq!(greet["dependentCount"].as_u64(), Some(1));
        assert_eq!(greet["dependentFileCount"].as_u64(), Some(1));
        assert_eq!(greet["fileCount"].as_u64(), Some(1));

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_augment_memory_values_with_playbooks_prefers_verified_workflow_outcome() {
        let (handler, memory_store, workspace_root) =
            build_memory_test_handler("session-memory-preference");
        let workspace_id = workspace_root.to_string_lossy().to_string();

        let observation_id = {
            let store = memory_store.lock().await;
            store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-memory-preference".to_string(),
                    content: "Observed a login timeout while replaying refresh flow".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Session,
                    confidence: 0.41,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: None,
                    source_query: Some("login timeout".to_string()),
                    created_at: 10,
                    last_accessed: 10,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store observation")
        };

        let refresh_key = format!(
            "workflow_outcome::{}",
            stable_refresh_key("login timeout", &[], &[])
        );
        {
            let store = memory_store.lock().await;
            let outcome_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-prior".to_string(),
                    content: "Workflow outcome: login timeout fix validated in code and tests"
                        .to_string(),
                    memory_type: MemoryType::Pattern,
                    scope: MemoryScope::Repo,
                    confidence: 0.96,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: Some(refresh_key),
                    source_query: Some("verified from code and tests".to_string()),
                    created_at: 20,
                    last_accessed: 20,
                    access_count: 2,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store outcome");
            let mut fields = store
                .get_structured_fields(&outcome_id)
                .expect("load outcome structured fields")
                .unwrap_or_default();
            fields.assertion_type = MemoryAssertionType::WorkflowOutcome;
            fields.verification_status = MemoryVerificationStatus::Verified;
            fields.confidence_reason = Some("Validated by the daemon workflow".to_string());
            store
                .update_structured_fields(&outcome_id, &fields)
                .expect("update outcome structured fields");
        }

        let observation_value = {
            let store = memory_store.lock().await;
            let current = store
                .get_session_memories("session-memory-preference", 5)
                .expect("load current session memories");
            let observation = current
                .iter()
                .find(|memory| memory.id == observation_id)
                .expect("expected stored observation");
            super::serialize_memory_value(&store, observation, true).expect("serialize observation")
        };

        let values = handler
            .augment_memory_values_with_playbooks(
                "login timeout",
                &[],
                &[],
                vec![observation_value],
                2,
            )
            .await
            .expect("augment memory values");

        assert_eq!(values.len(), 2);
        assert_eq!(
            values[0]
                .get("verification_status")
                .and_then(|value| value.as_str()),
            Some("verified")
        );
        assert_eq!(
            values[0]
                .get("assertion_type")
                .and_then(|value| value.as_str()),
            Some("workflow_outcome")
        );
        assert_eq!(
            values[1].get("id").and_then(|value| value.as_str()),
            Some(observation_id.as_str())
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_load_durable_memory_values_prefers_verified_workflow_outcome() {
        let (handler, memory_store, workspace_root) =
            build_memory_test_handler("session-durable-memory");
        let workspace_id = workspace_root.to_string_lossy().to_string();

        let observation_id = {
            let store = memory_store.lock().await;
            store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-observation".to_string(),
                    content: "Repo note about login timeout mitigation".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Repo,
                    confidence: 0.92,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: None,
                    source_query: Some("login timeout".to_string()),
                    created_at: 11,
                    last_accessed: 11,
                    access_count: 1,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store durable observation")
        };

        {
            let store = memory_store.lock().await;
            let outcome_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-outcome".to_string(),
                    content: "Workflow outcome: login timeout fix verified in repo".to_string(),
                    memory_type: MemoryType::Pattern,
                    scope: MemoryScope::Repo,
                    confidence: 0.89,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: Some("workflow_outcome::login-timeout".to_string()),
                    source_query: Some("verified from code and tests".to_string()),
                    created_at: 12,
                    last_accessed: 12,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store durable outcome");
            let mut fields = store
                .get_structured_fields(&outcome_id)
                .expect("load durable outcome structured fields")
                .unwrap_or_default();
            fields.assertion_type = MemoryAssertionType::WorkflowOutcome;
            fields.verification_status = MemoryVerificationStatus::Verified;
            store
                .update_structured_fields(&outcome_id, &fields)
                .expect("update durable outcome structured fields");
        }

        let values = handler
            .load_durable_memory_values(2)
            .await
            .expect("load durable memories");

        assert_eq!(values.len(), 2);
        assert_eq!(
            values[0]
                .get("assertion_type")
                .and_then(|value| value.as_str()),
            Some("workflow_outcome")
        );
        assert_eq!(
            values[1].get("id").and_then(|value| value.as_str()),
            Some(observation_id.as_str())
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_search_memory_surfaces_structured_weaker_metadata() {
        let (handler, memory_store, workspace_root) =
            build_memory_test_handler("session-memory-surface");
        let workspace_id = workspace_root.to_string_lossy().to_string();

        let (base_id, superseding_id, contradictor_id, stale_id) = {
            let store = memory_store.lock().await;

            let base_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-base".to_string(),
                    content: "Org isolation contract memory for daemon recall".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Repo,
                    confidence: 0.78,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: None,
                    source_query: Some("org isolation".to_string()),
                    created_at: 30,
                    last_accessed: 30,
                    access_count: 1,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store base memory");

            let superseding_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-superseding".to_string(),
                    content: "Org isolation contract was replaced by stricter repo guard"
                        .to_string(),
                    memory_type: MemoryType::Decision,
                    scope: MemoryScope::Repo,
                    confidence: 0.93,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: None,
                    source_query: Some("org isolation verified".to_string()),
                    created_at: 31,
                    last_accessed: 31,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store superseding memory");

            let contradictor_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-contradictor".to_string(),
                    content: "Org isolation fallback path contradicts the older contract"
                        .to_string(),
                    memory_type: MemoryType::Decision,
                    scope: MemoryScope::Repo,
                    confidence: 0.87,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: None,
                    source_query: Some("org isolation verified".to_string()),
                    created_at: 32,
                    last_accessed: 32,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store contradictor memory");

            let stale_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-stale".to_string(),
                    content: "Org isolation stale memory after contract change".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Repo,
                    confidence: 0.65,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    scope_organization_id: None,
                    refresh_key: None,
                    source_query: Some("org isolation".to_string()),
                    created_at: 33,
                    last_accessed: 33,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: MemoryVerificationStatus::Unverified,
                })
                .expect("store stale memory");

            let mut fields = store
                .get_structured_fields(&base_id)
                .expect("load base structured fields")
                .unwrap_or_else(MemoryStructuredFields::default);
            fields.confidence_reason =
                Some("Older observation retained for audit context".to_string());
            fields.freshness_policy = MemoryFreshnessPolicy::ManualReview;
            fields.freshness_policy_detail =
                Some("Re-review after org isolation contract edits".to_string());
            fields.provenance = vec![MemoryProvenance {
                source: "test".to_string(),
                reference: Some("search_memory".to_string()),
                captured_at: Some(40),
                note: Some("targeted daemon regression".to_string()),
            }];
            fields.evidence = vec![MemoryEvidence {
                kind: "file".to_string(),
                reference: Some("src/isolation.rs".to_string()),
                detail: Some("org isolation branch".to_string()),
                captured_at: Some(40),
                span: None,
                evidence_content_hash: None,
            }];
            store
                .update_structured_fields(&base_id, &fields)
                .expect("update base structured fields");

            store
                .mark_memory_superseded(&base_id, &superseding_id)
                .expect("mark superseded");
            store
                .mark_memory_contradicted(&base_id, &contradictor_id)
                .expect("mark contradicted");
            store
                .mark_stale_by_symbol("OrgIsolation", "contract changed")
                .expect("mark stale");

            (base_id, superseding_id, contradictor_id, stale_id)
        };

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "search_memory",
                "arguments": {
                    "query": "org isolation",
                    "limit": 10
                }
            }),
        )
        .await
        .expect("search_memory tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped search_memory response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        let memories = payload["memories"]
            .as_array()
            .expect("expected memories array");

        let base = memories
            .iter()
            .find(|memory| {
                memory.get("id").and_then(|value| value.as_str()) == Some(base_id.as_str())
            })
            .expect("expected contradicted memory");
        assert_eq!(
            base.get("verification_status")
                .and_then(|value| value.as_str()),
            Some("stale")
        );
        assert_eq!(
            base.get("superseded_by_memory_id")
                .and_then(|value| value.as_str()),
            Some(superseding_id.as_str())
        );
        assert!(base
            .get("contradicted_by_memory_ids")
            .and_then(|value| value.as_array())
            .is_some_and(|ids| ids
                .iter()
                .any(|id| id.as_str() == Some(contradictor_id.as_str()))));
        assert_eq!(
            base.get("freshness_policy")
                .and_then(|value| value.as_str()),
            Some("manual_review")
        );
        assert_eq!(
            base.get("freshness_policy_detail")
                .and_then(|value| value.as_str()),
            Some("Re-review after org isolation contract edits")
        );
        assert_eq!(
            base.get("confidence_reason")
                .and_then(|value| value.as_str()),
            Some("Older observation retained for audit context")
        );
        assert!(base
            .get("provenance")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty()));
        assert!(base
            .get("evidence")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty()));
        assert_eq!(
            base.get("type").and_then(|value| value.as_str()),
            Some("observation")
        );
        assert_eq!(
            base.get("scope").and_then(|value| value.as_str()),
            Some("repo")
        );
        assert_eq!(
            base.get("is_stale").and_then(|value| value.as_bool()),
            Some(true)
        );

        let stale = memories
            .iter()
            .find(|memory| {
                memory.get("id").and_then(|value| value.as_str()) == Some(stale_id.as_str())
            })
            .expect("expected stale memory");
        assert_eq!(
            stale
                .get("verification_status")
                .and_then(|value| value.as_str()),
            Some("stale")
        );
        assert_eq!(
            stale.get("stale_reason").and_then(|value| value.as_str()),
            Some("contract changed")
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_plan_edit_tool_path_returns_context_handle_and_origin() {
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/auth.ts".to_string(),
                name: "loginUser".to_string(),
                byte_offset: 41,
            },
            SymbolKind::Function,
            "loginUser".to_string(),
            "function loginUser(credentials) {}".to_string(),
            "function loginUser(credentials) {\n  return authenticate(credentials);\n}".to_string(),
            "src/auth.ts".to_string(),
            12,
            30,
            true,
            Language::TypeScript,
        );

        let workspace_root = unique_test_path("lattice-mcp-plan-edit");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-plan-edit".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            None,
            Vec::new(),
            Vec::new(),
        );

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "plan_edit",
                "arguments": {
                    "query": "fix loginUser timeout",
                    "mode": "compact",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("plan_edit tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped plan_edit response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        let origin = payload
            .get("context_origin")
            .or_else(|| payload.get("o"))
            .and_then(|value| value.as_str());
        assert_eq!(origin, Some("plan_edit"));
        let delivery_mode = payload
            .get("delivery_mode")
            .or_else(|| payload.get("dm"))
            .and_then(|value| value.as_str());
        assert!(
            matches!(delivery_mode, Some("compact" | "tiny")),
            "expected compact/tiny delivery mode for plan_edit payload, got {delivery_mode:?} in {payload:?}"
        );
        assert!(
            payload
                .get("context_handle")
                .or_else(|| payload.get("h"))
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty()),
            "expected non-empty context_handle in plan_edit payload: {payload:?}"
        );
        if let Some(contract) = payload
            .get("agent_retrieval_contract")
            .or_else(|| payload.get("arc"))
        {
            assert_eq!(contract.as_object().map(|fields| fields.len()), Some(1));
            assert!(contract
                .get("next_action")
                .or_else(|| contract.get("na"))
                .and_then(Value::as_str)
                .is_some_and(|action| !action.is_empty() && action.len() <= 96));
        }
        assert_eq!(
            payload
                .get("budget")
                .or_else(|| payload.get("bg"))
                .and_then(|value| value.as_str()),
            Some("tiny")
        );
        assert!(payload
            .get("budget_max_tokens")
            .or_else(|| payload.get("bmt"))
            .and_then(|value| value.as_u64())
            .is_some());

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_context_capsule_tool_path_is_bounded_and_strips_source() {
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/auth.ts".to_string(),
                name: "loginUser".to_string(),
                byte_offset: 41,
            },
            SymbolKind::Function,
            "loginUser".to_string(),
            "function loginUser(credentials) {}".to_string(),
            "function loginUser(credentials) {\n  const secret = credentials.password;\n  return authenticate(secret);\n}".repeat(20),
            "src/auth.ts".to_string(),
            12,
            30,
            true,
            Language::TypeScript,
        );

        let workspace_root = unique_test_path("lattice-mcp-context-capsule");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-context-capsule".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            None,
            Vec::new(),
            Vec::new(),
        );

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "get_context_capsule",
                "arguments": {
                    "query": "how does loginUser authenticate credentials",
                    "render": "json",
                    "wire_format": "standard",
                    "budget": "full"
                }
            }),
        )
        .await
        .expect("get_context_capsule tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped get_context_capsule response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        assert_eq!(
            payload["context_origin"].as_str(),
            Some("get_context_capsule")
        );
        if let Some(contract) = payload.get("agent_retrieval_contract") {
            assert_eq!(contract.as_object().map(|fields| fields.len()), Some(1));
            assert!(contract["next_action"]
                .as_str()
                .is_some_and(|action| !action.is_empty() && action.len() <= 96));
        }
        let pivots_array = payload["pivots"]
            .as_array()
            .or_else(|| payload["ranked_pivots"].as_array());
        assert!(
            pivots_array
                .is_some_and(|items| items.len() <= 3
                    && items.iter().all(|item| item.get("source").is_none())),
            "expected bounded pivots without full source: {payload:?}"
        );
        assert!(
            payload["budget_max_tokens"]
                .as_u64()
                .is_some_and(|tokens| tokens <= FULL_WORKFLOW_TOKEN_CAP as u64),
            "expected workflow token cap metadata: {payload:?}"
        );
        assert!(
            payload
                .get("context_handle")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty()),
            "expected context handle: {payload:?}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_prepare_change_reports_indexing_when_no_warm_graph_is_available() {
        let workspace_root = unique_test_path("lattice-mcp-prepare-change-indexing-empty-graph");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-prepare-change-indexing".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(true)),
            None,
            Vec::new(),
            Vec::new(),
        );

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "prepare_change",
                "arguments": {
                    "query": "fix product agent drift",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("prepare_change tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped prepare_change response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        assert_eq!(payload["indexing"].as_bool(), Some(true));
        assert_eq!(
            payload["primary_files"].as_array().map(Vec::len),
            Some(0),
            "indexing response should stay bounded and avoid misleading empty rankings"
        );
        assert!(
            payload["overview"]
                .as_str()
                .is_some_and(|overview| overview.contains("Indexing is still in progress")),
            "expected explicit indexing overview: {payload:?}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_prepare_change_promotes_live_indexer_graph_while_indexing() {
        let workspace_root = unique_test_path("lattice-mcp-prepare-change-live-indexer-graph");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let mut indexer = Indexer::new(workspace_root.clone());
        indexer
            .index_file_content(
                "backend/core/agent_version_drift.py",
                r#"
def detect_agent_version_drift(agent, rollout):
    return agent.version != rollout.target_version
"#,
            )
            .expect("index test file");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
            Arc::new(Mutex::new(indexer)),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-prepare-change-live-indexer".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(true)),
            None,
            Vec::new(),
            Vec::new(),
        );

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "prepare_change",
                "arguments": {
                    "query": "fix agent version drift",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("prepare_change tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped prepare_change response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        assert_ne!(
            payload["indexing"].as_bool(),
            Some(true),
            "live indexer graph should be promoted instead of returning an indexing placeholder: {payload:?}"
        );
        let primary_files = payload["primary_files"]
            .as_array()
            .or_else(|| payload["structured_payload"]["primary_files"].as_array());
        assert!(
            primary_files.is_some_and(|files| files
                .iter()
                .any(|file| file["file"].as_str() == Some("backend/core/agent_version_drift.py"))),
            "expected promoted live graph to produce a working set: {payload:?}"
        );
        let context_handle = payload
            .get("context_handle")
            .or_else(|| payload.get("h"))
            .and_then(|value| value.as_str());
        assert!(
            context_handle.is_some_and(|handle| !handle.is_empty()),
            "expected context handle from promoted live graph response: {payload:?}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_trace_scenario_tool_path_returns_context_handle_and_origin() {
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/auth.ts".to_string(),
                name: "loginUser".to_string(),
                byte_offset: 41,
            },
            SymbolKind::Function,
            "loginUser".to_string(),
            "function loginUser(credentials) {}".to_string(),
            "function loginUser(credentials) {\n  return authenticate(credentials);\n}".to_string(),
            "src/auth.ts".to_string(),
            12,
            30,
            true,
            Language::TypeScript,
        );

        let workspace_root = unique_test_path("lattice-mcp-trace-scenario");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-trace-scenario".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            None,
            Vec::new(),
            Vec::new(),
        );

        let response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "trace_scenario",
                "arguments": {
                    "scenario": "why does loginUser fail after refresh",
                    "mode": "compact",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("trace_scenario tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped trace_scenario response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        let origin = payload
            .get("context_origin")
            .or_else(|| payload.get("o"))
            .and_then(|value| value.as_str());
        assert_eq!(origin, Some("trace_scenario"));
        let delivery_mode = payload
            .get("delivery_mode")
            .or_else(|| payload.get("dm"))
            .and_then(|value| value.as_str());
        assert!(
            matches!(delivery_mode, Some("compact" | "tiny")),
            "expected compact/tiny delivery mode for trace_scenario payload, got {delivery_mode:?} in {payload:?}"
        );
        assert!(
            payload
                .get("context_handle")
                .or_else(|| payload.get("h"))
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty()),
            "expected non-empty context_handle in trace_scenario payload: {payload:?}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_expand_context_tool_path_supports_stable_and_legacy_focus() {
        let mut graph = CodeGraph::new();
        let cert_symbol_id = SymbolId {
            file: "routers/certificates.py".to_string(),
            name: "_verify_org_access".to_string(),
            byte_offset: 66,
        };
        graph.add_node(
            cert_symbol_id.clone(),
            SymbolKind::Function,
            "_verify_org_access".to_string(),
            "def _verify_org_access(org_id):".to_string(),
            "def _verify_org_access(org_id):\n    raise HTTPException(status_code=403)".to_string(),
            "routers/certificates.py".to_string(),
            66,
            72,
            false,
            Language::Python,
        );
        graph.add_node(
            SymbolId {
                file: "routers/compliance_mgmt/_shared.py".to_string(),
                name: "_verify_org_access".to_string(),
                byte_offset: 14,
            },
            SymbolKind::Function,
            "_verify_org_access".to_string(),
            "def _verify_org_access(perms, org_id):".to_string(),
            "def _verify_org_access(perms, org_id):\n    return perms.validate(org_id)".to_string(),
            "routers/compliance_mgmt/_shared.py".to_string(),
            14,
            19,
            false,
            Language::Python,
        );

        let workspace_root = unique_test_path("lattice-mcp-expand-context");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-expand-stable".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            None,
            Vec::new(),
            Vec::new(),
        );

        let stable_focus = cert_symbol_id.stable_handle();
        let seed = ExpandContextSeed {
            query: Some("Fix certificate access checks".to_string()),
            files: vec![
                "file_id:routers/certificates.py".to_string(),
                "routers/certificates.py".to_string(),
                "file_id:routers/compliance_mgmt/_shared.py".to_string(),
                "routers/compliance_mgmt/_shared.py".to_string(),
            ],
            symbols: vec![stable_focus.clone(), "_verify_org_access".to_string()],
            tests: Vec::new(),
            memories: Vec::new(),
        };

        let handle = handler.store_context_handle("prepare_change", seed).await;

        let stable_response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "expand_context",
                "arguments": {
                    "handle": handle.legacy_handle,
                    "focus": stable_focus
                }
            }),
        )
        .await
        .expect("stable focus expand_context call should succeed");
        let stable_text = stable_response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped stable response text");
        let stable_payload =
            parse_wrapped_tool_payload(stable_text).expect("expected parseable wrapped payload");
        assert_eq!(
            stable_payload["context_origin"].as_str(),
            Some("prepare_change")
        );
        assert_eq!(stable_payload["focus_type"].as_str(), Some("symbol"));
        let stable_first = stable_payload["symbols"]
            .as_array()
            .and_then(|items| items.first())
            .expect("expected expanded symbol context for stable focus");
        assert_eq!(stable_first["symbol"].as_str(), Some("_verify_org_access"));
        assert_eq!(
            stable_first["file"].as_str(),
            Some("routers/certificates.py")
        );

        let legacy_response = RequestHandler::handle(
            &handler,
            "lattice/tool_call",
            json!({
                "name": "expand_context",
                "arguments": {
                    "handle": stable_payload["context_handle"].as_str().expect("context handle"),
                    "focus": "symbol:_verify_org_access"
                }
            }),
        )
        .await
        .expect("legacy focus expand_context call should succeed");
        let legacy_text = legacy_response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped legacy response text");
        let legacy_payload =
            parse_wrapped_tool_payload(legacy_text).expect("expected parseable wrapped payload");
        assert_eq!(
            legacy_payload["context_origin"].as_str(),
            Some("prepare_change")
        );
        assert_eq!(legacy_payload["focus_type"].as_str(), Some("symbol"));
        let legacy_first = legacy_payload["symbols"]
            .as_array()
            .and_then(|items| items.first())
            .expect("expected expanded symbol context for legacy focus");
        assert_eq!(legacy_first["symbol"].as_str(), Some("_verify_org_access"));
        let legacy_file = legacy_first["file"]
            .as_str()
            .expect("legacy symbol should include file");
        assert!(
            legacy_file == "routers/certificates.py"
                || legacy_file == "routers/compliance_mgmt/_shared.py",
            "legacy focus should resolve to one of duplicate symbol files, got {legacy_file}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }
}

fn stable_refresh_key(query: &str, files: &[String], symbols: &[String]) -> String {
    let mut terms = memory_v2::get_task_memory::structured_query_terms(query, None);
    let query_without_paths = remove_absolute_paths(query);
    let mut generic_terms = extract_search_terms(&query_without_paths, 4);
    terms.append(&mut generic_terms);
    for file in files {
        terms.extend(memory_v2::get_task_memory::structured_query_terms(
            file, None,
        ));
    }
    for file in files.iter().take(2) {
        if let Some(term) = file_identity_term(file) {
            terms.push(term);
        }
    }
    for symbol in symbols.iter().take(2) {
        terms.extend(extract_search_terms(symbol, 1));
    }
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        "general".to_string()
    } else {
        terms.join("-")
    }
}

fn remove_absolute_paths(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|token| {
            let trimmed = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | ';' | ':' | ')' | '(' | '[' | ']' | '{' | '}' | '"' | '\''
                )
            });
            !trimmed.starts_with('/')
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn file_identity_term(file: &str) -> Option<String> {
    let path = PathBuf::from(file);
    path.file_stem()
        .or_else(|| path.file_name())
        .and_then(|value| value.to_str())
        .and_then(|value| extract_search_terms(value, 1).into_iter().next())
}

fn current_git_branch(workspace_root: &std::path::Path) -> Option<String> {
    resolve_repo_state(workspace_root)
        .and_then(|snapshot| snapshot.head_ref)
        .and_then(|head_ref| {
            head_ref
                .strip_prefix("refs/heads/")
                .map(ToString::to_string)
        })
}

fn detect_project_rules(
    graph: &lattice_core::graph::model::CodeGraph,
) -> Vec<lattice_core::intelligence::ProjectRule> {
    let mut files: Vec<String> = graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect();
    files.sort();
    files.dedup();
    RulesDetector::new().detect_rules(&files)
}

fn persist_session_consolidation_proposals(
    workspace_id: &str,
    store: &mut MemoryStore,
    slices: &[Vec<lattice_core::events::EventEnvelope>],
    mode: memory_v2::consolidate_session::ConsolidationMode,
) -> Result<Vec<memory_v2::consolidate_session::ConsolidationProposalItem>, (i32, String)> {
    let mut items = Vec::new();
    for slice in slices {
        if slice.is_empty() {
            continue;
        }
        let template = EpisodeTemplate::from_task_slice(slice)
            .map_err(|error| (-32603, format!("Failed to build episode template: {error}")))?;
        let proposal = memory_v2::consolidate_session::build_episode_proposal(
            workspace_id,
            store,
            &template,
            mode,
        )
        .map_err(|error| (-32603, error))?;
        let existing = store
            .with_connection(|conn| {
                lattice_core::consolidation::ConsolidationProposal::load_record(
                    conn,
                    &proposal.proposal_id,
                )
            })
            .map_err(|error| (-32603, format!("Failed to query proposal: {error}")))?;
        if existing.is_none() {
            store
                .with_connection(|conn| {
                    lattice_core::consolidation::persist_pending_proposal(
                        conn,
                        workspace_id,
                        &format!("session_consolidation:{}", template.task_id.value),
                        mode.job_mode(),
                        &proposal,
                    )
                    .map(|_| ())
                })
                .map_err(|error| (-32603, format!("Failed to persist proposal: {error}")))?;
        }
        let record = store
            .with_connection(|conn| {
                lattice_core::consolidation::ConsolidationProposal::load_record(
                    conn,
                    &proposal.proposal_id,
                )
                .map_err(Into::into)
                .and_then(|value| {
                    value.ok_or_else(|| {
                        lattice_core::LatticeError::Storage(format!(
                            "Proposal `{}` was not persisted for review",
                            proposal.proposal_id
                        ))
                    })
                })
            })
            .map_err(|error| (-32603, format!("Failed to reload proposal record: {error}")))?;
        items.push(consolidation_report_item(
            &record,
            &template.summary_text,
            &template.task_id.value,
        ));
    }
    Ok(items)
}

fn consolidation_report_item(
    record: &lattice_core::consolidation::ConsolidationProposalRecord,
    summary: &str,
    task_id: &str,
) -> memory_v2::consolidate_session::ConsolidationProposalItem {
    let proposed_state = record.proposed_state.clone();
    let prior_state = record.prior_state.clone();
    let evidence = record.evidence.clone();
    let proposed_memory = extract_review_memory_state(&proposed_state);
    let prior_memory = extract_review_memory_state(&prior_state);
    memory_v2::consolidate_session::ConsolidationProposalItem {
        proposal_id: record.proposal_id.clone(),
        job_id: record.job_id.clone(),
        proposal_kind: record.proposal_kind.as_str().to_string(),
        task_id: task_id.to_string(),
        category: "episode_summary".to_string(),
        summary: summary.to_string(),
        target_memory_id: record.target_memory_id.clone(),
        enqueued_at: record.enqueued_at,
        decision: record.decision.as_str().to_string(),
        proposed_class: string_field(&proposed_memory, "memory_class"),
        current_scope: string_field(&prior_memory, "scope"),
        target_scope: string_field(&proposed_memory, "scope"),
        confidence: number_field(&proposed_memory, "confidence"),
        evidence_count: evidence_count(&evidence),
        prior_state,
        proposed_state,
        evidence,
        provenance: record
            .provenance
            .as_ref()
            .map(|value| serde_json::to_value(value).unwrap_or(Value::Null)),
    }
}

fn extract_review_memory_state(value: &Value) -> Value {
    value
        .get("memory")
        .cloned()
        .unwrap_or_else(|| value.clone())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn number_field(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn evidence_count(value: &Value) -> usize {
    match value {
        Value::Array(entries) => entries.len(),
        Value::Object(entries) => entries
            .values()
            .map(|entry| match entry {
                Value::Array(values) => values.len(),
                Value::Null => 0,
                _ => 1,
            })
            .sum(),
        _ => 0,
    }
}

fn emit_consolidation_proposal_events(
    capture: &EventCapture,
    workspace_id: &str,
    proposals: &[memory_v2::consolidate_session::ConsolidationProposalItem],
) -> Result<(), (i32, String)> {
    for item in proposals {
        let memory_id = MemoryId {
            workspace_id: workspace_id.to_string(),
            ulid: format!("proposed-{}", item.task_id),
        };
        capture
            .record_memory_consolidation_proposed(
                &[],
                memory_id,
                &[],
                &item.summary,
                &item.proposal_id,
                None,
                None,
            )
            .map_err(|error| {
                (
                    -32603,
                    format!("Failed to capture consolidation event: {error}"),
                )
            })?;
    }
    Ok(())
}

fn build_consolidation_report(
    args: memory_v2::consolidate_session::ConsolidateSessionArgs,
    mode: memory_v2::consolidate_session::ConsolidationMode,
    render_mode: memory_v2::consolidate_session::ConsolidationRenderMode,
    proposals: Vec<memory_v2::consolidate_session::ConsolidationProposalItem>,
) -> memory_v2::consolidate_session::ConsolidationReport {
    let episode_ids: Vec<String> = proposals
        .iter()
        .map(|item| item.proposal_id.clone())
        .collect();
    let mut notes = Vec::new();
    if !matches!(
        render_mode,
        memory_v2::consolidate_session::ConsolidationRenderMode::Diagnostic
    ) {
        notes.push(
            "LLM-driven procedure, failure-pattern, supersession, and duplicate proposals were not generated by this bounded manual trigger."
                .to_string(),
        );
    }
    memory_v2::consolidate_session::ConsolidationReport {
        session_id: args.session_id,
        mode,
        render_mode,
        budget_ms: args.budget_ms,
        proposals,
        categories: vec![
            memory_v2::consolidate_session::ConsolidationCategoryReport {
                category: "episode_summary".to_string(),
                proposal_ids: episode_ids,
                note: None,
            },
            memory_v2::consolidate_session::ConsolidationCategoryReport {
                category: "procedure".to_string(),
                proposal_ids: Vec::new(),
                note: Some("No reusable multi-trace procedure candidate was produced by this bounded trigger.".to_string()),
            },
            memory_v2::consolidate_session::ConsolidationCategoryReport {
                category: "failure_pattern".to_string(),
                proposal_ids: Vec::new(),
                note: Some("No failure-pattern cluster was produced by this bounded trigger.".to_string()),
            },
            memory_v2::consolidate_session::ConsolidationCategoryReport {
                category: "supersession_candidate".to_string(),
                proposal_ids: Vec::new(),
                note: Some("No supersession candidate was produced by this bounded trigger.".to_string()),
            },
            memory_v2::consolidate_session::ConsolidationCategoryReport {
                category: "duplicate_detection".to_string(),
                proposal_ids: Vec::new(),
                note: Some("No duplicate-detection proposal was produced by this bounded trigger.".to_string()),
            },
        ],
        incomplete: true,
        notes,
    }
}

fn build_metric_snapshot_notes(signals: &[lattice_core::metrics::MetricValue]) -> Vec<String> {
    let mut notes = Vec::new();
    if signals
        .iter()
        .any(|signal| signal.source == lattice_core::metrics::MetricSource::SessionMetrics)
    {
        notes.push(
            "Some signals used the explicit SessionMetrics fallback because the canonical collector returned an honest null for this session scope."
                .to_string(),
        );
    }
    if signals.iter().any(|signal| signal.value.is_none()) {
        notes.push(
            "Signals with null values lacked enough bounded evidence; reasons are reported per signal and were not fabricated."
                .to_string(),
        );
    }
    if signals.iter().any(|signal| signal.incomplete) {
        notes.push(
            "At least one signal was computed from a truncated bounded evidence slice; inspect the per-signal `incomplete` flag."
                .to_string(),
        );
    }
    if notes.is_empty() {
        notes.push(
            "All requested signals were served by the canonical Phase 9 collector.".to_string(),
        );
    }
    notes
}

fn build_event_trace_query(
    args: &memory_v2::get_event_trace::GetEventTraceArgs,
    workspace_id: &str,
    branch: Option<&str>,
    limit: usize,
) -> EventQuery {
    let mut query = match (
        args.task_id.as_ref(),
        args.session_id.as_ref(),
        args.workspace_id.as_ref(),
    ) {
        (Some(task_id), None, None) => EventQuery::new()
            .task(task_id.clone())
            .workspace(workspace_id.to_string()),
        (None, Some(session_id), None) => EventQuery::new()
            .session(session_id.clone())
            .workspace(workspace_id.to_string()),
        (None, None, Some(_)) => EventQuery::new()
            .workspace(workspace_id.to_string())
            .branch(branch.unwrap_or("main").to_string()),
        _ => EventQuery::new()
            .workspace(workspace_id.to_string())
            .branch(branch.unwrap_or("main").to_string()),
    }
    .order(QueryOrder::OldestFirst)
    .limit(limit);
    if !args.kinds.is_empty() {
        query = query.kinds(&args.kinds);
    }
    if let Some(since) = args.since {
        query = query.after(since);
    }
    if let Some(until) = args.until {
        query = query.before(until);
    }
    query
}

fn build_event_trace_page(
    args: memory_v2::get_event_trace::GetEventTraceArgs,
    render_mode: memory_v2::get_event_trace::EventTraceRenderMode,
    events: Vec<lattice_core::events::EventEnvelope>,
    next_cursor_row_id: Option<i64>,
    limit: usize,
) -> memory_v2::get_event_trace::EventTracePage {
    let next_cursor = if events.len() == limit {
        next_cursor_row_id.map(memory_v2::get_event_trace::encode_cursor)
    } else {
        None
    };
    memory_v2::get_event_trace::EventTracePage {
        scope: memory_v2::get_event_trace::EventTraceScope {
            kind: event_trace_scope_kind(&args).to_string(),
            value: args
                .task_id
                .or(args.session_id)
                .or(args.workspace_id)
                .unwrap_or_default(),
        },
        render_mode,
        cursor: args.cursor,
        next_cursor,
        events: events
            .iter()
            .map(|event| memory_v2::get_event_trace::build_entry(event, render_mode))
            .collect(),
    }
}

fn event_trace_scope_kind(args: &memory_v2::get_event_trace::GetEventTraceArgs) -> &'static str {
    if args.task_id.is_some() {
        "task"
    } else if args.session_id.is_some() {
        "session"
    } else {
        "workspace"
    }
}

/// Wrap a tool result in the MCP content format.
fn wrap_tool_result(value: Value) -> Value {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    wrap_text_result(text)
}

fn wrap_workflow_tool_result(value: Value, render: WorkflowRenderMode) -> Value {
    let serialized = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    let summary = build_tool_result_summary(&value)
        .unwrap_or_else(|| "- Structured workflow result ready.".to_string());

    match render {
        WorkflowRenderMode::Json => wrap_text_result(serialized),
        WorkflowRenderMode::Markdown => wrap_text_result(format!("### Summary\n{summary}")),
    }
}

fn wrap_text_result(text: String) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": text
        }]
    })
}

fn object_get<'a>(object: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| object.get(*key))
}

fn build_tool_result_summary(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    let mut lines = Vec::new();
    let overview = object_get(object, &["overview", "ov"])
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty());

    if overview.is_none() {
        if let Some(scenario) = object_get(object, &["scenario", "sn"])
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            lines.push(format!("- Scenario: {}", truncate_text_value(scenario, 96)));
        } else if let Some(query) = object_get(object, &["query", "q"])
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            lines.push(format!("- Query: {}", truncate_text_value(query, 96)));
        }
    }

    if let Some(overview) = overview {
        lines.push(format!("- Overview: {}", overview));
    }

    if let Some(file) = first_result_file(object) {
        lines.push(format!("- Top file: `{}`", file));
    }

    if let Some(symbol) = first_result_symbol(object) {
        lines.push(format!("- Top symbol: `{}`", symbol));
    }

    if let Some(contract) =
        object_get(object, &["agent_retrieval_contract", "arc"]).and_then(|item| item.as_object())
    {
        if let Some(next_action) = object_get(contract, &["next_action", "na"])
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            lines.push(format!(
                "- Next action: {}",
                truncate_text_value(next_action, 96)
            ));
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn first_result_file(object: &serde_json::Map<String, Value>) -> Option<String> {
    [
        "pivots",
        "context",
        "primary_files",
        "pf",
        "edit_files",
        "efi",
        "supporting_files",
        "sfi",
        "likely_entrypoints",
        "le",
        "plausible_entrypoints",
        "pe",
        "execution_path",
        "ep",
        "plausible_paths",
        "pp",
        "guards",
        "gd",
        "side_effects",
        "sx",
        "failure_branches",
        "fb",
        "candidate_spans",
        "ps",
        "affected_callers",
        "ac",
        "affected_dependencies",
        "ad",
        "relevant_docs",
        "rd",
        "changed_files",
        "cf",
        "files",
        "fs",
        "key_files",
        "kf",
        "tests",
        "ts",
        "extracted_files",
        "ef",
        "changed_symbols",
        "cs",
    ]
    .iter()
    .find_map(|key| {
        object
            .get(*key)
            .and_then(|item| item.as_array())
            .and_then(|items| items.first())
            .and_then(first_item_file)
    })
}

fn first_result_symbol(object: &serde_json::Map<String, Value>) -> Option<String> {
    [
        "pivots",
        "context",
        "symbols",
        "sy",
        "likely_entrypoints",
        "le",
        "plausible_entrypoints",
        "pe",
        "execution_path",
        "ep",
        "plausible_paths",
        "pp",
        "guards",
        "gd",
        "side_effects",
        "sx",
        "failure_branches",
        "fb",
        "candidate_spans",
        "ps",
        "affected_callers",
        "ac",
        "affected_dependencies",
        "ad",
        "active_symbols",
        "as",
        "key_symbols",
        "ks",
        "suspects",
        "su",
        "related_symbols",
        "ry",
        "changed_symbols",
        "cs",
        "notable_symbols",
        "no",
    ]
    .iter()
    .find_map(|key| {
        object
            .get(*key)
            .and_then(|item| item.as_array())
            .and_then(|items| items.first())
            .and_then(first_item_symbol)
    })
}

fn first_item_file(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    value
        .get("file")
        .or_else(|| value.get("from_file"))
        .or_else(|| value.get("to_file"))
        .or_else(|| value.get("frf"))
        .or_else(|| value.get("tof"))
        .or_else(|| value.get("f"))
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
}

fn first_item_symbol(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    value
        .get("symbol")
        .or_else(|| value.get("from_symbol"))
        .or_else(|| value.get("to_symbol"))
        .or_else(|| value.get("frs"))
        .or_else(|| value.get("tos"))
        .or_else(|| value.get("s"))
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
}
