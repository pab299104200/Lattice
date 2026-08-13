//! Asserts the advertised tool list matches the canonical reference.
//!
//! Source of truth: `docs/plans/2026-06-11-agent-adoption-overhaul.md`
//! `## Phase 2 — Consolidate the agent-facing tool surface to 8 verbs`.

use serde_json::json;

use super::super::server::RequestHandler;
use super::super::mcp::{
    AGENT_CONTEXT_MODE_DEFAULT, AGENT_IMPACT_LIMIT_DEFAULT, AGENT_RECALL_MODE_DEFAULT,
    AGENT_STATUS_SCOPE_DEFAULT,
};
use super::{call_args, SchemaFixture};

/// The 8 advertised agent-facing tool names in the order they appear in the reference.
pub(crate) const ADVERTISED_TOOLS: &[&str] = &[
    "context",
    "prepare_change",
    "impact",
    "diagnose",
    "search",
    "remember",
    "recall",
    "status",
];

/// Names explicitly deleted by Phase 2 instead of kept as shims or aliases.
pub(crate) const REMOVED_TOOL_NAMES: &[&str] = &[
    "query_context",
    "blast_radius",
    "get_file_context",
    "recall_memories",
    "verify_memory",
    "explain_memory",
    "apply_memory_evolution",
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
async fn advertised_defaults_match_dispatcher_defaults() {
    let fixture = SchemaFixture::new("schema-defaults");
    let response = fixture
        .handler
        .handle("tools/list", json!({}))
        .await
        .expect("tools/list succeeds");
    let tools = response["tools"].as_array().expect("tools array");
    let schema_for = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"))["inputSchema"]
            .clone()
    };

    assert_eq!(
        schema_for("context")["properties"]["mode"]["default"],
        AGENT_CONTEXT_MODE_DEFAULT
    );
    assert_eq!(
        schema_for("impact")["properties"]["limit"]["default"],
        AGENT_IMPACT_LIMIT_DEFAULT
    );
    assert_eq!(
        schema_for("recall")["properties"]["mode"]["default"],
        AGENT_RECALL_MODE_DEFAULT
    );
    assert_eq!(
        schema_for("status")["properties"]["scope"]["default"],
        AGENT_STATUS_SCOPE_DEFAULT
    );
}

#[tokio::test]
async fn metadata_and_identifier_fields_are_typed() {
    let fixture = SchemaFixture::new("schema-types");
    let response = fixture
        .handler
        .handle("tools/list", json!({}))
        .await
        .expect("tools/list succeeds");
    let tools = response["tools"].as_array().expect("tools array");
    for tool_name in ADVERTISED_TOOLS {
        let schema = tools
            .iter()
            .find(|tool| tool["name"] == *tool_name)
            .expect("advertised tool")["inputSchema"]
            .clone();
        for field in ["_lattice_client", "_lattice_channel"] {
            assert_eq!(schema["properties"][field]["type"], "string");
        }
    }
    assert_eq!(schema_for_tool(&tools, "recall")["properties"]["memory_id"]["type"], "string");
    assert_eq!(schema_for_tool(&tools, "status")["properties"]["anchor"]["type"], "string");
}

fn schema_for_tool(tools: &[serde_json::Value], name: &str) -> serde_json::Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing tool {name}"))["inputSchema"]
        .clone()
}

#[tokio::test]
async fn removed_tool_names_are_rejected() {
    let fixture = SchemaFixture::new("removed-names");
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
    for removed_name in REMOVED_TOOL_NAMES {
        assert!(
            !names.contains(*removed_name),
            "removed tool `{removed_name}` must not appear in the advertised list"
        );
        let result = fixture
            .handler
            .handle("tools/call", call_args(removed_name, json!({})))
            .await;
        let (code, message) = result.expect_err("removed tool should be rejected");
        assert_eq!(code, -32602);
        assert!(
            message.contains(*removed_name),
            "unknown-tool error should name `{removed_name}`: {message}"
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
