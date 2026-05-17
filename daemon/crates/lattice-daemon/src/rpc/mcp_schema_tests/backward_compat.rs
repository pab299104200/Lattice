//! Backward-compatibility assertions for Phase 8 additive fields.
//!
//! Cited spec: [`## Contract details for additive evolution`](docs/architecture/2026-05-16-mcp-compatibility-policy.md#contract-details-for-additive-evolution)
//! requires Phase 8 additive fields to be optional with serde defaults so legacy
//! request shapes from prior phases keep parsing. This module asserts every
//! Phase 8 request type accepts the minimal documented payload.

use lattice_core::events::EventKind;
use serde_json::json;

use super::super::memory_v2::{
    consolidate_session::ConsolidateSessionArgs, get_event_trace::GetEventTraceArgs,
    get_memory_metrics::GetMemoryMetricsArgs, get_task_memory::GetTaskMemoryArgs,
    list_memory_conflicts::ListMemoryConflictsArgs,
    propose_memory_evolution::ProposeMemoryEvolutionArgs, save_memory::SaveMemoryArgs,
    verify_explain_memory::VerifyExplainArgs,
};
use super::super::working_memory_tool::InspectWorkingMemoryArgs;

#[test]
fn get_task_memory_accepts_only_task_id() {
    let parsed: GetTaskMemoryArgs =
        serde_json::from_value(json!({"task_id": "task"})).expect("minimal parse");
    assert_eq!(parsed.task_id, "task");
    assert!(parsed.intent_hint.is_none());
    assert!(parsed.budget_tokens.is_none());
}

#[test]
fn save_memory_accepts_minimum_payload_without_optional_links() {
    let parsed: SaveMemoryArgs = serde_json::from_value(json!({
        "content": "minimal",
        "memory_class": "observation",
        "scope": "repo",
        "confidence": 0.5,
        "confidence_reason": "ok",
        "freshness_policy": "manual_review",
    }))
    .expect("save_memory minimal parse");
    assert_eq!(parsed.content, "minimal");
    assert!(parsed.assertion_type.is_none());
    assert!(parsed.linked_files.is_empty());
    assert!(parsed.linked_docs.is_empty());
    assert!(parsed.provenance_event_ids.is_empty());
}

#[test]
fn propose_memory_evolution_accepts_only_action_for_dispatch() {
    let parsed: ProposeMemoryEvolutionArgs =
        serde_json::from_value(json!({"action": "propose"})).expect("minimal parse");
    assert!(parsed.proposal_id.is_none());
    assert!(parsed.memory_id.is_none());
    assert!(parsed.linked_files.is_empty());
}

#[test]
fn consolidate_session_accepts_only_session_id() {
    let parsed: ConsolidateSessionArgs =
        serde_json::from_value(json!({"session_id": "session-1"})).expect("minimal parse");
    assert!(parsed.mode.is_none());
    assert!(parsed.budget_ms.is_none());
    assert!(parsed.render_mode.is_none());
}

#[test]
fn get_memory_metrics_accepts_empty_request() {
    let parsed: GetMemoryMetricsArgs =
        serde_json::from_value(json!({})).expect("metrics empty parse");
    assert!(parsed.scope.is_none());
    assert!(parsed.signals.is_empty());
    assert!(parsed.render_mode.is_none());
}

#[test]
fn get_event_trace_accepts_empty_filter_request() {
    let parsed: GetEventTraceArgs =
        serde_json::from_value(json!({})).expect("event trace empty parse");
    assert!(parsed.task_id.is_none());
    assert!(parsed.session_id.is_none());
    assert!(parsed.kinds.is_empty());
}

#[test]
fn get_event_trace_accepts_typed_event_kinds() {
    let parsed: GetEventTraceArgs = serde_json::from_value(json!({
        "kinds": ["tool_called", "tool_result"],
    }))
    .expect("kinds parse");
    assert_eq!(
        parsed.kinds,
        vec![EventKind::ToolCalled, EventKind::ToolResult]
    );
}

#[test]
fn verify_explain_memory_accepts_legacy_memory_id_string() {
    let parsed: VerifyExplainArgs =
        serde_json::from_value(json!({"memory_id": "ulid"})).expect("verify_explain legacy parse");
    // Default mode is verify_and_explain, default render_mode is full.
    let value = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(value["mode"].as_str(), Some("verify_and_explain"));
    assert_eq!(value["render_mode"].as_str(), Some("full"));
}

#[test]
fn list_memory_conflicts_accepts_legacy_memory_anchor_string() {
    let parsed: ListMemoryConflictsArgs =
        serde_json::from_value(json!({"anchor": "ulid"})).expect("conflicts legacy parse");
    let value = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(value["limit"].as_u64(), Some(25));
    assert_eq!(value["render_mode"].as_str(), Some("full"));
}

#[test]
fn inspect_working_memory_accepts_minimal_request() {
    let parsed: InspectWorkingMemoryArgs =
        serde_json::from_value(json!({"task_id": "task"})).expect("inspect minimal parse");
    assert_eq!(parsed.task_id, "task");
    assert!(!parsed.include_excluded);
}
