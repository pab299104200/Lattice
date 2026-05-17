use std::sync::{Arc, Mutex};
use std::thread;

use tempfile::TempDir;

use super::{
    Actor, AssistantTaskStartedPayload, Bootstrap, BranchRef, CompactSummary, EventEnvelope,
    EventPayload, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy, PartialEnvelope,
    QueryOrder, SessionId, Snapshot, TaskId,
};
use crate::graph::{CodeGraph, EdgeKind};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use crate::symbols::{Language, SymbolId, SymbolKind};

#[test]
fn stream_returns_task_events_in_monotonic_event_id_order_after_concurrent_writes() {
    let fixture = ReplayFixture::new();
    let writer = fixture.writer("workspace-main");
    let task_id = "task-ordered";

    thread::scope(|scope| {
        for session in ["session-a", "session-b"] {
            let writer = writer.clone();
            scope.spawn(move || append_series(&writer, task_id, session, 500));
        }
    });

    let events = fixture.read_task(task_id, "workspace-main", "main", 2_000);
    let expected = fixture
        .store
        .query_events_after_row_id(0, 2_000)
        .expect("event rows load")
        .into_iter()
        .map(|row| row.event_uuid)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1_000);
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_id.ulid.clone())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn concurrent_appends_keep_event_row_ids_and_event_ids_monotonic() {
    let fixture = ReplayFixture::new();
    let writer = fixture.writer("workspace-main");
    let task_id = "task-monotonic";
    let inserted = Arc::new(Mutex::new(Vec::new()));

    thread::scope(|scope| {
        for session in ["session-a", "session-b"] {
            let writer = writer.clone();
            let inserted = inserted.clone();
            scope.spawn(move || append_and_collect_ids(&writer, &inserted, task_id, session, 250));
        }
    });

    let rows = fixture
        .store
        .query_events_after_row_id(0, 1_000)
        .expect("event rows load");
    let mut inserted = inserted.lock().expect("ids lock").clone();
    inserted.sort();
    assert_eq!(rows.len(), 500);
    assert!(rows
        .windows(2)
        .all(|window| window[0].event_id < window[1].event_id));
    assert_eq!(rows.len(), inserted.len());
    assert_eq!(
        sorted_strings(rows.iter().map(|row| row.event_uuid.as_str())),
        inserted
    );
}

#[test]
fn bootstrap_snapshot_plus_tail_matches_full_state_for_state_neutral_events() {
    let fixture = ReplayFixture::new();
    let writer = fixture.writer("workspace-main");
    writer
        .append(event_envelope(
            "task-bootstrap",
            "session-a",
            "workspace-main",
            "main",
            "before",
        ))
        .expect("seed event appends");

    let graph = sample_graph();
    let memory = sample_memory_store();
    let snapshot_path = fixture.tempdir.path().join("snapshot-1-1.bin");
    Snapshot::write(&snapshot_path, &graph, &memory, 1).expect("snapshot writes");

    writer
        .append(event_envelope(
            "task-bootstrap",
            "session-b",
            "workspace-main",
            "main",
            "after",
        ))
        .expect("tail event appends");

    let bootstrapped = Bootstrap::load(&snapshot_path, &fixture.store).expect("bootstrap loads");
    let full_replay_state = Snapshot::capture(&graph, &memory, 2).expect("full state captures");

    assert_eq!(bootstrapped.replayed_event_rows, 1);
    assert_eq!(
        bootstrapped.snapshot.graph_state,
        full_replay_state.graph_state
    );
    assert_eq!(
        bootstrapped.snapshot.memory_state,
        full_replay_state.memory_state
    );
}

#[test]
fn workspace_and_branch_scopes_do_not_leak_across_neighbors() {
    let fixture = ReplayFixture::new();
    fixture
        .writer("workspace-a")
        .append(event_envelope(
            "task-a",
            "session-a",
            "workspace-a",
            "main",
            "a-main",
        ))
        .expect("workspace a main appends");
    fixture
        .writer("workspace-a")
        .append(event_envelope(
            "task-a",
            "session-b",
            "workspace-a",
            "dev",
            "a-dev",
        ))
        .expect("workspace a dev appends");
    fixture
        .writer("workspace-b")
        .append(event_envelope(
            "task-b",
            "session-c",
            "workspace-b",
            "main",
            "b-main",
        ))
        .expect("workspace b main appends");

    let a_main = fixture.read_workspace_branch("workspace-a", "main");
    let a_dev = fixture.read_workspace_branch("workspace-a", "dev");

    assert_eq!(event_objectives(&a_main), vec!["a-main"]);
    assert_eq!(event_objectives(&a_dev), vec!["a-dev"]);
}

struct ReplayFixture {
    tempdir: TempDir,
    store: Arc<EventStore>,
    reader: EventReader,
}

impl ReplayFixture {
    fn new() -> Self {
        let tempdir = TempDir::new().expect("tempdir creates");
        let db_path = tempdir.path().join("events.db");
        let store = Arc::new(EventStore::open(&db_path).expect("event store opens"));
        let reader = EventReader::new(store.clone());
        Self {
            tempdir,
            store,
            reader,
        }
    }

    fn writer(&self, workspace_id: &str) -> Arc<EventWriter> {
        Arc::new(
            EventWriter::new(self.store.clone(), workspace_id.to_string(), 4_096)
                .with_flush_policy(FlushPolicy::Sync),
        )
    }

    fn read_task(
        &self,
        task_id: &str,
        workspace_id: &str,
        branch: &str,
        limit: usize,
    ) -> Vec<EventEnvelope> {
        self.reader
            .stream(
                EventQuery::new()
                    .task(task_id)
                    .workspace(workspace_id)
                    .branch(branch)
                    .order(QueryOrder::OldestFirst)
                    .limit(limit),
            )
            .collect::<Result<Vec<_>, _>>()
            .expect("task stream succeeds")
    }

    fn read_workspace_branch(&self, workspace_id: &str, branch: &str) -> Vec<EventEnvelope> {
        self.reader
            .execute(
                EventQuery::new()
                    .workspace(workspace_id)
                    .branch(branch)
                    .order(QueryOrder::OldestFirst)
                    .limit(100),
            )
            .expect("workspace branch query succeeds")
    }
}

fn append_series(writer: &EventWriter, task_id: &str, session_id: &str, count: usize) {
    for index in 0..count {
        writer
            .append(event_envelope(
                task_id,
                session_id,
                "workspace-main",
                "main",
                &format!("{session_id}-{index}"),
            ))
            .expect("event appends");
    }
}

fn append_and_collect_ids(
    writer: &EventWriter,
    inserted: &Arc<Mutex<Vec<String>>>,
    task_id: &str,
    session_id: &str,
    count: usize,
) {
    for index in 0..count {
        let event_id = writer
            .append(event_envelope(
                task_id,
                session_id,
                "workspace-main",
                "main",
                &format!("monotonic-{session_id}-{index}"),
            ))
            .expect("event appends");
        inserted.lock().expect("ids lock").push(event_id.ulid);
    }
}

fn event_envelope(
    task_id: &str,
    session_id: &str,
    workspace_id: &str,
    branch: &str,
    objective: &str,
) -> PartialEnvelope {
    let payload = EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
        context_handle_id: None,
        seed_event_ids: Vec::new(),
        initial_memory_ids: Vec::new(),
        objective: objective.to_string(),
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
        summary: CompactSummary::new(format!("summary {objective}")).expect("summary"),
        payload,
    }
}

fn sorted_strings<'a>(items: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut values = items.map(str::to_string).collect::<Vec<_>>();
    values.sort();
    values
}

fn event_objectives(events: &[EventEnvelope]) -> Vec<&str> {
    events
        .iter()
        .map(|event| match &event.payload {
            EventPayload::AssistantTaskStarted(payload) => payload.objective.as_str(),
            other => panic!("expected assistant task payload, got {other:?}"),
        })
        .collect()
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
        content: "Replay preserves graph and memory snapshots.".to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 0.9,
        linked_symbols: vec!["helper".to_string()],
        linked_files: vec!["src/lib.rs".to_string()],
        workspace_id: Some("workspace-main".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: Some("replay-test".to_string()),
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
