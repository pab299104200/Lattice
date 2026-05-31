use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    Actor, AssistantTaskStartedPayload, Bootstrap, BranchRef, CompactSummary, CompactionConfig,
    Compactor, EventPayload, EventStore, EventWriter, FlushPolicy, PartialEnvelope, SessionId,
    Snapshot, SnapshotError,
};
use crate::graph::{CodeGraph, EdgeKind};
use crate::identity::{EventId, MemoryId};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use crate::symbols::{Language, SymbolId, SymbolKind};

#[test]
fn snapshot_write_and_read_preserves_graph_and_memory_state() {
    let dir = temp_dir("snapshot-round-trip");
    let path = dir.join("snapshot-1-1.bin");
    let graph = sample_graph();
    let memory = sample_memory_store();

    Snapshot::write(&path, &graph, &memory, 7).expect("snapshot writes");
    let read = Snapshot::read(&path).expect("snapshot reads");
    let expected = Snapshot::capture(&graph, &memory, 7).expect("snapshot captures");

    assert_eq!(read.up_to_event_id, 7);
    assert_eq!(read.graph_state, expected.graph_state);
    assert_eq!(read.memory_state, expected.memory_state);
}

#[test]
fn format_version_mismatch_returns_unsupported_without_panic() {
    let path = write_sample_snapshot("snapshot-version");
    overwrite_at(&path, 8, &2u16.to_le_bytes());

    let error = Snapshot::read(&path).expect_err("version mismatch rejects");

    assert!(matches!(
        error,
        SnapshotError::FormatVersionUnsupported { found: 2, max: 1 }
    ));
}

#[test]
fn content_hash_mismatch_returns_content_hash_mismatch() {
    let path = write_sample_snapshot("snapshot-hash");
    let len = fs::metadata(&path).expect("metadata").len();
    overwrite_at(&path, len - 1, b"x");

    let error = Snapshot::read(&path).expect_err("hash mismatch rejects");

    assert!(matches!(error, SnapshotError::ContentHashMismatch));
}

#[test]
fn compaction_is_idempotent_when_no_new_compactable_events_arrive() {
    let dir = temp_dir("compaction-idempotent");
    let store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let writer = Arc::new(writer(store.clone()));
    writer
        .append(base_event("session-a"))
        .expect("event appends");
    let compactor = sample_compactor(dir.join("snapshots"), store.clone(), writer);

    let first = compactor.run_once().expect("first compaction runs");
    let second = compactor.run_once().expect("second compaction skips");

    assert!(!first.skipped);
    assert!(second.skipped);
    assert_eq!(second.events_truncated, 0);
    assert_eq!(store.event_count_through(i64::MAX).unwrap(), 1);
}

#[test]
fn bootstrap_from_snapshot_and_tail_events_matches_full_replay_count() {
    let dir = temp_dir("bootstrap-tail");
    let path = dir.join("snapshot-1-1.bin");
    let store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let writer = writer(store.clone());
    writer.append(base_event("session-a")).expect("first event");
    Snapshot::write(&path, &sample_graph(), &sample_memory_store(), 1).expect("snapshot writes");
    writer.append(base_event("session-b")).expect("tail event");

    let bootstrapped = Bootstrap::load(&path, &store).expect("bootstrap loads");
    let full_rows = store.query_events_after_row_id(0, 100).expect("full rows");

    assert_eq!(bootstrapped.snapshot.up_to_event_id, 1);
    assert_eq!(bootstrapped.replayed_event_rows, full_rows.len() - 1);
}

#[tokio::test]
async fn scheduler_shutdown_stops_without_partial_truncation() {
    let dir = temp_dir("scheduler-shutdown");
    let store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let writer = Arc::new(writer(store.clone()));
    writer
        .append(base_event("session-a"))
        .expect("event appends");
    let mut config = CompactionConfig::new(dir.join("snapshots"));
    config.interval = Duration::from_secs(60);
    config.min_events_since_last = 1;
    let compactor = Compactor::new(
        store.clone(),
        writer,
        Arc::new(Mutex::new(Arc::new(sample_graph()))),
        Arc::new(Mutex::new(sample_memory_store())),
        config,
    );

    let scheduler = compactor.spawn_scheduler();
    scheduler.shutdown().await;

    assert_eq!(store.event_count_through(i64::MAX).unwrap(), 1);
}

fn sample_compactor(
    snapshot_dir: PathBuf,
    store: Arc<EventStore>,
    writer: Arc<EventWriter>,
) -> Compactor {
    let mut config = CompactionConfig::new(snapshot_dir);
    config.min_events_since_last = 1;
    config.retain_snapshots = 2;
    Compactor::new(
        store,
        writer,
        Arc::new(Mutex::new(Arc::new(sample_graph()))),
        Arc::new(Mutex::new(sample_memory_store())),
        config,
    )
}

fn sample_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    let left = symbol_id("src/main.rs", "main", 1);
    let right = symbol_id("src/lib.rs", "helper", 2);
    graph.add_node(
        left.clone(),
        SymbolKind::Function,
        "main".to_string(),
        "fn main()".to_string(),
        "fn main() { helper(); }".to_string(),
        "src/main.rs".to_string(),
        1,
        3,
        true,
        Language::Rust,
    );
    graph.add_node(
        right.clone(),
        SymbolKind::Function,
        "helper".to_string(),
        "fn helper()".to_string(),
        "fn helper() {}".to_string(),
        "src/lib.rs".to_string(),
        1,
        1,
        false,
        Language::Rust,
    );
    graph.add_edge(&left, &right, EdgeKind::Calls);
    graph
}

fn sample_memory_store() -> MemoryStore {
    let store = MemoryStore::open_in_memory().expect("memory store");
    store.store(sample_memory()).expect("memory stores");
    store
}

fn sample_memory() -> Memory {
    Memory {
        id: "memory-a".to_string(),
        session_id: "session-a".to_string(),
        content: "Graph snapshots preserve memory.".to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 0.9,
        linked_symbols: vec!["helper".to_string()],
        linked_files: vec!["src/lib.rs".to_string()],
        workspace_id: Some("workspace-main".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: Some("snapshot-test".to_string()),
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

fn writer(store: Arc<EventStore>) -> EventWriter {
    EventWriter::new(store, "workspace-main".to_string(), 4096).with_flush_policy(FlushPolicy::Sync)
}

fn base_event(session: &str) -> PartialEnvelope {
    let payload = EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
        context_handle_id: None,
        seed_event_ids: Vec::<EventId>::new(),
        initial_memory_ids: Vec::<MemoryId>::new(),
        objective: "compact event log".to_string(),
    });
    PartialEnvelope {
        workspace_id: None,
        branch: BranchRef {
            name: "main".to_string(),
        },
        session_id: SessionId {
            value: session.to_string(),
        },
        task_id: None,
        actor: Actor::Daemon,
        kind: payload.kind(),
        references: Vec::new(),
        summary: CompactSummary::new("test event").expect("summary"),
        payload,
    }
}

fn symbol_id(file: &str, name: &str, byte_offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset,
    }
}

fn write_sample_snapshot(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    let path = dir.join("snapshot-1-1.bin");
    Snapshot::write(&path, &sample_graph(), &sample_memory_store(), 1).expect("snapshot writes");
    path
}

fn overwrite_at(path: &PathBuf, offset: u64, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open snapshot for mutation");
    file.seek(SeekFrom::Start(offset)).expect("seek snapshot");
    file.write_all(bytes).expect("write mutation");
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("lattice-{name}-{nanos}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}
