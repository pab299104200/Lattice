use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::params;
use tempfile::TempDir;

use super::{
    Actor, AssistantTaskStartedPayload, Bootstrap, BootstrapError, BranchRef, CompactSummary,
    EventPayload, EventQuery, EventQueryError, EventReader, EventStore, EventWriter, FlushPolicy,
    PartialEnvelope, SessionId, Snapshot, TaskId,
};
use crate::graph::{CodeGraph, EdgeKind};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use crate::symbols::{Language, SymbolId, SymbolKind};

#[test]
fn corrupted_payload_bytes_return_typed_error() {
    let fixture = CorruptionFixture::new();
    let writer = fixture.writer("workspace-main", 1);
    let event_id = writer
        .append(event_envelope(
            "task-payload",
            "session-a",
            "workspace-main",
            "main",
            5_000,
        ))
        .expect("spilled event appends");
    let spill_id = fixture.spill_row_id_for("task-payload");
    fixture.corrupt_spilled_bytes(spill_id);

    let error = fixture
        .reader()
        .execute(
            EventQuery::new()
                .task("task-payload")
                .workspace("workspace-main")
                .branch("main"),
        )
        .expect_err("corrupted payload should fail");

    match error {
        EventQueryError::PayloadCorruption {
            event_id: actual, ..
        } => assert_eq!(actual.ulid, event_id.ulid),
        other => panic!("expected typed corruption error, got {other:?}"),
    }
}

#[test]
fn corrupted_inline_payload_hash_returns_typed_error() {
    let fixture = CorruptionFixture::new();
    {
        let store = fixture.store();
        let writer = EventWriter::new(store.clone(), "workspace-main".to_string(), 4_096)
            .with_flush_policy(FlushPolicy::Sync);
        writer
            .append(event_envelope(
                "task-hash",
                "session-a",
                "workspace-main",
                "main",
                24,
            ))
            .expect("inline event appends");
        let hash = store
            .query_events_by_task("task-hash", 1)
            .expect("task rows load")[0]
            .payload_hash
            .clone();
        drop(writer);
        drop(store);
        flip_first_occurrence(fixture.db_path(), &hash);
    }

    let error = fixture
        .reopen_reader()
        .execute(
            EventQuery::new()
                .task("task-hash")
                .workspace("workspace-main")
                .branch("main"),
        )
        .expect_err("corrupted hash should fail");

    assert!(matches!(error, EventQueryError::PayloadCorruption { .. }));
}

#[test]
fn truncated_snapshot_without_fallback_returns_content_hash_mismatch() {
    let fixture = CorruptionFixture::new();
    let store = fixture.store();
    let writer = EventWriter::new(store, "workspace-main".to_string(), 4_096)
        .with_flush_policy(FlushPolicy::Sync);
    writer
        .append(event_envelope(
            "task-snapshot",
            "session-a",
            "workspace-main",
            "main",
            32,
        ))
        .expect("event appends");

    let path = fixture.tempdir.path().join("snapshot-1-1.bin");
    Snapshot::write(&path, &sample_graph(), &sample_memory_store(), 1).expect("snapshot writes");
    truncate_file(&path);

    let error = Bootstrap::load(&path, &fixture.reopen_store()).expect_err("truncated snapshot");
    assert!(matches!(error, BootstrapError::ContentHashMismatch { .. }));
}

#[test]
fn truncated_latest_snapshot_falls_back_to_prior_snapshot() {
    let fixture = CorruptionFixture::new();
    let store = fixture.store();
    let writer = EventWriter::new(store, "workspace-main".to_string(), 4_096)
        .with_flush_policy(FlushPolicy::Sync);
    writer
        .append(event_envelope(
            "task-fallback",
            "session-a",
            "workspace-main",
            "main",
            32,
        ))
        .expect("first event appends");
    let first = fixture.tempdir.path().join("snapshot-1-1.bin");
    Snapshot::write(&first, &sample_graph(), &sample_memory_store(), 1).expect("first snapshot");

    writer
        .append(event_envelope(
            "task-fallback",
            "session-b",
            "workspace-main",
            "main",
            64,
        ))
        .expect("second event appends");
    let second = fixture.tempdir.path().join("snapshot-2-2.bin");
    Snapshot::write(&second, &sample_graph(), &sample_memory_store(), 2).expect("second snapshot");
    truncate_file(&second);

    let bootstrapped = Bootstrap::load(&second, &fixture.reopen_store()).expect("fallback loads");
    assert_eq!(bootstrapped.snapshot.up_to_event_id, 1);
    assert_eq!(bootstrapped.replayed_event_rows, 1);
}

#[test]
fn events_table_update_fails_with_append_only_trigger_error() {
    let fixture = CorruptionFixture::new();
    let writer = fixture.writer("workspace-main", 4_096);
    writer
        .append(event_envelope(
            "task-trigger",
            "session-a",
            "workspace-main",
            "main",
            24,
        ))
        .expect("event appends");

    let error = fixture.store().with_connection(|conn| {
        conn.execute(
            "UPDATE events SET payload_hash = ?1 WHERE task_id = ?2",
            params![vec![1_u8; 32], "task-trigger"],
        )
        .expect_err("append-only trigger rejects update")
    });

    assert!(error.to_string().contains("append-only"));
}

struct CorruptionFixture {
    tempdir: TempDir,
    db_path: PathBuf,
}

impl CorruptionFixture {
    fn new() -> Self {
        let tempdir = TempDir::new().expect("tempdir creates");
        let db_path = tempdir.path().join("events.db");
        Self { tempdir, db_path }
    }

    fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn store(&self) -> Arc<EventStore> {
        Arc::new(EventStore::open(&self.db_path).expect("event store opens"))
    }

    fn reopen_store(&self) -> EventStore {
        EventStore::open(&self.db_path).expect("event store reopens")
    }

    fn reader(&self) -> EventReader {
        EventReader::new(self.store())
    }

    fn reopen_reader(&self) -> EventReader {
        EventReader::new(Arc::new(self.reopen_store()))
    }

    fn writer(&self, workspace_id: &str, inline_ceiling: usize) -> EventWriter {
        EventWriter::new(self.store(), workspace_id.to_string(), inline_ceiling)
            .with_flush_policy(FlushPolicy::Sync)
    }

    fn spill_row_id_for(&self, task_id: &str) -> i64 {
        self.store()
            .query_events_by_task(task_id, 1)
            .expect("task rows load")[0]
            .payload_spill_id
            .expect("payload spilled")
    }

    fn corrupt_spilled_bytes(&self, spill_id: i64) {
        self.store().with_connection(|conn| {
            conn.execute(
                "UPDATE event_payloads SET bytes = ?1, bytes_len = ?2 WHERE row_id = ?3",
                params![b"corrupted".to_vec(), 9_i64, spill_id],
            )
            .expect("payload bytes corrupt");
        });
    }
}

fn event_envelope(
    task_id: &str,
    session_id: &str,
    workspace_id: &str,
    branch: &str,
    objective_len: usize,
) -> PartialEnvelope {
    let payload = EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
        context_handle_id: None,
        seed_event_ids: Vec::new(),
        initial_memory_ids: Vec::new(),
        objective: "x".repeat(objective_len),
    });
    PartialEnvelope {
        workspace_id: Some(workspace_id.to_string()),
        branch: BranchRef {
            name: branch.to_string(),
        },
        session_id: SessionId {
            value: session_id.to_string(),
        },
        task_id: Some(TaskId {
            value: task_id.to_string(),
        }),
        actor: Actor::Assistant {
            model: "gpt-5.5".to_string(),
        },
        kind: payload.kind(),
        references: Vec::new(),
        summary: CompactSummary::new(format!("summary {task_id}")).expect("summary"),
        payload,
    }
}

fn flip_first_occurrence(path: &Path, needle: &[u8]) {
    let mut bytes = fs::read(path).expect("db bytes load");
    let Some(index) = bytes
        .windows(needle.len())
        .position(|window| window == needle)
    else {
        panic!("needle not found in {}", path.display());
    };
    bytes[index] ^= 0x01;
    fs::write(path, bytes).expect("db bytes rewrite");
}

fn truncate_file(path: &Path) {
    let bytes = fs::read(path).expect("snapshot reads");
    fs::write(path, &bytes[..bytes.len() / 2]).expect("snapshot truncates");
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
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    store.store(sample_memory()).expect("memory stores");
    store
}

fn sample_memory() -> Memory {
    Memory {
        id: "memory-a".to_string(),
        session_id: "session-a".to_string(),
        content: "Snapshot fallback preserves prior state.".to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 0.9,
        linked_symbols: vec!["helper".to_string()],
        linked_files: vec!["src/lib.rs".to_string()],
        workspace_id: Some("workspace-main".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: Some("corruption-test".to_string()),
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

fn symbol_id(file: &str, name: &str, byte_offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset,
    }
}
