use std::io::Write;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use serde_json::json;
use tempfile::tempdir;
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::events::{EventKind, EventPayload, EventQuery, EventReader, EventStore, EventWriter};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};

#[test]
fn deterministic_job_emits_proposal_without_mutating_memory() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime(8);
    runtime
        .submit(create_memory_job(
            "job-create",
            "proposal-create",
            "created memory",
        ))
        .expect("job enqueues");

    let proposals = runtime.run_due().expect("job runs");

    assert_eq!(proposals.len(), 1);
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 0);
    assert_eq!(
        proposal_decision(fixture.consolidation_db.path(), "proposal-create"),
        "pending"
    );
}

#[test]
fn apply_materializes_memory_and_writes_consolidated_event_with_states() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime(8);
    runtime
        .submit(create_memory_job(
            "job-apply",
            "proposal-apply",
            "applied memory",
        ))
        .expect("job enqueues");
    runtime.run_due().expect("job runs");

    let outcome = runtime
        .decide(
            "proposal-apply",
            ProposalDecision::Applied,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
        )
        .expect("proposal applies");

    assert_eq!(
        outcome,
        Some(ApplyOutcome::Applied {
            memory_id: "mem-proposal-apply".to_string()
        })
    );
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 1);
    let event = fixture.memory_consolidated_event();
    match event.payload {
        EventPayload::MemoryConsolidated(payload) => {
            assert_eq!(payload.proposal_id.as_deref(), Some("proposal-apply"));
            assert!(payload.prior_state_json.unwrap().contains("{}"));
            assert!(payload
                .proposed_state_json
                .unwrap()
                .contains("applied memory"));
        }
        _ => panic!("expected memory_consolidated payload"),
    }
}

#[test]
fn reject_leaves_memory_unchanged_and_records_decision() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime(8);
    runtime
        .submit(create_memory_job(
            "job-reject",
            "proposal-reject",
            "rejected memory",
        ))
        .expect("job enqueues");
    runtime.run_due().expect("job runs");

    let outcome = runtime
        .decide(
            "proposal-reject",
            ProposalDecision::Rejected,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
        )
        .expect("proposal rejects");

    assert_eq!(outcome, None);
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 0);
    assert_eq!(
        proposal_decision(fixture.consolidation_db.path(), "proposal-reject"),
        "rejected"
    );
}

#[test]
fn double_apply_is_no_op_after_first_decision() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime(8);
    runtime
        .submit(create_memory_job(
            "job-double",
            "proposal-double",
            "single memory",
        ))
        .expect("job enqueues");
    runtime.run_due().expect("job runs");

    runtime
        .decide(
            "proposal-double",
            ProposalDecision::Applied,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
        )
        .expect("first apply succeeds");
    let second = runtime
        .decide(
            "proposal-double",
            ProposalDecision::Applied,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
        )
        .expect("second apply is handled");

    assert_eq!(
        second,
        Some(ApplyOutcome::AlreadyDecided {
            decision: ProposalDecision::Applied
        })
    );
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 1);
}

#[test]
fn queue_drops_jobs_after_depth_bound_and_logs_warning() {
    let conn = Connection::open_in_memory().expect("db opens");
    initialize_schema(&conn).expect("schema initializes");
    let conn = Arc::new(Mutex::new(conn));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferWriter(logs.clone()))
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut queue = BoundedJobQueue::new(1, conn);

    let first = queue
        .enqueue(create_memory_job("job-one", "proposal-one", "one"))
        .expect("first enqueue succeeds");
    let second = queue
        .enqueue(create_memory_job("job-two", "proposal-two", "two"))
        .expect("second enqueue returns outcome");

    assert_eq!(first, EnqueueOutcome::Queued { depth: 1 });
    assert!(matches!(second, EnqueueOutcome::Dropped { .. }));
    let log_text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(log_text.contains("consolidation queue full; dropping job"));
}

#[test]
fn consolidation_schema_round_trips_under_memory_store_migration() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("memory.sqlite");
    let store = MemoryStore::open(&path).expect("memory store opens");
    drop(store);
    let conn = Connection::open(&path).expect("db reopens");
    conn.execute(
        "INSERT INTO consolidation_jobs
            (job_id, workspace_id, kind, mode, status, enqueued_at)
         VALUES ('job-schema', 'workspace-main', 'schema_test', 'background', 'queued', 1)",
        [],
    )
    .expect("job inserts");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM consolidation_jobs", [], |row| {
            row.get(0)
        })
        .expect("job count reads");
    assert_eq!(count, 1);
}

struct Fixture {
    _dir: tempfile::TempDir,
    consolidation_db: DbPath,
    memory_store: MemoryStore,
    event_store: Arc<EventStore>,
    event_writer: EventWriter,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let memory_store = MemoryStore::open(&dir.path().join("memory.sqlite")).unwrap();
        let event_store = Arc::new(EventStore::open_in_memory().unwrap());
        let event_writer =
            EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096);
        Self {
            consolidation_db: DbPath(dir.path().join("consolidation.sqlite")),
            _dir: dir,
            memory_store,
            event_store,
            event_writer,
        }
    }

    fn runtime(&self, max_queue_depth: usize) -> ConsolidationJobRuntime {
        let conn = Connection::open(self.consolidation_db.path()).expect("db opens");
        ConsolidationJobRuntime::new(
            conn,
            ConsolidationConfig {
                max_queue_depth,
                llm_budget_catalog: None,
            },
        )
        .unwrap()
    }

    fn memory_consolidated_event(&self) -> crate::events::EventEnvelope {
        let reader = EventReader::new(self.event_store.clone());
        let events = EventQuery::new()
            .workspace("workspace-main")
            .branch("main")
            .kind(EventKind::MemoryConsolidated)
            .execute(&reader)
            .expect("events query succeeds");
        events.into_iter().next().expect("event exists")
    }
}

struct DbPath(std::path::PathBuf);

impl DbPath {
    fn path(&self) -> &std::path::Path {
        self.0.as_path()
    }
}

#[derive(Clone)]
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for BufferWriter {
    type Writer = BufferGuard;

    fn make_writer(&'a self) -> Self::Writer {
        BufferGuard(self.0.clone())
    }
}

struct BufferGuard(Arc<Mutex<Vec<u8>>>);

impl Write for BufferGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn create_memory_job(job_id: &str, proposal_id: &str, content: &str) -> ConsolidationJobSpec {
    ConsolidationJobSpec {
        job_id: job_id.to_string(),
        workspace_id: "workspace-main".to_string(),
        kind: "deterministic_test".to_string(),
        mode: ConsolidationJobMode::Background,
        proposal: Some(PendingProposalSpec {
            proposal_id: proposal_id.to_string(),
            target_memory_id: None,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: empty_state(),
            proposed_state: serde_json::to_value(memory(proposal_id, content)).unwrap(),
            evidence: json!({ "source_memory_ids": [] }),
            provenance: None,
        }),
    }
}

fn memory(proposal_id: &str, content: &str) -> Memory {
    Memory {
        id: format!("mem-{proposal_id}"),
        session_id: "session-main".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Session,
        confidence: 1.0,
        linked_symbols: Vec::new(),
        linked_files: Vec::new(),
        workspace_id: Some("workspace-main".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

fn proposal_decision(path: &std::path::Path, proposal_id: &str) -> String {
    let conn = Connection::open(path).expect("db opens");
    conn.query_row(
        "SELECT decision FROM consolidation_proposals WHERE proposal_id = ?1",
        params![proposal_id],
        |row| row.get(0),
    )
    .expect("decision reads")
}
