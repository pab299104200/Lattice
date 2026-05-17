//! Daemon-managed event-log compaction and snapshot bootstrap.
//!
//! Schedule, retention, bootstrap, and observability semantics are documented in
//! `docs/architecture/2026-05-16-event-log-compaction.md`
//! `## Compaction schedule`, `## Bootstrap procedure`, and `## Observability`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{error, info, info_span, warn};

use crate::events::{
    Actor, BranchRef, CompactSummary, EventPayload, EventStore, EventStoreError, EventWriter,
    MemoryConsolidatedPayload, PartialEnvelope, SessionId, Snapshot, SnapshotError,
};
use crate::graph::CodeGraph;
use crate::identity::{EventId, MemoryId};
use crate::memory::MemoryStore;

pub type GraphHandle = Mutex<CodeGraph>;
pub type MemoryHandle = Mutex<MemoryStore>;

#[derive(Clone, Debug)]
pub struct CompactionConfig {
    pub interval: Duration,
    pub min_events_since_last: u64,
    pub snapshot_dir: PathBuf,
    pub retain_snapshots: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionReport {
    pub snapshot_path: Option<PathBuf>,
    pub bytes_written: u64,
    pub events_truncated: u64,
    pub duration: Duration,
    pub skipped: bool,
}

pub struct Compactor {
    pub store: Arc<EventStore>,
    pub writer: Arc<EventWriter>,
    pub graph: Arc<GraphHandle>,
    pub memory: Arc<MemoryHandle>,
    pub config: CompactionConfig,
    lock: Arc<AtomicBool>,
}

pub struct SchedulerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BootstrappedState {
    pub snapshot: Snapshot,
    pub replayed_event_rows: usize,
}

pub struct Bootstrap;

#[derive(Debug, Error)]
pub enum CompactionError {
    #[error("event compaction is already running")]
    LockHeld,
    #[error("snapshot failed: {0}")]
    SnapshotFailed(#[from] SnapshotError),
    #[error("event truncation failed: {0}")]
    TruncateFailed(#[from] EventStoreError),
    #[error("snapshot verification failed: {0}")]
    VerifyFailed(String),
    #[error("event write failed: {0}")]
    EventWriteFailed(String),
}

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("snapshot is missing at {path}")]
    SnapshotMissing { path: PathBuf },
    #[error("snapshot format version {found} is unsupported; max supported is {max}")]
    FormatVersionUnsupported { found: u16, max: u16 },
    #[error("snapshot content hash mismatch at {path}")]
    ContentHashMismatch { path: PathBuf },
    #[error("post-snapshot event {event_id} references missing state `{missing_ref}`")]
    ReferenceUnresolved { event_id: i64, missing_ref: String },
    #[error("snapshot load failed: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("event replay failed: {0}")]
    EventStore(#[from] EventStoreError),
}

impl CompactionConfig {
    pub fn new(snapshot_dir: PathBuf) -> Self {
        Self {
            interval: Duration::from_secs(900),
            min_events_since_last: 1_000,
            snapshot_dir,
            retain_snapshots: 5,
        }
    }
}

impl Compactor {
    pub fn new(
        store: Arc<EventStore>,
        writer: Arc<EventWriter>,
        graph: Arc<GraphHandle>,
        memory: Arc<MemoryHandle>,
        config: CompactionConfig,
    ) -> Self {
        Self {
            store,
            writer,
            graph,
            memory,
            config,
            lock: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn run_once(&self) -> Result<CompactionReport, CompactionError> {
        let guard = self.acquire_lock()?;
        let _guard = guard;
        let started = Instant::now();
        let _span = info_span!("compaction", interval = ?self.config.interval).entered();
        let latest = match self.store.latest_cursor()? {
            Some(cursor) => cursor,
            None => return Ok(skipped_report(started)),
        };
        let last_snapshot = latest_snapshot(&self.config.snapshot_dir)?;
        let baseline = last_snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.up_to_event_id);
        let events_since_last = self.store.compactable_event_count_after(baseline)?;
        if events_since_last < self.config.min_events_since_last {
            return Ok(skipped_report(started));
        }

        let snapshot_path = snapshot_path(&self.config.snapshot_dir, latest.row_id);
        let handle = {
            let graph = self.graph.lock().map_err(lock_error)?;
            let memory = self.memory.lock().map_err(lock_error)?;
            Snapshot::write(&snapshot_path, &graph, &memory, latest.row_id)?
        };
        let verified = Snapshot::read(&handle.path)?;
        if verified.up_to_event_id != latest.row_id {
            return Err(CompactionError::VerifyFailed(
                "snapshot cursor changed during verification".to_string(),
            ));
        }

        let events_truncated = self.store.truncate_through(latest.row_id)?;
        self.emit_snapshot_event(&verified, &latest)?;
        self.rotate_snapshots()?;
        info!(
            snapshot_path = %handle.path.display(),
            bytes_written = handle.bytes_written,
            events_truncated,
            "event log compaction completed"
        );
        Ok(CompactionReport {
            snapshot_path: Some(handle.path),
            bytes_written: handle.bytes_written,
            events_truncated,
            duration: started.elapsed(),
            skipped: false,
        })
    }

    pub fn spawn_scheduler(self) -> SchedulerHandle {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.config.interval);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(err) = self.run_once() {
                            error!(error = %err, "event log compaction failed");
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("event log compaction scheduler shutting down");
                        break;
                    }
                }
            }
        });
        SchedulerHandle {
            shutdown: Some(shutdown_tx),
            task,
        }
    }

    fn acquire_lock(&self) -> Result<CompactionLock, CompactionError> {
        if self.lock.swap(true, Ordering::Acquire) {
            return Err(CompactionError::LockHeld);
        }
        Ok(CompactionLock {
            lock: Arc::clone(&self.lock),
        })
    }

    fn emit_snapshot_event(
        &self,
        snapshot: &Snapshot,
        latest: &crate::events::EventCursor,
    ) -> Result<(), CompactionError> {
        let summary = CompactSummary::new(format!(
            "event log snapshot: up_to_event_row={} graph_nodes={} memories={}",
            snapshot.up_to_event_id,
            snapshot.graph_state.nodes.len(),
            snapshot.memory_state.memories.len()
        ))
        .map_err(|err| CompactionError::EventWriteFailed(err.to_string()))?;
        let payload = EventPayload::MemoryConsolidated(MemoryConsolidatedPayload {
            source_memory_ids: Vec::new(),
            consolidated_memory_id: MemoryId {
                workspace_id: latest.workspace_id.clone(),
                ulid: format!("snapshot-{}", snapshot.up_to_event_id),
            },
            source_event_ids: vec![EventId {
                workspace_id: latest.workspace_id.clone(),
                ulid: latest.event_uuid.clone(),
            }],
            consolidation_summary: "event log snapshot compaction marker".to_string(),
            proposal_id: None,
            prior_state_json: None,
            proposed_state_json: None,
            post_apply_state_hash: [0; 32],
            decided_by: None,
            decision_reason: None,
        });
        self.writer
            .append(PartialEnvelope {
                workspace_id: Some(latest.workspace_id.clone()),
                branch: BranchRef {
                    name: latest.branch.clone(),
                },
                session_id: SessionId {
                    value: "daemon-compaction".to_string(),
                },
                task_id: None,
                actor: Actor::Daemon,
                kind: payload.kind(),
                references: Vec::new(),
                summary,
                payload,
            })
            .map_err(|err| CompactionError::EventWriteFailed(err.to_string()))?;
        Ok(())
    }

    fn rotate_snapshots(&self) -> Result<(), CompactionError> {
        let retain = self.config.retain_snapshots.max(1);
        let mut snapshots = snapshot_files(&self.config.snapshot_dir)?;
        snapshots.sort_by(|left, right| right.cmp(left));
        for stale in snapshots.into_iter().skip(retain) {
            match std::fs::remove_file(&stale) {
                Ok(()) => {
                    warn!(snapshot_path = %stale.display(), "removed old compaction snapshot")
                }
                Err(err) => warn!(
                    snapshot_path = %stale.display(),
                    error = %err,
                    "failed to remove old compaction snapshot"
                ),
            }
        }
        Ok(())
    }
}

impl SchedulerHandle {
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Err(err) = self.task.await {
            warn!(error = %err, "event log compaction scheduler join failed");
        }
    }
}

impl Bootstrap {
    pub fn load(
        snapshot_path: &Path,
        event_store: &EventStore,
    ) -> Result<BootstrappedState, BootstrapError> {
        if !snapshot_path.exists() {
            return Err(BootstrapError::SnapshotMissing {
                path: snapshot_path.to_path_buf(),
            });
        }
        let Some(snapshot) = newest_valid_snapshot(snapshot_path)? else {
            return Err(BootstrapError::ContentHashMismatch {
                path: snapshot_path.to_path_buf(),
            });
        };
        let rows = event_store.query_events_after_row_id(snapshot.up_to_event_id, 10_000)?;
        validate_tail_references(&snapshot, &rows)?;
        info!(
            event_type = "recovery",
            snapshot_up_to_event_id = snapshot.up_to_event_id,
            replayed_event_rows = rows.len(),
            "event log recovery completed from snapshot plus tail"
        );
        Ok(BootstrappedState {
            snapshot,
            replayed_event_rows: rows.len(),
        })
    }
}

struct CompactionLock {
    lock: Arc<AtomicBool>,
}

impl Drop for CompactionLock {
    fn drop(&mut self) {
        self.lock.store(false, Ordering::Release);
    }
}

fn skipped_report(started: Instant) -> CompactionReport {
    CompactionReport {
        snapshot_path: None,
        bytes_written: 0,
        events_truncated: 0,
        duration: started.elapsed(),
        skipped: true,
    }
}

fn latest_snapshot(snapshot_dir: &Path) -> Result<Option<Snapshot>, SnapshotError> {
    let Some(path) = snapshot_files(snapshot_dir)?.into_iter().max() else {
        return Ok(None);
    };
    Snapshot::read(&path).map(Some)
}

fn newest_valid_snapshot(requested_path: &Path) -> Result<Option<Snapshot>, BootstrapError> {
    let requested = requested_path.to_path_buf();
    let mut candidates = vec![requested.clone()];
    if let Some(parent) = requested.parent() {
        let mut siblings = snapshot_files(parent).map_err(BootstrapError::Snapshot)?;
        siblings.sort_by(|left, right| right.cmp(left));
        candidates.extend(siblings.into_iter().filter(|path| path != &requested));
    }

    let mut saw_corruption = false;
    for candidate in candidates {
        match load_snapshot_candidate(&candidate) {
            Ok(snapshot) => return Ok(Some(snapshot)),
            Err(BootstrapError::ContentHashMismatch { .. }) => {
                warn!(
                    event_type = "recovery",
                    snapshot_path = %candidate.display(),
                    "event log recovery: ignoring corrupt snapshot candidate"
                );
                saw_corruption = true;
            }
            Err(error) => return Err(error),
        }
    }
    if saw_corruption {
        return Ok(None);
    }
    Err(BootstrapError::SnapshotMissing {
        path: requested_path.to_path_buf(),
    })
}

fn load_snapshot_candidate(path: &Path) -> Result<Snapshot, BootstrapError> {
    let header = Snapshot::read_header(path).map_err(|error| map_snapshot_error(path, error))?;
    if header.format_version > crate::events::snapshot::SNAPSHOT_FORMAT_VERSION {
        warn!(
            event_type = "recovery",
            snapshot_path = %path.display(),
            found = header.format_version,
            max = crate::events::snapshot::SNAPSHOT_FORMAT_VERSION,
            "event log recovery: snapshot format too new"
        );
        return Err(BootstrapError::FormatVersionUnsupported {
            found: header.format_version,
            max: crate::events::snapshot::SNAPSHOT_FORMAT_VERSION,
        });
    }
    Snapshot::read(path).map_err(|error| map_snapshot_error(path, error))
}

fn map_snapshot_error(path: &Path, error: SnapshotError) -> BootstrapError {
    match error {
        SnapshotError::ContentHashMismatch => BootstrapError::ContentHashMismatch {
            path: path.to_path_buf(),
        },
        SnapshotError::FormatVersionUnsupported { found, max } => {
            warn!(
                event_type = "recovery",
                snapshot_path = %path.display(),
                found,
                max,
                "event log recovery: snapshot format too new"
            );
            BootstrapError::FormatVersionUnsupported { found, max }
        }
        other => BootstrapError::Snapshot(other),
    }
}

fn snapshot_files(snapshot_dir: &Path) -> Result<Vec<PathBuf>, SnapshotError> {
    if !snapshot_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(snapshot_dir).map_err(|source| SnapshotError::IoFailed {
        path: snapshot_dir.to_path_buf(),
        source,
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| SnapshotError::IoFailed {
                path: snapshot_dir.to_path_buf(),
                source,
            })?
            .path();
        if is_snapshot_file(&path) {
            files.push(path);
        }
    }
    Ok(files)
}

fn is_snapshot_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("snapshot-") && name.ends_with(".bin"))
}

fn snapshot_path(snapshot_dir: &Path, event_row_id: i64) -> PathBuf {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    snapshot_dir.join(format!("snapshot-{event_row_id}-{micros}.bin"))
}

fn validate_tail_references(
    snapshot: &Snapshot,
    rows: &[crate::events::EventEnvelopeRow],
) -> Result<(), BootstrapError> {
    let mut memory_ids: HashSet<String> = snapshot
        .memory_state
        .memories
        .iter()
        .map(|entry| entry.memory.id.clone())
        .collect();
    let symbol_ids: HashSet<String> = snapshot
        .graph_state
        .nodes
        .iter()
        .map(|node| format!("{}:{}", node.id.file, node.id.name))
        .collect();
    for row in rows {
        validate_row_references(row, &memory_ids, &symbol_ids)?;
        collect_created_memory_ids(row, &mut memory_ids);
    }
    Ok(())
}

fn validate_row_references(
    row: &crate::events::EventEnvelopeRow,
    memory_ids: &HashSet<String>,
    symbol_ids: &HashSet<String>,
) -> Result<(), BootstrapError> {
    let refs: Vec<crate::events::StableRef> =
        serde_json::from_str(&row.references_json).unwrap_or_default();
    for reference in refs {
        match reference {
            crate::events::StableRef::MemoryRef(memory_id) => {
                let id = memory_id.ulid;
                if !memory_ids.contains(&id) {
                    return unresolved(row.event_id, id);
                }
            }
            crate::events::StableRef::SymbolRef(symbol_id) => {
                let id = format!(
                    "{}:{}",
                    symbol_id.file.repo_relative_path, symbol_id.qualified_name
                );
                if !symbol_ids.contains(&id) {
                    return unresolved(row.event_id, id);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_created_memory_ids(
    row: &crate::events::EventEnvelopeRow,
    memory_ids: &mut HashSet<String>,
) {
    if row.kind != "memory_created" {
        return;
    }
    let Some(bytes) = &row.payload_inline else {
        return;
    };
    if let Ok(EventPayload::MemoryCreated(payload)) = serde_json::from_slice::<EventPayload>(bytes)
    {
        memory_ids.insert(payload.memory_id.ulid);
    }
}

fn unresolved<T>(event_id: i64, missing_ref: String) -> Result<T, BootstrapError> {
    warn!(
        event_type = "dangling-reference",
        event_id,
        missing_ref = missing_ref.as_str(),
        "event log recovery: post-snapshot event references missing state"
    );
    Err(BootstrapError::ReferenceUnresolved {
        event_id,
        missing_ref,
    })
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> CompactionError {
    CompactionError::VerifyFailed("snapshot source lock was poisoned".to_string())
}
