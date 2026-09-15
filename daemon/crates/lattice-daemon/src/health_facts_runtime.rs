//! Runtime orchestration for bounded health-fact refreshes.
//!
//! Watchers only report that files changed. One latest-wins worker per
//! repository serializes production, shares the process-wide index-work
//! admission control, and delegates publication to the transactional H2
//! stores. This mirrors [`crate::git_intelligence_runtime`] deliberately: the
//! two runtimes solve the same problem — keep an expensive derived view off
//! the request path — and a reader who knows one should recognize the other.
//!
//! # Why a runtime at all
//!
//! H4 built the fact index from the live graph on every request. That made the
//! four generational stores H2 built dead weight, left complexity facts
//! permanently unavailable, and paid a whole-graph pass per request. Producing
//! here and publishing into the in-memory handoff means a request clones an
//! `Arc` instead (`docs/architecture/2026-08-13-health-engine.md`).
//!
//! # Incrementality, per family
//!
//! The three graph-derived families are **whole-graph but debounced**, and this
//! is a correctness decision rather than an unfinished one. Their facts are not
//! functions of a file in isolation:
//!
//! - Graph facts rank fan-in, fan-out and cycle membership. Cycle membership is
//!   an SCC property, and a single added edge can merge two components that
//!   share no file with the edited one, so no changed-file subset bounds what
//!   must be recomputed.
//! - Dead-symbol facts assert that *nothing anywhere* depends on a symbol. One
//!   new call edge in an unrelated file can revive a candidate, so the evidence
//!   for "dead" is global by construction.
//! - Test-proximity facts are reachability from test files to production files;
//!   a new edge anywhere can link a previously untested file.
//!
//! The `file_delta` / `candidate_delta` methods on those snapshots are
//! *comparison* helpers — each takes an already-produced snapshot and reports
//! which paths moved — not incremental producers. Coalescing to the settling
//! window is therefore the honest bound: the pass runs at most once per window
//! however many files were saved, instead of once per request as before.
//!
//! Complexity facts are the genuine exception. Their unit of computation is one
//! file, and `HealthComplexityFactsStore::write_file_facts` refreshes exactly
//! one file's rows inside the published generation, so only changed files are
//! recomputed once a generation exists.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use lattice_core::graph::CodeGraph;
use lattice_core::health::complexity_facts::{compute_file_complexity_facts, FileComplexityFacts};
use lattice_core::health::config::HEALTH_CONFIG_VERSION;
use lattice_core::health::dead_symbol_facts::{
    DeadSymbolExclusionInputs, DeadSymbolFactProducer, DeadSymbolFactsSnapshot,
};
use lattice_core::health::graph_facts::{GraphFactProducer, GraphFactsSnapshot};
use lattice_core::health::test_proximity_facts::{
    TestProximityFactProducer, TestProximitySnapshot,
};
use lattice_core::query::QueryEngine;
use lattice_core::storage::{
    HealthComplexityFactsStore, HealthDeadSymbolFactsStore, HealthGraphFactsStore,
    HealthTestProximityFactsStore,
};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;

use crate::index_health::IndexHealth;
use crate::index_work::IndexWorkCoordinator;
use crate::rpc::mcp::{HealthFactsSnapshotHandle, PublishedHealthFacts};

/// The publication surface the runtime writes through.
///
/// Kept as a trait for the same reason the Git-intelligence runtime keeps one:
/// a test needs to observe what was published, and how often, without standing
/// up an MCP handler.
trait HealthFactsPublisher: Send + Sync {
    fn publish(&self, repository_id: &str, facts: PublishedHealthFacts) -> Result<(), String>;
}

impl HealthFactsPublisher for HealthFactsSnapshotHandle {
    fn publish(&self, repository_id: &str, facts: PublishedHealthFacts) -> Result<(), String> {
        HealthFactsSnapshotHandle::publish(self, repository_id, facts)
    }
}

/// Non-blocking watcher-side trigger for a repository refresh.
///
/// Changed paths accumulate rather than replace. The sequence number is
/// latest-wins so the worker collapses a burst into one pass, but the *set* of
/// files that burst touched must survive the collapse, or an incremental
/// complexity refresh would silently skip files saved while a pass was running.
#[derive(Clone)]
pub(crate) struct HealthFactsRefreshHandle {
    sender: watch::Sender<u64>,
    next_sequence: Arc<AtomicU64>,
    pending: Arc<StdMutex<BTreeSet<String>>>,
    full_refresh: Arc<AtomicBool>,
}

impl HealthFactsRefreshHandle {
    /// Records changed workspace-relative paths and wakes the worker.
    pub(crate) fn request<I, S>(&self, changed: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        match self.pending.lock() {
            Ok(mut pending) => pending.extend(changed.into_iter().map(Into::into)),
            Err(_) => {
                tracing::warn!("Health-fact pending-change lock was poisoned; refresh skipped");
                return;
            }
        }
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        self.sender.send_replace(sequence);
    }

    /// Requests a complete pass over the repository represented by the
    /// currently published graph. Startup uses this after graph publication,
    /// including when the incremental index reports no changed files.
    pub(crate) fn request_full(&self) {
        self.full_refresh.store(true, Ordering::Release);
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        self.sender.send_replace(sequence);
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let (sender, _receiver) = watch::channel(0);
        Self {
            sender,
            next_sequence: Arc::new(AtomicU64::new(1)),
            pending: Arc::new(StdMutex::new(BTreeSet::new())),
            full_refresh: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(&self) -> Vec<String> {
        self.pending
            .lock()
            .expect("pending lock")
            .iter()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn latest_sequence_for_test(&self) -> u64 {
        *self.sender.borrow()
    }
}

/// The four H2 stores, opened once and shared with the blocking worker.
struct HealthFactStores {
    graph: HealthGraphFactsStore,
    complexity: HealthComplexityFactsStore,
    test_proximity: HealthTestProximityFactsStore,
    dead_symbols: HealthDeadSymbolFactsStore,
}

/// A validated worker which owns the only health-fact publication path for its
/// repository.
pub(crate) struct HealthFactsRuntime {
    workspace_root: PathBuf,
    repository_id: String,
    engine: Arc<Mutex<QueryEngine>>,
    stores: Arc<StdMutex<HealthFactStores>>,
    requests: watch::Receiver<u64>,
    pending: Arc<StdMutex<BTreeSet<String>>>,
    full_refresh: Arc<AtomicBool>,
    graph_path_prefix: Option<String>,
    index_work: Arc<IndexWorkCoordinator>,
    index_health: Arc<IndexHealth>,
    snapshots: Arc<dyn HealthFactsPublisher>,
    /// The complexity facts as last published, kept in memory so an
    /// incremental refresh mutates only the changed keys instead of re-reading
    /// every row out of SQLite to rebuild the map.
    complexity: Arc<StdMutex<BTreeMap<String, FileComplexityFacts>>>,
    runtime_work: Arc<crate::index_work::RuntimeWorkTracker>,
}

impl HealthFactsRuntime {
    /// Opens and validates all four persistence surfaces before a workspace
    /// runtime is admitted, then hydrates the handoff from whatever
    /// generations are already active.
    ///
    /// Production starts after the startup index publishes its immutable graph;
    /// watcher deltas drive subsequent incremental complexity refreshes.
    pub(crate) fn open(
        workspace_root: PathBuf,
        graph_path: &Path,
        repository_id: String,
        engine: Arc<Mutex<QueryEngine>>,
        index_work: Arc<IndexWorkCoordinator>,
        index_health: Arc<IndexHealth>,
        graph_path_prefix: Option<String>,
        snapshots: HealthFactsSnapshotHandle,
    ) -> Result<(HealthFactsRefreshHandle, Self)> {
        Self::open_with_publisher(
            workspace_root,
            graph_path,
            repository_id,
            engine,
            index_work,
            index_health,
            graph_path_prefix,
            Arc::new(snapshots),
        )
    }

    fn open_with_publisher(
        workspace_root: PathBuf,
        graph_path: &Path,
        repository_id: String,
        engine: Arc<Mutex<QueryEngine>>,
        index_work: Arc<IndexWorkCoordinator>,
        index_health: Arc<IndexHealth>,
        graph_path_prefix: Option<String>,
        snapshots: Arc<dyn HealthFactsPublisher>,
    ) -> Result<(HealthFactsRefreshHandle, Self)> {
        let context = |family: &str| {
            format!(
                "failed to initialize {family} health facts for `{}`",
                workspace_root.display()
            )
        };
        let complexity_path = complexity_store_path(graph_path, &repository_id);
        let complexity_store = HealthComplexityFactsStore::open(&complexity_path)
            .with_context(|| context("complexity"))?;
        complexity_store.prune_generations(2).with_context(|| {
            format!(
                "failed to prune stale complexity generations in `{}`",
                complexity_path.display()
            )
        })?;
        let stores = HealthFactStores {
            graph: HealthGraphFactsStore::open(graph_path).with_context(|| context("graph"))?,
            // Complexity's core store has one database-wide active pointer,
            // unlike the repository-keyed snapshot stores. Give each
            // repository its own database so one multi-repo runtime cannot
            // replace another repository's active generation.
            complexity: complexity_store,
            test_proximity: HealthTestProximityFactsStore::open(graph_path)
                .with_context(|| context("test-proximity"))?,
            dead_symbols: HealthDeadSymbolFactsStore::open(graph_path)
                .with_context(|| context("dead-symbol"))?,
        };

        // Audit every active pointer while construction can still fail
        // cleanly. Recovery is intentionally not automatic: an unreadable
        // generation must be retained for diagnosis rather than erased.
        let hydrated = Self::load_published(&stores, &repository_id)
            .with_context(|| format!("failed to validate health facts for `{repository_id}`"))?;
        let complexity = hydrated
            .complexity
            .as_ref()
            .map(|facts| facts.as_ref().clone())
            .unwrap_or_default();
        if !hydrated_is_empty(&hydrated) {
            snapshots
                .publish(&repository_id, hydrated)
                .map_err(anyhow::Error::msg)
                .context("failed to hydrate the health-fact handoff")?;
        }

        let (sender, requests) = watch::channel(0);
        let pending = Arc::new(StdMutex::new(BTreeSet::new()));
        let full_refresh = Arc::new(AtomicBool::new(false));
        let handle = HealthFactsRefreshHandle {
            sender,
            next_sequence: Arc::new(AtomicU64::new(1)),
            pending: Arc::clone(&pending),
            full_refresh: Arc::clone(&full_refresh),
        };
        Ok((
            handle,
            Self {
                workspace_root,
                repository_id,
                engine,
                stores: Arc::new(StdMutex::new(stores)),
                requests,
                pending,
                full_refresh,
                graph_path_prefix,
                index_work,
                index_health,
                snapshots,
                complexity: Arc::new(StdMutex::new(complexity)),
                runtime_work: Arc::new(crate::index_work::RuntimeWorkTracker::default()),
            },
        ))
    }

    pub(crate) fn with_runtime_work_tracker(
        mut self,
        runtime_work: Arc<crate::index_work::RuntimeWorkTracker>,
    ) -> Self {
        self.runtime_work = runtime_work;
        self
    }

    /// Reads every active generation into the shape the read path consumes.
    fn load_published(
        stores: &HealthFactStores,
        repository_id: &str,
    ) -> Result<PublishedHealthFacts> {
        let graph = stores
            .graph
            .load_active(repository_id)
            .context("failed to read the active graph-fact generation")?
            .map(|stored| Arc::new(stored.snapshot));
        let test_proximity = stores
            .test_proximity
            .load_active(repository_id)
            .context("failed to read the active test-proximity generation")?
            .map(|stored| Arc::new(stored.snapshot));
        let dead_symbols = stores
            .dead_symbols
            .load_active(repository_id)
            .context("failed to read the active dead-symbol generation")?
            .map(|stored| Arc::new(stored.snapshot));

        // The complexity store is row-generational rather than
        // snapshot-generational, so "active" is a generation number and its
        // rows, not a serialized blob.
        let complexity = match stores
            .complexity
            .active_generation()
            .context("failed to read the active complexity generation")?
        {
            Some(_) => {
                let rows = stores
                    .complexity
                    .active_file_facts()
                    .context("failed to read active complexity facts")?;
                Some(Arc::new(
                    rows.into_iter()
                        .map(|facts| (facts.file.clone(), facts))
                        .collect::<BTreeMap<_, _>>(),
                ))
            }
            None => None,
        };

        Ok(PublishedHealthFacts {
            graph,
            complexity,
            test_proximity,
            dead_symbols,
        })
    }

    pub(crate) fn spawn(mut self) -> JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(&mut self) {
        while self.requests.changed().await.is_ok() {
            let mut sequence = *self.requests.borrow_and_update();
            let mut retry_delay = std::time::Duration::from_secs(1);
            loop {
                match self.refresh(sequence).await {
                    Ok(()) => break,
                    Err(error) => tracing::warn!(
                        workspace = %self.workspace_root.display(),
                        repository_id = self.repository_id.as_str(),
                        %error,
                        retry_seconds = retry_delay.as_secs(),
                        "Health-fact refresh failed; retaining the prior generation and retrying"
                    ),
                }
                tokio::select! {
                    _ = tokio::time::sleep(retry_delay) => {}
                    changed = self.requests.changed() => {
                        if changed.is_err() { return; }
                        sequence = *self.requests.borrow_and_update();
                    }
                }
                retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(60));
            }
        }
    }

    /// Produces and publishes one generation of every family.
    ///
    /// Changed paths are drained before any work begins, so files saved while
    /// this pass runs re-arm the worker rather than being lost.
    async fn refresh(&self, _sequence: u64) -> Result<()> {
        let full_refresh = self.full_refresh.swap(false, Ordering::AcqRel);
        let changed = self.drain_pending()?;
        if changed.is_empty() && !full_refresh {
            return Ok(());
        }

        let permit = match self
            .index_work
            .acquire(
                self.workspace_root.to_string_lossy().to_string(),
                "health_facts",
            )
            .await
        {
            Ok(permit) => Arc::new(permit),
            Err(error) => {
                self.requeue(changed, full_refresh);
                return Err(error)
                    .context("index work coordinator closed before health-fact production");
            }
        };

        // The graph is an immutable `Arc`, so the engine lock is held only long
        // enough to clone the pointer. Producing under it would block every
        // request for the duration of a whole-graph pass.
        let graph = {
            let engine = self.engine.lock().await;
            engine.graph_snapshot()
        };
        let workspace_root = self.workspace_root.clone();
        let repository_id = self.repository_id.clone();
        let stores = Arc::clone(&self.stores);
        let complexity_cache = Arc::clone(&self.complexity);
        let index_complete = !self.index_health.snapshot(0).is_partial;
        let retry_changed = changed.clone();
        let graph_path_prefix = self.graph_path_prefix.clone();
        let runtime_work = self.runtime_work.begin();
        let child_permit = Arc::clone(&permit);

        let (published, production_failures) =
            tokio::task::spawn_blocking(move || -> Result<(PublishedHealthFacts, Vec<String>)> {
                let (_runtime_work, _child_permit) = (runtime_work, child_permit);
                let graph = repository_graph(graph, graph_path_prefix.as_deref());
                let mut changed = changed;
                if full_refresh {
                    changed.extend(graph.all_nodes().into_iter().map(|node| node.file.clone()));
                }
                produce_and_publish(
                    &graph,
                    &workspace_root,
                    &repository_id,
                    &changed,
                    full_refresh,
                    index_complete,
                    &stores,
                    &complexity_cache,
                )
            })
            .await
            .context("health-fact production worker panicked")
            .and_then(|result| result)
            .inspect_err(|_| self.requeue(retry_changed.clone(), full_refresh))?;

        self.snapshots
            .publish(&self.repository_id, published)
            .map_err(anyhow::Error::msg)
            .context("failed to publish the health-fact handoff")
            .inspect_err(|_| self.requeue(retry_changed.clone(), full_refresh))?;

        if !production_failures.is_empty() {
            self.requeue(retry_changed, full_refresh);
            anyhow::bail!(
                "health-fact families failed to publish: {}",
                production_failures.join("; ")
            );
        }

        tracing::info!(
            workspace = %self.workspace_root.display(),
            repository_id = self.repository_id.as_str(),
            "Published health fact generation"
        );
        Ok(())
    }

    fn drain_pending(&self) -> Result<BTreeSet<String>> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("health-fact pending-change lock was poisoned"))?;
        Ok(std::mem::take(&mut *pending))
    }

    fn requeue(&self, changed: BTreeSet<String>, full_refresh: bool) {
        if full_refresh {
            self.full_refresh.store(true, Ordering::Release);
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.extend(changed);
        }
    }
}

fn complexity_store_path(graph_path: &Path, repository_id: &str) -> PathBuf {
    graph_path.with_file_name(format!("health-complexity-{repository_id}.db"))
}

fn repository_graph(graph: Arc<CodeGraph>, prefix: Option<&str>) -> Arc<CodeGraph> {
    let Some(prefix) = prefix else {
        return graph;
    };
    let prefix = format!("{prefix}/");
    let mut localized = CodeGraph::new();
    let mut ids = HashMap::new();
    for node in graph.all_nodes() {
        let Some(file) = node.file.strip_prefix(&prefix) else {
            continue;
        };
        let mut id = node.id.clone();
        id.file = file.to_string();
        localized.add_node(
            id.clone(),
            node.kind,
            node.name.clone(),
            node.signature.clone(),
            node.body.clone(),
            file.to_string(),
            node.line,
            node.end_line,
            node.is_exported,
            node.language,
        );
        ids.insert(node.id.clone(), id);
    }
    for (from, to, kind) in graph.all_edges() {
        if let (Some(from), Some(to)) = (ids.get(&from.id), ids.get(&to.id)) {
            localized.add_edge(from, to, kind);
        }
    }
    Arc::new(localized)
}

fn hydrated_is_empty(facts: &PublishedHealthFacts) -> bool {
    facts.graph.is_none()
        && facts.complexity.is_none()
        && facts.test_proximity.is_none()
        && facts.dead_symbols.is_none()
}

/// The blocking half of a refresh: three whole-graph passes and an incremental
/// complexity update, each published through its own transactional store.
///
/// A single family failing to publish must not discard the others, so each is
/// attempted independently and a failure retains that family's prior
/// generation while the rest move forward. Availability is never fabricated:
/// a family whose publication failed is omitted from the new handoff while its
/// prior stored generation remains available for diagnosis and retry.
fn produce_and_publish(
    graph: &CodeGraph,
    workspace_root: &Path,
    repository_id: &str,
    changed: &BTreeSet<String>,
    full_refresh: bool,
    index_complete: bool,
    stores: &Arc<StdMutex<HealthFactStores>>,
    complexity_cache: &Arc<StdMutex<BTreeMap<String, FileComplexityFacts>>>,
) -> Result<(PublishedHealthFacts, Vec<String>)> {
    let refreshed_at = unix_timestamp()?;

    // The graph is immutable and fully published, while `index_complete`
    // separately preserves traversal/read/parse gaps from the startup or
    // watcher index report.
    let graph_snapshot: GraphFactsSnapshot =
        GraphFactProducer::default().produce(graph, index_complete);
    let test_snapshot: TestProximitySnapshot =
        TestProximityFactProducer::default().produce(graph, index_complete);
    let dead_snapshot: DeadSymbolFactsSnapshot = DeadSymbolFactProducer::default().produce(
        graph,
        &DeadSymbolExclusionInputs::default(),
        index_complete,
    );

    let mut failures = Vec::new();
    let complexity_ok = match refresh_complexity(
        workspace_root,
        changed,
        full_refresh,
        stores,
        complexity_cache,
    ) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "Complexity refresh failed; retaining the prior generation");
            failures.push(format!("complexity: {error:#}"));
            false
        }
    };

    let store_guard = stores
        .lock()
        .map_err(|_| anyhow::anyhow!("health fact store lock was poisoned"))?;

    let graph_ok = if let Err(error) =
        store_guard
            .graph
            .publish(repository_id, None, refreshed_at, &graph_snapshot)
    {
        tracing::warn!(%error, "Graph-fact publication failed; retaining the prior generation");
        failures.push(format!("graph: {error:#}"));
        false
    } else {
        true
    };
    let test_ok = if let Err(error) =
        store_guard
            .test_proximity
            .publish(repository_id, None, refreshed_at, &test_snapshot)
    {
        tracing::warn!(
            %error,
            "Test-proximity publication failed; retaining the prior generation"
        );
        failures.push(format!("test-proximity: {error:#}"));
        false
    } else {
        true
    };
    let dead_ok = if let Err(error) =
        store_guard
            .dead_symbols
            .publish(repository_id, None, refreshed_at, &dead_snapshot)
    {
        tracing::warn!(
            %error,
            "Dead-symbol publication failed; retaining the prior generation"
        );
        failures.push(format!("dead-symbol: {error:#}"));
        false
    } else {
        true
    };

    drop(store_guard);
    let store_guard = stores
        .lock()
        .map_err(|_| anyhow::anyhow!("health fact store lock was poisoned"))?;
    let mut published = HealthFactsRuntime::load_published(&store_guard, repository_id)?;
    if !graph_ok {
        published.graph = None;
    }
    if !complexity_ok {
        published.complexity = None;
    }
    if !test_ok {
        published.test_proximity = None;
    }
    if !dead_ok {
        published.dead_symbols = None;
    }
    Ok((published, failures))
}

/// Recomputes complexity for changed files only, once a generation exists.
///
/// This is the one family with true single-file incrementality.
/// `write_file_facts` replaces exactly one file's rows inside the published
/// generation, so steady-state cost tracks the number of files saved rather
/// than the size of the corpus.
///
/// `compute_file_complexity_facts` takes source text rather than a parsed
/// tree, so there is no AST from the indexer's own re-parse that could be
/// reused through this API; reading the changed file back is the minimal
/// available work and is bounded by the change, not the corpus.
fn refresh_complexity(
    workspace_root: &Path,
    changed: &BTreeSet<String>,
    full_refresh: bool,
    stores: &Arc<StdMutex<HealthFactStores>>,
    complexity_cache: &Arc<StdMutex<BTreeMap<String, FileComplexityFacts>>>,
) -> Result<()> {
    // Finish every fallible source read and computation before touching the
    // database. A transient failure therefore cannot partially mutate the
    // active durable generation.
    let mut next = complexity_cache
        .lock()
        .map_err(|_| anyhow::anyhow!("complexity cache lock was poisoned"))?
        .clone();
    if full_refresh {
        next.clear();
    }

    for path in changed {
        match lattice_core::security::workspace::read_source(workspace_root, Path::new(path)) {
            Ok(source) => {
                let facts = compute_file_complexity_facts(path, &source);
                next.insert(path.clone(), facts);
            }
            Err(error) if !full_refresh && error.kind() == std::io::ErrorKind::NotFound => {
                // A watcher-confirmed deletion must lose its facts rather than
                // keep stale ones. Other read failures retry without mutation.
                tracing::debug!(
                    path = path.as_str(),
                    %error,
                    "Complexity facts dropped for an unreadable file"
                );
                next.remove(path);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read complexity source `{path}` within the workspace")
                });
            }
        }
    }

    let stores = stores
        .lock()
        .map_err(|_| anyhow::anyhow!("health fact store lock was poisoned"))?;
    let store = &stores.complexity;
    let generation = store
        .begin_generation(HEALTH_CONFIG_VERSION)
        .context("failed to open a replacement complexity generation")?;
    let publish = (|| -> Result<()> {
        for facts in next.values() {
            store.write_file_facts(generation, facts).with_context(|| {
                format!("failed to write complexity facts for `{}`", facts.file)
            })?;
        }
        store
            .publish_generation(generation)
            .context("failed to publish the replacement complexity generation")?;
        if let Err(error) = store.prune_generations(2) {
            tracing::warn!(%error, "Failed to prune old complexity generations");
        }
        Ok(())
    })();
    if let Err(error) = publish {
        if let Err(cleanup) = store.discard_unpublished_generation(generation) {
            return Err(error).context(format!(
                "also failed to discard incomplete complexity generation {generation}: {cleanup}"
            ));
        }
        return Err(error);
    }

    *complexity_cache
        .lock()
        .map_err(|_| anyhow::anyhow!("complexity cache lock was poisoned"))? = next;
    Ok(())
}

fn unix_timestamp() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    i64::try_from(seconds).context("Unix timestamp exceeds SQLite integer range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_core::symbols::{Language, SymbolId, SymbolKind};

    #[derive(Default)]
    struct RecordingPublisher {
        publications: StdMutex<Vec<(String, usize)>>,
    }

    struct FailOncePublisher {
        fail_next: AtomicBool,
        publications: StdMutex<usize>,
    }

    impl HealthFactsPublisher for FailOncePublisher {
        fn publish(
            &self,
            _repository_id: &str,
            _facts: PublishedHealthFacts,
        ) -> Result<(), String> {
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err("injected handoff failure".to_string());
            }
            *self.publications.lock().expect("publication recorder lock") += 1;
            Ok(())
        }
    }

    impl HealthFactsPublisher for RecordingPublisher {
        fn publish(&self, repository_id: &str, facts: PublishedHealthFacts) -> Result<(), String> {
            let complexity_files = facts
                .complexity
                .as_ref()
                .map(|facts| facts.len())
                .unwrap_or(0);
            self.publications
                .lock()
                .expect("publication recorder lock")
                .push((repository_id.to_string(), complexity_files));
            Ok(())
        }
    }

    fn unique_test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lattice-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn graph_with(paths: &[&str]) -> Arc<CodeGraph> {
        let mut graph = CodeGraph::new();
        for path in paths {
            let symbol = SymbolId {
                file: (*path).to_string(),
                name: format!("{path}::run"),
                byte_offset: 0,
            };
            graph.add_node(
                symbol.clone(),
                SymbolKind::Function,
                symbol.name.clone(),
                format!("fn {}()", symbol.name),
                "",
                symbol.file.clone(),
                1,
                5,
                true,
                Language::Rust,
            );
        }
        Arc::new(graph)
    }

    fn engine_for(graph: Arc<CodeGraph>) -> Arc<Mutex<QueryEngine>> {
        Arc::new(Mutex::new(QueryEngine::new_shared(graph, None)))
    }

    /// A burst of saves must collapse into one pass while every touched path
    /// survives the collapse.
    ///
    /// This is the coalescing contract the whole-graph families depend on: the
    /// expensive pass is bounded by the settling window, not by how many files
    /// were saved, and no file is lost from the incremental complexity set.
    #[test]
    fn queued_changes_accumulate_and_collapse_to_one_pass() {
        let handle = HealthFactsRefreshHandle::for_test();

        handle.request(["src/a.rs".to_string()]);
        handle.request(["src/b.rs".to_string(), "src/c.rs".to_string()]);
        handle.request(["src/a.rs".to_string()]);

        assert_eq!(
            handle.pending_for_test(),
            vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/c.rs".to_string()
            ],
            "changed paths must accumulate and deduplicate across a burst"
        );
        assert_eq!(
            handle.latest_sequence_for_test(),
            3,
            "the worker observes only the latest sequence, so a burst is one pass"
        );
    }

    /// Startup must seed every indexed source even when the watcher observed
    /// no edit and the warm manifest reported an empty changed set.
    #[tokio::test]
    async fn full_request_publishes_complexity_without_a_watcher_edit() {
        let root = unique_test_root("health-runtime-startup-full");
        std::fs::create_dir_all(root.join("src")).expect("create source directory");
        std::fs::write(root.join("src/a.rs"), "fn a(flag: bool) { if flag { } }\n")
            .expect("write fixture");
        std::fs::write(root.join("src/b.rs"), "fn b() {}\n").expect("write second fixture");
        let publisher = Arc::new(RecordingPublisher::default());
        let engine = engine_for(graph_with(&["src/a.rs", "src/b.rs"]));
        let (handle, runtime) = HealthFactsRuntime::open_with_publisher(
            root.clone(),
            &root.join("graph.db"),
            "startup-full".to_string(),
            Arc::clone(&engine),
            IndexWorkCoordinator::new(1),
            Arc::new(IndexHealth::default()),
            None,
            publisher.clone(),
        )
        .expect("open runtime");

        handle.request_full();
        runtime.refresh(1).await.expect("run startup refresh");

        assert_eq!(
            publisher.publications.lock().expect("recorder").as_slice(),
            &[("startup-full".to_string(), 2)],
            "the graph-derived full file set must seed complexity"
        );

        engine
            .lock()
            .await
            .update_graph_arc(graph_with(&["src/a.rs"]));
        handle.request_full();
        runtime.refresh(2).await.expect("refresh warm graph");
        assert_eq!(
            publisher.publications.lock().expect("recorder")[1].1,
            1,
            "a warm full refresh must drop complexity rows absent from the new graph"
        );
        handle.request_full();
        runtime.refresh(3).await.expect("publish third generation");
        let persisted = HealthComplexityFactsStore::open(&complexity_store_path(
            &root.join("graph.db"),
            "startup-full",
        ))
        .expect("reopen complexity store");
        assert!(persisted
            .generation_status(1)
            .expect("old generation status")
            .is_none());
        assert!(persisted
            .generation_status(2)
            .expect("previous generation status")
            .is_some());
        assert!(persisted
            .generation_status(3)
            .expect("active generation status")
            .is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_full_publication_requeues_the_complete_startup_intent() {
        let root = unique_test_root("health-runtime-retry-full");
        std::fs::write(root.join("a.rs"), "fn a() {}\n").expect("write fixture");
        let publisher = Arc::new(FailOncePublisher {
            fail_next: AtomicBool::new(true),
            publications: StdMutex::new(0),
        });
        let (handle, runtime) = HealthFactsRuntime::open_with_publisher(
            root.clone(),
            &root.join("graph.db"),
            "retry-full".to_string(),
            engine_for(graph_with(&["a.rs"])),
            IndexWorkCoordinator::new(1),
            Arc::new(IndexHealth::default()),
            None,
            publisher.clone(),
        )
        .expect("open runtime");

        handle.request_full();
        assert!(runtime.refresh(1).await.is_err());
        assert!(
            runtime.full_refresh.load(Ordering::Acquire),
            "a transient handoff failure must retain the full-refresh intent"
        );

        runtime.refresh(2).await.expect("retry full publication");
        assert_eq!(*publisher.publications.lock().expect("recorder"), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn multi_repository_graph_is_localized_and_excludes_other_roots() {
        let graph = graph_with(&["alpha/src/a.rs", "beta/src/b.rs"]);
        let localized = repository_graph(Arc::clone(&graph), Some("alpha"));
        let files = localized
            .all_nodes()
            .into_iter()
            .map(|node| node.file.as_str())
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["src/a.rs"]);
    }

    #[test]
    fn multi_repository_complexity_generations_have_distinct_active_pointers() {
        let graph_path = Path::new("/cache/graph.db");
        assert_ne!(
            complexity_store_path(graph_path, "repo_alpha"),
            complexity_store_path(graph_path, "repo_beta")
        );
    }

    /// Publishing must survive a restart: a second runtime opened over the same
    /// database hydrates the handoff from the active generations rather than
    /// starting cold.
    #[tokio::test]
    async fn published_generations_are_reloaded_on_open() {
        let root = unique_test_root("health-runtime-reload");
        std::fs::write(root.join("a.rs"), "fn a(flag: bool) { if flag { } }\n")
            .expect("write fixture");
        let graph_path = root.join("graph.db");
        let repository_id = "health-fixture".to_string();
        let coordinator = IndexWorkCoordinator::new(1);
        let publisher = Arc::new(RecordingPublisher::default());

        let (handle, runtime) = HealthFactsRuntime::open_with_publisher(
            root.clone(),
            &graph_path,
            repository_id.clone(),
            engine_for(graph_with(&["a.rs"])),
            Arc::clone(&coordinator),
            Arc::new(IndexHealth::default()),
            None,
            publisher.clone(),
        )
        .expect("open runtime");

        // Nothing is published until the watcher reports a change.
        assert!(publisher.publications.lock().expect("recorder").is_empty());

        handle.request(["a.rs".to_string()]);
        runtime.refresh(1).await.expect("publish first generation");

        let published = publisher.publications.lock().expect("recorder").clone();
        assert_eq!(
            published,
            vec![(repository_id.clone(), 1)],
            "one pass must publish one generation covering the changed file"
        );

        // A refresh with nothing pending must not republish.
        runtime.refresh(2).await.expect("no-op refresh");
        assert_eq!(
            publisher.publications.lock().expect("recorder").len(),
            1,
            "a refresh with no pending change must do no work"
        );

        // Reopening must hydrate from the stores rather than start cold.
        let reopened = Arc::new(RecordingPublisher::default());
        let (_handle, _runtime) = HealthFactsRuntime::open_with_publisher(
            root.clone(),
            &graph_path,
            repository_id.clone(),
            engine_for(graph_with(&["a.rs"])),
            Arc::clone(&coordinator),
            Arc::new(IndexHealth::default()),
            None,
            reopened.clone(),
        )
        .expect("reopen runtime");
        let hydrated = reopened.publications.lock().expect("recorder").clone();
        assert_eq!(
            hydrated,
            vec![(repository_id, 1)],
            "open must republish the active generations it found"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// A deleted file must lose its complexity facts rather than keep stale
    /// ones, and the pass must still publish the surviving families.
    #[tokio::test]
    async fn unreadable_files_drop_their_complexity_facts() {
        let root = unique_test_root("health-runtime-delete");
        std::fs::write(root.join("a.rs"), "fn a(flag: bool) { if flag { } }\n")
            .expect("write fixture");
        let graph_path = root.join("graph.db");
        let coordinator = IndexWorkCoordinator::new(1);
        let publisher = Arc::new(RecordingPublisher::default());

        let (handle, runtime) = HealthFactsRuntime::open_with_publisher(
            root.clone(),
            &graph_path,
            "delete-fixture".to_string(),
            engine_for(graph_with(&["a.rs"])),
            Arc::clone(&coordinator),
            Arc::new(IndexHealth::default()),
            None,
            publisher.clone(),
        )
        .expect("open runtime");

        handle.request(["a.rs".to_string()]);
        runtime.refresh(1).await.expect("first pass");
        assert_eq!(
            publisher.publications.lock().expect("recorder")[0].1,
            1,
            "the changed file must gain complexity facts"
        );

        std::fs::remove_file(root.join("a.rs")).expect("remove fixture");
        handle.request(["a.rs".to_string()]);
        runtime.refresh(2).await.expect("second pass");
        assert_eq!(
            publisher.publications.lock().expect("recorder")[1].1,
            0,
            "a removed file must lose its facts rather than retain stale ones"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn transient_source_read_failure_requeues_without_dropping_complexity() {
        let root = unique_test_root("health-runtime-transient-read");
        std::fs::write(root.join("a.rs"), "fn a() {}\n").expect("write fixture");
        let publisher = Arc::new(RecordingPublisher::default());
        let (handle, runtime) = HealthFactsRuntime::open_with_publisher(
            root.clone(),
            &root.join("graph.db"),
            "transient-read".to_string(),
            engine_for(graph_with(&["a.rs"])),
            IndexWorkCoordinator::new(1),
            Arc::new(IndexHealth::default()),
            None,
            publisher,
        )
        .expect("open runtime");
        handle.request(["a.rs"]);
        runtime.refresh(1).await.expect("seed complexity");

        std::fs::remove_file(root.join("a.rs")).expect("remove fixture");
        std::fs::create_dir(root.join("a.rs")).expect("replace source with unreadable directory");
        handle.request(["a.rs"]);
        assert!(runtime.refresh(2).await.is_err());
        assert_eq!(
            runtime.complexity.lock().expect("complexity cache").len(),
            1
        );
        assert_eq!(
            runtime.drain_pending().expect("pending retry"),
            BTreeSet::from(["a.rs".to_string()])
        );
        let persisted = HealthComplexityFactsStore::open(&complexity_store_path(
            &root.join("graph.db"),
            "transient-read",
        ))
        .expect("reopen persisted complexity store")
        .file_facts("a.rs")
        .expect("read persisted complexity")
        .expect("prior active facts remain");
        assert_eq!(persisted.file, "a.rs");

        let _ = std::fs::remove_dir_all(root);
    }
}
