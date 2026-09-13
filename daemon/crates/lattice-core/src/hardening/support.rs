use std::cell::RefCell;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::params;
use tempfile::TempDir;
use tracing_subscriber::fmt::MakeWriter;

use crate::events::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, EventPayload, EventQuery,
    EventReader, EventStore, EventWriter, FlushPolicy, PartialEnvelope, QueryOrder, SessionId,
    StableRef, TaskId,
};
use crate::graph::{CodeGraph, EdgeKind};
use crate::identity::{FileId, MemoryId, SymbolId as StableSymbolId};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus};
use crate::symbols::{Language, SymbolId, SymbolKind};

pub(crate) struct HardeningFixture {
    pub(crate) tempdir: TempDir,
    pub(crate) db_path: PathBuf,
}

impl HardeningFixture {
    pub(crate) fn new() -> Self {
        let tempdir = TempDir::new().expect("hardening tempdir creates");
        let db_path = tempdir.path().join("events.db");
        Self { tempdir, db_path }
    }

    pub(crate) fn store(&self) -> Arc<EventStore> {
        Arc::new(EventStore::open(&self.db_path).expect("event store opens"))
    }

    pub(crate) fn reopen_store(&self) -> Arc<EventStore> {
        Arc::new(EventStore::open(&self.db_path).expect("event store reopens"))
    }

    pub(crate) fn reopen_reader(&self) -> EventReader {
        EventReader::new(self.reopen_store())
    }

    pub(crate) fn writer(&self, inline_ceiling: usize) -> EventWriter {
        EventWriter::new(self.store(), "workspace-main".to_string(), inline_ceiling)
            .with_flush_policy(FlushPolicy::Sync)
    }

    pub(crate) fn snapshot_path(&self, name: &str) -> PathBuf {
        self.tempdir.path().join(name)
    }

    pub(crate) fn query_task(&self, task_id: &str) -> EventQuery {
        EventQuery::new()
            .task(task_id)
            .workspace("workspace-main")
            .branch("main")
            .order(QueryOrder::OldestFirst)
            .limit(100)
    }

    pub(crate) fn spill_row_id_for(&self, task_id: &str) -> i64 {
        self.store()
            .query_events_by_task(task_id, 10)
            .expect("task rows load")
            .into_iter()
            .find_map(|row| row.payload_spill_id)
            .expect("spilled payload exists")
    }

    pub(crate) fn delete_spill_row(&self, row_id: i64) {
        self.store().with_connection(|conn| {
            conn.pragma_update(None, "foreign_keys", "OFF")
                .expect("foreign keys can be disabled for crash fixture");
            conn.execute(
                "DELETE FROM event_payloads WHERE row_id = ?1",
                params![row_id],
            )
            .expect("spill row deletes");
            conn.pragma_update(None, "foreign_keys", "ON")
                .expect("foreign keys re-enabled");
        });
    }

    pub(crate) fn overwrite_spill_bytes_preserving_hash(&self, row_id: i64, bytes: &[u8]) {
        self.store().with_connection(|conn| {
            conn.execute(
                "UPDATE event_payloads SET bytes = ?1, bytes_len = ?2 WHERE row_id = ?3",
                params![bytes, bytes.len() as i64, row_id],
            )
            .expect("spill bytes mutate");
        });
    }

    pub(crate) fn overwrite_spill_hash(&self, row_id: i64, hash: &[u8]) {
        self.store().with_connection(|conn| {
            conn.execute(
                "UPDATE event_payloads SET payload_hash = ?1 WHERE row_id = ?2",
                params![hash, row_id],
            )
            .expect("spill hash mutates");
        });
    }

    pub(crate) fn set_event_kind(&self, task_id: &str, kind: &str) {
        self.store().with_connection(|conn| {
            conn.execute("DROP TRIGGER IF EXISTS events_no_update", [])
                .expect("append-only update trigger drops for corruption fixture");
            conn.execute(
                "UPDATE events SET kind = ?1 WHERE task_id = ?2",
                params![kind, task_id],
            )
            .expect("event kind mutates");
        });
    }
}

pub(crate) fn event_envelope(task_id: &str, session_id: &str, objective: &str) -> PartialEnvelope {
    event_envelope_with_refs(task_id, session_id, objective, Vec::new())
}

pub(crate) fn event_envelope_with_refs(
    task_id: &str,
    session_id: &str,
    objective: &str,
    references: Vec<StableRef>,
) -> PartialEnvelope {
    let payload = EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
        context_handle_id: None,
        seed_event_ids: Vec::new(),
        initial_memory_ids: Vec::new(),
        objective: objective.to_string(),
    });
    PartialEnvelope {
        workspace_id: Some("workspace-main".to_string()),
        branch: BranchRef {
            name: "main".to_string(),
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
        references,
        summary: CompactSummary::new(format!("hardening {task_id}")).expect("summary"),
        payload,
    }
}

pub(crate) fn sample_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    let main = symbol_id("src/main.rs", "main", 1);
    let helper = symbol_id("src/lib.rs", "helper", 2);
    graph.add_node(
        main.clone(),
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
        helper.clone(),
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
    graph.add_edge(&main, &helper, EdgeKind::Calls);
    graph
}

pub(crate) fn sample_memory_store() -> MemoryStore {
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    store
        .store(sample_memory("memory-a", "Snapshot state is readable."))
        .expect("memory stores");
    store
}

pub(crate) fn sample_memory(id: &str, content: &str) -> Memory {
    Memory {
        id: id.to_string(),
        session_id: "session-a".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 0.9,
        linked_symbols: vec!["helper".to_string()],
        linked_files: vec!["src/lib.rs".to_string()],
        workspace_id: Some("workspace-main".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: Some("hardening-test".to_string()),
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

pub(crate) fn truncate_file(path: &Path) {
    let bytes = fs::read(path).expect("file reads before truncation");
    fs::write(path, &bytes[..bytes.len() / 2]).expect("file truncates");
}

pub(crate) fn overwrite_snapshot_version(path: &Path, version: u16) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("snapshot opens for version mutation");
    file.seek(SeekFrom::Start(8)).expect("snapshot seeks");
    file.write_all(&version.to_le_bytes())
        .expect("snapshot version writes");
}

pub(crate) fn missing_memory_ref(id: &str) -> StableRef {
    StableRef::MemoryRef(MemoryId {
        workspace_id: "workspace-main".to_string(),
        ulid: id.to_string(),
    })
}

pub(crate) fn missing_symbol_ref(path: &str, name: &str) -> StableRef {
    StableRef::SymbolRef(StableSymbolId {
        file: FileId {
            workspace_id: "workspace-main".to_string(),
            repo_relative_path: path.to_string(),
            content_hash: "missing".to_string(),
        },
        qualified_name: name.to_string(),
        byte_offset: 0,
        kind: "function".to_string(),
    })
}

pub(crate) fn capture_logs<R>(run: impl FnOnce() -> R) -> (R, String) {
    // A scoped subscriber makes log assertions independent of other suites
    // that install a global subscriber earlier in this test process.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(ThreadLocalWriter)
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let logs = Arc::new(Mutex::new(Vec::new()));
    CAPTURE_BUFFER.with(|cell| {
        cell.borrow_mut().replace(logs.clone());
    });
    let result = tracing::subscriber::with_default(subscriber, run);
    CAPTURE_BUFFER.with(|cell| {
        cell.borrow_mut().take();
    });
    let output = String::from_utf8(logs.lock().expect("log lock").clone()).expect("utf8 logs");
    (result, output)
}

thread_local! {
    static CAPTURE_BUFFER: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
struct ThreadLocalWriter;

impl MakeWriter<'_> for ThreadLocalWriter {
    type Writer = ThreadLocalGuard;

    fn make_writer(&self) -> Self::Writer {
        ThreadLocalGuard
    }
}

struct ThreadLocalGuard;

impl Write for ThreadLocalGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE_BUFFER.with(|cell| {
            if let Some(buffer) = cell.borrow().as_ref() {
                buffer
                    .lock()
                    .expect("log buffer lock")
                    .extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn symbol_id(file: &str, name: &str, byte_offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset,
    }
}
