use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use lattice_core::events::{
    EventKind, EventPayload, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy,
    QueryOrder, SessionId, SessionScope,
};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryStore;
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use serde_json::json;
use tokio::sync::Mutex;

use super::event_capture::{EventCapture, ToolOutcome};
use super::mcp::McpHandler;
use super::server::RequestHandler;

#[tokio::test]
async fn dispatch_success_records_task_tool_result_and_workflow_events() {
    let fixture = CaptureFixture::new("success");
    let response = RequestHandler::handle(
        &fixture.handler,
        "tools/call",
        json!({"name": "index_status", "arguments": {}}),
    )
    .await
    .expect("index_status succeeds");

    assert!(response["content"].is_array());
    let events = fixture.events();
    assert_event_kinds(
        &events,
        &[
            EventKind::AssistantTaskStarted,
            EventKind::ToolCalled,
            EventKind::ToolResult,
            EventKind::WorkflowSucceeded,
        ],
    );
}

#[tokio::test]
async fn dispatch_error_records_tool_result_and_preserves_json_rpc_error() {
    let fixture = CaptureFixture::new("error");
    let error = RequestHandler::handle(
        &fixture.handler,
        "tools/call",
        json!({"name": "prepare_change", "arguments": {}}),
    )
    .await
    .expect_err("missing query still returns JSON-RPC error");

    assert_eq!(error.0, -32602);
    assert!(error.1.contains("Missing required parameter: query"));
    let events = fixture.events();
    assert_event_kinds(
        &events,
        &[
            EventKind::AssistantTaskStarted,
            EventKind::ToolCalled,
            EventKind::ToolResult,
            EventKind::WorkflowFailed,
        ],
    );
    assert_failed_tool_result(&events);
}

#[tokio::test]
async fn dispatch_error_tail_includes_tool_called_and_failed_result_events() {
    let fixture = CaptureFixture::new("error-tail");
    let _ = RequestHandler::handle(
        &fixture.handler,
        "tools/call",
        json!({"name": "prepare_change", "arguments": {}}),
    )
    .await
    .expect_err("missing query still returns JSON-RPC error");

    let tail = fixture
        .reader
        .tail(
            SessionScope {
                session_id: SessionId {
                    value: fixture.session_id.clone(),
                },
            },
            3,
        )
        .expect("session tail loads");

    let kinds = tail.iter().map(|event| event.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            EventKind::WorkflowFailed,
            EventKind::ToolResult,
            EventKind::ToolCalled
        ]
    );
    assert_failed_tool_result(&tail);
}

#[tokio::test]
async fn all_dispatched_workflow_tools_emit_call_and_result_events() {
    let fixture = CaptureFixture::new("all-tools");
    for tool_name in instrumented_tool_names() {
        let _ = RequestHandler::handle(
            &fixture.handler,
            "tools/call",
            json!({"name": tool_name, "arguments": {}}),
        )
        .await;
    }

    let events = fixture.events();
    let called = events
        .iter()
        .filter(|event| event.kind == EventKind::ToolCalled)
        .count();
    let results = events
        .iter()
        .filter(|event| event.kind == EventKind::ToolResult)
        .count();
    assert_eq!(called, instrumented_tool_names().len());
    assert_eq!(results, instrumented_tool_names().len());
}

#[test]
fn event_capture_new_rejects_workspace_mismatch() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = Arc::new(EventWriter::new(store, "workspace-a".to_string(), 4096));
    let error = EventCapture::new(
        writer,
        "workspace-b".to_string(),
        "main".to_string(),
        SessionId {
            value: "session-a".to_string(),
        },
    )
    .expect_err("workspace mismatch is rejected");

    assert_eq!(
        error.to_string(),
        "event capture workspace `workspace-b` does not match writer workspace `workspace-a`"
    );
}

#[test]
fn batched_flush_writer_records_multiple_tool_calls_for_same_session() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = Arc::new(
        EventWriter::new(store.clone(), "workspace-a".to_string(), 4096)
            .with_flush_policy(FlushPolicy::Batched { interval_ms: 250 }),
    );
    let capture = EventCapture::new(
        writer,
        "workspace-a".to_string(),
        "main".to_string(),
        SessionId {
            value: "session-batched".to_string(),
        },
    )
    .expect("capture creates");

    let first = capture
        .record_tool_called("prepare_change", &json!({"query": "alpha"}))
        .expect("first tool call captured");
    let second = capture
        .record_tool_called("get_context_capsule", &json!({"query": "beta"}))
        .expect("second tool call captured");
    capture
        .record_tool_result("prepare_change", &ToolOutcome::Success(json!({})), first)
        .expect("first result captured");
    capture
        .record_tool_result(
            "get_context_capsule",
            &ToolOutcome::Success(json!({})),
            second,
        )
        .expect("second result captured");

    let reader = EventReader::new(store);
    let events = EventQuery::new()
        .session("session-batched")
        .order(QueryOrder::OldestFirst)
        .execute(&reader)
        .expect("events read");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::ToolCalled)
            .count(),
        2
    );
}

struct CaptureFixture {
    handler: McpHandler,
    reader: EventReader,
    session_id: String,
    _workspace_root: PathBuf,
}

impl CaptureFixture {
    fn new(name: &str) -> Self {
        let session_id = format!("session-{name}");
        let workspace_root = unique_test_path(name);
        std::fs::create_dir_all(&workspace_root).expect("workspace creates");
        let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
        let writer = Arc::new(
            EventWriter::new(
                store.clone(),
                workspace_root.to_string_lossy().to_string(),
                4096,
            )
            .with_flush_policy(FlushPolicy::Batched { interval_ms: 250 }),
        );
        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            workspace_root.join("context_handles.json"),
            session_id.clone(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            Some(writer),
            Vec::new(),
            Vec::new(),
        );
        Self {
            handler,
            reader: EventReader::new(store),
            session_id,
            _workspace_root: workspace_root,
        }
    }

    fn events(&self) -> Vec<lattice_core::events::EventEnvelope> {
        EventQuery::new()
            .session(self.session_id.clone())
            .order(QueryOrder::OldestFirst)
            .execute(&self.reader)
            .expect("events read")
    }
}

fn assert_event_kinds(events: &[lattice_core::events::EventEnvelope], expected: &[EventKind]) {
    let actual = events.iter().map(|event| event.kind).collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

fn assert_failed_tool_result(events: &[lattice_core::events::EventEnvelope]) {
    let result = events
        .iter()
        .find(|event| event.kind == EventKind::ToolResult)
        .expect("tool result exists");
    let EventPayload::ToolResult(payload) = &result.payload else {
        panic!("expected tool result payload");
    };
    assert_eq!(
        payload.status,
        lattice_core::events::ToolResultStatus::Failed
    );
}

fn instrumented_tool_names() -> &'static [&'static str] {
    &[
        "get_context_capsule",
        "prepare_change",
        "plan_edit",
        "trace_scenario",
        "find_relevant_tests",
        "impact_from_diff",
        "get_working_set_context",
        "summarize_subsystem",
        "get_repo_playbook",
        "get_docs_capsule",
        "get_backlinks",
        "get_outgoing_links",
        "find_stale_docs",
        "diagnose_failure",
        "record_workflow_outcome",
        "expand_context",
        "get_symbol",
        "get_dependents",
        "get_dependencies",
        "get_impact_graph",
        "search_symbols",
        "get_skeleton",
        "search_memory",
        "list_stale_memories",
        "search_logic_flow",
        "submit_lsp_edges",
        "workspace_setup",
        "index_status",
        "get_session_metrics",
        "get_project_rules",
    ]
}

fn unique_test_path(name: &str) -> PathBuf {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_micros();
    std::env::temp_dir().join(format!("lattice-event-capture-{name}-{micros}"))
}
