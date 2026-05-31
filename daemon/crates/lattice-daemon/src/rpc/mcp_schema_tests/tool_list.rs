//! Asserts the advertised tool list matches the canonical reference.
//!
//! Source of truth: `docs/architecture/2026-05-16-mcp-tool-reference.md`
//! `## Final tool list` and `## Callable deprecated aliases`.

use serde_json::json;

use super::super::server::RequestHandler;
use super::{call_args, SchemaFixture};

/// The 50 advertised tool names in the order they appear in the reference.
pub(crate) const ADVERTISED_TOOLS: &[&str] = &[
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
    "search_logic_flow",
    "submit_lsp_edges",
    "workspace_setup",
    "index_status",
    "get_session_metrics",
    "get_project_rules",
    "inspect_working_memory",
    "list_stale_memories",
    "consolidate_session",
    "get_memory_metrics",
    "get_event_trace",
    "get_task_memory",
    "save_quick_memory",
    "save_memory",
    "propose_memory_evolution",
    "apply_memory_evolution",
    "verify_explain_memory",
    "verify_memory",
    "explain_memory",
    "list_memory_conflicts",
];

/// The five callable-only legacy aliases that are not advertised but accepted.
pub(crate) const CALLABLE_ALIASES: &[(&str, &str)] = &[
    ("query_context", "get_context_capsule"),
    ("blast_radius", "get_impact_graph"),
    ("get_file_context", "get_skeleton"),
    ("recall_memories", "search_memory"),
];

#[tokio::test]
async fn advertised_tool_list_matches_the_reference_exactly() {
    let fixture = SchemaFixture::new("tools-list");
    let response = fixture
        .handler
        .handle("tools/list", json!({}))
        .await
        .expect("tools/list succeeds");
    let tools = response["tools"]
        .as_array()
        .expect("tools/list returned tools array");
    let names: Vec<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_string())
        .collect();
    assert_eq!(
        names.len(),
        ADVERTISED_TOOLS.len(),
        "tool list length must match the reference; got {names:?}"
    );
    for (index, expected) in ADVERTISED_TOOLS.iter().enumerate() {
        assert_eq!(
            names[index].as_str(),
            *expected,
            "tool at index {index} should be `{expected}` but found `{}`",
            names[index]
        );
    }
}

#[tokio::test]
async fn every_advertised_tool_carries_name_description_and_input_schema() {
    let fixture = SchemaFixture::new("tool-fields");
    let response = fixture
        .handler
        .handle("tools/list", json!({}))
        .await
        .expect("tools/list succeeds");
    let tools = response["tools"]
        .as_array()
        .expect("tools/list returned tools array");
    for tool in tools {
        let name = tool["name"].as_str().expect("tool name");
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty()),
            "tool `{name}` should advertise a non-empty description"
        );
        let schema = tool["inputSchema"]
            .as_object()
            .unwrap_or_else(|| panic!("tool `{name}` should advertise an inputSchema object"));
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "tool `{name}` schema should declare type=object"
        );
    }
}

#[tokio::test]
async fn callable_aliases_dispatch_without_being_advertised() {
    let fixture = SchemaFixture::new("aliases");
    let response = fixture
        .handler
        .handle("tools/list", json!({}))
        .await
        .expect("tools/list succeeds");
    let tools = response["tools"]
        .as_array()
        .expect("tools/list returned tools array");
    let names: std::collections::HashSet<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_string())
        .collect();
    for (alias, _canonical) in CALLABLE_ALIASES {
        assert!(
            !names.contains(*alias),
            "alias `{alias}` must not appear in the advertised list (compatibility policy ## Legacy aliases and deadlines)"
        );
    }
}

#[tokio::test]
async fn unknown_tool_call_returns_a_dispatcher_error() {
    let fixture = SchemaFixture::new("unknown-tool");
    let result = fixture
        .handler
        .handle("tools/call", call_args("definitely_not_a_tool", json!({})))
        .await;
    let (code, message) = result.expect_err("unknown tool should be rejected");
    assert_eq!(code, -32602);
    assert!(message.contains("definitely_not_a_tool"));
}
