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

use crate::events::snapshot::rewrite_without_memory_streaming;
use crate::events::{
    Actor, BranchRef, CompactSummary, EventPayload, EventStore, EventStoreError, EventWriter,
    MemoryConsolidatedPayload, PartialEnvelope, SessionId, Snapshot, SnapshotError,
};
use crate::graph::CodeGraph;
use crate::identity::{EventId, MemoryId};
use crate::storage::managed_fs::ManagedDirFingerprint;
use crate::storage::{ManagedDirCursor, SecureDir};

const SNAPSHOT_SCAN_LIMIT: usize = 4_096;

pub type GraphHandle = Mutex<Arc<CodeGraph>>;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotExpiryReport {
    pub inspected: usize,
    pub deleted: usize,
    pub bytes_deleted: u64,
    pub unknown_files: usize,
    pub rewritten_without_memory: usize,
    pub budget_deferred: usize,
    /// Descriptor directory cookie for a bounded continuation. Callers that
    /// persist this value can make progress through directories over the scan
    /// limit. Partial pages rewrite legacy memory copies but never delete.
    pub next_cookie: Option<SnapshotExpiryCursor>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SnapshotExpiryCursor {
    pub directory: Option<ManagedDirCursor>,
    /// Mutation stamp captured when this inventory/sweep page was published.
    #[serde(default)]
    pub fingerprint: Option<ManagedDirFingerprint>,
    pub newest: Option<(i64, u128)>,
    pub sweeping: bool,
    #[serde(default)]
    pub changed_in_sweep: bool,
}

/// Delete only recognized managed snapshots, keeping the newest valid graph
/// checkpoint. Work is bounded by both a deletion count and a byte budget.
pub fn expire_managed_snapshots(
    dir: &Path,
    now: u64,
    horizon_secs: u64,
    max_bytes: u64,
    max_deletes: usize,
) -> Result<SnapshotExpiryReport, SnapshotError> {
    if horizon_secs == 0 || max_bytes == 0 || max_deletes == 0 {
        return Err(SnapshotError::Serde(
            "snapshot expiry budgets must be positive".into(),
        ));
    }
    if !dir.exists() {
        return Ok(SnapshotExpiryReport::default());
    }
    let directory = SecureDir::open(dir).map_err(|source| SnapshotError::IoFailed {
        path: dir.into(),
        source,
    })?;
    expire_managed_snapshots_dir(&directory, now, horizon_secs, max_bytes, max_deletes)
}

pub fn expire_managed_snapshots_dir(
    directory: &SecureDir,
    now: u64,
    horizon_secs: u64,
    max_bytes: u64,
    max_deletes: usize,
) -> Result<SnapshotExpiryReport, SnapshotError> {
    expire_managed_snapshots_dir_page(directory, now, horizon_secs, max_bytes, max_deletes, None)
}

pub fn expire_managed_snapshots_dir_page(
    directory: &SecureDir,
    now: u64,
    horizon_secs: u64,
    max_bytes: u64,
    max_deletes: usize,
    start_cookie: Option<SnapshotExpiryCursor>,
) -> Result<SnapshotExpiryReport, SnapshotError> {
    if horizon_secs == 0 || max_bytes == 0 || max_deletes == 0 {
        return Err(SnapshotError::Serde(
            "snapshot expiry budgets must be positive".into(),
        ));
    }
    let dir = directory.path();
    let mut report = SnapshotExpiryReport::default();
    let mut managed = Vec::new();
    let was_continuation = start_cookie.is_some();
    let mut cursor = start_cookie.unwrap_or(SnapshotExpiryCursor {
        directory: None,
        fingerprint: None,
        newest: None,
        sweeping: false,
        changed_in_sweep: false,
    });
    let before = directory
        .directory_fingerprint()
        .map_err(|source| SnapshotError::IoFailed {
            path: dir.into(),
            source,
        })?;
    if cursor
        .fingerprint
        .is_some_and(|fingerprint| fingerprint != before)
    {
        // Directory cookies are meaningful only for the directory generation
        // that produced them. Restarting is bounded and keeps deletion fenced
        // until a complete, stable inventory has been observed.
        cursor.directory = None;
        cursor.fingerprint = Some(before);
        cursor.newest = None;
        cursor.sweeping = false;
        cursor.changed_in_sweep = false;
        report.budget_deferred += 1;
    }
    cursor.fingerprint = Some(before);
    while report.inspected + report.unknown_files < SNAPSHOT_SCAN_LIMIT {
        let remaining = SNAPSHOT_SCAN_LIMIT - report.inspected - report.unknown_files;
        let page_limit = if cursor.sweeping {
            remaining.min(256).min(max_deletes)
        } else {
            remaining.min(256)
        };
        let page = directory
            .read_dir_page(cursor.directory.clone(), page_limit)
            .map_err(|source| SnapshotError::IoFailed {
                path: dir.into(),
                source,
            })?;
        for entry in page.entries {
            let path = dir.join(&entry.name);
            if managed_snapshot_key(&path).is_some() && entry.is_file {
                report.inspected += 1;
                let key = managed_snapshot_key(&path).expect("managed snapshot key");
                if cursor.newest.is_none_or(|newest| key > newest) {
                    cursor.newest = Some(key);
                }
                managed.push((path, entry));
            } else {
                report.unknown_files += 1;
            }
        }
        cursor.directory = page.next_cookie;
        break;
    }
    let after = directory
        .directory_fingerprint()
        .map_err(|source| SnapshotError::IoFailed {
            path: dir.into(),
            source,
        })?;
    if after != before {
        cursor.directory = None;
        cursor.fingerprint = Some(after);
        cursor.newest = None;
        cursor.sweeping = false;
        cursor.changed_in_sweep = false;
        report.budget_deferred += 1;
        report.next_cookie = Some(cursor);
        return Ok(report);
    }
    if !cursor.sweeping {
        // Without a complete inventory we cannot prove which checkpoint is
        // newest. Rewrite old legacy copies in this page, but never delete a
        // recovery point based on a partial directory view.
        let mut processed = 0_u64;
        for (path, entry) in &managed {
            if report.rewritten_without_memory >= max_deletes {
                break;
            }
            if processed != 0 && processed.saturating_add(entry.len) > max_bytes {
                report.budget_deferred += 1;
                continue;
            }
            let (_, micros) = managed_snapshot_key(&path).expect("managed snapshot key");
            let created = u64::try_from(micros / 1_000_000).unwrap_or(u64::MAX);
            if now.saturating_sub(created) >= horizon_secs {
                let header = Snapshot::read_header(&path)?;
                if header.format_version < crate::events::snapshot::SNAPSHOT_FORMAT_VERSION {
                    rewrite_without_memory_streaming(directory, &entry.name)?;
                    report.rewritten_without_memory += 1;
                    processed = processed.saturating_add(entry.len);
                }
            }
        }
        if report.rewritten_without_memory > 0 {
            cursor.directory = None;
            cursor.newest = None;
            cursor.sweeping = false;
            cursor.changed_in_sweep = false;
            report.next_cookie = Some(cursor);
            return Ok(report);
        }
        if cursor.directory.is_some() {
            report.budget_deferred += 1;
            report.next_cookie = Some(cursor);
            return Ok(report);
        }
        cursor.sweeping = true;
        cursor.directory = None;
        if was_continuation {
            report.next_cookie = Some(cursor);
            return Ok(report);
        }
    }
    let newest = cursor.newest;
    let mut processed_bytes = 0u64;
    for (path, entry) in managed {
        if report.deleted + report.rewritten_without_memory >= max_deletes {
            break;
        }
        let bytes = entry.len;
        // Admit one oversized candidate per sweep. Otherwise a valid historical
        // snapshot larger than the normal byte budget can survive forever.
        if processed_bytes != 0 && processed_bytes.saturating_add(bytes) > max_bytes {
            report.budget_deferred += 1;
            continue;
        }
        processed_bytes += bytes;
        let name = entry.name.as_str();
        if bytes > 256 * 1024 * 1024 {
            let (_, micros) = managed_snapshot_key(&path).expect("managed snapshot key");
            let created = u64::try_from(micros / 1_000_000).unwrap_or(u64::MAX);
            if now.saturating_sub(created) < horizon_secs {
                continue;
            }
            rewrite_without_memory_streaming(&directory, name)?;
            report.rewritten_without_memory += 1;
            cursor.changed_in_sweep = true;
            continue;
        }
        let snapshot = Snapshot::read_from_dir(&directory, name)?;
        let age = now.saturating_sub(snapshot.taken_at.unix_seconds().max(0) as u64);
        if age < horizon_secs {
            continue;
        }
        if managed_snapshot_key(&path) == newest {
            // Preserve graph recovery while retiring the last historical memory copy.
            if snapshot.format_version < crate::events::snapshot::SNAPSHOT_FORMAT_VERSION
                || !snapshot.memory_state.memories.is_empty()
            {
                snapshot.without_memory()?.write_to_dir(&directory, name)?;
                report.rewritten_without_memory += 1;
                cursor.changed_in_sweep = true;
            }
            continue;
        }
        directory
            .remove_file(name, entry.identity)
            .map_err(|source| SnapshotError::IoFailed {
                path: path.clone(),
                source,
            })?;
        report.deleted += 1;
        cursor.changed_in_sweep = true;
        report.bytes_deleted += bytes;
    }
    if cursor.directory.is_some() {
        report.next_cookie = Some(cursor);
    } else if cursor.changed_in_sweep {
        cursor.changed_in_sweep = false;
        report.next_cookie = Some(cursor);
    }
    Ok(report)
}

pub struct Compactor {
    pub store: Arc<EventStore>,
    pub writer: Arc<EventWriter>,
    pub graph: Arc<GraphHandle>,
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
        config: CompactionConfig,
    ) -> Self {
        Self {
            store,
            writer,
            graph,
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
            Snapshot::write(&snapshot_path, graph.as_ref(), latest.row_id)?
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
            transition: None,
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
        snapshots.sort_by_key(|path| {
            std::cmp::Reverse(managed_snapshot_key(path).expect("filtered managed snapshot"))
        });
        for stale in snapshots.into_iter().skip(retain) {
            let result = stale
                .parent()
                .and_then(|parent| SecureDir::open(parent).ok())
                .and_then(|dir| {
                    let name = stale.file_name()?.to_str()?;
                    let identity = dir.metadata(name).ok()??.identity;
                    dir.remove_file(name, identity).ok()
                });
            match result {
                Some(()) => {
                    warn!(snapshot_path = %stale.display(), "removed old compaction snapshot")
                }
                None => warn!(
                    snapshot_path = %stale.display(),
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
    let Some(path) = snapshot_files(snapshot_dir)?
        .into_iter()
        .max_by_key(|path| managed_snapshot_key(path).expect("filtered managed snapshot"))
    else {
        return Ok(None);
    };
    Snapshot::read(&path).map(Some)
}

fn newest_valid_snapshot(requested_path: &Path) -> Result<Option<Snapshot>, BootstrapError> {
    let requested = requested_path.to_path_buf();
    let mut candidates = vec![requested.clone()];
    if let Some(parent) = requested.parent() {
        let mut siblings = snapshot_files(parent).map_err(BootstrapError::Snapshot)?;
        siblings.sort_by_key(|path| {
            std::cmp::Reverse(managed_snapshot_key(path).expect("filtered managed snapshot"))
        });
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
    let directory = SecureDir::open(snapshot_dir).map_err(|source| SnapshotError::IoFailed {
        path: snapshot_dir.into(),
        source,
    })?;
    let mut files = Vec::new();
    let mut cookie = None;
    let mut inspected = 0usize;
    while inspected < SNAPSHOT_SCAN_LIMIT {
        let page = directory
            .read_dir_page(cookie, (SNAPSHOT_SCAN_LIMIT - inspected).min(256))
            .map_err(|source| SnapshotError::IoFailed {
                path: snapshot_dir.into(),
                source,
            })?;
        inspected += page.entries.len();
        for entry in page.entries {
            let path = snapshot_dir.join(entry.name);
            if entry.is_file && is_snapshot_file(&path) {
                files.push(path);
            }
        }
        cookie = page.next_cookie;
        if cookie.is_none() {
            break;
        }
    }
    Ok(files)
}

fn is_snapshot_file(path: &Path) -> bool {
    managed_snapshot_key(path).is_some()
}

fn managed_snapshot_key(path: &Path) -> Option<(i64, u128)> {
    let name = path.file_name()?.to_str()?;
    let core = name.strip_prefix("snapshot-")?.strip_suffix(".bin")?;
    let (row, micros) = core.split_once('-')?;
    let row: i64 = row.parse().ok()?;
    if row < 0 {
        return None;
    }
    Some((row, micros.parse().ok()?))
}

fn snapshot_path(snapshot_dir: &Path, event_row_id: i64) -> PathBuf {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    snapshot_dir.join(format!("snapshot-{event_row_id}-{micros}.bin"))
}

#[cfg(test)]
mod ordering_tests {
    use super::managed_snapshot_key;
    use std::path::PathBuf;

    #[test]
    fn managed_snapshot_keys_order_numeric_rows() {
        let mut paths = [
            "snapshot-9-1.bin",
            "snapshot-100-1.bin",
            "snapshot-10-1.bin",
        ]
        .map(PathBuf::from);
        paths.sort_by_key(|path| managed_snapshot_key(path).unwrap());
        assert_eq!(
            paths.map(|path| path.file_name().unwrap().to_string_lossy().into_owned()),
            [
                "snapshot-9-1.bin",
                "snapshot-10-1.bin",
                "snapshot-100-1.bin"
            ]
        );
    }
}

fn validate_tail_references(
    snapshot: &Snapshot,
    rows: &[crate::events::EventEnvelopeRow],
) -> Result<(), BootstrapError> {
    let symbol_ids: HashSet<String> = snapshot
        .graph_state
        .nodes
        .iter()
        .map(|node| format!("{}:{}", node.id.file, node.id.name))
        .collect();
    for row in rows {
        validate_row_references(row, &symbol_ids)?;
    }
    Ok(())
}

fn validate_row_references(
    row: &crate::events::EventEnvelopeRow,
    symbol_ids: &HashSet<String>,
) -> Result<(), BootstrapError> {
    let refs: Vec<crate::events::StableRef> =
        serde_json::from_str(&row.references_json).unwrap_or_default();
    for reference in refs {
        match reference {
            crate::events::StableRef::MemoryRef(_) => {}
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
