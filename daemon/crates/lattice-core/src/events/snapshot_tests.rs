use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    expire_managed_snapshots, expire_managed_snapshots_dir_page, Actor,
    AssistantTaskStartedPayload, Bootstrap, BranchRef, CompactSummary, CompactionConfig, Compactor,
    EventPayload, EventStore, EventWriter, FlushPolicy, PartialEnvelope, SessionId, Snapshot,
    SnapshotError,
};
use crate::graph::{CodeGraph, EdgeKind};
use crate::identity::{EventId, MemoryId};
use crate::symbols::{Language, SymbolId, SymbolKind};

#[test]
fn snapshot_write_and_read_preserves_graph_without_memory_payloads() {
    let dir = temp_dir("snapshot-round-trip");
    let path = dir.join("snapshot-1-1.bin");
    let graph = sample_graph();

    Snapshot::write(&path, &graph, 7).expect("snapshot writes");
    let read = Snapshot::read(&path).expect("snapshot reads");
    let expected = Snapshot::capture(&graph, 7).expect("snapshot captures");

    assert_eq!(read.up_to_event_id, 7);
    assert_eq!(read.graph_state, expected.graph_state);
    assert_eq!(read.memory_state, expected.memory_state);
    assert!(read.memory_state.memories.is_empty());
    assert!(!std::fs::read(&path)
        .unwrap()
        .windows(b"Graph snapshots preserve memory.".len())
        .any(|w| w == b"Graph snapshots preserve memory."));
}

#[test]
fn expiry_is_bounded_and_preserves_unknown_and_newest_files() {
    let dir = temp_dir("snapshot-expiry");
    let graph = sample_graph();
    let old = dir.join("snapshot-1-1.bin");
    let newest = dir.join("snapshot-2-2.bin");
    let unknown = dir.join("snapshot-not-managed.bin");
    Snapshot::write(&old, &graph, 1).unwrap();
    Snapshot::write(&newest, &graph, 2).unwrap();
    fs::write(&unknown, b"operator backup").unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 31 * 86_400;
    let report = expire_managed_snapshots(&dir, now, 30 * 86_400, u64::MAX, 1).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!old.exists());
    assert!(newest.exists());
    assert!(unknown.exists());
    assert!(report.unknown_files >= 1);
}

#[test]
fn expiry_cursor_progresses_past_four_thousand_directory_entries() {
    let dir = temp_dir("snapshot-expiry-paging");
    for value in 0..4100 {
        fs::write(dir.join(format!("unknown-{value:04}")), b"x").unwrap();
    }
    let managed = crate::storage::SecureDir::open(&dir).unwrap();
    let mut cursor = None;
    let mut inspected = 0usize;
    let mut cursors = Vec::new();
    for _ in 0..600 {
        let report = expire_managed_snapshots_dir_page(
            &managed,
            u64::MAX,
            1,
            64 * 1024 * 1024,
            8,
            cursor.clone(),
        )
        .unwrap();
        assert!(report.inspected + report.unknown_files <= 256);
        inspected += report.inspected + report.unknown_files;
        cursor = report.next_cookie;
        cursors.push(cursor.clone());
        if cursor.is_none() {
            break;
        }
    }
    assert!(
        cursor.is_none(),
        "bounded continuation reaches directory end: {cursors:?}, inspected={inspected}"
    );
    assert!(inspected >= 4100, "inventory and sweep both remain bounded");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn interrupted_inventory_proves_global_newest_before_bounded_sweep() {
    let dir = temp_dir("snapshot-expiry-interrupted");
    let graph = sample_graph();
    for row in 1..=300i64 {
        Snapshot::write(
            &dir.join(format!("snapshot-{row}-{}.bin", row as u128)),
            &graph,
            row,
        )
        .unwrap();
    }
    let mut cursor = None;
    let mut deleted = 0;
    for turn in 0..100 {
        let managed = crate::storage::SecureDir::open(&dir).unwrap();
        let report =
            expire_managed_snapshots_dir_page(&managed, u64::MAX, 1, 64 * 1024 * 1024, 8, cursor)
                .unwrap();
        assert!(report.inspected + report.unknown_files <= 256);
        deleted += report.deleted;
        cursor = report.next_cookie;
        if turn == 0 {
            let encoded = serde_json::to_string(&cursor).unwrap();
            cursor = serde_json::from_str(&encoded).unwrap();
            assert_eq!(deleted, 0, "partial inventory cannot delete");
        }
        if cursor.is_none() {
            break;
        }
    }
    assert!(
        cursor.is_none(),
        "inventory and sweep must finish after restart"
    );
    assert_eq!(deleted, 299);
    assert!(dir.join("snapshot-300-300.bin").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn directory_mutation_restarts_inventory_before_any_deletion() {
    let dir = temp_dir("snapshot-expiry-mutated-inventory");
    let graph = sample_graph();
    for row in 1..=300i64 {
        Snapshot::write(
            &dir.join(format!("snapshot-{row}-{}.bin", row as u128)),
            &graph,
            row,
        )
        .unwrap();
    }
    let managed = crate::storage::SecureDir::open(&dir).unwrap();
    let first = expire_managed_snapshots_dir_page(&managed, u64::MAX, 1, 64 * 1024 * 1024, 8, None)
        .unwrap();
    assert_eq!(first.deleted, 0);
    let inserted = dir.join("snapshot-1000-1000.bin");
    Snapshot::write(&inserted, &graph, 1000).unwrap();

    let reopened = crate::storage::SecureDir::open(&dir).unwrap();
    let restarted = expire_managed_snapshots_dir_page(
        &reopened,
        u64::MAX,
        1,
        64 * 1024 * 1024,
        8,
        first.next_cookie,
    )
    .unwrap();
    assert_eq!(restarted.deleted, 0);
    let cursor = restarted
        .next_cookie
        .expect("mutation restarts a fenced inventory");
    assert!(!cursor.sweeping);
    // Restart makes bounded progress in the new directory generation in the
    // same call; it must not delete from its still-partial inventory.
    assert_eq!(
        cursor.fingerprint,
        Some(reopened.directory_fingerprint().unwrap())
    );
    assert!(restarted.inspected > 0 && restarted.inspected <= 256);

    let mut cursor = Some(cursor);
    for _ in 0..200 {
        let reopened = crate::storage::SecureDir::open(&dir).unwrap();
        let report =
            expire_managed_snapshots_dir_page(&reopened, u64::MAX, 1, 64 * 1024 * 1024, 8, cursor)
                .unwrap();
        cursor = report.next_cookie;
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none());
    assert!(
        inserted.exists(),
        "global numeric newest survives the restarted cycle"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn vanished_entry_error_retries_by_restarting_the_persisted_inventory() {
    let dir = temp_dir("snapshot-expiry-vanished-entry");
    for index in 0..300 {
        fs::write(dir.join(format!("unknown-{index:04}")), b"x").unwrap();
    }
    let managed = crate::storage::SecureDir::open(&dir).unwrap();
    let first = expire_managed_snapshots_dir_page(&managed, u64::MAX, 1, 1024, 8, None).unwrap();
    let persisted = first.next_cookie.expect("inventory requires another page");
    let mutation_root = dir.clone();
    crate::storage::managed_fs::set_before_directory_metadata_hook(move || {
        for entry in fs::read_dir(&mutation_root).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
    });
    let reopened = crate::storage::SecureDir::open(&dir).unwrap();
    assert!(expire_managed_snapshots_dir_page(
        &reopened,
        u64::MAX,
        1,
        1024,
        8,
        Some(persisted.clone()),
    )
    .is_err());

    let reopened = crate::storage::SecureDir::open(&dir).unwrap();
    let retry = expire_managed_snapshots_dir_page(&reopened, u64::MAX, 1, 1024, 8, Some(persisted))
        .unwrap();
    let restarted = retry
        .next_cookie
        .expect("changed directory restarts safely");
    assert!(restarted.directory.is_none());
    assert!(restarted.newest.is_none());
    assert_eq!(retry.deleted, 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn format_version_mismatch_returns_unsupported_without_panic() {
    let path = write_sample_snapshot("snapshot-version");
    overwrite_at(&path, 8, &3u16.to_le_bytes());

    let error = Snapshot::read(&path).expect_err("version mismatch rejects");

    assert!(matches!(
        error,
        SnapshotError::FormatVersionUnsupported { found: 3, max: 2 }
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
    Snapshot::write(&path, &sample_graph(), 1).expect("snapshot writes");
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
    Snapshot::write(&path, &sample_graph(), 1).expect("snapshot writes");
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
