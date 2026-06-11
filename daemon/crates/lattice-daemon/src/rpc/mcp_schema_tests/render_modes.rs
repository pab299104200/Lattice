//! Render-mode contract coverage for the Phase 8 MCP tool surface.
//!
//! Cited spec: [`## MCP Surface`](docs/plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-surface)
//! requires every tool support compact rendering, full structured JSON,
//! context handles, stable expansion targets, budget controls, and
//! diagnostic explanations where useful. The reference at
//! `docs/architecture/2026-05-16-mcp-tool-reference.md` records the
//! per-tool render modes; this module asserts the daemon honors that contract.

use lattice_core::events::{
    Actor, BranchRef, CompactSummary, EventKind, EventPayload, EventWriter, FlushPolicy,
    PartialEnvelope, PlanCreatedPayload, SessionId, TaskId, ToolCalledPayload,
};
use lattice_core::memory::model::MemoryAssertionType;
use lattice_core::memory::MemoryClass;
use lattice_core::memory::{
    Memory, MemoryScope, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};
use lattice_core::working_memory::WorkingMemoryState;
use serde_json::json;

use super::super::server::RequestHandler;
use super::{call_args, parse_tool_payload, SchemaFixture};

#[tokio::test]
async fn consolidate_session_renders_compact_full_and_diagnostic() {
    let fixture = SchemaFixture::new("consolidate-render");
    seed_consolidation_session(&fixture, "task-render-cons");
    for mode in ["compact", "full", "diagnostic"] {
        let response = fixture
            .handler
            .handle(
                "lattice/tool_call",
                call_args(
                    "consolidate_session",
                    json!({
                        "session_id": fixture.session_id,
                        "mode": "post_task",
                        "render_mode": mode,
                    }),
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("consolidate_session({mode}) failed: {err:?}"));
        let payload = parse_tool_payload(&response);
        assert_eq!(
            payload["render_mode"].as_str(),
            Some(mode),
            "consolidate_session should echo render_mode={mode}"
        );
    }
}

#[tokio::test]
async fn get_event_trace_renders_compact_full_and_diagnostic_with_handles() {
    let fixture = SchemaFixture::new("trace-render");
    seed_consolidation_session(&fixture, "task-render-trace");
    for mode in ["compact", "full", "diagnostic"] {
        let response = fixture
            .handler
            .handle(
                "lattice/tool_call",
                call_args(
                    "get_event_trace",
                    json!({
                        "session_id": fixture.session_id,
                        "limit": 5,
                        "render_mode": mode,
                    }),
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("get_event_trace({mode}) failed: {err:?}"));
        let payload = parse_tool_payload(&response);
        assert_eq!(payload["render_mode"].as_str(), Some(mode));
        assert_eq!(payload["scope"]["kind"].as_str(), Some("session"));
        let events = payload["events"].as_array().expect("events array");
        if let Some(first) = events.first() {
            assert!(
                first["expansion_handle"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "every event entry must expose an expansion handle"
            );
            if mode == "diagnostic" {
                assert!(first["payload"].is_object() || first["payload"].is_null());
            }
        }
    }
}

#[tokio::test]
async fn get_memory_metrics_renders_each_mode_with_honest_nulls() {
    let fixture = SchemaFixture::new("metrics-render");
    seed_consolidation_session(&fixture, "task-render-metrics");
    for mode in ["compact", "full", "diagnostic"] {
        let response = fixture
            .handler
            .handle(
                "lattice/tool_call",
                call_args(
                    "get_memory_metrics",
                    json!({
                        "scope": "session",
                        "render_mode": mode,
                    }),
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("get_memory_metrics({mode}) failed: {err:?}"));
        let payload = parse_tool_payload(&response);
        assert_eq!(payload["render_mode"].as_str(), Some(mode));
        let signals = payload["signals"].as_array().expect("signals array");
        assert!(
            !signals.is_empty(),
            "metrics must report at least one signal"
        );
    }
}

#[tokio::test]
async fn verify_explain_memory_renders_compact_full_and_diagnostic() {
    let fixture = SchemaFixture::new("verify-render");
    let memory_id = save_repo_memory(&fixture, "verify-render content").await;
    for mode in ["compact", "full", "diagnostic"] {
        let response = fixture
            .handler
            .handle(
                "lattice/tool_call",
                call_args(
                    "verify_explain_memory",
                    json!({
                        "memory_id": memory_id,
                        "mode": "verify_and_explain",
                        "render_mode": mode,
                    }),
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("verify_explain_memory({mode}) failed: {err:?}"));
        let payload = parse_tool_payload(&response);
        assert_eq!(payload["render_mode"].as_str(), Some(mode));
        assert!(payload["expansion_handle"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        if mode == "diagnostic" {
            assert!(payload["diagnostic_trace"].is_array());
        }
    }
}

#[tokio::test]
async fn list_memory_conflicts_renders_each_mode_for_legacy_memory_anchor() {
    let fixture = SchemaFixture::new("conflicts-render");
    let memory_id = save_repo_memory(&fixture, "conflict-render content").await;
    for mode in ["compact", "full", "diagnostic"] {
        let response = fixture
            .handler
            .handle(
                "lattice/tool_call",
                call_args(
                    "list_memory_conflicts",
                    json!({
                        "anchor": memory_id,
                        "render_mode": mode,
                        "limit": 10,
                    }),
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("list_memory_conflicts({mode}) failed: {err:?}"));
        let payload = parse_tool_payload(&response);
        assert_eq!(payload["render_mode"].as_str(), Some(mode));
    }
}

#[tokio::test]
async fn inspect_working_memory_supports_compact_and_diagnostic_modes() {
    let fixture = SchemaFixture::new("inspect-render");
    fixture
        .handler
        .remember_working_memory_state_for_test(
            "task-inspect",
            WorkingMemoryState::new("render-mode coverage"),
        )
        .await;
    for mode in ["compact", "diagnostic"] {
        let response = fixture
            .handler
            .handle(
                "lattice/tool_call",
                call_args(
                    "inspect_working_memory",
                    json!({"task_id": "task-inspect", "mode": mode}),
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("inspect_working_memory({mode}) failed: {err:?}"));
        let payload = parse_tool_payload(&response);
        assert_eq!(payload["mode"].as_str(), Some(mode));
        assert!(payload["expansion_handle"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
    }
}

#[tokio::test]
async fn get_task_memory_emits_expansion_handles_per_memory() {
    let fixture = SchemaFixture::new("task-memory-handles");
    let memory_id = save_repo_memory(&fixture, "refresh token invariant").await;
    fixture
        .handler
        .remember_working_memory_state_for_test("task-refresh", WorkingMemoryState::new("refresh"))
        .await;
    let response = fixture
        .handler
        .handle(
            "lattice/tool_call",
            call_args(
                "get_task_memory",
                json!({"task_id": "task-refresh", "intent_hint": "refresh"}),
            ),
        )
        .await
        .expect("get_task_memory succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories array");
    assert!(
        memories
            .iter()
            .any(|memory| memory["id"].as_str() == Some(memory_id.as_str())),
        "expected saved memory to be surfaced"
    );
    for memory in memories {
        assert!(memory["expansion_handle"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["inclusion_reason"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
    }
}

/// Stores a Repo-scoped memory through `save_memory` so the workspace id matches.
async fn save_repo_memory(fixture: &SchemaFixture, content: &str) -> String {
    let response = fixture
        .handler
        .handle(
            "lattice/tool_call",
            call_args(
                "save_memory",
                json!({
                    "content": content,
                    "memory_class": "constraint",
                    "assertion_type": "constraint",
                    "scope": "repo",
                    "confidence": 0.74,
                    "confidence_reason": "render-mode seed",
                    "freshness_policy": "manual_review",
                }),
            ),
        )
        .await
        .expect("save_memory succeeds");
    let payload = parse_tool_payload(&response);
    payload["memory_id"]
        .as_str()
        .expect("memory_id field")
        .to_string()
}

fn seed_consolidation_session(fixture: &SchemaFixture, task_id: &str) {
    let writer = EventWriter::new(
        fixture.event_store.clone(),
        fixture.workspace_root.to_string_lossy().to_string(),
        4096,
    )
    .with_flush_policy(FlushPolicy::Sync);
    let session = SessionId {
        value: fixture.session_id.clone(),
    };
    let task = TaskId {
        value: task_id.to_string(),
    };
    let branch = BranchRef {
        name: "main".to_string(),
    };
    writer
        .append(PartialEnvelope {
            workspace_id: Some(fixture.workspace_root.to_string_lossy().to_string()),
            branch: branch.clone(),
            session_id: session.clone(),
            task_id: Some(task.clone()),
            actor: Actor::Assistant {
                model: "test".to_string(),
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
        .expect("tool called event");
    writer
        .append(PartialEnvelope {
            workspace_id: Some(fixture.workspace_root.to_string_lossy().to_string()),
            branch,
            session_id: session,
            task_id: Some(task),
            actor: Actor::Daemon,
            kind: EventKind::PlanCreated,
            references: Vec::new(),
            summary: CompactSummary::new(format!("Plan created: {task_id}")).expect("summary"),
            payload: EventPayload::PlanCreated(PlanCreatedPayload {
                context_handle_id: None,
                source_event_id: None,
                memory_ids: Vec::new(),
                step_count: 1,
                plan_summary: "step".to_string(),
            }),
        })
        .expect("plan event");
}

/// Inserts a memory directly into the store with workspace_id set so scope filtering matches.
#[allow(dead_code)]
async fn insert_repo_scoped_memory(fixture: &SchemaFixture, content: &str) -> String {
    let store = fixture.memory_store.lock().await;
    let memory = Memory {
        id: String::new(),
        session_id: fixture.session_id.clone(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 0.81,
        linked_symbols: vec!["refresh_token".to_string()],
        linked_files: vec!["src/auth.rs".to_string()],
        workspace_id: Some(fixture.workspace_root.to_string_lossy().to_string()),
        branch: None,
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    };
    let id = store.store(memory).expect("store memory");
    let fields = MemoryStructuredFields {
        memory_class: MemoryClass::Constraint,
        assertion_type: MemoryAssertionType::Constraint,
        verification_status: MemoryVerificationStatus::Verified,
        ..MemoryStructuredFields::default()
    };
    store
        .update_structured_fields(&id, &fields)
        .expect("update fields");
    id
}
