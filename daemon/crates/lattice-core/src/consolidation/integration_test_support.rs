use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use tracing_subscriber::fmt::MakeWriter;

use super::llm::{
    BudgetCatalog, ConsolidationMode, EpisodeSummaryJob, LlmDriver, LlmDriverError, LlmJobContext,
    LlmJobServices, LlmProvenance, LlmRequest, LlmResponse,
};
use super::proposal::state_hash_from_json;
use super::*;
use crate::events::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, EventEnvelope, EventKind,
    EventPayload, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy, PartialEnvelope,
    QueryOrder, SessionId, TaskId, ToolCalledPayload, WorkflowSucceededPayload,
};
use crate::identity::OperatorId;
use crate::memory::{
    Memory, MemoryLinkRecord, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};

pub(crate) struct ConsolidationHarness {
    _dir: tempfile::TempDir,
    consolidation_db: PathBuf,
    memory_db: PathBuf,
    memory_store: MemoryStore,
    event_store: Arc<EventStore>,
    event_reader: EventReader,
    pub(crate) event_writer: EventWriter,
    pub(crate) driver: FakeDriver,
    clock: FixedReplayClock,
}

impl ConsolidationHarness {
    pub(crate) fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let event_store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
        let memory_db = dir.path().join("memory.sqlite");
        Self {
            consolidation_db: dir.path().join("consolidation.sqlite"),
            memory_store: MemoryStore::open(&memory_db).expect("memory store opens"),
            memory_db,
            event_reader: EventReader::new(event_store.clone()),
            event_writer: EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096)
                .with_flush_policy(FlushPolicy::Sync),
            event_store,
            driver: FakeDriver::new(),
            clock: FixedReplayClock::default(),
            _dir: dir,
        }
    }

    pub(crate) fn session_consolidator(&self) -> SessionConsolidator {
        SessionConsolidator::new(
            EventReader::new(self.event_store.clone()),
            MemoryStore::open(&self.memory_db).expect("memory store reopens"),
            self.runtime(32),
            SessionConsolidationConfig::default(),
        )
    }

    pub(crate) fn run_episode_job(
        &mut self,
        ctx: LlmJobContext,
        slice: &[EventEnvelope],
    ) -> Result<Option<ConsolidationProposal>, super::llm::LlmJobError> {
        let mut runtime = self.runtime(32);
        let mut services = LlmJobServices {
            driver: &self.driver,
            runtime: &mut runtime,
            memory_store: &self.memory_store,
            event_writer: &self.event_writer,
        };
        EpisodeSummaryJob::run(&ctx, &mut services, slice)
    }

    pub(crate) fn runtime(&self, max_queue_depth: usize) -> ConsolidationJobRuntime {
        ConsolidationJobRuntime::new(
            self.conn(),
            ConsolidationConfig {
                max_queue_depth,
                llm_budget_catalog: None,
            },
        )
        .expect("runtime opens")
    }

    pub(crate) fn llm_queue(&self, max_depth: usize) -> BoundedJobQueue {
        let conn = self.conn();
        initialize_schema(&conn).expect("schema initializes");
        let conn = Arc::new(Mutex::new(conn));
        BoundedJobQueue::new(max_depth.saturating_add(4), conn).with_per_kind_max_depth(max_depth)
    }

    pub(crate) fn run_supersession_scan(&self) -> String {
        let mut runtime = self.runtime(32);
        let mut scanner = SupersessionCandidates::new(&self.memory_store, &mut runtime);
        scanner.scan("workspace-main").expect("scan succeeds");
        self.only_proposal_id()
    }

    pub(crate) fn execute_ready(&self, job: ConsolidationJobSpec) -> String {
        let mut runtime = self.runtime(32);
        runtime.submit(job).expect("job submits");
        runtime
            .execute_ready(
                &self.memory_store,
                &self.event_writer,
                &self.operator("auto-policy"),
            )
            .expect("job executes");
        self.only_proposal_id()
    }

    pub(crate) fn apply_proposal(&self, proposal_id: &str) -> ApplyOutcome {
        let conn = self.conn();
        self.load_proposal(proposal_id)
            .apply(
                &conn,
                &self.memory_store,
                &self.event_writer,
                "operator",
                None,
            )
            .expect("proposal applies")
    }

    pub(crate) fn reverse_proposal(&self, proposal_id: &str) -> ReverseOutcome {
        let conn = self.conn();
        self.replay_driver(&conn)
            .reverse(proposal_id)
            .expect("reverse succeeds")
    }

    pub(crate) fn replay_driver_with_cache<'a>(
        &'a self,
        cache: HashMap<[u8; 32], Vec<u8>>,
        driver: &'a dyn LlmDriver,
    ) -> ReplayDriver<'a> {
        let conn = Box::leak(Box::new(self.conn()));
        ReplayDriver::new(
            &self.event_reader,
            self.event_store.clone(),
            conn,
            &self.memory_store,
            &self.event_writer,
            &self.clock,
        )
        .with_cached_responses(cache)
        .with_live_llm_driver(driver)
    }

    pub(crate) fn apply_replay_mix(&self) -> HashMap<[u8; 32], Vec<u8>> {
        let mut runtime = self.runtime(64);
        let mut cache = HashMap::new();
        let mut jobs = vec![
            create_job(
                "job-1",
                "proposal-1",
                memory_state("mem-a", "memory a", MemoryScope::Session),
                None,
            ),
            create_job(
                "job-2",
                "proposal-2",
                memory_state("mem-b", "memory b", MemoryScope::Branch),
                None,
            ),
            create_job(
                "job-3",
                "proposal-3",
                memory_state("mem-c", "memory c", MemoryScope::Session),
                None,
            ),
            create_job(
                "job-4",
                "proposal-4",
                memory_state("mem-d", "memory d", MemoryScope::Branch),
                None,
            ),
        ];
        jobs.extend(llm_create_jobs(&mut cache));
        for job in jobs {
            runtime.submit(job).expect("job submits");
        }
        runtime.run_due().expect("jobs run");
        apply_pending(
            runtime,
            &self.memory_store,
            &self.event_writer,
            &self.proposal_ids(),
        );

        let refresh_job = refresh_job("job-8", "proposal-8", &self.capture_state("mem-a"), 88);
        let update_job = update_job(
            "job-9",
            "proposal-9",
            &self.capture_state("mem-b"),
            "memory b updated",
        );
        let supersede_job = supersede_job(
            "job-10",
            "proposal-10",
            &self.capture_state("mem-c"),
            &self.capture_state("mem-d"),
        );
        for job in [refresh_job, update_job, supersede_job] {
            let mut runtime = self.runtime(64);
            runtime.submit(job).expect("job submits");
            runtime.run_due().expect("job runs");
            apply_pending(
                runtime,
                &self.memory_store,
                &self.event_writer,
                &self.proposal_ids(),
            );
        }
        cache
    }

    pub(crate) fn clear_memory_tables(&self) {
        self.memory_store.clear_all().expect("memory store clears");
    }

    pub(crate) fn seed_memory(
        &self,
        id: &str,
        content: &str,
        scope: MemoryScope,
        created_at: u64,
        linked_files: Vec<&str>,
        linked_symbols: Vec<&str>,
    ) -> String {
        let memory = Memory {
            id: id.to_string(),
            session_id: "session-main".to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Observation,
            scope,
            confidence: 0.9,
            linked_symbols: linked_symbols.into_iter().map(str::to_string).collect(),
            linked_files: linked_files.into_iter().map(str::to_string).collect(),
            workspace_id: Some("workspace-main".to_string()),
            branch: Some("main".to_string()),
            scope_organization_id: None,
            refresh_key: Some(format!("refresh-{id}")),
            source_query: Some("integration fixture".to_string()),
            created_at,
            last_accessed: created_at,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: crate::memory::MemoryVerificationStatus::Unverified,
        };
        let stored_id = self.memory_store.store(memory).expect("memory stores");
        let mut fields = self
            .memory_store
            .get_structured_fields(&stored_id)
            .expect("fields load")
            .unwrap_or_default();
        fields.verification_status = MemoryVerificationStatus::Verified;
        self.memory_store
            .update_structured_fields(&stored_id, &fields)
            .expect("fields update");
        stored_id
    }

    pub(crate) fn append_small_task(&self, task_id_value: &str) {
        self.append_task_event(
            task_id_value,
            EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
                context_handle_id: None,
                seed_event_ids: Vec::new(),
                initial_memory_ids: Vec::new(),
                objective: "small task".to_string(),
            }),
        );
        self.append_task_event(task_id_value, tool_called("read", "inspect sources"));
        self.append_task_event(
            task_id_value,
            EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
                workflow_name: "prepare_change".to_string(),
                terminal_event_id: None,
                output_context_handle_id: None,
                memory_ids: Vec::new(),
                result_summary: "success".to_string(),
            }),
        );
    }

    pub(crate) fn task_slice(&self, task_id_value: &str) -> Vec<EventEnvelope> {
        self.append_small_task(task_id_value);
        EventQuery::new()
            .workspace("workspace-main")
            .task(task_id_value)
            .order(QueryOrder::OldestFirst)
            .execute(&EventReader::new(self.event_store.clone()))
            .expect("task slice loads")
    }

    pub(crate) fn pending_repo_review_items(&self) -> Vec<ReviewItem> {
        let conn = self.conn();
        self.review_queue(&conn)
            .list_pending(
                "workspace-main",
                &ReviewQueueFilter {
                    scope: Some(crate::memory_graph::MemoryScope::Repo),
                    ..ReviewQueueFilter::default()
                },
            )
            .expect("review queue lists")
    }

    pub(crate) fn proposal_ids(&self) -> Vec<String> {
        let conn = self.conn();
        let mut statement = conn
            .prepare("SELECT proposal_id FROM consolidation_proposals ORDER BY rowid ASC")
            .expect("query prepares");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query runs")
            .map(|row| row.expect("row loads"))
            .collect()
    }

    pub(crate) fn load_proposal(&self, proposal_id: &str) -> ConsolidationProposal {
        ConsolidationProposal::load(&self.conn(), proposal_id)
            .expect("proposal loads")
            .expect("proposal exists")
    }

    pub(crate) fn proposal_record(&self, proposal_id: &str) -> ConsolidationProposalRecord {
        ConsolidationProposal::load_record(&self.conn(), proposal_id)
            .expect("record loads")
            .expect("record exists")
    }

    pub(crate) fn proposal_decision(&self, proposal_id: &str) -> ProposalDecision {
        self.proposal_record(proposal_id).decision
    }

    pub(crate) fn memory_count(&self) -> usize {
        self.memory_store.list_all().expect("memories list").len()
    }

    pub(crate) fn structured_fields(&self, memory_id: &str) -> MemoryStructuredFields {
        self.memory_store
            .get_structured_fields(memory_id)
            .expect("fields load")
            .expect("fields exist")
    }

    pub(crate) fn memory_links_from(&self, memory_id: &str) -> Vec<MemoryLinkRecord> {
        self.memory_store
            .list_memory_links_from(memory_id)
            .expect("links load")
    }

    pub(crate) fn consolidated_events(&self) -> Vec<EventEnvelope> {
        EventQuery::new()
            .workspace("workspace-main")
            .branch("main")
            .kind(EventKind::MemoryConsolidated)
            .execute(&EventReader::new(self.event_store.clone()))
            .expect("events load")
    }

    pub(crate) fn failure_events(&self) -> Vec<EventEnvelope> {
        EventQuery::new()
            .workspace("workspace-main")
            .branch("main")
            .kind(EventKind::ConsolidationFailed)
            .execute(&EventReader::new(self.event_store.clone()))
            .expect("events load")
    }

    pub(crate) fn store_hash(&self) -> [u8; 32] {
        let mut states = self
            .memory_store
            .query_unscoped_admin(None, usize::MAX)
            .expect("memories list");
        states.sort_by(|left, right| left.id.cmp(&right.id));
        let payload = states
            .into_iter()
            .map(|memory| {
                capture_memory_state(&self.memory_store, &memory).expect("state captures")
            })
            .map(|state| serde_json::to_value(state).expect("state serializes"))
            .collect::<Vec<_>>();
        state_hash_from_json(&Value::Array(payload)).expect("hash computes")
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.consolidation_db).expect("db opens")
    }

    fn replay_driver<'a>(&'a self, conn: &'a Connection) -> ReplayDriver<'a> {
        ReplayDriver::new(
            &self.event_reader,
            self.event_store.clone(),
            conn,
            &self.memory_store,
            &self.event_writer,
            &self.clock,
        )
    }

    fn capture_state(&self, memory_id: &str) -> ConsolidationMemoryState {
        let memory = self
            .memory_store
            .get_by_id(memory_id)
            .expect("memory loads")
            .expect("memory exists");
        capture_memory_state(&self.memory_store, &memory).expect("state captures")
    }

    fn append_task_event(&self, task_id_value: &str, payload: EventPayload) {
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
                    value: task_id_value.to_string(),
                }),
                actor: Actor::Daemon,
                kind,
                references: Vec::new(),
                summary: CompactSummary::new(kind.as_str()).expect("summary builds"),
                payload,
            })
            .expect("event appends");
    }

    fn review_queue<'a>(&'a self, conn: &'a Connection) -> ReviewQueue<'a> {
        ReviewQueue::new(conn, &self.memory_store, &self.event_writer)
    }

    fn only_proposal_id(&self) -> String {
        let ids = self.proposal_ids();
        assert_eq!(ids.len(), 1);
        ids[0].clone()
    }

    fn operator(&self, value: &str) -> OperatorId {
        OperatorId {
            value: value.to_string(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct CountingDriver(Arc<Mutex<usize>>);

impl CountingDriver {
    pub(crate) fn call_count(&self) -> usize {
        *self.0.lock().expect("count lock")
    }
}

impl LlmDriver for CountingDriver {
    fn complete(&self, _request: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
        let mut calls = self.0.lock().expect("count lock");
        *calls += 1;
        Err(LlmDriverError::Failed(
            "replay must not call the live driver".to_string(),
        ))
    }

    fn name(&self) -> &str {
        "counting-driver"
    }
}

#[derive(Clone)]
pub(crate) struct FakeDriver {
    responses: Arc<Mutex<VecDeque<Result<String, LlmDriverError>>>>,
    calls: Arc<Mutex<Vec<LlmRequest>>>,
}

impl FakeDriver {
    fn new() -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn push_ok(&self, response: String) {
        self.responses
            .lock()
            .expect("responses lock")
            .push_back(Ok(response));
    }

    pub(crate) fn call_count(&self) -> usize {
        self.calls.lock().expect("calls lock").len()
    }

    pub(crate) fn last_request(&self) -> Option<LlmRequest> {
        self.calls.lock().expect("calls lock").last().cloned()
    }
}

impl LlmDriver for FakeDriver {
    fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
        self.calls.lock().expect("calls lock").push(request);
        self.responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .unwrap_or_else(|| Err(LlmDriverError::Failed("missing fake response".to_string())))
            .map(|content| LlmResponse { content })
    }

    fn name(&self) -> &str {
        "fake-llm"
    }
}

#[derive(Clone)]
pub(crate) struct BufferWriter(pub(crate) Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for BufferWriter {
    type Writer = BufferGuard;

    fn make_writer(&'a self) -> Self::Writer {
        BufferGuard(self.0.clone())
    }
}

pub(crate) struct BufferGuard(Arc<Mutex<Vec<u8>>>);

impl Write for BufferGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn repo_scope_update_job(memory_id: &str) -> ConsolidationJobSpec {
    let prior = memory_state(memory_id, "Repo memory", MemoryScope::Repo);
    let proposed = memory_state(memory_id, "Repo memory updated", MemoryScope::Repo);
    ConsolidationJobSpec {
        job_id: "job-repo-review".to_string(),
        workspace_id: "workspace-main".to_string(),
        kind: "repo_scope_update".to_string(),
        mode: ConsolidationJobMode::ManualReview,
        proposal: Some(PendingProposalSpec {
            proposal_id: "proposal-repo-review".to_string(),
            target_memory_id: Some(memory_id.to_string()),
            proposal_kind: ProposalKind::UpdateMemory,
            prior_state: encode_memory_state(&prior),
            proposed_state: encode_memory_state(&proposed),
            evidence: json!({ "source_memory_ids": [memory_id] }),
            provenance: None,
        }),
    }
}

pub(crate) fn llm_queue_job(index: usize) -> ConsolidationJobSpec {
    ConsolidationJobSpec {
        job_id: format!("queue-job-{index}"),
        workspace_id: "workspace-main".to_string(),
        kind: "episode_summary".to_string(),
        mode: ConsolidationJobMode::Background,
        proposal: Some(PendingProposalSpec {
            proposal_id: format!("queue-proposal-{index}"),
            target_memory_id: None,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: empty_state(),
            proposed_state: encode_memory_state(&memory_state(
                &format!("queue-mem-{index}"),
                "queued memory",
                MemoryScope::Session,
            )),
            evidence: json!({ "source_memory_ids": [] }),
            provenance: None,
        }),
    }
}

pub(crate) fn memory_state(
    memory_id: &str,
    content: &str,
    scope: MemoryScope,
) -> ConsolidationMemoryState {
    ConsolidationMemoryState {
        memory: Memory {
            id: memory_id.to_string(),
            session_id: "session-main".to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Observation,
            scope,
            confidence: 0.9,
            linked_symbols: Vec::new(),
            linked_files: Vec::new(),
            workspace_id: Some("workspace-main".to_string()),
            branch: Some("main".to_string()),
            scope_organization_id: None,
            refresh_key: Some(format!("refresh-{memory_id}")),
            source_query: Some("integration test".to_string()),
            created_at: 1,
            last_accessed: 1,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: crate::memory::MemoryVerificationStatus::Unverified,
        },
        structured_fields: MemoryStructuredFields::default(),
        last_verified_at: Some(10),
        last_verified_graph_snapshot_id: None,
        expires_at: None,
        memory_links: Vec::new(),
    }
}

pub(crate) fn background_ctx() -> LlmJobContext {
    LlmJobContext {
        workspace_id: "workspace-main".to_string(),
        mode: ConsolidationMode::Background,
        budget_catalog: BudgetCatalog::default(),
    }
}

pub(crate) fn synchronous_ctx() -> LlmJobContext {
    LlmJobContext {
        workspace_id: "workspace-main".to_string(),
        mode: ConsolidationMode::SynchronousPostTask,
        budget_catalog: BudgetCatalog::default(),
    }
}

pub(crate) fn task_id(value: &str) -> TaskId {
    TaskId {
        value: value.to_string(),
    }
}

pub(crate) fn episode_response() -> String {
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

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

fn apply_pending(
    runtime: ConsolidationJobRuntime,
    memory_store: &MemoryStore,
    event_writer: &EventWriter,
    proposal_ids: &[String],
) {
    for proposal_id in proposal_ids {
        let outcome = runtime
            .decide(
                proposal_id,
                ProposalDecision::Applied,
                memory_store,
                event_writer,
                "operator",
            )
            .expect("decision succeeds");
        assert!(outcome.is_some(), "proposal {proposal_id} did not apply");
    }
}

fn create_job(
    job_id: &str,
    proposal_id: &str,
    state: ConsolidationMemoryState,
    provenance: Option<LlmProvenance>,
) -> ConsolidationJobSpec {
    ConsolidationJobSpec {
        job_id: job_id.to_string(),
        workspace_id: "workspace-main".to_string(),
        kind: "integration_create".to_string(),
        mode: ConsolidationJobMode::Background,
        proposal: Some(PendingProposalSpec {
            proposal_id: proposal_id.to_string(),
            target_memory_id: None,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: empty_state(),
            proposed_state: encode_memory_state(&state),
            evidence: json!({ "source_memory_ids": [state.memory.id.clone()] }),
            provenance,
        }),
    }
}

fn llm_create_jobs(cache: &mut HashMap<[u8; 32], Vec<u8>>) -> Vec<ConsolidationJobSpec> {
    [
        ("job-5", "proposal-5", "mem-e"),
        ("job-6", "proposal-6", "mem-f"),
        ("job-7", "proposal-7", "mem-g"),
    ]
    .into_iter()
    .map(|(job_id, proposal_id, memory_id)| llm_create_job(job_id, proposal_id, memory_id, cache))
    .collect()
}

fn llm_create_job(
    job_id: &str,
    proposal_id: &str,
    memory_id: &str,
    cache: &mut HashMap<[u8; 32], Vec<u8>>,
) -> ConsolidationJobSpec {
    let prompt_bytes = format!("prompt-{proposal_id}").into_bytes();
    let response_bytes = format!("response-{proposal_id}").into_bytes();
    let provenance = LlmProvenance::record(
        "fake-llm",
        &prompt_bytes,
        &response_bytes,
        8,
        4,
        Duration::from_millis(12),
    )
    .expect("provenance records");
    cache.insert(provenance.prompt_sha256, response_bytes);
    create_job(
        job_id,
        proposal_id,
        memory_state(
            memory_id,
            &format!("llm memory {memory_id}"),
            MemoryScope::Session,
        ),
        Some(provenance),
    )
}

fn update_job(
    job_id: &str,
    proposal_id: &str,
    prior: &ConsolidationMemoryState,
    content: &str,
) -> ConsolidationJobSpec {
    let mut proposed = prior.clone();
    proposed.memory.content = content.to_string();
    direct_existing_job(
        job_id,
        proposal_id,
        ProposalKind::UpdateMemory,
        prior,
        &proposed,
    )
}

fn refresh_job(
    job_id: &str,
    proposal_id: &str,
    prior: &ConsolidationMemoryState,
    last_verified_at: u64,
) -> ConsolidationJobSpec {
    let mut proposed = prior.clone();
    proposed.last_verified_at = Some(last_verified_at);
    direct_existing_job(job_id, proposal_id, ProposalKind::Refresh, prior, &proposed)
}

fn supersede_job(
    job_id: &str,
    proposal_id: &str,
    prior: &ConsolidationMemoryState,
    newer: &ConsolidationMemoryState,
) -> ConsolidationJobSpec {
    let mut proposed = prior.clone();
    proposed.structured_fields.verification_status = MemoryVerificationStatus::Superseded;
    proposed.structured_fields.superseded_by_memory_id = Some(newer.memory.id.clone());
    proposed.memory_links.push(MemoryLinkRecord {
        link_id: format!("supersession:{}:{}", prior.memory.id, newer.memory.id),
        source_memory_id: prior.memory.id.clone(),
        target_memory_id: newer.memory.id.clone(),
        link_type: "supersedes".to_string(),
        reason: "integration test".to_string(),
        created_at: newer.memory.created_at,
        verification_status: "verified".to_string(),
    });
    direct_existing_job(
        job_id,
        proposal_id,
        ProposalKind::Supersede,
        prior,
        &proposed,
    )
}

fn direct_existing_job(
    job_id: &str,
    proposal_id: &str,
    proposal_kind: ProposalKind,
    prior: &ConsolidationMemoryState,
    proposed: &ConsolidationMemoryState,
) -> ConsolidationJobSpec {
    ConsolidationJobSpec {
        job_id: job_id.to_string(),
        workspace_id: "workspace-main".to_string(),
        kind: "integration_existing".to_string(),
        mode: ConsolidationJobMode::Background,
        proposal: Some(PendingProposalSpec {
            proposal_id: proposal_id.to_string(),
            target_memory_id: Some(prior.memory.id.clone()),
            proposal_kind,
            prior_state: encode_memory_state(prior),
            proposed_state: encode_memory_state(proposed),
            evidence: json!({ "source_memory_ids": [prior.memory.id.clone()] }),
            provenance: None,
        }),
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
