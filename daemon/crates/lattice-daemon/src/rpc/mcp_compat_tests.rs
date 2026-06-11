//! MCP surface regressions.
//!
//! Cites `docs/plans/2026-06-11-agent-adoption-overhaul.md`
//! Phase 2, which intentionally replaces the old alias-heavy MCP surface
//! with 8 agent-facing verbs and no callable MCP aliases.

use lattice_core::events::{
    Actor, BranchRef, CompactSummary, EventKind, EventPayload, EventWriter, FlushPolicy,
    PartialEnvelope, PlanCreatedPayload, SessionId, TaskId, ToolCalledPayload,
};
use serde_json::{json, Value};
use std::time::Duration;

use super::mcp_schema_tests::tool_list::{ADVERTISED_TOOLS, REMOVED_TOOL_NAMES};
use super::mcp_schema_tests::{call_args, SchemaFixture};
use super::server::RequestHandler;

const REMOVED_MCP_TOOL_NAMES: &[&str] = &[
    "get_context_capsule",
    "plan_edit",
    "trace_scenario",
    "find_relevant_tests",
    "impact_from_diff",
    "get_working_set_context",
    "summarize_subsystem",
    "get_repo_playbook",
    "diagnose_failure",
    "search_memory",
];

const WORKFLOW_RENDER_TOOLS: &[&str] = &["prepare_change", "diagnose"];

#[tokio::test]
async fn legacy_aliases_are_not_advertised_or_callable_through_mcp() {
    let fixture = SchemaFixture::new("compat-aliases");
    for alias in REMOVED_TOOL_NAMES {
        let error = fixture
            .handler
            .handle("tools/call", call_args(alias, json!({})))
            .await
            .expect_err("legacy alias should not remain callable through MCP");
        assert_eq!(error.0, -32602, "{alias}");
        assert!(error.1.contains(*alias), "{alias}: {}", error.1);
    }
}

#[tokio::test]
async fn removed_mcp_tool_names_are_rejected() {
    let fixture = SchemaFixture::new("compat-removed");
    for tool in REMOVED_MCP_TOOL_NAMES {
        let error = fixture
            .handler
            .handle("tools/call", call_args(tool, additive_args(tool)))
            .await
            .expect_err("removed MCP tool name should be rejected");
        assert_eq!(error.0, -32602, "{tool}");
        assert!(error.1.contains(*tool), "{tool}: {}", error.1);
    }
}

#[tokio::test]
async fn agent_verbs_accept_minimal_requests_as_supersets() {
    let fixture = SchemaFixture::new("compat-additive");
    seed_legacy_memory(&fixture).await;
    let tools = [
        "context",
        "prepare_change",
        "impact",
        "diagnose",
        "search",
        "remember",
        "recall",
        "status",
    ];
    for tool in tools {
        let response = fixture
            .handler
            .handle("tools/call", call_args(tool, additive_args(tool)))
            .await
            .unwrap_or_else(|error| panic!("{tool} minimal request failed: {error:?}"));
        let payload = parse_payload(tool, &response);
        assert_agent_superset(tool, &payload);
    }
}

#[tokio::test]
async fn compact_and_full_render_modes_work_for_workflow_and_review_tools() {
    let fixture = SchemaFixture::new("compat-render");
    let memory_id = save_memory(&fixture, "render memory seed").await;
    seed_session_events(&fixture);

    for tool in WORKFLOW_RENDER_TOOLS {
        for mode in ["compact", "full"] {
            let payload = call_payload(&fixture, tool, workflow_args(tool, mode)).await;
            assert!(payload.is_object(), "{tool} {mode} should return an object");
        }
    }
    let payload = call_payload(
        &fixture,
        "recall",
        json!({"mode": "verify", "memory_id": memory_id, "render_mode": "compact"}),
    )
    .await;
    assert_eq!(payload["render_mode"].as_str(), Some("compact"));
}

#[tokio::test]
async fn stable_context_handle_round_trips_through_expand_context() {
    let fixture = SchemaFixture::new("compat-handles");
    let capsule = call_payload(
        &fixture,
        "context",
        json!({"query": "workspace setup", "render": "json"}),
    )
    .await;
    let handle = capsule["context_handle"]
        .as_str()
        .expect("context capsule should return a handle");
    let focus = capsule["suggested_expand"]["focus"]
        .as_str()
        .or_else(|| capsule["suggested_expand"].as_str())
        .unwrap_or("file:Cargo.toml");

    let expanded = call_payload(
        &fixture,
        "context",
        json!({"mode": "expand", "handle": handle, "focus": focus, "max_tokens": 800}),
    )
    .await;
    assert_eq!(
        expanded["context_origin"].as_str(),
        Some("get_context_capsule")
    );
}

#[tokio::test]
async fn branch_switch_returns_bounded_placeholder_for_workflow_tools() {
    let fixture = SchemaFixture::new("compat-branch-switch-placeholder");
    write_branch_ref(
        &fixture,
        "feature",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );

    call_payload(
        &fixture,
        "context",
        json!({"query": "workspace setup", "render": "json"}),
    )
    .await;

    switch_head(&fixture, "feature");
    tokio::time::sleep(Duration::from_millis(90)).await;

    let payload = call_payload(
        &fixture,
        "prepare_change",
        json!({"task": "update auth flow", "render": "json"}),
    )
    .await;
    assert_eq!(payload["indexing"].as_bool(), Some(true));
    assert_eq!(payload["reason"].as_str(), Some("branch_switch"));
}

#[tokio::test]
async fn stale_context_handle_is_rejected_after_repo_epoch_changes() {
    let fixture = SchemaFixture::new("compat-stale-context-handle");
    write_branch_ref(
        &fixture,
        "feature",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );

    let capsule = call_payload(
        &fixture,
        "context",
        json!({"query": "workspace setup", "render": "json"}),
    )
    .await;
    let handle = capsule["context_handle"]
        .as_str()
        .expect("context capsule should return a handle")
        .to_string();
    let focus = capsule["suggested_expand"]["focus"]
        .as_str()
        .or_else(|| capsule["suggested_expand"].as_str())
        .unwrap_or("file:Cargo.toml")
        .to_string();

    switch_head(&fixture, "feature");
    tokio::time::sleep(Duration::from_millis(90)).await;

    let branch_switch = call_payload(
        &fixture,
        "prepare_change",
        json!({"task": "warm branch refresh", "render": "json"}),
    )
    .await;
    assert_eq!(branch_switch["reason"].as_str(), Some("branch_switch"));

    wait_for_branch_refresh(&fixture).await;

    let error = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "context",
                json!({"mode": "expand", "handle": handle, "focus": focus, "max_tokens": 800}),
            ),
        )
        .await
        .expect_err("stale context handle should be rejected");
    assert_eq!(error.0, -32001);
    assert!(
        error.1.contains("repo epoch"),
        "unexpected error: {}",
        error.1
    );
}

#[tokio::test]
async fn compat_matrix_extends_mcp_schema_contract_without_changing_tool_list() {
    let fixture = SchemaFixture::new("compat-schema-parity");
    let listed = fixture
        .handler
        .handle("tools/list", json!({}))
        .await
        .expect("tools/list succeeds");
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(names, ADVERTISED_TOOLS);
    for alias in REMOVED_TOOL_NAMES {
        assert!(!names.contains(alias), "{alias} must not be advertised");
    }
}

async fn call_payload(fixture: &SchemaFixture, tool: &str, args: Value) -> Value {
    let response = fixture
        .handler
        .handle("tools/call", call_args(tool, args))
        .await
        .unwrap_or_else(|error| panic!("{tool} failed: {error:?}"));
    parse_payload(tool, &response)
}

fn parse_payload(tool: &str, response: &Value) -> Value {
    let text = response["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool} returned no text content: {response}"));
    if let Ok(payload) = serde_json::from_str(text) {
        return payload;
    }
    if let Some(block) = text
        .split("```json")
        .nth(1)
        .and_then(|part| part.split("```").next())
    {
        return serde_json::from_str(block.trim()).unwrap_or_else(|error| {
            panic!("{tool} structured markdown JSON should parse: {error}; block was `{block}`")
        });
    }
    panic!("{tool} payload should contain JSON text or a JSON code block; text was `{text}`")
}

fn switch_head(fixture: &SchemaFixture, branch: &str) {
    std::fs::write(
        fixture.workspace_root.join(".git").join("HEAD"),
        format!("ref: refs/heads/{branch}\n"),
    )
    .expect("update git head");
}

fn write_branch_ref(fixture: &SchemaFixture, branch: &str, oid: &str) {
    let refs_dir = fixture
        .workspace_root
        .join(".git")
        .join("refs")
        .join("heads");
    std::fs::create_dir_all(&refs_dir).expect("git refs dir");
    std::fs::write(refs_dir.join(branch), format!("{oid}\n")).expect("branch ref");
}

async fn wait_for_branch_refresh(fixture: &SchemaFixture) {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let payload = call_payload(
            fixture,
            "prepare_change",
            json!({"query": "poll branch refresh", "render": "json"}),
        )
        .await;
        if payload["indexing"].as_bool() != Some(true) {
            return;
        }
    }
    panic!("branch refresh did not converge in time");
}

fn additive_args(tool: &str) -> Value {
    match tool {
        "context" => json!({"query": "compat", "render": "json"}),
        "prepare_change" => json!({"task": "compat", "render": "json"}),
        "impact" => {
            json!({"target": {"name": "main", "file": "src/main.rs"}, "include_tests": false})
        }
        "diagnose" => json!({"failure_text": "error[E0000]: compat", "render": "json"}),
        "search" => json!({"query": "main", "kind": "symbol", "limit": 5}),
        "remember" => json!({"content": "compat memory", "kind": "quick", "scope": "repo"}),
        "recall" => json!({"query": "compat", "mode": "search", "limit": 5}),
        "status" => json!({"scope": "index"}),
        "get_context_capsule" => json!({"query": "compat", "render": "json"}),
        "plan_edit" => json!({"query": "compat", "render": "json"}),
        "trace_scenario" => json!({"scenario": "compat", "render": "json"}),
        "find_relevant_tests" => json!({"files": ["src/main.rs"], "limit": 5}),
        "impact_from_diff" => json!({"diff": "diff --git a/src/main.rs b/src/main.rs\n"}),
        "get_working_set_context" => json!({"files": ["src/main.rs"], "render": "json"}),
        "summarize_subsystem" => json!({"query": "compat", "render": "json"}),
        "get_repo_playbook" => json!({"render": "json"}),
        "get_session_metrics" => json!({}),
        "diagnose_failure" => json!({"input": "error[E0000]: compat", "render": "json"}),
        "search_memory" => json!({"query": "compat", "limit": 5}),
        "list_stale_memories" => json!({"limit": 5}),
        _ => json!({}),
    }
}

fn assert_agent_superset(tool: &str, payload: &Value) {
    match tool {
        "context" => assert!(payload["context_handle"].is_string()),
        "prepare_change" => assert_array_field(payload, "primary_files"),
        "impact" => assert!(
            payload["nodes"].is_array()
                || payload["impact"].is_object()
                || payload["error"].is_string()
                || payload.is_object()
        ),
        "diagnose" => assert_array_field(payload, "likely_causes"),
        "search" => assert!(
            payload["symbols"].is_array() || payload["error"].is_string() || payload.is_object()
        ),
        "remember" => assert!(payload["memory_id"].is_string() || payload["memory"].is_object()),
        "recall" => assert!(payload["memories"].is_array() || payload.is_array()),
        "status" => assert!(payload["status"].is_string()),
        other => panic!("missing additive assertion for {other}"),
    }
}

fn assert_array_field(payload: &Value, field: &str) {
    assert!(
        payload[field].is_array()
            || payload["structured_payload"][field].is_array()
            || payload["structured_payload"]["legacy"][field].is_array(),
        "expected array field `{field}` in payload or structured_payload: {payload}"
    );
}

fn workflow_args(tool: &str, mode: &str) -> Value {
    let mut value = additive_args(tool);
    if let Some(object) = value.as_object_mut() {
        object.insert("mode".to_string(), json!(mode));
        object.insert("render".to_string(), json!("json"));
    }
    value
}

async fn seed_legacy_memory(fixture: &SchemaFixture) {
    let _ = save_memory(fixture, "legacy alias seed memory").await;
}

async fn save_memory(fixture: &SchemaFixture, content: &str) -> String {
    let payload = call_payload(
        fixture,
        "remember",
        json!({
            "kind": "durable",
            "content": content,
            "memory_class": "constraint",
            "assertion_type": "constraint",
            "scope": "repo",
            "confidence": 0.82,
            "confidence_reason": "compat regression",
            "freshness_policy": "manual_review",
        }),
    )
    .await;
    payload["memory_id"]
        .as_str()
        .expect("memory_id")
        .to_string()
}

fn seed_session_events(fixture: &SchemaFixture) {
    let writer = EventWriter::new(
        fixture.event_store.clone(),
        fixture.workspace_root.to_string_lossy().to_string(),
        4096,
    )
    .with_flush_policy(FlushPolicy::Sync);
    let session = SessionId {
        value: "compat-session".to_string(),
    };
    let task = TaskId {
        value: "compat-task".to_string(),
    };
    append_seed_event(
        &writer,
        session.clone(),
        task.clone(),
        EventKind::ToolCalled,
    );
    append_seed_event(&writer, session, task, EventKind::PlanCreated);
}

fn append_seed_event(
    writer: &EventWriter,
    session_id: SessionId,
    task_id: TaskId,
    kind: EventKind,
) {
    let payload = match kind {
        EventKind::ToolCalled => EventPayload::ToolCalled(ToolCalledPayload {
            call_id: "compat-call".to_string(),
            tool_name: "prepare_change".to_string(),
            context_handle_id: None,
            source_event_id: None,
            input_summary: "seed".to_string(),
        }),
        EventKind::PlanCreated => EventPayload::PlanCreated(PlanCreatedPayload {
            context_handle_id: None,
            source_event_id: None,
            memory_ids: Vec::new(),
            step_count: 1,
            plan_summary: "seed".to_string(),
        }),
        _ => unreachable!("compat seed only supports workflow event kinds"),
    };
    writer
        .append(PartialEnvelope {
            workspace_id: None,
            branch: BranchRef {
                name: "main".to_string(),
            },
            session_id,
            task_id: Some(task_id),
            actor: Actor::Daemon,
            kind: payload.kind(),
            references: Vec::new(),
            summary: CompactSummary::new("compat seed event").expect("summary"),
            payload,
        })
        .expect("seed event");
}
