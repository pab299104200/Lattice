//! Deterministic candidate-to-delivery interleavings through the real finalizer.
//! These use its private seam, not a transport or agent-efficacy harness.

use super::*;
use crate::rpc::mcp_schema_tests::SchemaFixture;
use lattice_core::memory::{Memory, MemoryScope, MemoryType, MemoryVerificationStatus};

const LESSON: &str = "canonical correction: preserve the committed idempotency key";
const OLD: &str = "OBSOLETE_CANDIDATE_SENTINEL";

async fn fixture(suffix: &str) -> SchemaFixture {
    let fixture = SchemaFixture::new(suffix);
    fixture
        .memory_store
        .lock()
        .await
        .store(Memory {
            id: "canonical-lesson".into(),
            session_id: "prior".into(),
            content: LESSON.into(),
            memory_type: MemoryType::Pattern,
            scope: MemoryScope::Repo,
            confidence: 0.9,
            linked_symbols: vec![],
            linked_files: vec![],
            workspace_id: Some(fixture.handler.memory_workspace_id.clone()),
            branch: None,
            scope_organization_id: None,
            refresh_key: None,
            source_query: None,
            created_at: current_unix_seconds() as u64,
            last_accessed: current_unix_seconds() as u64,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: MemoryVerificationStatus::Verified,
        })
        .unwrap();
    fixture
}

fn candidate(fixture: &SchemaFixture) -> Value {
    json!({
        "query": "graph result remains useful",
        "context_handle": "private-finalizer-fixture",
        "overview": OLD,
        "rationale": OLD,
        "memory_highlights": [{
            "memory_id": format!("repository:{}:canonical-lesson", fixture.handler.memory_workspace_id),
            "content": OLD, "ct": OLD,
            "verification_status": "verified", "vs": OLD,
            "trust_status": OLD, "trust_reason": OLD,
            "contradiction_state": OLD, "reverification_reason": OLD,
            "stale_label": OLD, "evidence_strength": OLD,
            "risk_domains": [OLD], "requires_reverification": true
        }],
        "structured_payload": {"overview": OLD, "rationale": OLD}
    })
}

async fn finalize(fixture: &SchemaFixture, value: Value) -> Value {
    let arguments =
        json!({"render":"json", "budget":"full", "max_tokens":4000, "wire_format":"standard"});
    let options = parse_workflow_response_options(&arguments).unwrap();
    let metadata = WorkflowRunMetadata {
        delivery_mode: "full".into(),
        wire_format: "standard".into(),
        single_anchor_used: false,
        _mode_reason: "controlled interleaving".into(),
        semantic_fallback_used: false,
        outcome_memory_reuse_count: 0,
    };
    let response = fixture
        .handler
        .finalize_workflow_value("prepare_change", &arguments, value, &metadata, &options)
        .await
        .expect("graph workflow survives memory delivery changes");
    unwrap_tool_text_json(&response).expect("actual wrapped JSON response")
}

async fn mutate(fixture: &SchemaFixture, sql: &str) {
    fixture
        .memory_store
        .lock()
        .await
        .with_connection(|connection| {
            connection
                .execute_batch(sql)
                .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))
        })
        .unwrap();
}

#[tokio::test]
async fn finalizer_reloads_current_trust_and_removes_candidate_only_labels() {
    let fixture = fixture("canonical-trust-interleaving").await;
    let candidate = candidate(&fixture);
    mutate(
        &fixture,
        "UPDATE memories SET verification_status='unverified' WHERE id='canonical-lesson'",
    )
    .await;
    let payload = finalize(&fixture, candidate).await;
    assert!(!payload.to_string().contains(OLD), "{payload}");
    let memory = &payload["memory_highlights"][0];
    assert_eq!(memory["content"], LESSON, "{payload}");
    assert_eq!(memory["verification_status"], "unverified", "{payload}");
    assert_eq!(
        payload["memory_deliveries"][0]["ack_required"], true,
        "{payload}"
    );
}

#[tokio::test]
async fn finalizer_withholds_superseded_candidate_and_all_derived_summaries() {
    let fixture = fixture("canonical-supersession-interleaving").await;
    let candidate = candidate(&fixture);
    mutate(
        &fixture,
        "UPDATE memories SET verification_status='superseded' WHERE id='canonical-lesson'",
    )
    .await;
    let payload = finalize(&fixture, candidate).await;
    assert!(!payload.to_string().contains(OLD), "{payload}");
    assert!(!payload.to_string().contains(LESSON), "{payload}");
    assert!(payload.get("memory_deliveries").is_none(), "{payload}");
    assert_eq!(payload["query"], "graph result remains useful", "{payload}");
}

#[tokio::test]
async fn finalizer_preserves_graph_and_reports_canonical_read_failure_without_receipt() {
    let fixture = fixture("canonical-read-failure").await;
    let candidate = candidate(&fixture);
    // Only this fixture's in-memory database is affected.
    mutate(
        &fixture,
        "ALTER TABLE memories RENAME TO fixture_unavailable_memories",
    )
    .await;
    let payload = finalize(&fixture, candidate).await;
    assert!(!payload.to_string().contains(OLD), "{payload}");
    assert!(!payload.to_string().contains(LESSON), "{payload}");
    assert!(payload.get("memory_deliveries").is_none(), "{payload}");
    assert_eq!(
        payload["memory_delivery"]["status"], "degraded",
        "{payload}"
    );
    assert!(
        payload["memory_delivery"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("memory") && reason.contains("table")),
        "{payload}"
    );
    assert_eq!(payload["query"], "graph result remains useful", "{payload}");
}

#[tokio::test]
async fn finalizer_foreign_authority_candidates_are_withheld_with_degraded_status() {
    let fixture = fixture("canonical-foreign-authority").await;
    for identity in [
        "repository:foreign-workspace:canonical-lesson",
        "organization:foreign-org:canonical-lesson",
    ] {
        let mut value = candidate(&fixture);
        value["memory_highlights"][0]["memory_id"] = json!(identity);
        let payload = finalize(&fixture, value).await;
        assert!(!payload.to_string().contains(OLD), "{payload}");
        assert!(payload.get("memory_highlights").is_none(), "{payload}");
        assert!(payload.get("memory_deliveries").is_none(), "{payload}");
        assert_eq!(
            payload["memory_delivery"]["status"], "degraded",
            "{payload}"
        );
        assert_eq!(payload["query"], "graph result remains useful", "{payload}");
    }
}
