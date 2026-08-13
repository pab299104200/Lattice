use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
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

use crate::adoption_metrics::AdoptionMetricsStore;
use crate::git_intelligence_runtime::GitIntelligenceRefreshHandle;
use crate::index_health::IndexHealth;
use crate::index_work::{IndexReadiness, IndexWorkCoordinator};
use crate::repo_state::RepoStateTracker;
use crate::watcher_health::WatcherHealth;

const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);
const WORKSPACE_INVALIDATION_BATCH_THRESHOLD: usize = 20;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Git commonly rewrites HEAD and rebase control files in a short sequence.
/// One bounded retry captures a settled state without allowing an unavailable
/// control file to turn into a synthetic workspace invalidation.
const GIT_HEAD_READ_RETRIES: u64 = 1;
const GIT_HEAD_RETRY_DELAY: Duration = Duration::from_millis(10);

/// The part of a checkout state that makes source contents potentially differ.
///
/// A symbolic ref name deliberately participates in equality even when both
/// refs resolve to the same object: changing branches changes the checkout
/// contract and must not be treated as a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedCheckoutHead {
    ref_name: Option<String>,
    target: HeadTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HeadTarget {
    Unborn,
    Resolved(String),
}

#[derive(Debug, Default)]
struct ClassifiedChanges {
    source_paths: Vec<PathBuf>,
    has_owned_git_state: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathClassification {
    Source,
    OwnedGitState,
    IgnoredGitState,
    OutsideWorkspace,
}

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
    index_work: Arc<IndexWorkCoordinator>,
    index_readiness: Arc<IndexReadiness>,
    watcher_health: Arc<WatcherHealth>,
    index_health: Arc<IndexHealth>,
    git_intelligence: Option<GitIntelligenceRefreshHandle>,
    adoption_metrics: Arc<AdoptionMetricsStore>,
    session_id: String,
    /// This baseline is intentionally owned by the watcher rather than the
    /// shared repo-state tracker.  It is updated only after a successful Git
    /// read, so transient rebase/checkout writes cannot manufacture an epoch.
    observed_head: Arc<Mutex<Option<ObservedCheckoutHead>>>,
    #[cfg(test)]
    forced_watch_failure: Option<String>,
    #[cfg(test)]
    head_read_count: AtomicU64,
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
        index_work: Arc<IndexWorkCoordinator>,
        index_readiness: Arc<IndexReadiness>,
        watcher_health: Arc<WatcherHealth>,
        index_health: Arc<IndexHealth>,
        git_intelligence: Option<GitIntelligenceRefreshHandle>,
        session_id: String,
    ) -> Self {
        let adoption_metrics = Arc::new(AdoptionMetricsStore::new(&workspace_root));
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
            index_work,
            index_readiness,
            watcher_health,
            index_health,
            git_intelligence,
            adoption_metrics,
            session_id,
            observed_head: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            forced_watch_failure: None,
            #[cfg(test)]
            head_read_count: AtomicU64::new(0),
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
        for control_path in checkout_git_control_paths(&self.workspace_root) {
            if !control_path.exists() {
                continue;
            }
            if let Err(error) = watcher.watch(&control_path, RecursiveMode::NonRecursive) {
                tracing::warn!(
                    workspace = %self.workspace_root.display(),
                    path = %control_path.display(),
                    %error,
                    "Watcher could not register an exact checkout Git control path"
                );
            }
        }
        // The baseline is captured only after the source watch is active. A
        // checkout that changes between construction and watch installation is
        // therefore represented by the first observed state, not by a
        // synthetic startup invalidation.
        self.capture_head_baseline().await;
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
        self.capture_head_baseline().await;
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
        // Native notification setup can fail on remote or constrained file
        // systems. Preserve HEAD transition behavior in degraded polling mode
        // by observing the same exact checkout-owned control paths.
        for path in checkout_git_control_paths(&self.workspace_root) {
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
                    if lattice_core::watcher::EXCLUDED_DIRS.contains(&dir_name) {
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
        !path.is_dir()
            && (path_is_checkout_git_metadata(&self.workspace_root, path)
                || path
                    .strip_prefix(&self.workspace_root)
                    .ok()
                    .is_some_and(|relative| {
                        lattice_core::watcher::should_index_file(&relative.to_string_lossy())
                    }))
    }

    async fn process_changes(&self, paths: Vec<PathBuf>) {
        self.index_readiness.wait().await;
        let changes = self.classify_changes(paths).await;
        self.record_observed_edits(&changes.source_paths);
        let head_changed = if changes.has_owned_git_state {
            self.observe_head_transition().await
        } else {
            false
        };
        let requires_workspace_invalidation =
            changes.source_paths.len() >= WORKSPACE_INVALIDATION_BATCH_THRESHOLD || head_changed;
        let target_epoch = if requires_workspace_invalidation {
            self.indexing.store(true, Ordering::Relaxed);
            let mut repo_state = self.repo_state.lock().await;
            Some(repo_state.mark_workspace_change())
        } else {
            None
        };

        // Git-only batches whose checkout state has not changed must be a true
        // no-op: no index-work permit, graph write, or indexing flag change.
        if changes.source_paths.is_empty() && !head_changed {
            return;
        }

        if self.workspace_manager.is_none() && self.indexer.is_none() {
            return;
        }

        self.indexing.store(true, Ordering::Relaxed);
        let Ok(_permit) = self
            .index_work
            .acquire(
                self.workspace_root.to_string_lossy().to_string(),
                "watcher_batch",
            )
            .await
        else {
            tracing::warn!(
                workspace = %self.workspace_root.display(),
                "Index work coordinator closed before watcher batch could run"
            );
            return;
        };

        let workspace_root = self.workspace_root.clone();
        let repo_name = self.repo_name.clone();
        let workspace_manager = self.workspace_manager.clone();
        let indexer = self.indexer.clone();
        let batch_result = tokio::task::spawn_blocking(move || {
            let batch =
                prepare_change_batch(&workspace_root, repo_name.as_deref(), changes.source_paths);
            if batch.upserts.is_empty() && batch.removals.is_empty() {
                return None;
            }
            if let Some(manager) = workspace_manager {
                let repo_name =
                    repo_name.unwrap_or_else(|| crate::repo_name_for_root(&workspace_root));
                let mut manager = manager.blocking_lock();
                let report =
                    manager.apply_file_batch_contents(&repo_name, batch.upserts, batch.removals)?;
                if report.indexed_count == 0 && batch.changed_graph_files.is_empty() {
                    return None;
                }
                manager.detect_cross_repo_edges();
                return Some((
                    Arc::new(manager.unified_graph()),
                    batch.changed_graph_files,
                    report,
                ));
            }

            let indexer = indexer?;
            let PreparedChangeBatch {
                upserts,
                removals,
                changed_graph_files,
            } = batch;
            let (upserts, removals) = if let Some(repo_name) = repo_name.as_deref() {
                (
                    upserts
                        .into_iter()
                        .map(|(path, content)| (repo_rel_path(repo_name, &path), content))
                        .collect(),
                    removals
                        .into_iter()
                        .map(|path| repo_rel_path(repo_name, &path))
                        .collect(),
                )
            } else {
                (upserts, removals)
            };
            let mut indexer = indexer.blocking_lock();
            let before_snapshot = indexer.graph_snapshot_id();
            let report = indexer.apply_file_batch_contents(upserts, removals);
            if indexer.graph_snapshot_id() == before_snapshot && !report.is_partial {
                return None;
            }
            Some((indexer.graph_arc(), changed_graph_files, report))
        })
        .await;

        match batch_result {
            Ok(Some((new_graph, changed_graph_files, report))) => {
                self.index_health.merge_change_report(&report);
                for failure in &report.failures {
                    tracing::warn!(
                        workspace = %self.workspace_root.display(),
                        file = failure.file.as_str(),
                        error = failure.message.as_str(),
                        "Watcher could not parse changed file; retaining its last valid graph entry"
                    );
                }
                self.persist_publish_and_sync(new_graph, changed_graph_files, target_epoch)
                    .await;
            }
            Ok(None) => {
                if let Some(epoch) = target_epoch {
                    let mut repo_state = self.repo_state.lock().await;
                    repo_state.mark_published_epoch(epoch);
                }
                self.indexing.store(false, Ordering::Relaxed);
            }
            Err(error) => {
                tracing::error!(
                    workspace = %self.workspace_root.display(),
                    %error,
                    "Watcher batch worker failed"
                );
                self.indexing.store(false, Ordering::Relaxed);
            }
        }
    }

    async fn capture_head_baseline(&self) {
        let snapshot = self.read_current_checkout_head();
        if let Some(snapshot) = snapshot {
            self.watcher_health.mark_git_state_healthy();
            self.request_git_intelligence_refresh(&snapshot);
            *self.observed_head.lock().await = Some(snapshot);
        }
    }

    async fn observe_head_transition(&self) -> bool {
        let mut retries = 0;
        let current = loop {
            if let Some(current) = self.read_current_checkout_head() {
                break current;
            }
            if retries == GIT_HEAD_READ_RETRIES {
                // Git rewrites HEAD and its ref files in several steps.
                // Retaining the last good baseline is safer than invalidating
                // on uncertainty; a later owned event can compare again.
                let reason = "Watcher could not read a coherent checkout HEAD after bounded retry";
                tracing::warn!(
                    workspace = %self.workspace_root.display(),
                    retries,
                    "{reason}; retaining last baseline"
                );
                self.watcher_health.mark_git_state_unknown(reason, retries);
                return false;
            }
            retries += 1;
            tokio::time::sleep(GIT_HEAD_RETRY_DELAY).await;
        };

        let mut observed = self.observed_head.lock().await;
        let changed = observed
            .as_ref()
            .is_some_and(|previous| previous != &current);
        if changed {
            self.request_git_intelligence_refresh(&current);
        }
        *observed = Some(current);
        self.watcher_health.mark_git_state_healthy();
        changed
    }

    fn read_current_checkout_head(&self) -> Option<ObservedCheckoutHead> {
        #[cfg(test)]
        self.head_read_count.fetch_add(1, Ordering::AcqRel);
        read_checkout_head(&self.workspace_root)
    }

    #[cfg(test)]
    fn reset_head_read_count_for_test(&self) {
        self.head_read_count.store(0, Ordering::Release);
    }

    #[cfg(test)]
    fn head_read_count_for_test(&self) -> u64 {
        self.head_read_count.load(Ordering::Acquire)
    }

    fn request_git_intelligence_refresh(&self, head: &ObservedCheckoutHead) {
        let Some(runtime) = self.git_intelligence.as_ref() else {
            return;
        };
        let target = match &head.target {
            HeadTarget::Unborn => None,
            HeadTarget::Resolved(oid) => Some(oid.clone()),
        };
        runtime.request(target);
    }

    async fn classify_changes(&self, paths: Vec<PathBuf>) -> ClassifiedChanges {
        let observed = self.observed_head.lock().await.clone();
        let mut classified = ClassifiedChanges::default();
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            let key = normalized_path_key(&path);
            if !seen.insert(key) {
                continue;
            }
            match classify_path(&self.workspace_root, observed.as_ref(), &path) {
                PathClassification::Source => classified.source_paths.push(path),
                PathClassification::OwnedGitState => classified.has_owned_git_state = true,
                PathClassification::IgnoredGitState | PathClassification::OutsideWorkspace => {}
            }
        }
        classified
    }

    fn record_observed_edits(&self, paths: &[PathBuf]) {
        for path in paths {
            let Ok(relative) = path.strip_prefix(&self.workspace_root) else {
                continue;
            };
            let rel_path = relative.to_string_lossy().replace('\\', "/");
            if !lattice_core::watcher::should_index_file(&rel_path) {
                continue;
            }
            let file = self
                .repo_name
                .as_deref()
                .map(|repo| repo_rel_path(repo, &rel_path))
                .unwrap_or(rel_path);
            if let Err(error) = self
                .adoption_metrics
                .record_observed_edit(&self.session_id, &file)
            {
                tracing::warn!(%error, file = file.as_str(), "failed to record watcher-observed edit for adoption metrics");
            }
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
                self.indexing.store(false, Ordering::Relaxed);
                return;
            }
        }

        let graph_store = Arc::clone(&self.graph_store);
        let graph_to_persist = Arc::clone(&new_graph);
        match tokio::task::spawn_blocking(move || {
            let graph_store = graph_store.blocking_lock();
            graph_store.save_graph(&graph_to_persist)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                workspace = %self.workspace_root.display(),
                %error,
                "Watcher failed to persist graph snapshot"
            ),
            Err(error) => tracing::warn!(
                workspace = %self.workspace_root.display(),
                %error,
                "Watcher graph persistence worker failed"
            ),
        }

        let mut engine = self.query_engine.lock().await;
        engine.update_graph_arc(Arc::clone(&new_graph));
        drop(engine);

        let Some(vector_index) = self.vector_index.as_ref() else {
            self.finish_indexing(target_epoch).await;
            return;
        };
        let Some(embedding_engine) = self.embedding_engine.get() else {
            self.finish_indexing(target_epoch).await;
            return;
        };

        // Move CPU-intensive embedding work onto the blocking thread pool so the
        // async executor stays free to service RPC calls during heavy file-change
        // storms (e.g. a remediation run writing thousands of files).
        let files_count = changed_graph_files.len();
        let graph_for_sync = Arc::clone(&new_graph);
        let files_for_sync = changed_graph_files;
        let engine_for_sync = Arc::clone(embedding_engine);
        let index_for_sync = Arc::clone(vector_index);

        let sync_started = std::time::Instant::now();
        let sync_result = tokio::task::spawn_blocking(move || {
            crate::vector_sync::sync_changed_files_embeddings(
                &graph_for_sync,
                &files_for_sync,
                engine_for_sync.as_ref(),
                index_for_sync.as_ref(),
            )
        })
        .await;

        match sync_result {
            Ok(Ok(stats)) => {
                let watcher_sync_elapsed_ms = sync_started.elapsed().as_millis();
                if files_count > 0 {
                    info!(
                        mode = stats.mode,
                        implementation = stats.implementation,
                        changed_files = files_count,
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
            Ok(Err(err)) => {
                tracing::warn!("Failed to sync semantic index after file change: {}", err)
            }
            Err(err) => tracing::warn!("Semantic sync task panicked: {}", err),
        }

        self.finish_indexing(target_epoch).await;
    }

    async fn finish_indexing(&self, target_epoch: Option<u64>) {
        if let Some(epoch) = target_epoch {
            let mut repo_state = self.repo_state.lock().await;
            repo_state.mark_published_epoch(epoch);
        }
        self.indexing.store(false, Ordering::Relaxed);
    }
}

struct PreparedChangeBatch {
    upserts: Vec<(String, String)>,
    removals: Vec<String>,
    changed_graph_files: Vec<String>,
}

fn prepare_change_batch(
    workspace_root: &Path,
    repo_name: Option<&str>,
    paths: Vec<PathBuf>,
) -> PreparedChangeBatch {
    let mut upserts = Vec::new();
    let mut removals = Vec::new();
    let mut changed_graph_files = Vec::new();
    for path in paths {
        let Ok(relative) = path.strip_prefix(workspace_root) else {
            continue;
        };
        let rel_path = relative.to_string_lossy().replace('\\', "/");
        // Callers pass only paths classified as Source. Keep this guard as a
        // boundary check so a future caller cannot accidentally parse Git
        // metadata into the graph.
        if !lattice_core::watcher::should_index_file(&rel_path) {
            continue;
        }
        let graph_path = repo_name
            .map(|repo| repo_rel_path(repo, &rel_path))
            .unwrap_or_else(|| rel_path.clone());
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => upserts.push((rel_path, content)),
                Err(error) => tracing::warn!(
                    file = %path.display(),
                    %error,
                    "Watcher could not read changed file"
                ),
            }
        } else {
            removals.push(rel_path);
        }
        changed_graph_files.push(graph_path);
    }
    PreparedChangeBatch {
        upserts,
        removals,
        changed_graph_files,
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

fn classify_path(
    workspace_root: &Path,
    observed_head: Option<&ObservedCheckoutHead>,
    path: &Path,
) -> PathClassification {
    if let Ok(relative) = path.strip_prefix(workspace_root) {
        let rel = relative.to_string_lossy().replace('\\', "/");
        if lattice_core::watcher::should_index_file(&rel) {
            return PathClassification::Source;
        }
    }
    let Some(git_rel) = checkout_git_relative_path(workspace_root, path) else {
        return PathClassification::OutsideWorkspace;
    };

    // A primary checkout sees linked worktree administrative files under its
    // own `.git`. They are never evidence about this checkout's HEAD.
    if git_rel.starts_with("worktrees/") {
        return PathClassification::IgnoredGitState;
    }
    if matches!(
        git_rel.as_str(),
        "HEAD" | "ORIG_HEAD" | "MERGE_HEAD" | "REBASE_HEAD" | "packed-refs"
    ) || git_rel.starts_with("rebase-apply/")
        || git_rel.starts_with("rebase-merge/")
    {
        return PathClassification::OwnedGitState;
    }

    let active_ref = observed_head.and_then(|head| head.ref_name.as_deref());
    if let Some(active_ref) = active_ref {
        if git_rel == active_ref {
            return PathClassification::OwnedGitState;
        }
    }
    PathClassification::IgnoredGitState
}

fn path_is_checkout_git_metadata(workspace_root: &Path, path: &Path) -> bool {
    checkout_git_relative_path(workspace_root, path).is_some()
}

fn checkout_git_relative_path(workspace_root: &Path, path: &Path) -> Option<String> {
    if let Ok(relative) = path.strip_prefix(workspace_root) {
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative == ".git" {
            return Some(String::new());
        }
        if let Some(git_relative) = relative.strip_prefix(".git/") {
            return Some(git_relative.to_string());
        }
    }
    let git_dir = checkout_git_dir(workspace_root)?;
    git_ref_directories(&git_dir)
        .into_iter()
        .find_map(|directory| {
            path.strip_prefix(directory)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
}

fn normalized_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn read_checkout_head(workspace_root: &Path) -> Option<ObservedCheckoutHead> {
    let git_dir = checkout_git_dir(workspace_root)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head.trim();
    if let Some(ref_name) = trimmed.strip_prefix("ref:").map(str::trim) {
        let target = resolve_ref_target(&git_dir, ref_name)
            .map(HeadTarget::Resolved)
            .unwrap_or(HeadTarget::Unborn);
        return Some(ObservedCheckoutHead {
            ref_name: Some(ref_name.to_string()),
            target,
        });
    }
    if trimmed.is_empty() {
        return None;
    }
    Some(ObservedCheckoutHead {
        ref_name: None,
        target: HeadTarget::Resolved(trimmed.to_string()),
    })
}

fn checkout_git_dir(workspace_root: &Path) -> Option<PathBuf> {
    let git_path = workspace_root.join(".git");
    if git_path.is_dir() {
        return git_path.canonicalize().ok();
    }
    let gitdir = std::fs::read_to_string(git_path).ok()?;
    let location = gitdir.trim().strip_prefix("gitdir:")?.trim();
    workspace_root.join(location).canonicalize().ok()
}

fn checkout_git_control_paths(workspace_root: &Path) -> Vec<PathBuf> {
    let Some(git_dir) = checkout_git_dir(workspace_root) else {
        return Vec::new();
    };
    let mut paths = vec![
        git_dir.join("HEAD"),
        git_dir.join("ORIG_HEAD"),
        git_dir.join("MERGE_HEAD"),
        git_dir.join("REBASE_HEAD"),
        git_dir.join("rebase-apply"),
        git_dir.join("rebase-merge"),
    ];
    for ref_dir in git_ref_directories(&git_dir) {
        paths.push(ref_dir.join("packed-refs"));
        if let Some(head) = read_checkout_head(workspace_root) {
            if let Some(ref_name) = head.ref_name {
                paths.push(ref_dir.join(ref_name));
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn resolve_ref_target(git_dir: &Path, ref_name: &str) -> Option<String> {
    for dir in git_ref_directories(git_dir) {
        if let Ok(contents) = std::fs::read_to_string(dir.join(ref_name)) {
            let oid = contents.trim();
            if !oid.is_empty() {
                return Some(oid.to_string());
            }
        }
        if let Some(oid) = resolve_packed_ref(&dir, ref_name) {
            return Some(oid);
        }
    }
    None
}

fn git_ref_directories(git_dir: &Path) -> Vec<PathBuf> {
    let mut directories = vec![git_dir.to_path_buf()];
    let Ok(common_dir) = std::fs::read_to_string(git_dir.join("commondir")) else {
        return directories;
    };
    let common_dir = common_dir.trim();
    if common_dir.is_empty() {
        return directories;
    }
    let common = git_dir
        .join(common_dir)
        .canonicalize()
        .unwrap_or_else(|_| git_dir.join(common_dir));
    if common != git_dir {
        directories.push(common);
    }
    directories
}

fn resolve_packed_ref(git_dir: &Path, ref_name: &str) -> Option<String> {
    let packed_refs = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    packed_refs.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            return None;
        }
        let mut parts = line.split_whitespace();
        let oid = parts.next()?;
        (parts.next()? == ref_name).then(|| oid.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature, WorktreeAddOptions};
    use lattice_core::graph::CodeGraph;
    use lattice_core::memory::MemoryStore;

    #[tokio::test]
    async fn forced_watch_failure_enters_degraded_polling_mode() {
        let root = unique_test_root("watch-degraded");
        std::fs::write(root.join("src.rs"), "fn main() {}\n").expect("write source");
        let (watcher, _, _, health, _) = test_watcher(root.clone());
        let watcher = watcher.with_forced_watch_failure("forced notify setup failure");

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

    #[tokio::test]
    async fn change_storm_rebuilds_graph_once_and_keeps_internal_files_out() {
        let root = unique_test_root("watch-batch");
        let paths = (0..64)
            .map(|index| {
                let path = root.join(format!("src/file_{index}.ts"));
                std::fs::create_dir_all(path.parent().expect("source parent"))
                    .expect("create source directory");
                std::fs::write(&path, format!("export function f{index}(): void {{}}"))
                    .expect("write source");
                path
            })
            .collect::<Vec<_>>();
        let (watcher, indexer, indexing, _, index_work) = test_watcher(root.clone());

        watcher.process_changes(paths).await;

        let indexer = indexer.lock().await;
        assert_eq!(indexer.graph_snapshot_id(), 1);
        assert_eq!(indexer.file_count(), 64);
        assert!(!indexing.load(Ordering::Acquire));
        assert_eq!(index_work.snapshot().completed_jobs, 1);
        assert!(!watcher.should_process(&root.join(".lattice/graph.db")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn standalone_multi_root_watcher_preserves_repo_namespace() {
        let root = unique_test_root("watch-repo-namespace");
        let path = root.join("src/lib.ts");
        std::fs::create_dir_all(path.parent().expect("source parent"))
            .expect("create source directory");
        std::fs::write(&path, "export function namespaced(): void {}").expect("write source");
        let (watcher, indexer, _, _, _) =
            test_watcher_with_repo(root.clone(), Some("service-a".to_string()));

        watcher.process_changes(vec![path]).await;

        let indexer = indexer.lock().await;
        assert!(indexer.parsed_files().contains_key("service-a/src/lib.ts"));
        assert!(!indexer.parsed_files().contains_key("src/lib.ts"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn watcher_records_real_edit_for_its_runtime_session_only() {
        let root = unique_test_root("watch-adoption");
        let path = root.join("src/auth.rs");
        std::fs::create_dir_all(path.parent().expect("source parent"))
            .expect("create source directory");
        std::fs::write(&path, "pub fn auth() {}\n").expect("write source");
        let (watcher, _, _, _, _) = test_watcher(root.clone());
        watcher
            .adoption_metrics
            .record(crate::adoption_metrics::ToolCallRecord {
                session_id: watcher.session_id.clone(),
                client: "codex".to_string(),
                channel: "mcp".to_string(),
                tool: "context".to_string(),
                latency_ms: 1,
                suggested_files: vec!["src/auth.rs".to_string()],
            })
            .expect("record assistance");

        watcher.record_observed_edits(&[path]);

        assert!(watcher
            .adoption_metrics
            .render_table(1)
            .expect("render metrics")
            .contains("codex | mcp | context | 1 | 1 | 1 | 100%"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn watcher_batch_uses_cold_scan_index_eligibility() {
        let root = unique_test_root("watch-eligibility");
        std::fs::create_dir_all(root.join("src")).expect("create source directory");
        let supported = root.join("src/runtime.mjs");
        let stub = root.join("src/types.pyi");
        let unsupported = root.join("src/native.cpp");
        std::fs::write(&supported, "export const value = 1;").expect("write mjs");
        std::fs::write(&stub, "def value() -> int: ...").expect("write pyi");
        std::fs::write(&unsupported, "int value();").expect("write cpp");

        let batch = prepare_change_batch(
            &root,
            None,
            vec![supported.clone(), stub.clone(), unsupported.clone()],
        );
        assert_eq!(batch.upserts.len(), 2);
        assert!(batch
            .upserts
            .iter()
            .any(|(path, _)| path.ends_with("runtime.mjs")));
        assert!(batch
            .upserts
            .iter()
            .any(|(path, _)| path.ends_with("types.pyi")));
        assert!(!batch
            .changed_graph_files
            .iter()
            .any(|path| path.ends_with("native.cpp")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn classifier_ignores_sibling_worktree_churn_even_in_large_batches() {
        let root = unique_test_root("watch-sibling-git");
        let sibling_events = (0..32)
            .map(|index| root.join(format!(".git/worktrees/sibling-{index}/HEAD")))
            .collect::<Vec<_>>();
        let classified = sibling_events
            .iter()
            .map(|path| classify_path(&root, None, path))
            .collect::<Vec<_>>();

        assert!(classified
            .iter()
            .all(|classification| *classification == PathClassification::IgnoredGitState));
        assert!(
            classified
                .iter()
                .filter(|classification| **classification == PathClassification::Source)
                .count()
                < WORKSPACE_INVALIDATION_BATCH_THRESHOLD
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn classifier_observes_only_the_current_symbolic_ref() {
        let root = unique_test_root("watch-ref-classification");
        let observed = ObservedCheckoutHead {
            ref_name: Some("refs/heads/main".to_string()),
            target: HeadTarget::Resolved("abc".to_string()),
        };

        assert_eq!(
            classify_path(&root, Some(&observed), &root.join(".git/refs/heads/main")),
            PathClassification::OwnedGitState
        );
        assert_eq!(
            classify_path(
                &root,
                Some(&observed),
                &root.join(".git/refs/heads/sibling")
            ),
            PathClassification::IgnoredGitState
        );
        assert_eq!(
            classify_path(&root, Some(&observed), &root.join(".git/index")),
            PathClassification::IgnoredGitState
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn own_branch_switch_at_same_target_invalidates_once() {
        let root = unique_test_root("watch-own-branch-switch");
        std::fs::create_dir_all(root.join(".git/refs/heads")).expect("create git refs");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write head");
        std::fs::write(root.join(".git/refs/heads/main"), "same\n").expect("write main");
        std::fs::write(root.join(".git/refs/heads/feature"), "same\n").expect("write feature");
        let (watcher, _, _, _, index_work) = test_watcher(root.clone());
        watcher.capture_head_baseline().await;

        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature\n").expect("switch branch");
        watcher.process_changes(vec![root.join(".git/HEAD")]).await;
        watcher.process_changes(vec![root.join(".git/HEAD")]).await;

        assert_eq!(index_work.snapshot().completed_jobs, 1);
        assert_eq!(watcher.repo_state.lock().await.current_epoch(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn head_baseline_and_transition_request_git_intelligence_refreshes() {
        let root = unique_test_root("watch-git-intelligence");
        std::fs::create_dir_all(root.join(".git/refs/heads")).expect("create git refs");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write head");
        std::fs::write(root.join(".git/refs/heads/main"), "first-oid\n")
            .expect("write initial ref");
        let (mut watcher, _, _, _, _) = test_watcher(root.clone());
        let refresh = GitIntelligenceRefreshHandle::for_test();
        watcher.git_intelligence = Some(refresh.clone());

        watcher.capture_head_baseline().await;
        assert_eq!(
            refresh.latest_request_for_test(),
            (1, Some("first-oid".to_string()))
        );

        std::fs::write(root.join(".git/refs/heads/main"), "second-oid\n").expect("advance ref");
        assert!(watcher.observe_head_transition().await);
        assert_eq!(
            refresh.latest_request_for_test(),
            (2, Some("second-oid".to_string()))
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sibling_worktree_events_and_unchanged_packed_refs_do_no_index_work() {
        let root = unique_test_root("watch-sibling-noop");
        std::fs::create_dir_all(root.join(".git/refs/heads")).expect("create git refs");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write head");
        std::fs::write(root.join(".git/refs/heads/main"), "main\n").expect("write main");
        std::fs::write(root.join(".git/packed-refs"), "").expect("write packed refs");
        let (watcher, _, _, _, index_work) = test_watcher(root.clone());
        watcher.capture_head_baseline().await;
        let initial_epoch = watcher.repo_state.lock().await.current_epoch();

        let sibling_events = (0..32)
            .map(|index| root.join(format!(".git/worktrees/sibling-{index}/HEAD")))
            .collect();
        watcher.process_changes(sibling_events).await;
        watcher
            .process_changes(vec![root.join(".git/packed-refs")])
            .await;

        assert_eq!(index_work.snapshot().completed_jobs, 0);
        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn checkout_head_resolves_a_linked_worktrees_common_ref() {
        let root = unique_test_root("watch-linked-head");
        let private_git_dir = root.join("private-git");
        let common_git_dir = root.join("common-git");
        std::fs::create_dir_all(common_git_dir.join("refs/heads")).expect("create common refs");
        std::fs::create_dir_all(&private_git_dir).expect("create private git dir");
        std::fs::write(root.join(".git"), "gitdir: private-git\n").expect("write pointer");
        std::fs::write(private_git_dir.join("HEAD"), "ref: refs/heads/linked\n")
            .expect("write head");
        std::fs::write(private_git_dir.join("commondir"), "../common-git\n")
            .expect("write common dir");
        std::fs::write(common_git_dir.join("refs/heads/linked"), "linked-oid\n")
            .expect("write linked ref");

        assert_eq!(
            read_checkout_head(&root),
            Some(ObservedCheckoutHead {
                ref_name: Some("refs/heads/linked".to_string()),
                target: HeadTarget::Resolved("linked-oid".to_string()),
            })
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn linked_checkout_registers_only_exact_owned_git_control_paths() {
        let root = unique_test_root("watch-linked-controls");
        let private_git_dir = root.join("private-git");
        let common_git_dir = root.join("common-git");
        std::fs::create_dir_all(common_git_dir.join("refs/heads")).expect("create common refs");
        std::fs::create_dir_all(&private_git_dir).expect("create private git dir");
        std::fs::write(root.join(".git"), "gitdir: private-git\n").expect("write pointer");
        std::fs::write(private_git_dir.join("HEAD"), "ref: refs/heads/linked\n")
            .expect("write head");
        std::fs::write(private_git_dir.join("commondir"), "../common-git\n")
            .expect("write common dir");
        std::fs::write(common_git_dir.join("refs/heads/linked"), "linked-oid\n")
            .expect("write linked ref");
        std::fs::write(common_git_dir.join("packed-refs"), "").expect("write packed refs");

        let paths = checkout_git_control_paths(&root);
        assert!(paths
            .iter()
            .any(|path| path.ends_with(Path::new("private-git/HEAD"))));
        assert!(paths
            .iter()
            .any(|path| path.ends_with(Path::new("common-git/refs/heads/linked"))));
        assert!(paths
            .iter()
            .any(|path| path.ends_with(Path::new("common-git/packed-refs"))));
        assert!(paths.iter().all(|path| !path.ends_with("worktrees")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn real_git2_linked_worktree_churn_does_not_invalidate_primary_checkout() {
        let mut fixture = LinkedWorktreeFixture::new("watch-real-linked-worktree");
        let (watcher, indexer, _, _, index_work) = test_watcher(fixture.primary_root.clone());
        watcher.capture_head_baseline().await;
        let initial_epoch = watcher.repo_state.lock().await.current_epoch();

        // This commit changes both the linked checkout's private HEAD metadata
        // and its shared branch ref. Neither belongs to the primary checkout.
        let sibling_commit = fixture.commit_in_sibling("src/sibling.rs", "pub fn sibling() {}\n");
        watcher
            .process_changes(vec![
                fixture.sibling_git_dir().join("HEAD"),
                fixture.primary_git_dir().join("refs/heads/sibling"),
            ])
            .await;
        fixture
            .primary
            .reference(
                "refs/heads/unrelated",
                sibling_commit,
                true,
                "test unrelated ref",
            )
            .expect("create unrelated branch");
        std::fs::write(
            fixture.primary_git_dir().join("packed-refs"),
            "# unchanged current head\n",
        )
        .expect("write packed refs fixture");
        watcher
            .process_changes(vec![
                fixture.primary_git_dir().join("refs/heads/unrelated"),
                fixture.primary_git_dir().join("packed-refs"),
            ])
            .await;

        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch
        );
        assert_eq!(index_work.snapshot().completed_jobs, 0);
        assert_eq!(indexer.lock().await.graph_snapshot_id(), 0);

        // `feature` and `main` intentionally begin at the same object. The
        // ref name still changes the checkout contract and must invalidate once.
        fixture
            .primary
            .set_head("refs/heads/feature")
            .expect("switch primary branch");
        watcher
            .process_changes(vec![fixture.primary_git_dir().join("HEAD")])
            .await;
        watcher
            .process_changes(vec![fixture.primary_git_dir().join("HEAD")])
            .await;
        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch + 1
        );
        assert_eq!(index_work.snapshot().completed_jobs, 1);

        // Moving the active ref is a reset-style transition and must also run
        // one reindex cycle, even when the move is observed more than once.
        fixture
            .primary
            .reference("refs/heads/feature", sibling_commit, true, "test reset")
            .expect("move active branch");
        watcher
            .process_changes(vec![fixture.primary_git_dir().join("refs/heads/feature")])
            .await;
        watcher
            .process_changes(vec![fixture.primary_git_dir().join("refs/heads/feature")])
            .await;
        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch + 2
        );
        assert_eq!(index_work.snapshot().completed_jobs, 2);

        fixture
            .primary
            .set_head_detached(sibling_commit)
            .expect("detach primary HEAD");
        watcher
            .process_changes(vec![fixture.primary_git_dir().join("HEAD")])
            .await;
        watcher
            .process_changes(vec![fixture.primary_git_dir().join("HEAD")])
            .await;
        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch + 3
        );
        assert_eq!(index_work.snapshot().completed_jobs, 3);
        assert_eq!(indexer.lock().await.graph_snapshot_id(), 0);

        fixture.cleanup();
    }

    #[tokio::test]
    async fn git_head_rebase_retry_is_bounded_and_never_manufactures_an_epoch() {
        let mut fixture = LinkedWorktreeFixture::new("watch-rebase-retry");
        let (watcher, _, _, health, index_work) = test_watcher(fixture.primary_root.clone());
        watcher.capture_head_baseline().await;
        watcher.reset_head_read_count_for_test();
        let initial_epoch = watcher.repo_state.lock().await.current_epoch();
        let settled_target = fixture.commit_in_sibling("src/settled.rs", "pub fn settled() {}\n");
        let head_path = fixture.primary_git_dir().join("HEAD");
        let hidden_head = fixture.primary_git_dir().join("HEAD.rebase-fixture");
        std::fs::rename(&head_path, &hidden_head).expect("hide primary HEAD during rebase");
        std::fs::create_dir_all(fixture.primary_git_dir().join("rebase-merge"))
            .expect("create rebase metadata");

        let settling_head = head_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            std::fs::write(settling_head, format!("{settled_target}\n"))
                .expect("settle detached HEAD");
        });
        watcher
            .process_changes(vec![fixture
                .primary_git_dir()
                .join("rebase-merge/head-name")])
            .await;

        assert_eq!(watcher.head_read_count_for_test(), 2);
        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch + 1
        );
        assert_eq!(index_work.snapshot().completed_jobs, 1);
        assert_eq!(
            health.snapshot().git_state,
            crate::watcher_health::GitStateHealth::Healthy
        );

        std::fs::remove_file(&head_path).expect("hide permanent unreadable HEAD");
        watcher.reset_head_read_count_for_test();
        watcher
            .process_changes(vec![fixture
                .primary_git_dir()
                .join("rebase-merge/head-name")])
            .await;

        let snapshot = health.snapshot();
        assert_eq!(watcher.head_read_count_for_test(), 2);
        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch + 1
        );
        assert_eq!(index_work.snapshot().completed_jobs, 1);
        assert_eq!(
            snapshot.git_state,
            crate::watcher_health::GitStateHealth::Unknown
        );
        assert_eq!(snapshot.git_state_retry_count, GIT_HEAD_READ_RETRIES);
        assert!(snapshot.git_state_reason.is_some());

        // The private HEAD was deliberately hidden only for the test. Restore
        // it before removing the fixture so libgit2 handles remain coherent.
        std::fs::rename(hidden_head, head_path).expect("restore primary HEAD");
        fixture.cleanup();
    }

    #[tokio::test]
    async fn mixed_source_and_sibling_worktree_metadata_uses_only_source_threshold() {
        let fixture = LinkedWorktreeFixture::new("watch-mixed-worktree-batch");
        let (watcher, indexer, _, _, index_work) = test_watcher(fixture.primary_root.clone());
        watcher.capture_head_baseline().await;
        let initial_epoch = watcher.repo_state.lock().await.current_epoch();
        let source_paths = (0..WORKSPACE_INVALIDATION_BATCH_THRESHOLD - 1)
            .map(|index| {
                let path = fixture.primary_root.join(format!("src/file_{index}.ts"));
                std::fs::create_dir_all(path.parent().expect("source parent"))
                    .expect("create source directory");
                std::fs::write(&path, format!("export function f{index}(): void {{}}"))
                    .expect("write source");
                path
            })
            .collect::<Vec<_>>();
        let sibling_paths = (0..WORKSPACE_INVALIDATION_BATCH_THRESHOLD + 5)
            .map(|index| fixture.sibling_git_dir().join(format!("logs/HEAD-{index}")))
            .collect::<Vec<_>>();

        watcher
            .process_changes(source_paths.into_iter().chain(sibling_paths).collect())
            .await;

        assert_eq!(
            watcher.repo_state.lock().await.current_epoch(),
            initial_epoch
        );
        assert_eq!(index_work.snapshot().completed_jobs, 1);
        assert_eq!(
            indexer.lock().await.file_count(),
            WORKSPACE_INVALIDATION_BATCH_THRESHOLD - 1
        );

        fixture.cleanup();
    }

    struct LinkedWorktreeFixture {
        primary_root: PathBuf,
        linked_root: PathBuf,
        primary: Repository,
        sibling: Repository,
    }

    impl LinkedWorktreeFixture {
        fn new(name: &str) -> Self {
            let primary_root = unique_test_root(name);
            let linked_root = primary_root.with_extension("linked-worktree");
            let primary = Repository::init(&primary_root).expect("initialize primary repository");
            let initial = commit_fixture_file(
                &primary,
                &primary_root,
                "src/lib.rs",
                "pub fn initial() {}\n",
                "initial fixture commit",
            );
            let initial_commit = primary.find_commit(initial).expect("find initial commit");
            primary
                .branch("main", &initial_commit, true)
                .expect("create main branch");
            primary
                .branch("feature", &initial_commit, true)
                .expect("create same-target feature branch");
            primary
                .branch("sibling", &initial_commit, true)
                .expect("create sibling branch");
            drop(initial_commit);
            primary
                .set_head("refs/heads/main")
                .expect("set primary branch");

            let sibling_ref = primary
                .find_reference("refs/heads/sibling")
                .expect("find sibling branch");
            let mut options = WorktreeAddOptions::new();
            options.reference(Some(&sibling_ref));
            primary
                .worktree("sibling", &linked_root, Some(&options))
                .expect("create linked worktree with libgit2");
            drop(sibling_ref);
            let sibling = Repository::open(&linked_root).expect("open linked worktree");
            Self {
                primary_root,
                linked_root,
                primary,
                sibling,
            }
        }

        fn primary_git_dir(&self) -> PathBuf {
            self.primary.path().to_path_buf()
        }

        fn sibling_git_dir(&self) -> PathBuf {
            self.sibling.path().to_path_buf()
        }

        fn commit_in_sibling(&mut self, relative: &str, contents: &str) -> git2::Oid {
            commit_fixture_file(
                &self.sibling,
                &self.linked_root,
                relative,
                contents,
                "linked worktree fixture commit",
            )
        }

        fn cleanup(self) {
            drop(self.sibling);
            drop(self.primary);
            let _ = std::fs::remove_dir_all(&self.linked_root);
            let _ = std::fs::remove_dir_all(&self.primary_root);
        }
    }

    fn commit_fixture_file(
        repository: &Repository,
        checkout_root: &Path,
        relative: &str,
        contents: &str,
        message: &str,
    ) -> git2::Oid {
        let path = checkout_root.join(relative);
        std::fs::create_dir_all(path.parent().expect("fixture source parent"))
            .expect("create fixture source directory");
        std::fs::write(&path, contents).expect("write fixture source");
        let mut index = repository.index().expect("open fixture index");
        index
            .add_path(Path::new(relative))
            .expect("stage fixture source through libgit2");
        index.write().expect("write fixture index");
        let tree_id = index.write_tree().expect("write fixture tree");
        let tree = repository.find_tree(tree_id).expect("find fixture tree");
        let signature = Signature::now("Lattice Fixture", "fixture@lattice.test")
            .expect("build fixture signature");
        let parent = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repository.find_commit(oid).ok());
        let parents = parent.iter().collect::<Vec<_>>();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents,
            )
            .expect("create fixture commit through libgit2")
    }

    type TestWatcher = (
        FileWatcher,
        Arc<Mutex<Indexer>>,
        Arc<AtomicBool>,
        Arc<WatcherHealth>,
        Arc<IndexWorkCoordinator>,
    );

    fn test_watcher(root: PathBuf) -> TestWatcher {
        test_watcher_with_repo(root, None)
    }

    fn test_watcher_with_repo(root: PathBuf, repo_name: Option<String>) -> TestWatcher {
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
        let index_work = IndexWorkCoordinator::new(1);
        let readiness = Arc::new(IndexReadiness::default());
        readiness.mark_ready();
        let watcher = FileWatcher::new(
            root.clone(),
            repo_name,
            Some(Arc::clone(&indexer)),
            None,
            graph_store,
            engine,
            Arc::new(OnceLock::new()),
            None,
            repo_state,
            Arc::clone(&indexing),
            Arc::clone(&index_work),
            readiness,
            Arc::clone(&health),
            Arc::new(IndexHealth::default()),
            None,
            "test-session".to_string(),
        );
        (watcher, indexer, indexing, health, index_work)
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
