use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use lattice_core::events::{
    Actor, BranchRef, CompactSummary, EventEnvelope, EventKind, EventPayload, EventQuery,
    EventReader, EventStore, EventWriter, FlushPolicy, PartialEnvelope, QueryOrder, SessionId,
    TaskId, ToolCalledPayload,
};
use lattice_core::graph::model::{CodeGraph, EdgeKind};
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryStore;
use lattice_core::query::QueryEngine;
use lattice_core::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use lattice_core::storage::GraphStore;
use lattice_core::symbols::{Language, SymbolId, SymbolKind};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::rpc::event_capture::{EventCapture, ToolOutcome};
use crate::rpc::mcp::McpHandler;
use crate::rpc::server::RequestHandler;
use crate::rpc::session_metrics::SessionMetrics;

use super::outcome_capture::WorkflowOutcomeRecorder;

#[tokio::test]
async fn prepare_plan_patch_and_manual_outcome_emit_expected_sequence() {
    let (
        handler,
        _memory_store,
        event_store,
        _indexer,
        _graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("prepare-plan");
    let prepare = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "prepare_change",
                "arguments": {
                    "query": "Investigate login workflow",
                    "entry_files": ["src/auth.ts"],
                    "entry_symbols": ["loginUser"],
                    "render": "json"
                }
            }),
        )
        .await
        .expect("prepare_change succeeds");
    let prepare_payload = parse_tool_payload(&prepare);
    let plan = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "plan_edit",
                "arguments": {
                    "query": "Investigate login workflow",
                    "entry_files": ["src/auth.ts"],
                    "entry_symbols": ["loginUser"],
                    "render": "json"
                }
            }),
        )
        .await
        .expect("plan_edit succeeds");
    let _plan_payload = parse_tool_payload(&plan);
    append_patch_applied(
        &event_store,
        &workspace_root,
        "session-test-prepare-plan",
        "session-test-prepare-plan:default",
    );
    handler
        .handle("lattice/tool_call",
            json!({
                "name": "record_workflow_outcome",
                "arguments": {
                    "task": "login workflow patch",
                    "status": "success",
                    "summary": "patched login route",
                    "context_handle": payload_string(&prepare_payload, &["context_handle", "h"]).expect("prepare handle"),
                    "files": ["src/auth.ts"],
                    "symbols": ["loginUser"],
                    "tests": ["tests/auth_test.rs"]
                }
            }),
        )
        .await
        .expect("record_workflow_outcome succeeds");

    let events = read_session_events(&event_store, "session-test-prepare-plan");
    assert_in_order(
        &events,
        &[
            EventKind::ToolCalled,
            EventKind::ToolResult,
            EventKind::PlanCreated,
            EventKind::WorkflowSucceeded,
            EventKind::PatchApplied,
            EventKind::WorkflowSucceeded,
        ],
    );
    let workflow_events = workflow_events(&events);
    assert_eq!(workflow_events.len(), 3);
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn get_context_capsule_and_expand_context_backlink_to_origin_without_redundant_payload() {
    let (
        handler,
        _memory_store,
        event_store,
        _indexer,
        _graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("expand-context");
    let capsule = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "get_context_capsule",
                "arguments": {
                    "query": "Investigate login workflow",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("context capsule succeeds");
    let capsule_payload = parse_tool_payload(&capsule);
    let focus = capsule_payload["suggested_next_expansion"]["focus"]
        .as_str()
        .unwrap_or("symbol:loginUser");
    let expand = handler
        .handle("lattice/tool_call",
            json!({
                "name": "expand_context",
                "arguments": {
                    "handle": payload_string(&capsule_payload, &["context_handle", "h"]).expect("context handle"),
                    "focus": focus,
                    "max_tokens": 800
                }
            }),
        )
        .await
        .expect("expand context succeeds");
    let expand_payload = parse_tool_payload(&expand);
    assert_eq!(
        payload_string(&expand_payload, &["context_handle", "h"]),
        payload_string(&capsule_payload, &["context_handle", "h"])
    );
    assert!(
        expand["content"][0]["text"]
            .as_str()
            .expect("expand text")
            .len()
            < capsule["content"][0]["text"]
                .as_str()
                .expect("capsule text")
                .len()
    );

    let events = workflow_events(&read_session_events(
        &event_store,
        "session-test-expand-context",
    ));
    assert_eq!(events.len(), 2);
    assert!(events[1]
        .references
        .iter()
        .any(|reference| matches!(reference, lattice_core::events::StableRef::EventRef(event_id) if event_id == &events[0].event_id)));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn diagnose_failure_and_prepare_change_reuse_failure_anchors() {
    let (
        handler,
        _memory_store,
        event_store,
        _indexer,
        _graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("diagnose-prepare");
    let diagnose = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "diagnose_failure",
                "arguments": {
                    "input": "thread panicked at src/auth.ts:12: loginUser failed",
                    "kind": "test",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("diagnose succeeds");
    let diagnose_payload = parse_tool_payload(&diagnose);
    let anchor_file = diagnose_payload["workflow_record"]["resolved_anchors"][0]["value"]
        ["repo_relative_path"]
        .as_str()
        .unwrap_or("src/auth.ts");
    let prepare = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "prepare_change",
                "arguments": {
                    "query": "Fix the login failure",
                    "entry_files": [anchor_file],
                    "entry_symbols": ["loginUser"],
                    "render": "json"
                }
            }),
        )
        .await
        .expect("prepare succeeds");
    let prepare_payload = parse_tool_payload(&prepare);
    assert!(prepare_payload["workflow_record"]["selected_candidates"]
        .as_array()
        .expect("selected")
        .iter()
        .any(|item| item
            .as_str()
            .is_some_and(|value| value.contains("src/auth.ts"))));
    let events = workflow_events(&read_session_events(
        &event_store,
        "session-test-diagnose-prepare",
    ));
    assert_eq!(events.len(), 2);
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn save_memory_and_verify_explain_emit_linked_outcomes_and_verified_status() {
    let (
        handler,
        _memory_store,
        event_store,
        indexer,
        graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("save-verify");
    seed_indexed_file(
        &workspace_root,
        &indexer,
        &graph_store,
        "src/auth.rs",
        "fn refresh_token() {}\n",
    )
    .await;
    let save = handler
        .handle(
            "lattice/tool_call",
            json!({
                    "name": "save_memory",
                    "arguments": {
                        "content": "refresh token exists",
                    "memory_class": "constraint",
                    "scope": "repo",
                    "confidence": 0.9,
                    "confidence_reason": "verified from code",
                    "freshness_policy": "manual_review",
                    "linked_files": ["src/auth.rs"],
                    "evidence": [{
                        "kind": "file",
                        "reference": "src/auth.rs",
                        "captured_at": 1
                    }]
                }
            }),
        )
        .await
        .expect("save succeeds");
    let save_payload = parse_tool_payload(&save);
    let verify = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "verify_explain_memory",
                "arguments": {
                    "memory_id": save_payload["memory_id"].as_str().expect("memory id"),
                    "render_mode": "full"
                }
            }),
        )
        .await
        .expect("verify succeeds");
    let verify_payload = parse_tool_payload(&verify);
    assert_eq!(verify_payload["status"].as_str(), Some("verified"));
    let events = workflow_events(&read_session_events(
        &event_store,
        "session-test-save-verify",
    ));
    assert_eq!(events.len(), 2);
    assert!(events[1]
        .references
        .iter()
        .any(|reference| matches!(reference, lattice_core::events::StableRef::EventRef(event_id) if event_id == &events[0].event_id)));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn consolidate_session_apply_chain_and_scope_protection_survive_recorder() {
    let (
        handler,
        _memory_store,
        event_store,
        _indexer,
        _graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("consolidate-apply");
    seed_task_events(
        &event_store,
        &workspace_root,
        "session-test-consolidate-apply",
        "task-consolidate",
    );
    let consolidate = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "consolidate_session",
                "arguments": {
                    "session_id": "session-test-consolidate-apply",
                    "mode": "post_task"
                }
            }),
        )
        .await
        .expect("consolidate succeeds");
    let proposal_id = parse_tool_payload(&consolidate)["proposals"][0]["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
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
        .expect("apply succeeds");
    assert_eq!(
        parse_tool_payload(&apply)["decision"].as_str(),
        Some("applied")
    );
    let denied = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "verify_explain_memory",
                "arguments": {
                    "memory_id": {
                        "workspace_id": "/tmp/foreign-workspace",
                        "ulid": "foreign-memory"
                    },
                    "render_mode": "compact"
                }
            }),
        )
        .await;
    assert!(denied.is_err());
    let workflow_events = workflow_events(&read_session_events(
        &event_store,
        "session-test-consolidate-apply",
    ));
    assert_eq!(workflow_events.len(), 4);
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[test]
#[ignore = "p99 timing assertion; run via --include-ignored so it is not affected by parallel test CPU contention"]
fn every_outer_call_emits_exactly_one_outcome_event_and_excluded_candidates_are_recorded() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let writer = Arc::new(
        EventWriter::new(store.clone(), "workspace-main".to_string(), 4096)
            .with_flush_policy(FlushPolicy::Sync),
    );
    let capture = EventCapture::new(
        writer,
        "workspace-main".to_string(),
        "main".to_string(),
        SessionId {
            value: "session-budget".to_string(),
        },
    )
    .expect("capture");
    capture
        .begin_task(
            TaskId {
                value: "task-budget".to_string(),
            },
            "budget task",
        )
        .expect("task started");
    let recorder = WorkflowOutcomeRecorder::new();
    let mut metrics = SessionMetrics::new();
    let result = wrapped_result(json!({
        "overview": "Investigate login workflow",
        "workflow_record": {
            "input": "Investigate login workflow",
            "resolved_anchors": [{
                "kind": "File",
                "value": {
                    "workspace_id": "workspace-main",
                    "repo_relative_path": "src/auth.ts",
                    "content_hash": "unknown"
                }
            }],
            "selected_candidates": ["src/auth.ts"],
            "excluded_high_scoring_candidates": [
                "src/session.ts rejected: ranked pivot was not selected for the compact result"
            ],
            "working_memory_summary": "rationale"
        }
    }));
    let budget_result = wrapped_result(json!({
        "overview": "Investigate login workflow",
        "context_handle": "ctx-budget",
        "workflow_record": {
            "input": "Investigate login workflow",
            "resolved_anchors": [],
            "selected_candidates": ["src/auth.ts"],
            "excluded_high_scoring_candidates": [],
            "working_memory_summary": "rationale"
        }
    }));
    let mut latencies = Vec::new();
    for _ in 0..40 {
        let called = capture
            .record_tool_called(
                "get_context_capsule",
                &json!({"query": "Investigate login workflow"}),
            )
            .expect("tool call");
        let terminal = capture
            .record_tool_result(
                "get_context_capsule",
                &ToolOutcome::Success(budget_result.clone()),
                called,
            )
            .expect("tool result");
        let started = Instant::now();
        recorder
            .record(
                &capture,
                &mut metrics,
                "get_context_capsule",
                &json!({"query": "Investigate login workflow"}),
                &Ok(budget_result.clone()),
                &terminal,
            )
            .expect("record outcome");
        latencies.push(started.elapsed().as_micros() as u64);
    }
    latencies.sort_unstable();
    let p99_index = ((latencies.len() as f64) * 0.99).ceil() as usize - 1;
    assert!(
        latencies[p99_index] <= 5_000,
        "p99 {}us exceeded 5ms",
        latencies[p99_index]
    );

    let events = EventReader::new(store)
        .execute(
            EventQuery::new()
                .session("session-budget".to_string())
                .order(QueryOrder::OldestFirst)
                .limit(256),
        )
        .expect("events");
    let recorded_workflow_events = workflow_events(&events);
    assert_eq!(recorded_workflow_events.len(), 40);
    let extra_called = capture
        .record_tool_called(
            "prepare_change",
            &json!({"query": "Investigate login workflow"}),
        )
        .expect("extra tool call");
    let extra_terminal = capture
        .record_tool_result(
            "prepare_change",
            &ToolOutcome::Success(result.clone()),
            extra_called,
        )
        .expect("extra tool result");
    recorder
        .record(
            &capture,
            &mut metrics,
            "prepare_change",
            &json!({"query": "Investigate login workflow", "entry_files": ["src/auth.ts"]}),
            &Ok(result),
            &extra_terminal,
        )
        .expect("record excluded candidates");
    let final_events = EventReader::new(capture.writer().store())
        .execute(
            EventQuery::new()
                .session("session-budget".to_string())
                .order(QueryOrder::OldestFirst)
                .limit(512),
        )
        .expect("events");
    let final_workflow_events = workflow_events(&final_events);
    let payload = match &final_workflow_events
        .last()
        .expect("workflow event")
        .payload
    {
        EventPayload::WorkflowSucceeded(payload) => payload,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert!(payload.result_summary.contains("excluded"));
}

fn build_handler(
    suffix: &str,
) -> (
    McpHandler,
    Arc<Mutex<MemoryStore>>,
    Arc<EventStore>,
    Arc<Mutex<Indexer>>,
    Arc<Mutex<GraphStore>>,
    PathBuf,
    PathBuf,
) {
    let workspace_root = unique_test_path(&format!("lattice-composition-{suffix}"));
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
    let graph_store = Arc::new(Mutex::new(
        GraphStore::open_in_memory().expect("graph store"),
    ));
    let indexer = Arc::new(Mutex::new(Indexer::new(workspace_root.clone())));
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
        Arc::new(Mutex::new(QueryEngine::new(build_graph(), None, None))),
        indexer.clone(),
        memory_store.clone(),
        graph_store.clone(),
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
        indexer,
        graph_store,
        workspace_root,
        context_cache_path,
    )
}

async fn seed_indexed_file(
    workspace_root: &PathBuf,
    indexer: &Arc<Mutex<Indexer>>,
    graph_store: &Arc<Mutex<GraphStore>>,
    rel_path: &str,
    content: &str,
) {
    let path = workspace_root.join(rel_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(&path, content).expect("write file");
    let mut indexer = indexer.lock().await;
    indexer
        .index_file_content(rel_path, content)
        .expect("index content");
    let graph = indexer.graph().clone();
    let parsed_files = indexer.parsed_files().clone();
    drop(indexer);

    let graph_store = graph_store.lock().await;
    graph_store.save_graph(&graph).expect("save graph");
    graph_store
        .save_parsed_files(&parsed_files)
        .expect("save parsed files");
    graph_store
        .save_file_index(&[FileIndexEntry {
            file: rel_path.to_string(),
            content_hash: "hash".to_string(),
            mtime_ns: 1,
            size_bytes: content.len() as i64,
            parser_version: FILE_INDEX_PARSER_VERSION,
            schema_version: FILE_INDEX_SCHEMA_VERSION,
            last_indexed_at: 10,
        }])
        .expect("save file index");
}

fn append_patch_applied(
    store: &Arc<EventStore>,
    workspace_root: &PathBuf,
    session_id: &str,
    task_id: &str,
) {
    EventWriter::new(
        store.clone(),
        workspace_root.to_string_lossy().to_string(),
        4096,
    )
    .with_flush_policy(FlushPolicy::Sync)
    .append(PartialEnvelope {
        workspace_id: Some(workspace_root.to_string_lossy().to_string()),
        branch: BranchRef {
            name: "main".to_string(),
        },
        session_id: SessionId {
            value: session_id.to_string(),
        },
        task_id: Some(TaskId {
            value: task_id.to_string(),
        }),
        actor: Actor::Daemon,
        kind: EventKind::PatchApplied,
        references: Vec::new(),
        summary: CompactSummary::new("Patch applied".to_string()).expect("summary"),
        payload: EventPayload::PatchApplied(lattice_core::events::PatchAppliedPayload {
            patch_id: "patch-simulated".to_string(),
            source_event_id: None,
            file_ids: Vec::new(),
            symbol_ids: Vec::new(),
            lines_added: 3,
            lines_removed: 1,
        }),
    })
    .expect("patch event");
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
        .append(PartialEnvelope {
            workspace_id: Some(workspace_root.to_string_lossy().to_string()),
            branch: BranchRef {
                name: "main".to_string(),
            },
            session_id: SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(TaskId {
                value: task_id.to_string(),
            }),
            actor: Actor::Tool {
                name: "prepare_change".to_string(),
            },
            kind: EventKind::ToolCalled,
            references: Vec::new(),
            summary: CompactSummary::new(format!("Tool called: {task_id}")).expect("summary"),
            payload: EventPayload::ToolCalled(ToolCalledPayload {
                call_id: format!("{task_id}:call"),
                tool_name: "prepare_change".to_string(),
                context_handle_id: None,
                source_event_id: None,
                input_summary: "seed".to_string(),
            }),
        })
        .expect("tool called");
    writer
        .append(PartialEnvelope {
            workspace_id: Some(workspace_root.to_string_lossy().to_string()),
            branch: BranchRef {
                name: "main".to_string(),
            },
            session_id: SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(TaskId {
                value: task_id.to_string(),
            }),
            actor: Actor::Daemon,
            kind: EventKind::PlanCreated,
            references: Vec::new(),
            summary: CompactSummary::new(format!("Plan created: {task_id}")).expect("summary"),
            payload: EventPayload::PlanCreated(lattice_core::events::PlanCreatedPayload {
                context_handle_id: None,
                source_event_id: None,
                memory_ids: Vec::new(),
                step_count: 1,
                plan_summary: "seed plan".to_string(),
            }),
        })
        .expect("plan created");
    writer
        .append(PartialEnvelope {
            workspace_id: Some(workspace_root.to_string_lossy().to_string()),
            branch: BranchRef {
                name: "main".to_string(),
            },
            session_id: SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(TaskId {
                value: task_id.to_string(),
            }),
            actor: Actor::Daemon,
            kind: EventKind::WorkflowSucceeded,
            references: Vec::new(),
            summary: CompactSummary::new(format!("Workflow succeeded: {task_id}"))
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
        .expect("workflow succeeded");
}

fn workflow_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::WorkflowSucceeded | EventKind::WorkflowFailed
            )
        })
        .cloned()
        .collect()
}

fn assert_in_order(events: &[EventEnvelope], expected: &[EventKind]) {
    let mut offset = 0;
    for kind in expected {
        let Some(index) = events[offset..]
            .iter()
            .position(|event| &event.kind == kind)
        else {
            panic!("missing event kind: {kind:?}");
        };
        offset += index + 1;
    }
}

fn read_session_events(store: &Arc<EventStore>, session_id: &str) -> Vec<EventEnvelope> {
    EventReader::new(store.clone())
        .execute(
            EventQuery::new()
                .session(session_id.to_string())
                .order(QueryOrder::OldestFirst)
                .limit(256),
        )
        .expect("event query")
}

fn parse_tool_payload(response: &Value) -> Value {
    let text = response["content"][0]["text"].as_str().expect("tool text");
    serde_json::from_str(text).unwrap_or_else(|_| {
        let (_, json_block) = text
            .split_once("```json\n")
            .expect("structured payload block");
        serde_json::from_str(json_block.strip_suffix("\n```").expect("json fence"))
            .expect("tool payload json")
    })
}

fn payload_string<'a>(payload: &'a Value, keys: &[&str]) -> Option<&'a str> {
    let object = payload.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
}

fn wrapped_result(payload: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string(&payload).expect("payload json")
        }]
    })
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

fn build_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    let login = id("src/auth.ts", "loginUser", 0);
    let session = id("src/session.ts", "createSession", 10);
    let route = id("src/routes/auth.ts", "loginRoute", 20);
    let test = id("tests/auth_test.rs", "login_user_rejects_timeout", 30);
    let doc = id("docs/auth.md", "Login Flow", 40);
    add_node(
        &mut graph,
        login.clone(),
        SymbolKind::Function,
        "loginUser",
        "src/auth.ts",
        10,
        Language::Rust,
    );
    add_node(
        &mut graph,
        session.clone(),
        SymbolKind::Function,
        "createSession",
        "src/session.ts",
        30,
        Language::Rust,
    );
    add_node(
        &mut graph,
        route.clone(),
        SymbolKind::Function,
        "loginRoute",
        "src/routes/auth.ts",
        6,
        Language::Rust,
    );
    add_node(
        &mut graph,
        test.clone(),
        SymbolKind::Function,
        "login_user_rejects_timeout",
        "tests/auth_test.rs",
        4,
        Language::Rust,
    );
    add_node(
        &mut graph,
        doc.clone(),
        SymbolKind::Module,
        "Login Flow",
        "docs/auth.md",
        1,
        Language::Markdown,
    );
    graph.add_edge(&route, &login, EdgeKind::Calls);
    graph.add_edge(&login, &session, EdgeKind::Calls);
    graph.add_edge(&test, &route, EdgeKind::Calls);
    graph.add_edge(&doc, &login, EdgeKind::Mentions);
    graph
}

fn add_node(
    graph: &mut CodeGraph,
    id: SymbolId,
    kind: SymbolKind,
    name: &str,
    file: &str,
    line: usize,
    language: Language,
) {
    graph.add_node(
        id,
        kind,
        name.to_string(),
        format!("fn {name}()"),
        format!("fn {name}() {{}}\n"),
        file.to_string(),
        line,
        line + 2,
        true,
        language,
    );
}

fn id(file: &str, symbol: &str, salt: u64) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: symbol.to_string(),
        byte_offset: salt as usize,
    }
}
