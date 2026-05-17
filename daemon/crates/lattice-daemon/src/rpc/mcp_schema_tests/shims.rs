//! Deprecation shim and legacy alias coverage for the Phase 8 MCP surface.
//!
//! Cited spec: [`## Backward compatibility`](docs/architecture/2026-05-16-mcp-compatibility-policy.md#backward-compatibility)
//! and [`## Shim removal protocol`](docs/architecture/2026-05-16-mcp-compatibility-policy.md#shim-removal-protocol).
//! Every advertised shim must continue to respond, must forward to its canonical
//! successor, and must attach a `deprecation_warning`. Every callable-only alias
//! must dispatch to its canonical target without being advertised.

use serde_json::json;

use super::super::server::RequestHandler;
use super::{call_args, parse_tool_payload, SchemaFixture};

#[tokio::test]
async fn apply_memory_evolution_shim_forwards_to_propose_action_apply() {
    let fixture = SchemaFixture::new("apply-shim");
    let memory_id = save_memory(&fixture, "shim seed").await;
    let propose = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "propose_memory_evolution",
                json!({
                    "action": "propose",
                    "memory_id": memory_id,
                    "content": "updated by shim test",
                    "reason": "shim coverage",
                }),
            ),
        )
        .await
        .expect("propose succeeds");
    let proposal_id = parse_tool_payload(&propose)["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
    let shim = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "apply_memory_evolution",
                json!({"proposal_id": proposal_id}),
            ),
        )
        .await
        .expect("apply shim succeeds");
    let payload = parse_tool_payload(&shim);
    assert_eq!(payload["decision"].as_str(), Some("applied"));
    assert_eq!(payload["action"].as_str(), Some("apply"));
    let warning = payload["deprecation_warning"]
        .as_str()
        .expect("apply shim must attach deprecation_warning");
    assert!(
        warning.contains("propose_memory_evolution"),
        "warning should reference the canonical tool name: got `{warning}`"
    );
}

#[tokio::test]
async fn verify_memory_shim_forces_verify_mode_and_attaches_deprecation_warning() {
    let fixture = SchemaFixture::new("verify-shim");
    let memory_id = save_memory(&fixture, "verify shim seed").await;
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "verify_memory",
                json!({"memory_id": memory_id, "render_mode": "compact"}),
            ),
        )
        .await
        .expect("verify shim succeeds");
    let payload = parse_tool_payload(&response);
    let warning = payload["deprecation_warning"]
        .as_str()
        .expect("verify shim must attach deprecation_warning");
    assert!(warning.contains("verify_explain_memory"));
}

#[tokio::test]
async fn explain_memory_shim_forces_explain_mode_and_attaches_deprecation_warning() {
    let fixture = SchemaFixture::new("explain-shim");
    let memory_id = save_memory(&fixture, "explain shim seed").await;
    fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "verify_explain_memory",
                json!({"memory_id": memory_id, "mode": "verify_and_explain"}),
            ),
        )
        .await
        .expect("seed verification report");
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args("explain_memory", json!({"memory_id": memory_id})),
        )
        .await
        .expect("explain shim succeeds");
    let payload = parse_tool_payload(&response);
    let warning = payload["deprecation_warning"]
        .as_str()
        .expect("explain shim must attach deprecation_warning");
    assert!(warning.contains("verify_explain_memory"));
}

#[tokio::test]
async fn query_context_alias_dispatches_to_get_context_capsule() {
    let fixture = SchemaFixture::new("alias-query-context");
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "query_context",
                json!({"query": "anything", "render": "json"}),
            ),
        )
        .await
        .expect("query_context alias succeeds");
    let payload = parse_tool_payload(&response);
    assert!(
        payload.get("overview").is_some() || payload.get("context_handle").is_some(),
        "alias must produce a get_context_capsule shape: {payload}"
    );
}

#[tokio::test]
async fn blast_radius_alias_dispatches_to_get_impact_graph() {
    let fixture = SchemaFixture::new("alias-blast-radius");
    let result = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "blast_radius",
                json!({"name": "missing_symbol", "file": "src/missing.rs"}),
            ),
        )
        .await;
    match result {
        Ok(value) => {
            let _ = parse_tool_payload(&value);
        }
        Err((code, _message)) => {
            assert_eq!(
                code, -32602,
                "alias must produce dispatcher error not unknown-tool"
            );
        }
    }
}

#[tokio::test]
async fn get_file_context_alias_dispatches_to_get_skeleton() {
    let fixture = SchemaFixture::new("alias-file-context");
    let result = fixture
        .handler
        .handle(
            "tools/call",
            call_args("get_file_context", json!({"file": "src/missing.rs"})),
        )
        .await;
    match result {
        Ok(value) => {
            let _ = parse_tool_payload(&value);
        }
        Err((code, _message)) => {
            assert_eq!(code, -32602);
        }
    }
}

#[tokio::test]
async fn store_memory_alias_dispatches_to_save_observation() {
    let fixture = SchemaFixture::new("alias-store-memory");
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "store_memory",
                json!({
                    "content": "stored via alias",
                    "memory_type": "observation",
                }),
            ),
        )
        .await
        .expect("store_memory alias succeeds");
    let payload = parse_tool_payload(&response);
    assert!(payload["id"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
}

#[tokio::test]
async fn recall_memories_alias_dispatches_to_search_memory() {
    let fixture = SchemaFixture::new("alias-recall");
    fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "save_observation",
                json!({
                    "content": "alias seed memory",
                    "memory_type": "observation",
                }),
            ),
        )
        .await
        .expect("seed save_observation");
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "recall_memories",
                json!({"query": "alias seed", "limit": 5}),
            ),
        )
        .await
        .expect("recall_memories alias succeeds");
    let payload = parse_tool_payload(&response);
    assert!(payload["memories"].is_array() || payload.is_array());
}

#[tokio::test]
async fn deprecation_warning_field_is_absent_from_canonical_responses() {
    let fixture = SchemaFixture::new("canonical-no-warning");
    let memory_id = save_memory(&fixture, "canonical seed").await;
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "verify_explain_memory",
                json!({"memory_id": memory_id, "mode": "verify", "render_mode": "compact"}),
            ),
        )
        .await
        .expect("canonical call succeeds");
    let payload = parse_tool_payload(&response);
    assert!(
        payload.get("deprecation_warning").is_none() || payload["deprecation_warning"].is_null(),
        "canonical tool must not attach a deprecation_warning"
    );
}

async fn save_memory(fixture: &SchemaFixture, content: &str) -> String {
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "save_memory",
                json!({
                    "content": content,
                    "memory_class": "constraint",
                    "assertion_type": "constraint",
                    "scope": "repo",
                    "confidence": 0.74,
                    "confidence_reason": "shim coverage",
                    "freshness_policy": "manual_review",
                }),
            ),
        )
        .await
        .expect("save_memory succeeds");
    parse_tool_payload(&response)["memory_id"]
        .as_str()
        .expect("memory_id field")
        .to_string()
}
