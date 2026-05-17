use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::Value;
use tempfile::tempdir;

use super::{
    ConsolidationConfig, ConsolidationJobMode, ConsolidationJobRuntime, EpisodeOutcome,
    SessionConsolidationConfig, SessionConsolidationOutcome, SessionConsolidator,
};
use crate::events::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, EventKind, EventPayload,
    EventReader, EventStore, EventWriter, FlushPolicy, PartialEnvelope, SessionId, TaskId,
    ToolCalledPayload, WorkflowSucceededPayload,
};
use crate::identity::FileId;
use crate::memory::MemoryStore;

#[test]
fn small_task_slice_emits_create_memory_proposal_with_deterministic_summary() {
    let fixture = FileFixture::new();
    fixture.append_small_task("task-small");
    let mut consolidator = fixture.consolidator();
    let outcome = consolidator
        .on_task_complete(
            "workspace-main",
            &task_id("task-small"),
            EpisodeOutcome::Success,
        )
        .expect("task consolidates");

    match outcome {
        SessionConsolidationOutcome::Proposed {
            proposal_kind,
            proposal_id,
            ..
        } => {
            assert_eq!(proposal_kind.as_str(), "create_memory");
            let proposal = load_proposal(&fixture.memory_db_path, &proposal_id);
            assert_eq!(proposal.0, "create_memory");
            assert_eq!(
                proposal
                    .1
                    .get("summary_text")
                    .and_then(Value::as_str)
                    .expect("summary text present"),
                "Task task-small: 1 tools, 1 files, outcome=success"
            );
            assert_eq!(
                proposal
                    .1
                    .get("tools_used")
                    .and_then(Value::as_array)
                    .expect("tools array present")
                    .len(),
                1
            );
        }
        other => panic!("expected proposed outcome, got {other:?}"),
    }
}

#[test]
fn oversized_task_slice_redirects_to_background_mode() {
    let fixture = FileFixture::new();
    fixture.append_large_task("task-large", 250);
    let mut consolidator = fixture.consolidator();
    let outcome = consolidator
        .on_task_complete(
            "workspace-main",
            &task_id("task-large"),
            EpisodeOutcome::Success,
        )
        .expect("task redirect succeeds");

    match outcome {
        SessionConsolidationOutcome::RedirectedToBackground { job_id, .. } => {
            let (mode, status) = load_job(&fixture.memory_db_path, &job_id);
            assert_eq!(mode, ConsolidationJobMode::Background.as_str());
            assert_eq!(status, "queued");
        }
        other => panic!("expected redirect outcome, got {other:?}"),
    }
}

#[test]
fn session_consolidation_never_mutates_memories_directly() {
    let fixture = FileFixture::new();
    fixture.append_small_task("task-no-mutate");
    let mut consolidator = fixture.consolidator();
    assert_eq!(
        consolidator
            .memory_store()
            .list_all()
            .expect("memories list")
            .len(),
        0
    );

    consolidator
        .on_task_complete(
            "workspace-main",
            &task_id("task-no-mutate"),
            EpisodeOutcome::Success,
        )
        .expect("task consolidates");

    assert_eq!(
        consolidator
            .memory_store()
            .list_all()
            .expect("memories list")
            .len(),
        0
    );
}

#[test]
#[ignore = "p99 timing assertion; run via --include-ignored so it is not affected by parallel test CPU contention"]
fn session_consolidation_hot_path_stays_under_five_milliseconds_p99() {
    let mut fixture = BudgetFixture::new();
    let mut samples = Vec::new();

    for index in 0..16 {
        let task = format!("task-budget-warmup-{index}");
        fixture.append_small_task(&task);
        fixture
            .consolidator
            .on_task_complete("workspace-main", &task_id(&task), EpisodeOutcome::Success)
            .expect("warmup task consolidates");
    }

    for index in 0..64 {
        let task = format!("task-budget-{index}");
        fixture.append_small_task(&task);
        let started = Instant::now();
        fixture
            .consolidator
            .on_task_complete("workspace-main", &task_id(&task), EpisodeOutcome::Success)
            .expect("task consolidates");
        samples.push(started.elapsed());
    }

    samples.sort();
    let p99 = percentile_99(&samples);
    debug_assert!(p99 <= Duration::from_millis(5));
    assert!(p99 <= Duration::from_millis(5), "p99 was {:?}", p99);
}

#[test]
fn session_module_forbids_llm_references() {
    let source = std::fs::read_to_string(session_source_path()).expect("session source reads");
    assert!(!source.contains("consolidation::llm"));
    assert!(!source.contains("llm::"));
}

struct FileFixture {
    _dir: tempfile::TempDir,
    memory_db_path: PathBuf,
    event_store: Arc<EventStore>,
    writer: EventWriter,
}

impl FileFixture {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let memory_db_path = dir.path().join("memories.sqlite");
        let event_store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
        let writer = EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096)
            .with_flush_policy(FlushPolicy::Sync);
        Self {
            _dir: dir,
            memory_db_path,
            event_store,
            writer,
        }
    }

    fn consolidator(&self) -> SessionConsolidator {
        SessionConsolidator::new(
            EventReader::new(self.event_store.clone()),
            MemoryStore::open(&self.memory_db_path).expect("memory store opens"),
            ConsolidationJobRuntime::new(
                Connection::open(&self.memory_db_path).expect("consolidation db opens"),
                ConsolidationConfig::default(),
            )
            .expect("runtime opens"),
            SessionConsolidationConfig::default(),
        )
    }

    fn append_small_task(&self, task_id_value: &str) {
        let task_id = task_id(task_id_value);
        self.append_event(
            task_id.clone(),
            EventKind::AssistantTaskStarted,
            EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
                context_handle_id: None,
                seed_event_ids: Vec::new(),
                initial_memory_ids: Vec::new(),
                objective: "small task".to_string(),
            }),
            Vec::new(),
            "Task started",
        );
        self.append_event(
            task_id.clone(),
            EventKind::ToolCalled,
            EventPayload::ToolCalled(ToolCalledPayload {
                call_id: format!("call-{task_id_value}"),
                tool_name: "get_context_capsule".to_string(),
                context_handle_id: None,
                source_event_id: None,
                input_summary: "query=small".to_string(),
            }),
            vec![crate::events::StableRef::FileRef(file_id("src/lib.rs"))],
            "Tool called",
        );
        self.append_event(
            task_id,
            EventKind::WorkflowSucceeded,
            EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
                workflow_name: "prepare_change".to_string(),
                terminal_event_id: None,
                output_context_handle_id: None,
                memory_ids: Vec::new(),
                result_summary: "success".to_string(),
            }),
            Vec::new(),
            "Workflow succeeded",
        );
    }

    fn append_large_task(&self, task_id_value: &str, event_count: usize) {
        let task_id = task_id(task_id_value);
        self.append_event(
            task_id.clone(),
            EventKind::AssistantTaskStarted,
            EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
                context_handle_id: None,
                seed_event_ids: Vec::new(),
                initial_memory_ids: Vec::new(),
                objective: "large task".to_string(),
            }),
            Vec::new(),
            "Task started",
        );
        for index in 0..event_count.saturating_sub(2) {
            self.append_event(
                task_id.clone(),
                EventKind::ToolCalled,
                EventPayload::ToolCalled(ToolCalledPayload {
                    call_id: format!("call-{task_id_value}-{index}"),
                    tool_name: "get_context_capsule".to_string(),
                    context_handle_id: None,
                    source_event_id: None,
                    input_summary: "query=scan".to_string(),
                }),
                vec![crate::events::StableRef::FileRef(file_id(&format!(
                    "src/file-{index}.rs"
                )))],
                "Tool called",
            );
        }
        self.append_event(
            task_id,
            EventKind::WorkflowSucceeded,
            EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
                workflow_name: "prepare_change".to_string(),
                terminal_event_id: None,
                output_context_handle_id: None,
                memory_ids: Vec::new(),
                result_summary: "success".to_string(),
            }),
            Vec::new(),
            "Workflow succeeded",
        );
    }

    fn append_event(
        &self,
        task_id: TaskId,
        kind: EventKind,
        payload: EventPayload,
        references: Vec<crate::events::StableRef>,
        summary: &str,
    ) {
        self.writer
            .append(PartialEnvelope {
                workspace_id: None,
                branch: BranchRef {
                    name: "main".to_string(),
                },
                session_id: SessionId {
                    value: "session-main".to_string(),
                },
                task_id: Some(task_id),
                actor: Actor::Daemon,
                kind,
                references,
                summary: CompactSummary::new(summary).expect("summary fits"),
                payload,
            })
            .expect("event appends");
    }
}

struct BudgetFixture {
    writer: EventWriter,
    consolidator: SessionConsolidator,
}

impl BudgetFixture {
    fn new() -> Self {
        let event_store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
        let writer = EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096)
            .with_flush_policy(FlushPolicy::Sync);
        let consolidator = SessionConsolidator::new(
            EventReader::new(event_store.clone()),
            MemoryStore::open_in_memory().expect("memory store opens"),
            ConsolidationJobRuntime::new(
                Connection::open_in_memory().expect("consolidation db opens"),
                ConsolidationConfig::default(),
            )
            .expect("runtime opens"),
            SessionConsolidationConfig::default(),
        );
        Self {
            writer,
            consolidator,
        }
    }

    fn append_small_task(&self, task_id_value: &str) {
        let task_id = task_id(task_id_value);
        self.writer
            .append(PartialEnvelope {
                workspace_id: None,
                branch: BranchRef {
                    name: "main".to_string(),
                },
                session_id: SessionId {
                    value: "session-budget".to_string(),
                },
                task_id: Some(task_id.clone()),
                actor: Actor::Daemon,
                kind: EventKind::AssistantTaskStarted,
                references: Vec::new(),
                summary: CompactSummary::new("Task started").expect("summary fits"),
                payload: EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
                    context_handle_id: None,
                    seed_event_ids: Vec::new(),
                    initial_memory_ids: Vec::new(),
                    objective: "budget".to_string(),
                }),
            })
            .expect("event appends");
        self.writer
            .append(PartialEnvelope {
                workspace_id: None,
                branch: BranchRef {
                    name: "main".to_string(),
                },
                session_id: SessionId {
                    value: "session-budget".to_string(),
                },
                task_id: Some(task_id.clone()),
                actor: Actor::Daemon,
                kind: EventKind::ToolCalled,
                references: vec![crate::events::StableRef::FileRef(file_id("src/lib.rs"))],
                summary: CompactSummary::new("Tool called").expect("summary fits"),
                payload: EventPayload::ToolCalled(ToolCalledPayload {
                    call_id: format!("call-{task_id_value}"),
                    tool_name: "get_context_capsule".to_string(),
                    context_handle_id: None,
                    source_event_id: None,
                    input_summary: "budget".to_string(),
                }),
            })
            .expect("event appends");
        self.writer
            .append(PartialEnvelope {
                workspace_id: None,
                branch: BranchRef {
                    name: "main".to_string(),
                },
                session_id: SessionId {
                    value: "session-budget".to_string(),
                },
                task_id: Some(task_id),
                actor: Actor::Daemon,
                kind: EventKind::WorkflowSucceeded,
                references: Vec::new(),
                summary: CompactSummary::new("Workflow succeeded").expect("summary fits"),
                payload: EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
                    workflow_name: "prepare_change".to_string(),
                    terminal_event_id: None,
                    output_context_handle_id: None,
                    memory_ids: Vec::new(),
                    result_summary: "success".to_string(),
                }),
            })
            .expect("event appends");
    }
}

fn load_proposal(path: &Path, proposal_id: &str) -> (String, Value) {
    let conn = Connection::open(path).expect("db opens");
    conn.query_row(
        "SELECT proposal_kind, proposed_state FROM consolidation_proposals WHERE proposal_id = ?1",
        [proposal_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .map(|(kind, proposed_state)| {
        (
            kind,
            serde_json::from_str::<Value>(&proposed_state).expect("proposal json parses"),
        )
    })
    .expect("proposal loads")
}

fn load_job(path: &Path, job_id: &str) -> (String, String) {
    let conn = Connection::open(path).expect("db opens");
    conn.query_row(
        "SELECT mode, status FROM consolidation_jobs WHERE job_id = ?1",
        [job_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("job loads")
}

fn percentile_99(samples: &[Duration]) -> Duration {
    let index = ((samples.len() as f64 * 0.99).ceil() as usize).saturating_sub(1);
    samples[index]
}

fn file_id(path: &str) -> FileId {
    FileId {
        workspace_id: "workspace-main".to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "hash".to_string(),
    }
}

fn task_id(value: &str) -> TaskId {
    TaskId {
        value: value.to_string(),
    }
}

fn session_source_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("consolidation")
        .join("session.rs")
}
