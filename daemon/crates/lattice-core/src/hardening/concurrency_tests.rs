//! Phase 11 hardening tests for concurrent event, memory, and consolidation use.
//!
//! These tests pin the concurrency invariants from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `### 3. Event Log`, `### 6. Consolidation Engine`, and
//! `### Phase 11: Hardening`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;
use tokio::task;
use tokio::time::{sleep, Duration};
use tracing::info;

use crate::consolidation::{
    empty_state, ApplyOutcome, ConsolidationConfig, ConsolidationJobMode, ConsolidationJobRuntime,
    ConsolidationJobSpec, PendingProposalSpec, ProposalDecision, ProposalKind,
};
use crate::events::{
    Actor, BranchRef, CompactSummary, ContextBundleReturnedPayload, EventPayload, EventQuery,
    EventReader, EventStore, EventWriter, FlushPolicy, PartialEnvelope, SessionId,
    ToolCalledPayload, ToolResultPayload, ToolResultStatus,
};
use crate::graph::CodeGraph;
use crate::identity::{ContextHandleId, EventId};
use crate::memory::{Memory, MemoryLinkRecord, MemoryScope, MemoryStore, MemoryType};

const TARGET: &str = "lattice_core::hardening::concurrency_tests";
const WORKSPACE: &str = "workspace-hardening";
const BRANCH: &str = "main";
const SESSION_COUNT: usize = 32;
const EVENTS_PER_SESSION: usize = 6;
const WRITER_COUNT: usize = 32;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn test_parallel_sessions_preserve_order_scope_and_unique_event_ids() {
    let fixture = EventFixture::new();
    let handles = (0..SESSION_COUNT)
        .map(|index| spawn_event_stream(fixture.writer.clone(), index, EVENTS_PER_SESSION))
        .collect::<Vec<_>>();
    let expected_sessions = join_all(handles).await;

    let reader = EventReader::new(fixture.store.clone());
    let mut all_event_ids = HashSet::new();
    for session in &expected_sessions {
        let events = read_session(&reader, session, EVENTS_PER_SESSION);
        assert_session_order(session, &events);
        for event in events {
            assert!(
                all_event_ids.insert(event.event_id.ulid),
                "event id collision"
            );
        }
    }

    let scoped = read_session(&reader, &expected_sessions[0], EVENTS_PER_SESSION);
    assert!(scoped
        .iter()
        .all(|event| event.session_id.value == expected_sessions[0]));
    let cross_session = read_workspace(&reader, SESSION_COUNT * EVENTS_PER_SESSION);
    assert_eq!(cross_session.len(), SESSION_COUNT * EVENTS_PER_SESSION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn test_parallel_event_writers_do_not_drop_events_or_reorder_each_writer() {
    let fixture = EventFixture::new();
    let handles = (0..WRITER_COUNT)
        .map(|index| spawn_event_stream(fixture.writer.clone(), index, EVENTS_PER_SESSION))
        .collect::<Vec<_>>();
    let sessions = join_all(handles).await;
    let reader = EventReader::new(fixture.store.clone());
    let events = read_workspace(&reader, WRITER_COUNT * EVENTS_PER_SESSION);

    assert_eq!(events.len(), WRITER_COUNT * EVENTS_PER_SESSION);
    for session in sessions {
        let events = events
            .iter()
            .filter(|event| event.session_id.value == session)
            .cloned()
            .collect::<Vec<_>>();
        assert_session_order(&session, &events);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn test_parallel_memory_creates_keep_unique_ids_and_valid_invariants() {
    let fixture = TempDbFixture::new();
    let handles = (0..WRITER_COUNT)
        .map(|index| {
            let path = fixture.memory_path.clone();
            task::spawn_blocking(move || {
                let store = MemoryStore::open(&path).expect("memory store opens");
                let id = store.store(test_memory(index)).expect("memory stores");
                info!(target: TARGET, index, memory_id = id, "memory writer committed");
                Ok::<_, crate::LatticeError>(id)
            })
        })
        .collect::<Vec<_>>();
    let ids = join_all(handles).await;
    let store = MemoryStore::open(&fixture.memory_path).expect("memory store reopens");
    let records = store.list_all().expect("memories list");

    assert_eq!(records.len(), WRITER_COUNT);
    assert_eq!(ids.into_iter().collect::<HashSet<_>>().len(), WRITER_COUNT);
    for record in records {
        assert!((0.0..=1.0).contains(&record.confidence));
        assert!(matches!(record.verification_status.as_str(), "unverified"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn test_parallel_memory_links_and_accesses_do_not_tear_or_drop_rows() {
    let fixture = TempDbFixture::new();
    seed_link_memory_pair(&fixture.memory_path, "source", "target");
    let handles = (0..WRITER_COUNT)
        .map(|index| spawn_link_and_touch(fixture.memory_path.clone(), index))
        .collect::<Vec<_>>();
    join_all(handles).await;

    let store = MemoryStore::open(&fixture.memory_path).expect("memory store reopens");
    let links = store.list_memory_links_from("source").expect("links list");
    let source = store
        .get_by_id("source")
        .expect("source reads")
        .expect("source exists");

    assert_eq!(links.len(), WRITER_COUNT);
    assert_eq!(source.access_count, WRITER_COUNT as u32);
    for link in links {
        assert!(!link.source_memory_id.is_empty(), "link source was torn");
        assert!(!link.target_memory_id.is_empty(), "link target was torn");
        assert_eq!(link.source_memory_id, "source");
        assert_eq!(link.target_memory_id, "target");
    }
}

#[test]
#[ignore]
fn test_consolidation_replay_proposals_are_deterministic_and_apply_is_idempotent() {
    let left = run_consolidation_trace("left");
    let right = run_consolidation_trace("right");
    assert_eq!(left.proposals, right.proposals);
    assert_eq!(left.memory_contents, right.memory_contents);

    assert_eq!(
        left.second_apply,
        Some(ApplyOutcome::AlreadyDecided {
            decision: ProposalDecision::Applied
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn test_reader_never_observes_partial_or_per_session_time_regressions() {
    let fixture = EventFixture::new();
    let writer_handles = (0..WRITER_COUNT)
        .map(|index| spawn_event_stream(fixture.writer.clone(), index, EVENTS_PER_SESSION))
        .collect::<Vec<_>>();
    let reader_store = fixture.store.clone();
    let reader_handle = task::spawn_blocking(move || observe_reader_snapshots(reader_store));

    join_all(writer_handles).await;
    let observed = reader_handle.await.expect("reader task joins");

    assert!(
        observed > 0,
        "reader observed no events during writer contention"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn test_compaction_during_write_preserves_snapshot_plus_post_snapshot_log() {
    let fixture = EventFixture::new();
    for index in 0..EVENTS_PER_SESSION {
        append_event(&fixture.writer, "seed", index).expect("seed appends");
    }
    let writer = spawn_event_stream(fixture.writer.clone(), 99, EVENTS_PER_SESSION);
    sleep(Duration::from_millis(5)).await;
    let report = fixture.run_compaction().await;
    writer
        .await
        .expect("writer task joins")
        .expect("writer succeeds");

    let snapshot_path = report.snapshot_path.expect("snapshot is written");
    let snapshot = crate::events::Snapshot::read(&snapshot_path).expect("snapshot reads");
    let rows = fixture
        .store
        .query_events_after_row_id(0, 100)
        .expect("post-compaction events read");
    let non_marker_rows = rows
        .iter()
        .filter(|row| !row.summary.starts_with("event log snapshot:"))
        .count() as u64;

    assert_eq!(report.events_truncated, snapshot.up_to_event_id as u64);
    assert_eq!(
        report.events_truncated + non_marker_rows,
        2 * EVENTS_PER_SESSION as u64
    );
    assert_unique_row_ids(&rows);
}

struct EventFixture {
    _dir: TempDir,
    store: Arc<EventStore>,
    writer: Arc<EventWriter>,
    snapshot_dir: PathBuf,
}

impl EventFixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let store = Arc::new(EventStore::open(&dir.path().join("events.sqlite")).unwrap());
        let writer = Arc::new(
            EventWriter::new(store.clone(), WORKSPACE.to_string(), 4096)
                .with_flush_policy(FlushPolicy::Sync),
        );
        Self {
            snapshot_dir: dir.path().join("snapshots"),
            _dir: dir,
            store,
            writer,
        }
    }

    async fn run_compaction(&self) -> crate::events::CompactionReport {
        let compactor = crate::events::Compactor::new(
            self.store.clone(),
            self.writer.clone(),
            Arc::new(std::sync::Mutex::new(CodeGraph::new())),
            Arc::new(std::sync::Mutex::new(
                MemoryStore::open_in_memory().unwrap(),
            )),
            crate::events::CompactionConfig {
                interval: Duration::from_secs(60),
                min_events_since_last: 1,
                snapshot_dir: self.snapshot_dir.clone(),
                retain_snapshots: 2,
            },
        );
        task::spawn_blocking(move || compactor.run_once().expect("compaction succeeds"))
            .await
            .expect("compaction task joins")
    }
}

struct TempDbFixture {
    _dir: TempDir,
    memory_path: PathBuf,
}

impl TempDbFixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let memory_path = dir.path().join("memory.sqlite");
        MemoryStore::open(&memory_path).expect("memory schema initializes");
        Self {
            _dir: dir,
            memory_path,
        }
    }
}

#[derive(Debug)]
struct ConsolidationTrace {
    proposals: Vec<String>,
    memory_contents: Vec<String>,
    second_apply: Option<ApplyOutcome>,
}

fn spawn_event_stream(
    writer: Arc<EventWriter>,
    writer_index: usize,
    event_count: usize,
) -> task::JoinHandle<Result<String, crate::events::EventWriteError>> {
    task::spawn_blocking(move || {
        let session = format!("session-{writer_index:02}");
        for event_index in 0..event_count {
            append_event(&writer, &session, event_index)?;
        }
        Ok(session)
    })
}

fn append_event(
    writer: &EventWriter,
    session: &str,
    index: usize,
) -> Result<EventId, crate::events::EventWriteError> {
    let payload = event_payload(session, index);
    writer.append(PartialEnvelope {
        workspace_id: Some(WORKSPACE.to_string()),
        branch: BranchRef {
            name: BRANCH.to_string(),
        },
        session_id: SessionId {
            value: session.to_string(),
        },
        task_id: None,
        actor: Actor::Assistant {
            model: "hardening-test".to_string(),
        },
        kind: payload.kind(),
        references: Vec::new(),
        summary: CompactSummary::new(format!("{session}:{index:03}")).unwrap(),
        payload,
    })
}

fn event_payload(session: &str, index: usize) -> EventPayload {
    match index % 3 {
        0 => EventPayload::ToolCalled(ToolCalledPayload {
            call_id: format!("{session}-{index}"),
            tool_name: "prepare_change".to_string(),
            context_handle_id: None,
            source_event_id: None,
            input_summary: format!("input {index}"),
        }),
        1 => EventPayload::ToolResult(ToolResultPayload {
            call_id: format!("{session}-{}", index - 1),
            tool_name: "prepare_change".to_string(),
            status: ToolResultStatus::Succeeded,
            tool_call_event_id: None,
            output_context_handle_id: None,
            created_memory_ids: Vec::new(),
            output_summary: format!("output {index}"),
        }),
        _ => EventPayload::ContextBundleReturned(ContextBundleReturnedPayload {
            context_handle_id: ContextHandleId {
                workspace_id: WORKSPACE.to_string(),
                session_id: session.to_string(),
                ulid: format!("handle-{session}-{index}"),
            },
            source_event_id: None,
            file_ids: Vec::new(),
            symbol_ids: Vec::new(),
            doc_section_ids: Vec::new(),
            memory_ids: Vec::new(),
            token_estimate: 128,
        }),
    }
}

async fn join_all<T>(handles: Vec<task::JoinHandle<Result<T, impl std::fmt::Debug>>>) -> Vec<T> {
    let mut values = Vec::with_capacity(handles.len());
    for handle in handles {
        values.push(handle.await.expect("task joins").expect("task succeeds"));
    }
    values
}

fn read_session(
    reader: &EventReader,
    session: &str,
    expected: usize,
) -> Vec<crate::events::EventEnvelope> {
    let events = EventQuery::new()
        .session(session)
        .limit(expected)
        .execute(reader)
        .expect("session query succeeds");
    assert_eq!(events.len(), expected, "session event count mismatch");
    events
}

fn read_workspace(reader: &EventReader, expected: usize) -> Vec<crate::events::EventEnvelope> {
    let events = EventQuery::new()
        .workspace(WORKSPACE)
        .branch(BRANCH)
        .limit(expected)
        .execute(reader)
        .expect("workspace query succeeds");
    assert_eq!(events.len(), expected, "workspace event count mismatch");
    events
}

fn assert_session_order(session: &str, events: &[crate::events::EventEnvelope]) {
    let summaries = events
        .iter()
        .map(|event| event.summary.as_str().to_string())
        .collect::<Vec<_>>();
    let expected = (0..events.len())
        .map(|index| format!("{session}:{index:03}"))
        .collect::<Vec<_>>();
    assert_eq!(summaries, expected, "per-session event ordering changed");
}

fn test_memory(index: usize) -> Memory {
    Memory {
        id: String::new(),
        session_id: format!("session-{index}"),
        content: format!("concurrent memory {index}"),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 0.5 + (index as f64 / 100.0).min(0.49),
        linked_symbols: vec![format!("symbol_{index}")],
        linked_files: vec![format!("src/{index}.rs")],
        workspace_id: Some(WORKSPACE.to_string()),
        branch: Some(BRANCH.to_string()),
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

fn seed_link_memory_pair(path: &Path, source_id: &str, target_id: &str) {
    let store = MemoryStore::open(path).expect("memory store opens");
    let mut source = test_memory(0);
    source.id = source_id.to_string();
    let mut target = test_memory(1);
    target.id = target_id.to_string();
    store.store(source).expect("source stores");
    store.store(target).expect("target stores");
}

fn spawn_link_and_touch(
    path: PathBuf,
    index: usize,
) -> task::JoinHandle<Result<(), crate::LatticeError>> {
    task::spawn_blocking(move || {
        let store = MemoryStore::open(&path)?;
        store.insert_memory_link(&MemoryLinkRecord {
            link_id: format!("link-{index:02}"),
            source_memory_id: "source".to_string(),
            target_memory_id: "target".to_string(),
            link_type: "supports".to_string(),
            reason: "parallel link writer".to_string(),
            created_at: index as u64,
            verification_status: "unverified".to_string(),
        })?;
        store.touch_memory("source")
    })
}

fn run_consolidation_trace(label: &str) -> ConsolidationTrace {
    let fixture = ConsolidationFixture::new(label);
    seed_deterministic_event_trace(&fixture.event_writer);
    let mut runtime = fixture.runtime();
    for kind in [
        "episode_summary",
        "procedure_extraction",
        "contradiction_detection",
    ] {
        runtime
            .submit(consolidation_job(kind))
            .expect("job enqueues");
    }
    let proposals = runtime.run_due().expect("jobs run");
    let proposal_json = proposals
        .iter()
        .map(|proposal| {
            json!({
                "kind": proposal.proposal_kind.as_str(),
                "target": proposal.target.memory_id(),
                "proposed": proposal.proposed_state,
                "evidence": proposal.evidence,
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    let first_id = proposals[0].proposal_id.clone();
    runtime
        .decide(
            &first_id,
            ProposalDecision::Applied,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
        )
        .expect("first apply succeeds");
    let second_apply = runtime
        .decide(
            &first_id,
            ProposalDecision::Applied,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
        )
        .expect("second apply succeeds");
    ConsolidationTrace {
        proposals: proposal_json,
        memory_contents: fixture.memory_contents(),
        second_apply,
    }
}

struct ConsolidationFixture {
    _dir: TempDir,
    consolidation_path: PathBuf,
    memory_store: MemoryStore,
    event_writer: EventWriter,
}

impl ConsolidationFixture {
    fn new(label: &str) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let memory_store =
            MemoryStore::open(&dir.path().join(format!("{label}-memory.sqlite"))).unwrap();
        let event_writer = EventWriter::new(
            Arc::new(EventStore::open_in_memory().unwrap()),
            WORKSPACE.to_string(),
            4096,
        );
        Self {
            consolidation_path: dir.path().join(format!("{label}-consolidation.sqlite")),
            _dir: dir,
            memory_store,
            event_writer,
        }
    }

    fn runtime(&self) -> ConsolidationJobRuntime {
        ConsolidationJobRuntime::new(
            Connection::open(&self.consolidation_path).unwrap(),
            ConsolidationConfig::default(),
        )
        .unwrap()
    }

    fn memory_contents(&self) -> Vec<String> {
        let mut contents = self
            .memory_store
            .list_all()
            .expect("memories list")
            .into_iter()
            .map(|memory| memory.content)
            .collect::<Vec<_>>();
        contents.sort();
        contents
    }
}

fn seed_deterministic_event_trace(writer: &EventWriter) {
    for index in 0..EVENTS_PER_SESSION {
        append_event(writer, "replay-session", index).expect("trace appends");
    }
}

fn consolidation_job(kind: &str) -> ConsolidationJobSpec {
    ConsolidationJobSpec {
        job_id: format!("job-{kind}"),
        workspace_id: WORKSPACE.to_string(),
        kind: kind.to_string(),
        mode: ConsolidationJobMode::Replay,
        proposal: Some(PendingProposalSpec {
            proposal_id: format!("proposal-{kind}"),
            target_memory_id: None,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: empty_state(),
            proposed_state: serde_json::to_value(consolidation_memory(kind)).unwrap(),
            evidence: json!({ "event_trace": "replay-session", "job_kind": kind }),
            provenance: None,
        }),
    }
}

fn consolidation_memory(kind: &str) -> Memory {
    Memory {
        id: format!("memory-{kind}"),
        session_id: "replay-session".to_string(),
        content: format!("deterministic {kind} memory"),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 1.0,
        linked_symbols: Vec::new(),
        linked_files: Vec::new(),
        workspace_id: Some(WORKSPACE.to_string()),
        branch: Some(BRANCH.to_string()),
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

fn observe_reader_snapshots(store: Arc<EventStore>) -> usize {
    let reader = EventReader::new(store);
    let mut observed = 0;
    for _ in 0..64 {
        let events = EventQuery::new()
            .workspace(WORKSPACE)
            .branch(BRANCH)
            .limit(10_000)
            .execute(&reader)
            .expect("reader snapshot succeeds");
        assert_reader_snapshot(&events);
        observed = observed.max(events.len());
        if observed == WRITER_COUNT * EVENTS_PER_SESSION {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    observed
}

fn assert_reader_snapshot(events: &[crate::events::EventEnvelope]) {
    for event in events {
        assert!(
            !event.event_id.ulid.is_empty(),
            "reader observed partial event id"
        );
        assert!(
            !event.summary.as_str().is_empty(),
            "reader observed partial summary"
        );
    }
    for session in unique_sessions(events) {
        let mut previous: Option<i64> = None;
        for event in events
            .iter()
            .filter(|event| event.session_id.value == session)
        {
            let timestamp = event.timestamp.unix_seconds();
            if let Some(previous_timestamp) = previous {
                assert!(
                    previous_timestamp <= timestamp,
                    "session timestamp regressed"
                );
            }
            previous = Some(timestamp);
        }
    }
}

fn unique_sessions(events: &[crate::events::EventEnvelope]) -> HashSet<String> {
    events
        .iter()
        .map(|event| event.session_id.value.clone())
        .collect()
}

fn assert_unique_row_ids(rows: &[crate::events::EventEnvelopeRow]) {
    let mut row_ids = HashSet::new();
    for row in rows {
        assert!(row_ids.insert(row.event_id), "duplicate event row observed");
    }
}
