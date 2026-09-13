use serde_json::json;

use super::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, ConsolidationFailedPayload,
    ContextBundleReturnedPayload, DiagnosticObservedPayload, DiagnosticSeverity, EventEnvelope,
    EventKind, EventModelError, EventPayload, FileReadPayload, MemoryConsolidatedPayload,
    MemoryCreatedPayload, MemoryExpandedPayload, MemoryInvalidatedPayload, MemoryRetrievedPayload,
    MemoryUpdatedPayload, PatchAppliedPayload, PayloadLocation, PlanCreatedPayload, SessionId,
    StableRef, TaskId, TestRunCompletedPayload, TestRunStartedPayload, TestRunStatus,
    ToolCalledPayload, ToolResultPayload, ToolResultStatus, UserCorrectionPayload,
    UserPreferenceObservedPayload, WorkflowFailedPayload, WorkflowSucceededPayload,
};
use crate::events::DocSectionId;
use crate::identity::{ContextHandleId, EventId, FileId, MemoryId, SymbolId};
use crate::{DateTime, Utc};

fn file_id() -> FileId {
    FileId {
        workspace_id: "workspace-main".to_string(),
        repo_relative_path: "src/events/mod.rs".to_string(),
        content_hash: "abcdef12".to_string(),
    }
}

fn symbol_id() -> SymbolId {
    SymbolId {
        file: file_id(),
        qualified_name: "events::build_payload".to_string(),
        byte_offset: 128,
        kind: "function".to_string(),
    }
}

fn doc_section_id() -> DocSectionId {
    DocSectionId {
        doc: crate::identity::DocId {
            workspace_id: "workspace-main".to_string(),
            repo_relative_path: "docs/spec.md".to_string(),
            content_hash: "fedcba98".to_string(),
        },
        heading_path: vec!["Event Log".to_string(), "Kinds".to_string()],
        byte_offset: 256,
    }
}

fn event_id(suffix: &str) -> EventId {
    EventId {
        workspace_id: "workspace-main".to_string(),
        ulid: format!("01ARZ3NDEKTSV4RRFFQ69G5F{suffix}"),
    }
}

fn memory_id(suffix: &str) -> MemoryId {
    MemoryId {
        workspace_id: "workspace-main".to_string(),
        ulid: format!("01ARZ3NDEKTSV4RRFFQ69H7M{suffix}"),
    }
}

fn context_handle_id(suffix: &str) -> ContextHandleId {
    ContextHandleId {
        workspace_id: "workspace-main".to_string(),
        session_id: "session-main".to_string(),
        ulid: format!("01ARZ3NDEKTSV4RRFFQ69J9P{suffix}"),
    }
}

fn event_kind_cases() -> Vec<(EventKind, &'static str)> {
    vec![
        (EventKind::AssistantTaskStarted, "assistant_task_started"),
        (EventKind::ToolCalled, "tool_called"),
        (EventKind::ToolResult, "tool_result"),
        (EventKind::ContextBundleReturned, "context_bundle_returned"),
        (EventKind::MemoryRetrieved, "memory_retrieved"),
        (EventKind::MemoryExpanded, "memory_expanded"),
        (EventKind::PlanCreated, "plan_created"),
        (EventKind::FileRead, "file_read"),
        (EventKind::PatchApplied, "patch_applied"),
        (EventKind::TestRunStarted, "test_run_started"),
        (EventKind::TestRunCompleted, "test_run_completed"),
        (EventKind::DiagnosticObserved, "diagnostic_observed"),
        (EventKind::UserCorrection, "user_correction"),
        (
            EventKind::UserPreferenceObserved,
            "user_preference_observed",
        ),
        (EventKind::WorkflowSucceeded, "workflow_succeeded"),
        (EventKind::WorkflowFailed, "workflow_failed"),
        (EventKind::MemoryCreated, "memory_created"),
        (EventKind::MemoryUpdated, "memory_updated"),
        (EventKind::MemoryInvalidated, "memory_invalidated"),
        (EventKind::MemoryConsolidated, "memory_consolidated"),
        (EventKind::ConsolidationFailed, "consolidation_failed"),
    ]
}

fn payload_cases() -> Vec<(EventKind, EventPayload, &'static str)> {
    vec![
        (
            EventKind::AssistantTaskStarted,
            EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
                context_handle_id: Some(context_handle_id("0")),
                seed_event_ids: vec![event_id("0")],
                initial_memory_ids: vec![memory_id("0")],
                objective: "debug event serialization".to_string(),
            }),
            r#"{"kind":"assistant_task_started","payload":{"context_handle_id":{"workspace_id":"workspace-main","session_id":"session-main","ulid":"01ARZ3NDEKTSV4RRFFQ69J9P0"},"seed_event_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F0"}],"initial_memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M0"}],"objective":"debug event serialization"}}"#,
        ),
        (
            EventKind::ToolCalled,
            EventPayload::ToolCalled(ToolCalledPayload {
                call_id: "call-1".to_string(),
                tool_name: "prepare_change".to_string(),
                context_handle_id: Some(context_handle_id("1")),
                source_event_id: Some(event_id("1")),
                input_summary: "prepare event scope".to_string(),
            }),
            r#"{"kind":"tool_called","payload":{"call_id":"call-1","tool_name":"prepare_change","context_handle_id":{"workspace_id":"workspace-main","session_id":"session-main","ulid":"01ARZ3NDEKTSV4RRFFQ69J9P1"},"source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F1"},"input_summary":"prepare event scope"}}"#,
        ),
        (
            EventKind::ToolResult,
            EventPayload::ToolResult(ToolResultPayload {
                call_id: "call-1".to_string(),
                tool_name: "prepare_change".to_string(),
                status: ToolResultStatus::Succeeded,
                tool_call_event_id: Some(event_id("2")),
                output_context_handle_id: Some(context_handle_id("2")),
                created_memory_ids: vec![memory_id("2")],
                output_summary: "returned edit plan".to_string(),
            }),
            r#"{"kind":"tool_result","payload":{"call_id":"call-1","tool_name":"prepare_change","status":"succeeded","tool_call_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F2"},"output_context_handle_id":{"workspace_id":"workspace-main","session_id":"session-main","ulid":"01ARZ3NDEKTSV4RRFFQ69J9P2"},"created_memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M2"}],"output_summary":"returned edit plan"}}"#,
        ),
        (
            EventKind::ContextBundleReturned,
            EventPayload::ContextBundleReturned(ContextBundleReturnedPayload {
                context_handle_id: context_handle_id("3"),
                source_event_id: Some(event_id("3")),
                file_ids: vec![file_id()],
                symbol_ids: vec![symbol_id()],
                doc_section_ids: vec![doc_section_id()],
                memory_ids: vec![memory_id("3")],
                token_estimate: 320,
            }),
            r#"{"kind":"context_bundle_returned","payload":{"context_handle_id":{"workspace_id":"workspace-main","session_id":"session-main","ulid":"01ARZ3NDEKTSV4RRFFQ69J9P3"},"source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F3"},"file_ids":[{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"}],"symbol_ids":[{"file":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"qualified_name":"events::build_payload","byte_offset":128,"kind":"function"}],"doc_section_ids":[{"doc":{"workspace_id":"workspace-main","repo_relative_path":"docs/spec.md","content_hash":"fedcba98"},"heading_path":["Event Log","Kinds"],"byte_offset":256}],"memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M3"}],"token_estimate":320}}"#,
        ),
        (
            EventKind::MemoryRetrieved,
            EventPayload::MemoryRetrieved(MemoryRetrievedPayload {
                retrieval_query: "event model hashing".to_string(),
                context_handle_id: Some(context_handle_id("4")),
                memory_ids: vec![memory_id("4")],
                supporting_event_ids: vec![event_id("4")],
                included_context: Vec::new(),
                excluded_context: Vec::new(),
            }),
            r#"{"kind":"memory_retrieved","payload":{"retrieval_query":"event model hashing","context_handle_id":{"workspace_id":"workspace-main","session_id":"session-main","ulid":"01ARZ3NDEKTSV4RRFFQ69J9P4"},"memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M4"}],"supporting_event_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F4"}]}}"#,
        ),
        (
            EventKind::MemoryExpanded,
            EventPayload::MemoryExpanded(MemoryExpandedPayload {
                memory_id: Some(memory_id("5")),
                source_event_id: Some(event_id("5")),
                linked_memory_ids: vec![memory_id("6")],
                linked_symbol_ids: vec![symbol_id()],
                linked_doc_section_ids: vec![doc_section_id()],
                expansion_identity: None,
                included_context: Vec::new(),
                excluded_context: Vec::new(),
            }),
            r#"{"kind":"memory_expanded","payload":{"memory_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M5"},"source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F5"},"linked_memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M6"}],"linked_symbol_ids":[{"file":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"qualified_name":"events::build_payload","byte_offset":128,"kind":"function"}],"linked_doc_section_ids":[{"doc":{"workspace_id":"workspace-main","repo_relative_path":"docs/spec.md","content_hash":"fedcba98"},"heading_path":["Event Log","Kinds"],"byte_offset":256}]}}"#,
        ),
        (
            EventKind::PlanCreated,
            EventPayload::PlanCreated(PlanCreatedPayload {
                context_handle_id: Some(context_handle_id("6")),
                source_event_id: Some(event_id("6")),
                memory_ids: vec![memory_id("7")],
                step_count: 4,
                plan_summary: "add event envelope and tests".to_string(),
            }),
            r#"{"kind":"plan_created","payload":{"context_handle_id":{"workspace_id":"workspace-main","session_id":"session-main","ulid":"01ARZ3NDEKTSV4RRFFQ69J9P6"},"source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F6"},"memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M7"}],"step_count":4,"plan_summary":"add event envelope and tests"}}"#,
        ),
        (
            EventKind::FileRead,
            EventPayload::FileRead(FileReadPayload {
                file_id: file_id(),
                source_event_id: Some(event_id("7")),
                byte_start: Some(0),
                byte_end: Some(256),
                reason: "inspect module exports".to_string(),
            }),
            r#"{"kind":"file_read","payload":{"file_id":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F7"},"byte_start":0,"byte_end":256,"reason":"inspect module exports"}}"#,
        ),
        (
            EventKind::PatchApplied,
            EventPayload::PatchApplied(PatchAppliedPayload {
                patch_id: "patch-1".to_string(),
                source_event_id: Some(event_id("8")),
                file_ids: vec![file_id()],
                symbol_ids: vec![symbol_id()],
                lines_added: 42,
                lines_removed: 3,
            }),
            r#"{"kind":"patch_applied","payload":{"patch_id":"patch-1","source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F8"},"file_ids":[{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"}],"symbol_ids":[{"file":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"qualified_name":"events::build_payload","byte_offset":128,"kind":"function"}],"lines_added":42,"lines_removed":3}}"#,
        ),
        (
            EventKind::TestRunStarted,
            EventPayload::TestRunStarted(TestRunStartedPayload {
                run_id: "run-1".to_string(),
                source_event_id: Some(event_id("9")),
                file_ids: vec![file_id()],
                symbol_ids: vec![symbol_id()],
                command: "cargo test -p lattice-core --lib events::tests".to_string(),
            }),
            r#"{"kind":"test_run_started","payload":{"run_id":"run-1","source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5F9"},"file_ids":[{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"}],"symbol_ids":[{"file":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"qualified_name":"events::build_payload","byte_offset":128,"kind":"function"}],"command":"cargo test -p lattice-core --lib events::tests"}}"#,
        ),
        (
            EventKind::TestRunCompleted,
            EventPayload::TestRunCompleted(TestRunCompletedPayload {
                run_id: "run-1".to_string(),
                started_event_id: Some(event_id("A")),
                status: TestRunStatus::Passed,
                passed: 20,
                failed: 0,
                skipped: 1,
                diagnostic_event_ids: vec![event_id("B")],
            }),
            r#"{"kind":"test_run_completed","payload":{"run_id":"run-1","started_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FA"},"status":"passed","passed":20,"failed":0,"skipped":1,"diagnostic_event_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FB"}]}}"#,
        ),
        (
            EventKind::DiagnosticObserved,
            EventPayload::DiagnosticObserved(DiagnosticObservedPayload {
                diagnostic_id: "diag-1".to_string(),
                source_event_id: Some(event_id("C")),
                file_id: file_id(),
                symbol_id: Some(symbol_id()),
                severity: DiagnosticSeverity::Error,
                message: "mismatched types".to_string(),
            }),
            r#"{"kind":"diagnostic_observed","payload":{"diagnostic_id":"diag-1","source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FC"},"file_id":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"symbol_id":{"file":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"qualified_name":"events::build_payload","byte_offset":128,"kind":"function"},"severity":"error","message":"mismatched types"}}"#,
        ),
        (
            EventKind::UserCorrection,
            EventPayload::UserCorrection(UserCorrectionPayload {
                corrected_event_id: event_id("D"),
                file_ids: vec![file_id()],
                superseded_memory_ids: vec![memory_id("8")],
                correction_summary: "use stable section ids".to_string(),
            }),
            r#"{"kind":"user_correction","payload":{"corrected_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FD"},"file_ids":[{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"}],"superseded_memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M8"}],"correction_summary":"use stable section ids"}}"#,
        ),
        (
            EventKind::UserPreferenceObserved,
            EventPayload::UserPreferenceObserved(UserPreferenceObservedPayload {
                preference_key: "preferred_test_scope".to_string(),
                preference_value: "targeted".to_string(),
                observed_from_event_id: Some(event_id("E")),
                memory_id: Some(memory_id("9")),
            }),
            r#"{"kind":"user_preference_observed","payload":{"preference_key":"preferred_test_scope","preference_value":"targeted","observed_from_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FE"},"memory_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7M9"}}}"#,
        ),
        (
            EventKind::WorkflowSucceeded,
            EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
                workflow_name: "prepare_change".to_string(),
                terminal_event_id: Some(event_id("F")),
                output_context_handle_id: Some(context_handle_id("7")),
                memory_ids: vec![memory_id("A")],
                result_summary: "edit plan emitted".to_string(),
            }),
            r#"{"kind":"workflow_succeeded","payload":{"workflow_name":"prepare_change","terminal_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FF"},"output_context_handle_id":{"workspace_id":"workspace-main","session_id":"session-main","ulid":"01ARZ3NDEKTSV4RRFFQ69J9P7"},"memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MA"}],"result_summary":"edit plan emitted"}}"#,
        ),
        (
            EventKind::WorkflowFailed,
            EventPayload::WorkflowFailed(WorkflowFailedPayload {
                workflow_name: "impact_from_diff".to_string(),
                terminal_event_id: Some(event_id("G")),
                diagnostic_event_ids: vec![event_id("H")],
                retryable: true,
                failure_summary: "index unavailable".to_string(),
            }),
            r#"{"kind":"workflow_failed","payload":{"workflow_name":"impact_from_diff","terminal_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FG"},"diagnostic_event_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FH"}],"retryable":true,"failure_summary":"index unavailable"}}"#,
        ),
        (
            EventKind::MemoryCreated,
            EventPayload::MemoryCreated(MemoryCreatedPayload {
                memory_id: memory_id("B"),
                class: "observation".to_string(),
                stream: "code_topology".to_string(),
                scope: "branch".to_string(),
                idempotency_key: "idem-create-b".to_string(),
                source_event_id: Some(event_id("I")),
                evidence_event_ids: vec![event_id("J")],
                symbol_ids: vec![symbol_id()],
                doc_section_ids: vec![doc_section_id()],
                replay_snapshot_json: None,
            }),
            r#"{"kind":"memory_created","payload":{"memory_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MB"},"class":"observation","stream":"code_topology","scope":"branch","idempotency_key":"idem-create-b","source_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FI"},"evidence_event_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FJ"}],"symbol_ids":[{"file":{"workspace_id":"workspace-main","repo_relative_path":"src/events/mod.rs","content_hash":"abcdef12"},"qualified_name":"events::build_payload","byte_offset":128,"kind":"function"}],"doc_section_ids":[{"doc":{"workspace_id":"workspace-main","repo_relative_path":"docs/spec.md","content_hash":"fedcba98"},"heading_path":["Event Log","Kinds"],"byte_offset":256}]}}"#,
        ),
        (
            EventKind::MemoryUpdated,
            EventPayload::MemoryUpdated(MemoryUpdatedPayload {
                memory_id: memory_id("C"),
                previous_event_id: Some(event_id("K")),
                evidence_event_ids: vec![event_id("L")],
                superseded_memory_id: Some(memory_id("D")),
                update_summary: "strengthened invariant".to_string(),
                replay_snapshot_json: None,
            }),
            r#"{"kind":"memory_updated","payload":{"memory_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MC"},"previous_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FK"},"evidence_event_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FL"}],"superseded_memory_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MD"},"update_summary":"strengthened invariant"}}"#,
        ),
        (
            EventKind::MemoryInvalidated,
            EventPayload::MemoryInvalidated(MemoryInvalidatedPayload {
                memory_id: memory_id("E"),
                invalidated_by_event_id: Some(event_id("M")),
                contradicting_memory_ids: vec![memory_id("F")],
                reason: "stale after rename".to_string(),
                replay_snapshot_json: None,
            }),
            r#"{"kind":"memory_invalidated","payload":{"memory_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7ME"},"invalidated_by_event_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FM"},"contradicting_memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MF"}],"reason":"stale after rename"}}"#,
        ),
        (
            EventKind::MemoryConsolidated,
            EventPayload::MemoryConsolidated(MemoryConsolidatedPayload {
                source_memory_ids: vec![memory_id("G"), memory_id("H")],
                consolidated_memory_id: memory_id("I"),
                source_event_ids: vec![event_id("N"), event_id("O")],
                consolidation_summary: "merged duplicate memories".to_string(),
                proposal_id: None,
                transition: None,
                prior_state_json: None,
                proposed_state_json: None,
                post_apply_state_hash: [0; 32],
                decided_by: None,
                decision_reason: None,
            }),
            r#"{"kind":"memory_consolidated","payload":{"source_memory_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MG"},{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MH"}],"consolidated_memory_id":{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69H7MI"},"source_event_ids":[{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FN"},{"workspace_id":"workspace-main","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FO"}],"consolidation_summary":"merged duplicate memories","post_apply_state_hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}}"#,
        ),
        (
            EventKind::ConsolidationFailed,
            EventPayload::ConsolidationFailed(ConsolidationFailedPayload {
                job_id: "job-llm".to_string(),
                job_kind: "episode_summary".to_string(),
                mode: "background".to_string(),
                model_name: "fake-llm".to_string(),
                error_kind: "malformed_response".to_string(),
                error_message: "invalid JSON".to_string(),
            }),
            r#"{"kind":"consolidation_failed","payload":{"job_id":"job-llm","job_kind":"episode_summary","mode":"background","model_name":"fake-llm","error_kind":"malformed_response","error_message":"invalid JSON"}}"#,
        ),
    ]
}

fn timestamp() -> DateTime<Utc> {
    DateTime::parse_rfc3339("2026-05-17T14:30:00Z").expect("valid RFC3339 timestamp")
}

#[test]
fn event_kind_serde_round_trips_with_canonical_wire_names() {
    for (kind, wire_name) in event_kind_cases() {
        let serialized = serde_json::to_string(&kind).expect("serialize event kind");
        let deserialized: EventKind =
            serde_json::from_str(&serialized).expect("deserialize event kind");

        assert_eq!(serialized, format!("\"{wire_name}\""));
        assert_eq!(kind.as_str(), wire_name);
        assert_eq!(deserialized, kind);
    }
}

#[test]
fn every_payload_serializes_deterministically() {
    for (kind, payload, expected_json) in payload_cases() {
        let first = serde_json::to_string(&payload).expect("serialize payload");
        let second = serde_json::to_string(&payload).expect("serialize payload twice");
        let round_trip: EventPayload =
            serde_json::from_str(&first).expect("deserialize event payload");

        assert_eq!(payload.kind(), kind);
        assert_eq!(first, second);
        assert_eq!(first, expected_json);
        assert_eq!(round_trip, payload);
    }
}

#[test]
fn compact_summary_enforces_byte_ceiling() {
    let oversized = "a".repeat(513);
    let error = CompactSummary::new(oversized).expect_err("summary should be rejected");

    assert_eq!(
        error,
        EventModelError::SummaryTooLong {
            actual_bytes: 513,
            max_bytes: 512,
        }
    );
}

#[test]
fn compact_summary_rejects_oversized_deserialization() {
    let oversized = json!("a".repeat(513));
    let error = serde_json::from_value::<CompactSummary>(oversized)
        .expect_err("deserialization should enforce the same ceiling");

    assert!(error
        .to_string()
        .contains("compact summary exceeds 512 bytes"));
}

#[test]
fn event_envelope_constructor_rejects_kind_payload_mismatch() {
    let error = EventEnvelope::new(
        event_id("P"),
        "workspace-main".to_string(),
        BranchRef {
            name: "main".to_string(),
        },
        SessionId {
            value: "session-main".to_string(),
        },
        Some(TaskId {
            value: "T11".to_string(),
        }),
        Actor::Assistant {
            model: "gpt-5.5".to_string(),
        },
        timestamp(),
        EventKind::ToolCalled,
        vec![StableRef::FileRef(file_id())],
        serde_json::from_str(
            "\"sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef\"",
        )
        .expect("valid payload hash"),
        CompactSummary::new("tool call summary").expect("valid summary"),
        PayloadLocation::Inline { bytes_len: 128 },
        EventPayload::FileRead(FileReadPayload {
            file_id: file_id(),
            source_event_id: None,
            byte_start: None,
            byte_end: None,
            reason: "wrong payload".to_string(),
        }),
    )
    .expect_err("mismatched payload kind should fail");

    assert_eq!(
        error,
        EventModelError::KindPayloadMismatch {
            expected: EventKind::ToolCalled,
            actual: EventKind::FileRead,
        }
    );
}
