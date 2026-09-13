use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::consolidation::ConsolidationConfig;
use crate::events::{
    Actor, BranchRef, CompactSummary, DiagnosticObservedPayload, DiagnosticSeverity, EventKind,
    EventPayload, EventQuery, EventReader, EventStore, EventWriter, PartialEnvelope, QueryOrder,
    SessionId, TaskId, ToolCalledPayload, ToolResultPayload, ToolResultStatus,
    WorkflowSucceededPayload,
};
use crate::identity::{EventId, FileId};
use crate::memory::model::MemoryAssertionType;
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus};

#[test]
fn well_formed_episode_response_enqueues_proposal_without_mutating_memory() {
    let mut fixture = Fixture::new();
    fixture.driver.push_ok(episode_response());
    let slice = fixture.task_slice("task-episode");
    let mut services = fixture.services();

    let proposal =
        EpisodeSummaryJob::run(&background_ctx(), &mut services, &slice).expect("job succeeds");

    assert!(proposal.is_some());
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 0);
    assert_eq!(proposal_count(&fixture.memory_store), 1);
    assert_eq!(fixture.driver.call_count(), 1);
}

#[test]
fn malformed_json_emits_failure_event_and_no_proposal_for_every_job() {
    for scenario in JobScenario::all() {
        let mut fixture = Fixture::new();
        fixture.driver.push_ok("{not json".to_string());
        let result = scenario.run(&mut fixture, background_ctx());

        assert!(matches!(result, Err(LlmJobError::MalformedResponse(_))));
        assert_eq!(proposal_count(&fixture.memory_store), 0);
        assert_eq!(failure_event_count(&fixture), 1);
    }
}

#[test]
fn driver_error_emits_failure_event_and_no_proposal_for_every_job() {
    for scenario in JobScenario::all() {
        let mut fixture = Fixture::new();
        fixture
            .driver
            .push_error(LlmDriverError::Unavailable("offline".to_string()));
        let result = scenario.run(&mut fixture, background_ctx());

        assert!(matches!(result, Err(LlmJobError::DriverError(_))));
        assert_eq!(proposal_count(&fixture.memory_store), 0);
        assert_eq!(failure_event_count(&fixture), 1);
    }
}

#[test]
fn synchronous_post_task_mode_is_forbidden_before_driver_call_for_every_job() {
    for scenario in JobScenario::all() {
        let mut fixture = Fixture::new();
        fixture.driver.push_ok(scenario.success_response());
        let result = scenario.run(&mut fixture, synchronous_ctx());

        assert!(matches!(result, Err(LlmJobError::ForbiddenOnHotPath(_))));
        assert_eq!(fixture.driver.call_count(), 0);
        assert_eq!(proposal_count(&fixture.memory_store), 0);
    }
}

#[test]
fn structured_outputs_round_trip_into_expected_memory_states() {
    for scenario in JobScenario::all() {
        let mut fixture = Fixture::new();
        fixture.driver.push_ok(scenario.success_response());
        let proposal = scenario
            .run(&mut fixture, background_ctx())
            .expect("job succeeds")
            .expect("proposal emitted");
        let state: ConsolidationMemoryState =
            serde_json::from_value(proposal.proposed_state).expect("state round-trips");

        match scenario {
            JobScenario::Episode => {
                assert_eq!(state.memory.memory_type, MemoryType::Pattern);
                assert_eq!(
                    state.structured_fields.assertion_type,
                    MemoryAssertionType::WorkflowOutcome
                );
            }
            JobScenario::Procedure => {
                assert_eq!(state.memory.memory_type, MemoryType::Pattern);
                assert_eq!(
                    state.structured_fields.assertion_type,
                    MemoryAssertionType::Pattern
                );
            }
            JobScenario::Contradiction => {
                assert_eq!(
                    state.structured_fields.verification_status,
                    MemoryVerificationStatus::Contradicted
                );
                assert_eq!(state.memory_links.len(), 1);
            }
            JobScenario::FailurePattern => {
                assert_eq!(state.memory.memory_type, MemoryType::AntiPattern);
                assert_eq!(
                    state.structured_fields.assertion_type,
                    MemoryAssertionType::AntiPattern
                );
            }
        }
    }
}

#[derive(Clone, Copy)]
enum JobScenario {
    Episode,
    Procedure,
    Contradiction,
    FailurePattern,
}

impl JobScenario {
    fn all() -> Vec<Self> {
        vec![
            Self::Episode,
            Self::Procedure,
            Self::Contradiction,
            Self::FailurePattern,
        ]
    }

    fn run(
        self,
        fixture: &mut Fixture,
        ctx: LlmJobContext,
    ) -> Result<Option<crate::consolidation::ConsolidationProposal>, LlmJobError> {
        match self {
            Self::Episode => {
                let slice = fixture.task_slice("task-episode");
                let mut services = fixture.services();
                EpisodeSummaryJob::run(&ctx, &mut services, &slice)
            }
            Self::Procedure => {
                let occurrences = fixture.workflow_occurrences();
                let mut services = fixture.services();
                ProcedureExtractionJob::run(&ctx, &mut services, "workflow-build", &occurrences)
            }
            Self::Contradiction => {
                let pair = fixture.contradiction_pair();
                let mut services = fixture.services();
                ContradictionDetectionJob::run(&ctx, &mut services, &pair)
            }
            Self::FailurePattern => {
                let cluster = fixture.diagnostic_cluster();
                let mut services = fixture.services();
                FailurePatternJob::run(&ctx, &mut services, &cluster)
            }
        }
    }

    fn success_response(self) -> String {
        match self {
            Self::Episode => episode_response(),
            Self::Procedure => procedure_response(),
            Self::Contradiction => contradiction_response(),
            Self::FailurePattern => failure_pattern_response(),
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    consolidation_db: DbPath,
    memory_store: MemoryStore,
    runtime: ConsolidationJobRuntime,
    event_store: Arc<EventStore>,
    event_writer: EventWriter,
    driver: FakeDriver,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let memory_store = MemoryStore::open(&dir.path().join("memory.sqlite")).unwrap();
        let consolidation_db = DbPath(dir.path().join("consolidation.sqlite"));
        let runtime = ConsolidationJobRuntime::new(
            Connection::open(consolidation_db.path()).unwrap(),
            ConsolidationConfig {
                max_queue_depth: 32,
                llm_budget_catalog: None,
            },
        )
        .unwrap();
        let event_store = Arc::new(EventStore::open_in_memory().unwrap());
        let event_writer =
            EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096);
        Self {
            _dir: dir,
            consolidation_db,
            memory_store,
            runtime,
            event_store,
            event_writer,
            driver: FakeDriver::new(),
        }
    }

    fn services(&mut self) -> LlmJobServices<'_> {
        LlmJobServices {
            driver: &self.driver,
            runtime: &mut self.runtime,
            memory_store: &self.memory_store,
            event_writer: &self.event_writer,
            authority: &crate::consolidation::EvolutionAuthority {
                repository_id: "workspace-main",
                checkout_id: "checkout-main",
                branch: "main",
            },
        }
    }

    fn task_slice(&self, task_id: &str) -> Vec<crate::events::EventEnvelope> {
        self.append_task_event(task_id, tool_called("read", "inspect sources"));
        self.append_task_event(task_id, tool_result("read", "loaded source"));
        self.append_task_event(task_id, workflow_succeeded("implemented change"));
        EventQuery::new()
            .workspace("workspace-main")
            .task(task_id)
            .order(QueryOrder::OldestFirst)
            .execute(&EventReader::new(self.event_store.clone()))
            .unwrap()
    }

    fn workflow_occurrences(&self) -> Vec<WorkflowOccurrence> {
        (0..3)
            .map(|index| WorkflowOccurrence {
                occurrence_id: format!("occurrence-{index}"),
                events: self.task_slice(&format!("workflow-{index}")),
            })
            .collect()
    }

    fn contradiction_pair(&self) -> ContradictionCandidatePair {
        let first = memory("mem-a", "The build uses cargo check for verification.");
        let second = memory("mem-b", "The build never uses cargo check.");
        self.memory_store.store(first.clone()).unwrap();
        self.memory_store.store(second.clone()).unwrap();
        ContradictionCandidatePair {
            first,
            second,
            deterministic_reason: "overlapping build verification assertions".to_string(),
        }
    }

    fn diagnostic_cluster(&self) -> DiagnosticCluster {
        self.append_task_event("diag-task", diagnostic("diag-a", "borrow checker failure"));
        self.append_task_event("diag-task", diagnostic("diag-b", "borrow checker failure"));
        let events = EventQuery::new()
            .workspace("workspace-main")
            .task("diag-task")
            .order(QueryOrder::OldestFirst)
            .execute(&EventReader::new(self.event_store.clone()))
            .unwrap();
        DiagnosticCluster {
            cluster_id: "borrow-checker".to_string(),
            diagnostics: events,
        }
    }

    fn append_task_event(&self, task_id: &str, payload: EventPayload) -> EventId {
        let kind = payload.kind();
        self.event_writer
            .append(PartialEnvelope {
                workspace_id: Some("workspace-main".to_string()),
                branch: BranchRef {
                    name: "main".to_string(),
                },
                session_id: SessionId {
                    value: "session-main".to_string(),
                },
                task_id: Some(TaskId {
                    value: task_id.to_string(),
                }),
                actor: Actor::Daemon,
                kind,
                references: Vec::new(),
                summary: CompactSummary::new(kind.as_str()).unwrap(),
                payload,
            })
            .unwrap()
    }
}

#[derive(Clone)]
struct FakeDriver {
    responses: Arc<Mutex<Vec<Result<String, LlmDriverError>>>>,
    calls: Arc<Mutex<Vec<LlmRequest>>>,
}

impl FakeDriver {
    fn new() -> Self {
        Self {
            responses: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_ok(&self, response: String) {
        self.responses.lock().unwrap().push(Ok(response));
    }

    fn push_error(&self, error: LlmDriverError) {
        self.responses.lock().unwrap().push(Err(error));
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl LlmDriver for FakeDriver {
    fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
        self.calls.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Err(LlmDriverError::Failed("missing fake response".to_string())))
            .map(|content| LlmResponse { content })
    }

    fn name(&self) -> &str {
        "fake-llm"
    }
}

struct DbPath(std::path::PathBuf);

impl DbPath {
    fn path(&self) -> &std::path::Path {
        self.0.as_path()
    }
}

fn background_ctx() -> LlmJobContext {
    LlmJobContext {
        workspace_id: "workspace-main".to_string(),
        mode: ConsolidationMode::Background,
        budget_catalog: BudgetCatalog::default(),
    }
}

fn synchronous_ctx() -> LlmJobContext {
    LlmJobContext {
        workspace_id: "workspace-main".to_string(),
        mode: ConsolidationMode::SynchronousPostTask,
        budget_catalog: BudgetCatalog::default(),
    }
}

fn tool_called(tool: &str, summary: &str) -> EventPayload {
    EventPayload::ToolCalled(ToolCalledPayload {
        call_id: format!("call-{tool}"),
        tool_name: tool.to_string(),
        context_handle_id: None,
        source_event_id: None,
        input_summary: summary.to_string(),
    })
}

fn tool_result(tool: &str, summary: &str) -> EventPayload {
    EventPayload::ToolResult(ToolResultPayload {
        call_id: format!("call-{tool}"),
        tool_name: tool.to_string(),
        status: ToolResultStatus::Succeeded,
        tool_call_event_id: None,
        output_context_handle_id: None,
        created_memory_ids: Vec::new(),
        output_summary: summary.to_string(),
    })
}

fn workflow_succeeded(summary: &str) -> EventPayload {
    EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
        workflow_name: "workflow-build".to_string(),
        terminal_event_id: None,
        output_context_handle_id: None,
        memory_ids: Vec::new(),
        result_summary: summary.to_string(),
    })
}

fn diagnostic(id: &str, message: &str) -> EventPayload {
    EventPayload::DiagnosticObserved(DiagnosticObservedPayload {
        diagnostic_id: id.to_string(),
        source_event_id: None,
        file_id: FileId {
            workspace_id: "workspace-main".to_string(),
            repo_relative_path: "src/lib.rs".to_string(),
            content_hash: "abcdef12".to_string(),
        },
        symbol_id: None,
        severity: DiagnosticSeverity::Error,
        message: message.to_string(),
    })
}

fn memory(id: &str, content: &str) -> Memory {
    Memory {
        id: id.to_string(),
        session_id: "session-main".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Session,
        confidence: 0.9,
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

fn proposal_count(store: &MemoryStore) -> i64 {
    store
        .with_connection(|conn| {
            conn.query_row("SELECT COUNT(*) FROM consolidation_proposals", [], |row| {
                row.get(0)
            })
            .map_err(|error| crate::LatticeError::Storage(error.to_string()))
        })
        .unwrap()
}

fn failure_event_count(fixture: &Fixture) -> usize {
    EventQuery::new()
        .workspace("workspace-main")
        .branch("main")
        .kind(EventKind::ConsolidationFailed)
        .execute(&EventReader::new(fixture.event_store.clone()))
        .unwrap()
        .len()
}

fn episode_response() -> String {
    json!({
        "summary": "Implemented the build workflow and verified it.",
        "outcome": "success",
        "salient_facts": ["cargo test passed"],
        "linked_files": ["src/lib.rs"],
        "linked_symbols": ["build_workflow"],
        "confidence": 0.91
    })
    .to_string()
}

fn procedure_response() -> String {
    json!({
        "title": "Build verification procedure",
        "steps": ["Inspect code", "Run cargo test", "Record result"],
        "preconditions": ["Rust workspace is present"],
        "postconditions": ["Tests pass"],
        "tools_used": ["cargo test"],
        "confidence": 0.88
    })
    .to_string()
}

fn contradiction_response() -> String {
    json!({
        "is_contradiction": true,
        "contradicted_memory_id": "mem-b",
        "contradicting_memory_id": "mem-a",
        "rationale": "One asserts cargo check is never used while the other says it is used."
    })
    .to_string()
}

fn failure_pattern_response() -> String {
    json!({
        "summary": "Repeated borrow checker failures after shared state edits.",
        "recurrence_signal": "Two diagnostics share the same compiler failure shape.",
        "likely_causes": ["mutable aliasing in shared state"],
        "recovery_steps": ["isolate ownership", "add regression test"],
        "confidence": 0.84
    })
    .to_string()
}
