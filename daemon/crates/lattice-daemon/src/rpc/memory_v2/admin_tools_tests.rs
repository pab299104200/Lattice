use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lattice_core::events::{
    EventKind, EventPayload, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy,
    QueryOrder,
};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::{MemoryStore, MemoryType};
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::super::mcp::McpHandler;
use super::super::server::RequestHandler;
use super::{consolidate_session, get_event_trace, get_memory_metrics};

#[test]
fn serde_round_trips_admin_tool_requests_and_responses() {
    let consolidate_args = consolidate_session::ConsolidateSessionArgs {
        session_id: "session-1".to_string(),
        mode: Some(consolidate_session::ConsolidationMode::PostTask),
        budget_ms: Some(250),
        render_mode: Some(consolidate_session::ConsolidationRenderMode::Diagnostic),
    };
    round_trip(&consolidate_args);

    let metrics_args = get_memory_metrics::GetMemoryMetricsArgs {
        scope: Some(get_memory_metrics::MetricScopeKind::Session),
        time_range: None,
        signals: vec![get_memory_metrics::MetricSignal::ToolCallsPerSuccessfulTask],
        render_mode: Some(get_memory_metrics::MetricRenderMode::Full),
    };
    round_trip(&metrics_args);

    let trace_args = get_event_trace::GetEventTraceArgs {
        task_id: Some("task-1".to_string()),
        session_id: None,
        workspace_id: None,
        kinds: vec![EventKind::ToolCalled],
        since: None,
        until: None,
        cursor: Some(get_event_trace::encode_cursor(7)),
        limit: Some(10),
        render_mode: Some(get_event_trace::EventTraceRenderMode::Diagnostic),
    };
    round_trip(&trace_args);

    round_trip(&consolidate_session::ConsolidationReport {
        session_id: "session-1".to_string(),
        mode: consolidate_session::ConsolidationMode::PostTask,
        render_mode: consolidate_session::ConsolidationRenderMode::Compact,
        budget_ms: None,
        proposals: Vec::new(),
        categories: Vec::new(),
        incomplete: true,
        notes: vec!["fallback".to_string()],
    });
}

#[tokio::test]
async fn consolidate_session_emits_proposals_without_direct_writes_and_proposals_are_actionable() {
    let (handler, memory_store, event_store, workspace_root, context_cache_path) =
        build_handler("consolidate-session");
    seed_task_events(
        &event_store,
        &workspace_root,
        "session-test-consolidate-session",
        "task-consolidate",
    );

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "consolidate_session",
                "arguments": {
                    "session_id": "session-test-consolidate-session",
                    "mode": "post_task",
                    "render_mode": "diagnostic"
                }
            }),
        )
        .await
        .expect("consolidate_session succeeds");
    let payload = parse_tool_payload(&response);
    let proposals = payload["proposals"].as_array().expect("proposals");
    assert!(
        !proposals.is_empty(),
        "expected at least one consolidation proposal"
    );

    let proposal_id = proposals[0]["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
    let proposed_memory_id = {
        let store = memory_store.lock().await;
        store
            .with_connection(|conn| {
                lattice_core::consolidation::ConsolidationProposal::load_record(conn, &proposal_id)
                    .map_err(Into::into)
                    .and_then(|value| {
                        value.ok_or_else(|| {
                            lattice_core::LatticeError::Storage("missing proposal".to_string())
                        })
                    })
            })
            .expect("proposal record")
            .proposed_state["id"]
            .as_str()
            .expect("proposed memory id")
            .to_string()
    };
    let existing = memory_store
        .lock()
        .await
        .get_by_id(&proposed_memory_id)
        .expect("proposal target lookup");
    assert!(
        existing.is_none(),
        "proposal should not materialize memory before apply"
    );

    let apply = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "apply",
                    "proposal_id": proposal_id
                }
            }),
        )
        .await
        .expect("proposal apply succeeds");
    let apply_payload = parse_tool_payload(&apply);
    assert_eq!(apply_payload["decision"].as_str(), Some("applied"));
    let current = memory_store
        .lock()
        .await
        .get_by_id(&proposed_memory_id)
        .expect("proposal target lookup")
        .expect("memory exists after apply");
    assert_eq!(current.memory_type, MemoryType::Pattern);

    let events = read_session_events(&event_store, "session-test-consolidate-session");
    assert!(events
        .iter()
        .any(|event| event.kind == EventKind::MemoryConsolidated));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn get_memory_metrics_returns_every_required_signal_or_honest_null() {
    let (handler, _memory_store, event_store, workspace_root, context_cache_path) =
        build_handler("memory-metrics");
    seed_task_events(
        &event_store,
        &workspace_root,
        "session-test-memory-metrics",
        "task-metrics",
    );

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "get_memory_metrics",
                "arguments": {
                    "scope": "session",
                    "render_mode": "diagnostic"
                }
            }),
        )
        .await
        .expect("get_memory_metrics succeeds");
    let payload = parse_tool_payload(&response);
    let signals = payload["signals"].as_array().expect("signals");
    assert_eq!(signals.len(), 9);
    for signal in signals {
        assert!(signal["signal"].as_str().is_some());
        if signal["value"].is_null() {
            assert!(signal["reason_if_null"]
                .as_str()
                .is_some_and(|value| !value.is_empty()));
        } else {
            assert!(signal["value"].as_f64().is_some_and(|value| value >= 0.0));
        }
    }
    let events = read_session_events(&event_store, "session-test-memory-metrics");
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ToolCalled(payload) if payload.tool_name == "get_memory_metrics")));
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ToolResult(payload) if payload.tool_name == "get_memory_metrics")));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn get_event_trace_enforces_workspace_boundary_and_has_stable_pagination() {
    let (handler, _memory_store, event_store, workspace_root, context_cache_path) =
        build_handler("event-trace");
    seed_task_events(
        &event_store,
        &workspace_root,
        "session-test-event-trace",
        "task-trace-a",
    );
    seed_task_events(
        &event_store,
        &workspace_root,
        "session-test-event-trace",
        "task-trace-b",
    );

    let boundary_error = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "get_event_trace",
                "arguments": {
                    "workspace_id": "/tmp/not-this-workspace"
                }
            }),
        )
        .await
        .expect_err("cross-workspace request should fail");
    assert_eq!(boundary_error.0, -32011);

    let first_page = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "get_event_trace",
                "arguments": {
                    "session_id": "session-test-event-trace",
                    "limit": 1
                }
            }),
        )
        .await
        .expect("first trace page succeeds");
    let first_payload = parse_tool_payload(&first_page);
    let next_cursor = first_payload["next_cursor"]
        .as_str()
        .expect("next cursor")
        .to_string();
    let first_event_id = first_payload["events"][0]["event_id"]
        .as_str()
        .expect("event id")
        .to_string();

    let second_page = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "get_event_trace",
                "arguments": {
                    "session_id": "session-test-event-trace",
                    "limit": 1,
                    "cursor": next_cursor
                }
            }),
        )
        .await
        .expect("second trace page succeeds");
    let second_payload = parse_tool_payload(&second_page);
    let second_event_id = second_payload["events"][0]["event_id"]
        .as_str()
        .expect("event id")
        .to_string();
    assert_ne!(first_event_id, second_event_id);

    let events = read_session_events(&event_store, "session-test-event-trace");
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ToolCalled(payload) if payload.tool_name == "get_event_trace")));
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ToolResult(payload) if payload.tool_name == "get_event_trace")));
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
) {
    let workspace_root = unique_test_path(&format!("lattice-admin-tools-{suffix}"));
    std::fs::create_dir_all(&workspace_root).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join(".git")).expect("git dir");
    std::fs::write(
        workspace_root.join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .expect("git head");
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
        format!("session-test-{suffix}"),
        None,
        vec![workspace_root.clone()],
        Arc::new(AtomicBool::new(false)),
        Some(event_writer),
        Vec::new(),
        Vec::new(),
    );
    (
        handler,
        memory_store,
        event_store,
        workspace_root,
        context_cache_path,
    )
}

fn seed_task_events(
    store: &Arc<EventStore>,
    workspace_root: &PathBuf,
    session_id: &str,
    task_id: &str,
) {
    let writer = EventWriter::new(
        store.clone(),
        workspace_root.to_string_lossy().to_string(),
        4096,
    )
    .with_flush_policy(FlushPolicy::Sync);
    writer
        .append(lattice_core::events::PartialEnvelope {
            workspace_id: Some(workspace_root.to_string_lossy().to_string()),
            branch: lattice_core::events::BranchRef {
                name: "main".to_string(),
            },
            session_id: lattice_core::events::SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(lattice_core::events::TaskId {
                value: task_id.to_string(),
            }),
            actor: lattice_core::events::Actor::Assistant {
                model: "test".to_string(),
            },
            kind: EventKind::ToolCalled,
            references: Vec::new(),
            summary: lattice_core::events::CompactSummary::new(format!("Tool called: {task_id}"))
                .expect("summary"),
            payload: EventPayload::ToolCalled(lattice_core::events::ToolCalledPayload {
                call_id: format!("{task_id}:call"),
                tool_name: "prepare_change".to_string(),
                context_handle_id: None,
                source_event_id: None,
                input_summary: "seed".to_string(),
            }),
        })
        .expect("tool called event");
    writer
        .append(lattice_core::events::PartialEnvelope {
            workspace_id: Some(workspace_root.to_string_lossy().to_string()),
            branch: lattice_core::events::BranchRef {
                name: "main".to_string(),
            },
            session_id: lattice_core::events::SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(lattice_core::events::TaskId {
                value: task_id.to_string(),
            }),
            actor: lattice_core::events::Actor::Daemon,
            kind: EventKind::PlanCreated,
            references: Vec::new(),
            summary: lattice_core::events::CompactSummary::new(format!("Plan created: {task_id}"))
                .expect("summary"),
            payload: EventPayload::PlanCreated(lattice_core::events::PlanCreatedPayload {
                context_handle_id: None,
                source_event_id: None,
                memory_ids: Vec::new(),
                step_count: 1,
                plan_summary: "step".to_string(),
            }),
        })
        .expect("plan event");
    writer
        .append(lattice_core::events::PartialEnvelope {
            workspace_id: Some(workspace_root.to_string_lossy().to_string()),
            branch: lattice_core::events::BranchRef {
                name: "main".to_string(),
            },
            session_id: lattice_core::events::SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(lattice_core::events::TaskId {
                value: task_id.to_string(),
            }),
            actor: lattice_core::events::Actor::Daemon,
            kind: EventKind::WorkflowSucceeded,
            references: Vec::new(),
            summary: lattice_core::events::CompactSummary::new(format!(
                "Workflow succeeded: {task_id}"
            ))
            .expect("summary"),
            payload: EventPayload::WorkflowSucceeded(
                lattice_core::events::WorkflowSucceededPayload {
                    workflow_name: "seed".to_string(),
                    terminal_event_id: None,
                    output_context_handle_id: None,
                    memory_ids: Vec::new(),
                    result_summary: "done".to_string(),
                },
            ),
        })
        .expect("workflow success event");
}

fn read_session_events(
    store: &Arc<EventStore>,
    session_id: &str,
) -> Vec<lattice_core::events::EventEnvelope> {
    EventReader::new(store.clone())
        .execute(
            EventQuery::new()
                .session(session_id.to_string())
                .order(QueryOrder::OldestFirst)
                .limit(128),
        )
        .expect("event query")
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
