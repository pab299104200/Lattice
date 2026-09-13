use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lattice_core::events::{
    Actor, BranchRef, EventPayload, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy,
    QueryOrder, SessionId, TaskId,
};
use lattice_core::graph::CodeGraph;
use lattice_core::identity::{Identity, IdentityResolver, MemoryId};
use lattice_core::indexer::Indexer;
use lattice_core::memory::{MemoryStore, MemoryVerificationStatus};
use lattice_core::query::QueryEngine;
use lattice_core::retrieval_v1::{
    schema::{BudgetReport, BundleResult, RetrievalBundle},
    DiagnosticMode, RetrievalBudget, RetrievalProfile, ShaperBudget, ShaperContext,
};
use lattice_core::storage::GraphStore;
use lattice_core::working_memory::operations::RetrievalExecution;
use lattice_core::working_memory::{
    emit_memory_expanded, emit_memory_retrieved, expand, retrieve, CheckpointScope, ExcludedMemory,
    ExpandArgs, MutationEventSummary, OpContext, RetrieveArgs, StateMutationObserver,
    StateMutationRecord, WorkingMemoryEventContext, WorkingMemoryOp, WorkingMemoryRetriever,
    WorkingMemoryState,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::mcp::McpHandler;
use super::server::RequestHandler;

const WORKSPACE_ID: &str = "workspace-test";
const TASK_ID: &str = "task-working-memory";

#[test]
fn tools_list_registers_inspect_working_memory_schema() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let (handler, _memory_store, workspace_root, context_cache_path) =
            build_handler("tools-list");
        let response = handler
            .handle("lattice/tools/list_all", json!({}))
            .await
            .expect("internal tools/list succeeds");
        let tools = response["tools"].as_array().expect("tool array");
        let tool = tools
            .iter()
            .find(|item| item["name"].as_str() == Some("inspect_working_memory"))
            .expect("inspect_working_memory registered");
        assert_eq!(
            tool["inputSchema"]["required"].as_array(),
            Some(&vec![json!("task_id")])
        );
        assert_eq!(
            tool["inputSchema"]["properties"]["mode"]["enum"].as_array(),
            Some(&vec![json!("compact"), json!("diagnostic")])
        );
        assert_eq!(
            tool["inputSchema"]["properties"]["include_excluded"]["type"].as_str(),
            Some("boolean")
        );
        cleanup_paths(&workspace_root, &context_cache_path);
    });
}

#[tokio::test]
async fn compact_mode_returns_summary_and_snapshot_handle() {
    let (handler, _memory_store, workspace_root, context_cache_path) = build_handler("compact");
    let state = sample_state();
    handler
        .remember_working_memory_state_for_test(TASK_ID, state.clone())
        .await;

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "inspect_working_memory",
                "arguments": {
                    "task_id": TASK_ID
                }
            }),
        )
        .await
        .expect("inspect_working_memory compact succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(payload["task_id"].as_str(), Some(TASK_ID));
    assert_eq!(payload["mode"].as_str(), Some("compact"));
    assert_eq!(
        payload["summary"]["selected_count"].as_u64(),
        Some(state.selected_memories.len() as u64)
    );
    assert_eq!(
        payload["summary"]["excluded_count"].as_u64(),
        Some(state.excluded_memories.len() as u64)
    );
    let expansion_handle = payload["expansion_handle"]
        .as_str()
        .expect("expansion handle");
    let round_tripped = handler
        .resolve_working_memory_snapshot_for_test(expansion_handle)
        .await
        .expect("snapshot resolves");
    assert_eq!(round_tripped, state);

    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn diagnostic_mode_returns_full_state_and_include_excluded_controls_visibility() {
    let (handler, _memory_store, workspace_root, context_cache_path) = build_handler("diagnostic");
    let state = sample_state();
    handler
        .remember_working_memory_state_for_test(TASK_ID, state.clone())
        .await;

    let without_excluded = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "inspect_working_memory",
                "arguments": {
                    "task_id": TASK_ID,
                    "mode": "diagnostic"
                }
            }),
        )
        .await
        .expect("diagnostic succeeds");
    let without_payload = parse_tool_payload(&without_excluded);
    assert_eq!(without_payload["mode"].as_str(), Some("diagnostic"));
    assert_eq!(
        without_payload["state"]["selected_memories"]
            .as_array()
            .map(Vec::len),
        Some(state.selected_memories.len())
    );
    assert_eq!(
        without_payload["state"]["excluded_memories"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );

    let with_excluded = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "inspect_working_memory",
                "arguments": {
                    "task_id": TASK_ID,
                    "mode": "diagnostic",
                    "include_excluded": true
                }
            }),
        )
        .await
        .expect("diagnostic include_excluded succeeds");
    let with_payload = parse_tool_payload(&with_excluded);
    let excluded = with_payload["state"]["excluded_memories"]
        .as_array()
        .expect("excluded memories");
    assert_eq!(excluded.len(), state.excluded_memories.len());
    assert_eq!(
        excluded[0]["exclusion_reason"].as_str(),
        Some("compressed: budget=120 tokens")
    );

    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn tool_falls_back_to_latest_checkpoint_when_session_cache_is_empty() {
    let (handler, memory_store, workspace_root, context_cache_path) = build_handler("checkpoint");
    let state = sample_state();
    let checkpoint_id = memory_store
        .lock()
        .await
        .save_working_memory_checkpoint_for_scope(
            &state,
            "latest",
            &CheckpointScope::new(
                workspace_root.to_string_lossy().to_string(),
                "session-test-checkpoint".to_string(),
                TASK_ID.to_string(),
            ),
        )
        .expect("checkpoint saved");

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "inspect_working_memory",
                "arguments": {
                    "task_id": TASK_ID,
                    "mode": "diagnostic",
                    "include_excluded": true
                }
            }),
        )
        .await
        .expect("inspect_working_memory checkpoint fallback succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(payload["checkpoint_id"].as_i64(), Some(checkpoint_id));
    assert_eq!(
        payload["state"]["task_statement"].as_str(),
        Some(state.task_statement.as_str())
    );

    cleanup_paths(&workspace_root, &context_cache_path);
}

#[test]
fn retrieve_and_expand_emit_event_diffs_through_single_mutation_observer() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let writer = Arc::new(
        EventWriter::new(store.clone(), WORKSPACE_ID.to_string(), 4096)
            .with_flush_policy(FlushPolicy::Sync),
    );
    let observer = EventHookObserver::new(writer.clone());
    let harness = WorkingMemoryHarness::new(FakeRetriever::with_event_results(), &observer);
    let mut state = sample_state();

    let retrieve_outcome = retrieve(
        &mut state,
        RetrieveArgs {
            task_id: TASK_ID.to_string(),
            query_text: Some("retrieve active context".to_string()),
            budget: RetrievalBudget::default(),
            diagnostic_mode: DiagnosticMode::Compact,
            query_embedding: None,
        },
        &harness.ctx(),
    )
    .expect("retrieve succeeds");
    assert_eq!(retrieve_outcome.added_identities.len(), 1);

    let expansion_handle = state.selected_memories[0].expansion_handle.clone();
    let expand_outcome = expand(
        &mut state,
        ExpandArgs {
            task_id: TASK_ID.to_string(),
            expansion_handle,
            budget: RetrievalBudget::default(),
            diagnostic_mode: DiagnosticMode::Compact,
            query_embedding: None,
        },
        &harness.ctx(),
    )
    .expect("expand succeeds");
    assert_eq!(expand_outcome.added_count, 1);

    let reader = EventReader::new(store.clone());
    let events = reader
        .execute(
            EventQuery::new()
                .task(TASK_ID)
                .order(QueryOrder::OldestFirst)
                .limit(10),
        )
        .expect("event query succeeds");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind.as_str(), "memory_retrieved");
    assert_eq!(events[1].kind.as_str(), "memory_expanded");

    let EventPayload::MemoryRetrieved(retrieved) = &events[0].payload else {
        panic!("expected memory_retrieved payload");
    };
    assert_eq!(retrieved.included_context.len(), 1);
    assert_eq!(retrieved.excluded_context.len(), 1);
    assert_eq!(
        retrieved.excluded_context[0].exclusion_reason,
        "compressed: budget=64 tokens"
    );

    let EventPayload::MemoryExpanded(expanded) = &events[1].payload else {
        panic!("expected memory_expanded payload");
    };
    assert_eq!(expanded.included_context.len(), 1);
    assert_eq!(expanded.excluded_context.len(), 1);
    assert_eq!(
        expanded.excluded_context[0].exclusion_reason,
        "filtered: low confidence"
    );
    assert!(
        expanded.memory_id.is_some(),
        "expand should carry memory id"
    );
}

fn build_handler(suffix: &str) -> (McpHandler, Arc<Mutex<MemoryStore>>, PathBuf, PathBuf) {
    let workspace_root = unique_test_path(&format!("lattice-working-memory-{suffix}"));
    std::fs::create_dir_all(&workspace_root).expect("workspace dir");
    let context_cache_path = workspace_root.join("context_handles.json");
    let memory_store = Arc::new(Mutex::new(
        MemoryStore::open_in_memory().expect("memory store"),
    ));
    let handler = McpHandler::new(
        Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None))),
        Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
        memory_store.clone(),
        Arc::new(Mutex::new(
            GraphStore::open_in_memory().expect("graph store"),
        )),
        Arc::new(std::sync::OnceLock::new()),
        None,
        workspace_root.clone(),
        context_cache_path.clone(),
        format!("session-test-{suffix}"),
        None,
        vec![workspace_root.clone()],
        Arc::new(AtomicBool::new(false)),
        None,
        Vec::new(),
        Vec::new(),
    );
    (handler, memory_store, workspace_root, context_cache_path)
}

fn cleanup_paths(workspace_root: &PathBuf, context_cache_path: &PathBuf) {
    let _ = std::fs::remove_file(context_cache_path);
    let _ = std::fs::remove_dir_all(workspace_root);
}

fn unique_test_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}"))
}

fn parse_tool_payload(response: &Value) -> Value {
    let text = response["content"][0]["text"].as_str().expect("tool text");
    serde_json::from_str(text).expect("tool payload json")
}

fn sample_state() -> WorkingMemoryState {
    let selected = sample_bundle_result("memory-selected", 0.91, "selected memory");
    WorkingMemoryState {
        task_statement: "Inspect working memory state".to_string(),
        interpreted_intent: lattice_core::retrieval_v1::classify_intent(
            "Inspect working memory state",
        ),
        active_files: Default::default(),
        active_symbols: Default::default(),
        active_hypotheses: vec![lattice_core::working_memory::Hypothesis {
            text: "The active context should be inspectable".to_string(),
            evidence_refs: vec!["memory-selected".to_string()],
            confidence: 0.8,
        }],
        active_failures: Vec::new(),
        current_plan: None,
        selected_memories: vec![selected],
        excluded_memories: vec![ExcludedMemory {
            result: sample_bundle_result("memory-excluded", 0.44, "compressed memory"),
            exclusion_reason: "compressed: budget=120 tokens".to_string(),
        }],
        budget_decisions: Default::default(),
        unresolved_questions: vec!["Should excluded memories be visible?".to_string()],
        verification_status: lattice_core::working_memory::WorkingMemoryVerification {
            last_verified_at: None,
            status: MemoryVerificationStatus::Unverified,
            notes: Vec::new(),
        },
    }
}

fn sample_bundle_result(ulid: &str, score: f32, reason: &str) -> BundleResult {
    BundleResult {
        identity: Identity::Memory(MemoryId {
            workspace_id: WORKSPACE_ID.to_string(),
            ulid: ulid.to_string(),
        }),
        kind: lattice_core::identity::IdentityKind::Memory,
        headline: format!("Memory {ulid}"),
        snippet: format!("Snippet for {ulid}"),
        inclusion_reason: reason.to_string(),
        expansion_handle: lattice_core::identity::encode_identity(&Identity::Memory(MemoryId {
            workspace_id: WORKSPACE_ID.to_string(),
            ulid: ulid.to_string(),
        })),
        source: Vec::new(),
        score,
    }
}

#[derive(Clone)]
struct FakeRetriever {
    retrieve_execution: RetrievalExecution,
    expand_execution: RetrievalExecution,
}

impl FakeRetriever {
    fn with_event_results() -> Self {
        Self {
            retrieve_execution: RetrievalExecution {
                bundle: sample_bundle(vec![sample_bundle_result(
                    "memory-retrieved",
                    0.88,
                    "retrieved from query",
                )]),
                excluded_memories: vec![ExcludedMemory {
                    result: sample_bundle_result("memory-dropped", 0.31, "dropped"),
                    exclusion_reason: "compressed: budget=64 tokens".to_string(),
                }],
            },
            expand_execution: RetrievalExecution {
                bundle: sample_bundle(vec![sample_bundle_result(
                    "memory-expanded",
                    0.77,
                    "expanded around anchor",
                )]),
                excluded_memories: vec![ExcludedMemory {
                    result: sample_bundle_result("memory-filtered", 0.22, "filtered"),
                    exclusion_reason: "filtered: low confidence".to_string(),
                }],
            },
        }
    }
}

impl WorkingMemoryRetriever for FakeRetriever {
    fn retrieve(
        &self,
        _state: &WorkingMemoryState,
        _args: &RetrieveArgs,
        _ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, lattice_core::error::LatticeError> {
        Ok(self.retrieve_execution.clone())
    }

    fn expand(
        &self,
        _state: &WorkingMemoryState,
        _target: &BundleResult,
        _args: &ExpandArgs,
        _ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, lattice_core::error::LatticeError> {
        Ok(self.expand_execution.clone())
    }
}

struct EventHookObserver {
    writer: Arc<EventWriter>,
    context: WorkingMemoryEventContext,
}

impl EventHookObserver {
    fn new(writer: Arc<EventWriter>) -> Self {
        Self {
            writer,
            context: WorkingMemoryEventContext {
                workspace_id: WORKSPACE_ID.to_string(),
                branch: BranchRef {
                    name: "test".to_string(),
                },
                session_id: SessionId {
                    value: "session-events".to_string(),
                },
                task_id: TaskId {
                    value: TASK_ID.to_string(),
                },
                actor: Actor::Daemon,
            },
        }
    }
}

impl StateMutationObserver for EventHookObserver {
    fn record(
        &self,
        mutation: StateMutationRecord,
    ) -> Result<(), lattice_core::error::LatticeError> {
        let summary = MutationEventSummary {
            op_name: mutation.op_name.clone(),
            summary: mutation.summary.clone(),
        };
        let result = match mutation.op {
            WorkingMemoryOp::Expand => emit_memory_expanded(
                &mutation.state_before,
                &mutation.state_after,
                &summary,
                &self.context,
                self.writer.as_ref(),
            ),
            WorkingMemoryOp::Summarize => Ok(None),
            _ => emit_memory_retrieved(
                &mutation.state_before,
                &mutation.state_after,
                &summary,
                &self.context,
                self.writer.as_ref(),
            ),
        };
        result
            .map(|_| ())
            .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))
    }
}

struct WorkingMemoryHarness<'a> {
    conn: rusqlite::Connection,
    resolver: &'static IdentityResolver<'static>,
    retriever: FakeRetriever,
    observer: &'a EventHookObserver,
}

impl<'a> WorkingMemoryHarness<'a> {
    fn new(retriever: FakeRetriever, observer: &'a EventHookObserver) -> Self {
        let conn = rusqlite::Connection::open_in_memory().expect("memory db");
        lattice_core::working_memory::initialize_schema(&conn).expect("working memory schema");
        let graph = Box::leak(Box::new(CodeGraph::default()));
        let file_index = Box::leak(Box::new(Default::default()));
        let parsed_files = Box::leak(Box::new(Default::default()));
        let resolver = Box::leak(Box::new(IdentityResolver::new(
            graph,
            file_index,
            parsed_files,
            WORKSPACE_ID.to_string(),
            Vec::new(),
        )));
        Self {
            conn,
            resolver,
            retriever,
            observer,
        }
    }

    fn ctx(&self) -> OpContext<'_> {
        OpContext {
            conn: &self.conn,
            retrieval_profile: RetrievalProfile::Balanced,
            identity_resolver: self.resolver,
            shaper: ShaperContext {
                pins: HashSet::new(),
                budget: ShaperBudget { max_tokens: 128 },
            },
            retriever: &self.retriever,
            mutation_observer: Some(self.observer),
        }
    }
}

fn sample_bundle(results: Vec<BundleResult>) -> RetrievalBundle {
    RetrievalBundle {
        task_id: TASK_ID.to_string(),
        intent_summary: "Inspect".to_string(),
        anchors_summary: "working memory".to_string(),
        results,
        diagnostics: None,
        budget_report: BudgetReport {
            token_budget: 128,
            estimated_tokens: 64,
            trimmed_snippets: 0,
            dropped_results: 0,
            truncated: false,
        },
    }
}
