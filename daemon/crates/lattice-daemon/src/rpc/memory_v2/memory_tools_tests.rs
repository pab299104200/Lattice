use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lattice_core::events::{
    EventPayload, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy, QueryOrder,
};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::model::MemoryAssertionType;
use lattice_core::memory::{
    Memory, MemoryClass, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use lattice_core::working_memory::WorkingMemoryState;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::super::mcp::McpHandler;
use super::super::server::RequestHandler;
use super::{
    get_task_memory, propose_memory_evolution, save_memory, EvolutionAction, TaskMemoryBundle,
};

const TASK_ID: &str = "task-memory-v2";

#[test]
fn serde_round_trips_memory_tool_requests_and_responses() {
    let get_args = get_task_memory::GetTaskMemoryArgs {
        task_id: TASK_ID.to_string(),
        intent_hint: Some("refresh auth memory".to_string()),
        budget_tokens: Some(800),
    };
    round_trip(&get_args);

    let save_args = save_memory::SaveMemoryArgs {
        content: "Auth refresh invariant".to_string(),
        memory_class: MemoryClass::ArchitectureInvariant,
        assertion_type: Some(MemoryAssertionType::Constraint),
        scope: save_memory::MemoryScopeArg::Repo,
        confidence: 0.81,
        confidence_reason: "Derived from code and tests".to_string(),
        freshness_policy: save_memory::FreshnessPolicyArg::ManualReview,
        validity_conditions: vec!["auth router unchanged".to_string()],
        invalidation_triggers: vec!["refresh flow refactor".to_string()],
        provenance_event_ids: vec!["evt-1".to_string()],
        evidence: Vec::new(),
        linked_files: vec!["src/auth.rs".to_string()],
        linked_symbols: vec!["refresh_token".to_string()],
        linked_docs: vec!["docs/auth.md#Refresh".to_string()],
        linked_tests: vec!["tests/auth_refresh.rs::refresh_token".to_string()],
        linked_memories: vec!["mem-1".to_string()],
        source_query: Some("refresh token failures".to_string()),
        refresh_key: Some("auth::refresh".to_string()),
        branch: None,
        organization_id: None,
    };
    round_trip(&save_args);

    let proposal_args = propose_memory_evolution::ProposeMemoryEvolutionArgs {
        action: EvolutionAction::Propose,
        proposal_id: None,
        memory_id: Some("mem-1".to_string()),
        content: Some("Updated content".to_string()),
        linked_files: vec!["src/auth.rs".to_string()],
        linked_symbols: vec!["refresh_token".to_string()],
        linked_docs: vec!["docs/auth.md#Refresh".to_string()],
        linked_tests: vec!["tests/auth_refresh.rs::refresh_token".to_string()],
        linked_memories: vec!["mem-2".to_string()],
        validity_conditions: vec!["router still present".to_string()],
        invalidation_triggers: vec!["handler removed".to_string()],
        superseded_by_memory_id: None,
        invalidate_reason: None,
        reason: Some("refresh contract evolved".to_string()),
        decided_by: Some("assistant".to_string()),
    };
    round_trip(&proposal_args);

    let bundle = TaskMemoryBundle {
        task_id: TASK_ID.to_string(),
        checkpoint_id: Some(7),
        working_memory_verification_status: "verified".to_string(),
        memories: Vec::new(),
    };
    round_trip(&bundle);
}

#[tokio::test]
async fn get_task_memory_surfaces_inclusion_reason_and_verification_status() {
    let (handler, memory_store, event_store, workspace_root, context_cache_path, session_id) =
        build_handler("get-task-memory");
    {
        let store = memory_store.lock().await;
        let mut memory = seed_memory(
            "refresh token must preserve session",
            MemoryScope::Repo,
        );
        memory.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        let id = store.store(memory).expect("store memory");
        let mut fields = MemoryStructuredFields::default();
        fields.memory_class = MemoryClass::Constraint;
        fields.assertion_type = MemoryAssertionType::Constraint;
        fields.verification_status = MemoryVerificationStatus::Verified;
        fields.linked_docs = vec!["docs/auth.md#Refresh".to_string()];
        store
            .update_structured_fields(&id, &fields)
            .expect("update fields");
    }
    handler
        .remember_working_memory_state_for_test(
            TASK_ID,
            WorkingMemoryState::new("refresh token session diagnosis"),
        )
        .await;

    let response = handler
        .handle(
            "tools/call",
            json!({"name": "get_task_memory", "arguments": {"task_id": TASK_ID, "intent_hint": "refresh token"}}),
        )
        .await
        .expect("tool succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories array");
    assert!(
        !memories.is_empty(),
        "expected at least one surfaced memory"
    );
    for memory in memories {
        assert!(memory["inclusion_reason"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["verification_status"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
    }

    let events = read_events(&event_store, &workspace_root);
    assert!(events
        .iter()
        .any(|event| matches!(event.payload, EventPayload::MemoryRetrieved(_))));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn save_memory_defaults_unverified_persists_fields_emits_event_and_queues_verification() {
    let (handler, memory_store, event_store, workspace_root, context_cache_path, session_id) =
        build_handler("save-memory");
    let response = handler
        .handle(
            "tools/call",
            json!({
                "name": "save_memory",
                "arguments": {
                    "content": "Refresh token invariants rely on session scope",
                    "memory_class": "constraint",
                    "assertion_type": "constraint",
                    "scope": "repo",
                    "confidence": 0.74,
                    "confidence_reason": "Observed in handler and tests",
                    "freshness_policy": "manual_review",
                    "validity_conditions": ["session middleware stays enabled"],
                    "invalidation_triggers": ["refresh handler rewrite"],
                    "provenance_event_ids": ["evt-refresh"],
                    "linked_files": ["src/auth.rs"],
                    "linked_symbols": ["refresh_token"],
                    "linked_docs": ["docs/auth.md#Refresh"],
                    "linked_tests": ["tests/auth_refresh.rs::refresh_token"],
                    "linked_memories": ["mem-upstream"],
                    "source_query": "refresh token failures",
                    "refresh_key": "auth::refresh"
                }
            }),
        )
        .await
        .expect("save_memory succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(
        payload["memory"]["verification_status"].as_str(),
        Some("unverified")
    );
    let memory_id = payload["memory_id"].as_str().expect("memory id");

    let store = memory_store.lock().await;
    let fields = store
        .get_structured_fields(memory_id)
        .expect("fields query")
        .expect("fields exist");
    assert_eq!(fields.memory_class, MemoryClass::Constraint);
    assert_eq!(fields.assertion_type, MemoryAssertionType::Constraint);
    assert_eq!(fields.linked_docs, vec!["docs/auth.md#Refresh".to_string()]);
    assert_eq!(
        fields.linked_tests,
        vec!["tests/auth_refresh.rs::refresh_token".to_string()]
    );
    assert_eq!(fields.linked_memories, vec!["mem-upstream".to_string()]);
    assert_eq!(
        fields.validity_conditions,
        vec!["session middleware stays enabled".to_string()]
    );
    assert_eq!(
        fields.invalidation_triggers,
        vec!["refresh handler rewrite".to_string()]
    );
    let queued_jobs: i64 = store
        .with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM verification_jobs WHERE target_memory_id = ?1 AND status = 'queued'",
                [memory_id],
                |row| row.get(0),
            )
            .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))
        })
        .expect("verification job count");
    assert_eq!(queued_jobs, 1);
    drop(store);

    let events = read_events(&event_store, &workspace_root);
    assert!(events
        .iter()
        .any(|event| matches!(event.payload, EventPayload::MemoryCreated(_))));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn propose_apply_and_reject_memory_evolution_are_auditable() {
    let (handler, memory_store, event_store, workspace_root, context_cache_path, session_id) =
        build_handler("memory-evolution");
    let memory_id = {
        let mut store = memory_store.lock().await;
        let id = store
            .store(seed_memory("refresh behavior old", MemoryScope::Repo))
            .expect("store memory");
        let mut fields = MemoryStructuredFields::default();
        fields.memory_class = MemoryClass::Observation;
        store
            .update_structured_fields(&id, &fields)
            .expect("update fields");
        id
    };

    let propose = handler
        .handle(
            "tools/call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "propose",
                    "memory_id": memory_id,
                    "content": "refresh behavior new",
                    "linked_docs": ["docs/auth.md#Refresh"],
                    "reason": "refresh path changed"
                }
            }),
        )
        .await
        .expect("propose succeeds");
    let propose_payload = parse_tool_payload(&propose);
    let proposal_id = propose_payload["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
    {
        let store = memory_store.lock().await;
        let current = store
            .get_by_id(&memory_id)
            .expect("memory query")
            .expect("memory exists");
        assert_eq!(current.content, "refresh behavior old");
        let record = store
            .with_connection(|conn| {
                lattice_core::consolidation::ConsolidationProposal::load_record(conn, &proposal_id)
                    .map_err(Into::into)
                    .and_then(|value| {
                        value.ok_or_else(|| {
                            lattice_core::LatticeError::Storage("missing proposal".to_string())
                        })
                    })
            })
            .expect("proposal record");
        assert_eq!(record.decision.as_str(), "pending");
    }

    let apply = handler
        .handle(
            "tools/call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "apply",
                    "proposal_id": proposal_id,
                    "reason": "ship updated refresh memory"
                }
            }),
        )
        .await
        .expect("apply succeeds");
    let apply_payload = parse_tool_payload(&apply);
    assert_eq!(apply_payload["decision"].as_str(), Some("applied"));
    {
        let store = memory_store.lock().await;
        let current = store
            .get_by_id(&memory_id)
            .expect("memory query")
            .expect("memory exists");
        assert_eq!(current.content, "refresh behavior new");
    }

    let reject_proposal = handler
        .handle(
            "tools/call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "propose",
                    "memory_id": memory_id,
                    "content": "refresh behavior rejected",
                    "reason": "candidate rejected"
                }
            }),
        )
        .await
        .expect("second propose succeeds");
    let reject_payload = parse_tool_payload(&reject_proposal);
    let reject_id = reject_payload["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
    let reject = handler
        .handle(
            "tools/call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "reject",
                    "proposal_id": reject_id,
                    "reason": "do not accept"
                }
            }),
        )
        .await
        .expect("reject succeeds");
    let reject_payload = parse_tool_payload(&reject);
    assert_eq!(reject_payload["decision"].as_str(), Some("rejected"));

    let events = read_events(&event_store, &workspace_root);
    assert!(events
        .iter()
        .any(|event| matches!(event.payload, EventPayload::MemoryConsolidated(_))));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn get_task_memory_respects_scope_boundaries() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("scope-boundary");
    {
        let mut store = memory_store.lock().await;
        let mut branch_memory = seed_memory("other branch memory", MemoryScope::Branch);
        branch_memory.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        branch_memory.branch = Some("different-branch".to_string());
        store.store(branch_memory).expect("store branch memory");
    }
    handler
        .remember_working_memory_state_for_test(TASK_ID, WorkingMemoryState::new("branch mismatch"))
        .await;

    let response = handler
        .handle(
            "tools/call",
            json!({"name": "get_task_memory", "arguments": {"task_id": TASK_ID}}),
        )
        .await
        .expect("get_task_memory succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories");
    assert!(memories
        .iter()
        .all(|memory| memory["content"].as_str() != Some("other branch memory")));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn apply_memory_evolution_shim_forwards_with_deprecation_warning() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("apply-shim");
    let memory_id = {
        let mut store = memory_store.lock().await;
        store
            .store(seed_memory("shim target", MemoryScope::Repo))
            .expect("store memory")
    };
    let propose = handler
        .handle(
            "tools/call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "propose",
                    "memory_id": memory_id,
                    "content": "shim target updated"
                }
            }),
        )
        .await
        .expect("propose succeeds");
    let proposal_id = parse_tool_payload(&propose)["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
    let shim = handler
        .handle(
            "tools/call",
            json!({
                "name": "apply_memory_evolution",
                "arguments": {
                    "proposal_id": proposal_id
                }
            }),
        )
        .await
        .expect("shim succeeds");
    let payload = parse_tool_payload(&shim);
    assert_eq!(payload["decision"].as_str(), Some("applied"));
    assert!(payload["deprecation_warning"].as_str().is_some());
    cleanup_paths(&workspace_root, &context_cache_path);
}

fn round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let restored: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&restored, value);
}

fn build_handler(
    suffix: &str,
) -> (
    McpHandler,
    Arc<Mutex<MemoryStore>>,
    Arc<EventStore>,
    PathBuf,
    PathBuf,
    String,
) {
    let workspace_root = unique_test_path(&format!("lattice-memory-v2-{suffix}"));
    std::fs::create_dir_all(&workspace_root).expect("workspace dir");
    let context_cache_path = workspace_root.join("context_handles.json");
    let memory_store = Arc::new(Mutex::new(
        MemoryStore::open_in_memory().expect("memory store"),
    ));
    let event_store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let event_writer = Arc::new(
        EventWriter::new(
            event_store.clone(),
            workspace_root.to_string_lossy().to_string(),
            4096,
        )
        .with_flush_policy(FlushPolicy::Sync),
    );
    let session_id = format!("session-test-{suffix}");
    let handler = McpHandler::new(
        Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
        Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
        memory_store.clone(),
        Arc::new(Mutex::new(
            GraphStore::open_in_memory().expect("graph store"),
        )),
        Arc::new(std::sync::OnceLock::new()),
        None,
        workspace_root.clone(),
        context_cache_path.clone(),
        session_id.clone(),
        None,
        vec![workspace_root.clone()],
        Arc::new(AtomicBool::new(false)),
        Some(event_writer),
    );
    (
        handler,
        memory_store,
        event_store,
        workspace_root,
        context_cache_path,
        session_id,
    )
}

fn seed_memory(content: &str, scope: MemoryScope) -> Memory {
    Memory {
        id: String::new(),
        session_id: "session-seed".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope,
        confidence: 0.81,
        linked_symbols: vec!["refresh_token".to_string()],
        linked_files: vec!["src/auth.rs".to_string()],
        workspace_id: None,
        branch: None,
        scope_organization_id: None,
        refresh_key: None,
        source_query: Some("refresh token".to_string()),
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn read_events(
    store: &Arc<EventStore>,
    workspace_root: &PathBuf,
) -> Vec<lattice_core::events::EventEnvelope> {
    let workspace_id = workspace_root.to_string_lossy().to_string();
    // McpHandler events use the workspace branch (defaults to "unknown" when there's
    // no git repo); proposal apply emits MemoryConsolidated events on branch "main".
    // Fold both branches so apply/reject is auditable end-to-end.
    let reader = EventReader::new(store.clone());
    let mut events = reader
        .execute(
            EventQuery::new()
                .workspace(workspace_id.clone())
                .branch("unknown")
                .order(QueryOrder::OldestFirst)
                .limit(128),
        )
        .expect("event query");
    let mut consolidation = reader
        .execute(
            EventQuery::new()
                .workspace(workspace_id)
                .branch("main")
                .order(QueryOrder::OldestFirst)
                .limit(128),
        )
        .unwrap_or_default();
    events.append(&mut consolidation);
    events
}

fn parse_tool_payload(response: &Value) -> Value {
    let text = response["content"][0]["text"].as_str().expect("tool text");
    serde_json::from_str(text).expect("tool payload json")
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
