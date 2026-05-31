//! Serde round-trip assertions for every Phase 8 MCP request/response type.
//!
//! Backs the contract clause in
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## MCP Tool
//! Contract Principles` requiring stable, structured payloads. Each typed
//! request/response struct must survive a JSON round-trip so the wire form
//! advertised in `docs/architecture/2026-05-16-mcp-tool-reference.md` cannot
//! drift away from the daemon's Rust types without a test failure.

use lattice_core::events::EventKind;
use lattice_core::identity::{FileId, MemoryId};
use lattice_core::memory::model::{MemoryAssertionType, MemoryFreshnessPolicy};
use lattice_core::memory::{
    MemoryClass, MemoryScope, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};

use super::super::memory_v2::{
    consolidate_session::{
        ConsolidateSessionArgs, ConsolidationCategoryReport, ConsolidationMode,
        ConsolidationProposalItem, ConsolidationRenderMode, ConsolidationReport,
    },
    get_event_trace::{
        encode_cursor, EventTraceEntry, EventTracePage, EventTraceRenderMode, EventTraceScope,
        GetEventTraceArgs,
    },
    get_memory_metrics::{
        GetMemoryMetricsArgs, MetricRenderMode, MetricScopeKind, MetricSignal, MetricTimeRange,
    },
    get_task_memory::GetTaskMemoryArgs,
    list_memory_conflicts::{
        ConflictAnchor, ConflictRecord, ListMemoryConflictsArgs, ListMemoryConflictsResponse,
        MemoryIdInput as ConflictMemoryIdInput,
    },
    propose_memory_evolution::ProposeMemoryEvolutionArgs,
    save_memory::{FreshnessPolicyArg, MemoryScopeArg, SaveMemoryArgs, SaveMemoryResponse},
    save_quick_memory::SaveQuickMemoryArgs,
    verify_explain_memory::{
        CheckOutcome, CheckResult, MemoryIdInput, VerifyExplainArgs, VerifyExplainMode,
        VerifyExplainRenderMode, VerifyExplainResponse,
    },
    EvolutionAction, EvolutionProposal, MemoryCheckoutState, MemoryRecord, TaskMemoryBundle,
};
use super::super::workflow_v2::{
    Pivot, RenderChoice, RiskNote, StableIdentity, WorkflowBundle, WorkflowRecord,
};
use super::super::working_memory_tool::{InspectWorkingMemoryArgs, InspectWorkingMemoryMode};
use super::assert_round_trip;

#[test]
fn save_memory_args_round_trip_preserves_snake_case_assertion_type() {
    let args = SaveMemoryArgs {
        content: "refresh-token invariant".to_string(),
        memory_class: MemoryClass::Constraint,
        assertion_type: Some(MemoryAssertionType::Constraint),
        scope: MemoryScopeArg::Repo,
        confidence: 0.72,
        confidence_reason: "tests and review".to_string(),
        freshness_policy: FreshnessPolicyArg::ManualReview,
        validity_conditions: vec!["router unchanged".to_string()],
        invalidation_triggers: vec!["refresh refactor".to_string()],
        provenance_event_ids: vec!["evt-a".to_string()],
        evidence: Vec::new(),
        linked_files: vec!["src/auth.rs".to_string()],
        linked_symbols: vec!["refresh_token".to_string()],
        linked_docs: vec!["docs/auth.md#Refresh".to_string()],
        linked_tests: vec!["tests/auth.rs::refresh".to_string()],
        linked_memories: vec!["mem-1".to_string()],
        source_query: Some("refresh failures".to_string()),
        refresh_key: Some("auth::refresh".to_string()),
        branch: None,
        organization_id: None,
    };
    assert_round_trip(&args);
    let payload = serde_json::to_value(&args).expect("serialize");
    assert_eq!(payload["assertion_type"].as_str(), Some("constraint"));
    assert_eq!(payload["memory_class"].as_str(), Some("constraint"));
}

#[test]
fn get_task_memory_args_round_trip_with_and_without_optionals() {
    let with_hint = GetTaskMemoryArgs {
        task_id: "task-1".to_string(),
        task_statement: Some("Tighten memory retrieval".to_string()),
        intent_hint: Some("focus".to_string()),
        focus_files: vec!["src/auth.rs".to_string()],
        focus_dirs: vec!["src".to_string()],
        budget_tokens: Some(800),
    };
    let bare = GetTaskMemoryArgs {
        task_id: "task-2".to_string(),
        task_statement: None,
        intent_hint: None,
        focus_files: Vec::new(),
        focus_dirs: Vec::new(),
        budget_tokens: None,
    };
    assert_round_trip(&with_hint);
    assert_round_trip(&bare);
}

#[test]
fn save_quick_memory_args_round_trip_with_minimal_and_prefilled_context() {
    let minimal = SaveQuickMemoryArgs {
        content: "Observed reusable harness failure".to_string(),
        task_id: None,
        task_statement: None,
        memory_class: None,
        scope: None,
        confidence: None,
        confidence_reason: None,
        linked_files: Vec::new(),
        linked_symbols: Vec::new(),
        linked_docs: Vec::new(),
        linked_tests: Vec::new(),
        linked_memories: Vec::new(),
        validity_conditions: Vec::new(),
        invalidation_triggers: Vec::new(),
        source_query: None,
        refresh_key: None,
        branch: None,
    };
    let contextual = SaveQuickMemoryArgs {
        content: "Record the MCP stdin invocation rule".to_string(),
        task_id: Some("task-r06".to_string()),
        task_statement: Some("Fix Claude MCP invocation".to_string()),
        memory_class: Some(MemoryClass::Procedure),
        scope: Some(MemoryScopeArg::Repo),
        confidence: Some(0.91),
        confidence_reason: Some("reproduced and fixed".to_string()),
        linked_files: vec!["runner.py".to_string()],
        linked_symbols: vec!["run_agent_task".to_string()],
        linked_docs: Vec::new(),
        linked_tests: vec!["tests/test_runner.py::test_claude_uses_stdin".to_string()],
        linked_memories: Vec::new(),
        validity_conditions: vec!["Claude CLI still accepts stdin".to_string()],
        invalidation_triggers: vec!["Claude CLI contract changes".to_string()],
        source_query: Some("Meridian feature-build R06".to_string()),
        refresh_key: Some("feature-build::claude-stdin".to_string()),
        branch: None,
    };
    assert_round_trip(&minimal);
    assert_round_trip(&contextual);
}

#[test]
fn propose_memory_evolution_args_round_trip_for_each_action() {
    for action in [
        EvolutionAction::Propose,
        EvolutionAction::Apply,
        EvolutionAction::Reject,
    ] {
        let args = ProposeMemoryEvolutionArgs {
            action,
            proposal_id: Some("prop".to_string()),
            memory_id: Some("mem".to_string()),
            content: Some("update".to_string()),
            linked_files: vec!["src/x.rs".to_string()],
            linked_symbols: vec!["sym".to_string()],
            linked_docs: vec!["docs/x.md#a".to_string()],
            linked_tests: vec!["tests/x.rs::t".to_string()],
            linked_memories: vec!["mem2".to_string()],
            validity_conditions: vec!["cond".to_string()],
            invalidation_triggers: vec!["trigger".to_string()],
            superseded_by_memory_id: None,
            invalidate_reason: None,
            reason: Some("evolved".to_string()),
            decided_by: Some("assistant".to_string()),
        };
        assert_round_trip(&args);
    }
}

#[test]
fn consolidate_session_request_and_response_round_trip() {
    let args = ConsolidateSessionArgs {
        session_id: "session-1".to_string(),
        mode: Some(ConsolidationMode::Background),
        budget_ms: Some(500),
        render_mode: Some(ConsolidationRenderMode::Diagnostic),
    };
    assert_round_trip(&args);
    let report = ConsolidationReport {
        session_id: "session-1".to_string(),
        mode: ConsolidationMode::PostTask,
        render_mode: ConsolidationRenderMode::Full,
        budget_ms: Some(250),
        proposals: vec![ConsolidationProposalItem {
            proposal_id: "p1".to_string(),
            job_id: "j1".to_string(),
            proposal_kind: "create_memory".to_string(),
            task_id: "t1".to_string(),
            category: "decision".to_string(),
            summary: "captured a decision".to_string(),
            target_memory_id: Some("memory-1".to_string()),
            enqueued_at: 42,
            decision: "pending".to_string(),
            proposed_class: Some("Decision".to_string()),
            current_scope: Some("session".to_string()),
            target_scope: Some("repo".to_string()),
            confidence: Some(0.91),
            evidence_count: 2,
            prior_state: serde_json::json!({"id": "memory-1"}),
            proposed_state: serde_json::json!({"id": "memory-1", "scope": "repo"}),
            evidence: serde_json::json!({"source_event_ids": ["e1", "e2"]}),
            provenance: Some(serde_json::json!({"model": "test-llm"})),
        }],
        categories: vec![ConsolidationCategoryReport {
            category: "decision".to_string(),
            proposal_ids: vec!["p1".to_string()],
            note: None,
        }],
        incomplete: false,
        notes: vec!["ok".to_string()],
    };
    assert_round_trip(&report);
}

#[test]
fn get_memory_metrics_request_round_trip_includes_all_signals() {
    let args = GetMemoryMetricsArgs {
        scope: Some(MetricScopeKind::Repo),
        time_range: Some(MetricTimeRange {
            since: None,
            until: None,
        }),
        signals: vec![
            MetricSignal::ToolCallsPerSuccessfulTask,
            MetricSignal::IrrelevantFilesOpenedPerTask,
            MetricSignal::RelevantAnchorRecall,
            MetricSignal::MemoryInclusionPrecision,
            MetricSignal::MemoryLaterUsedRate,
            MetricSignal::StaleMemorySurfacedRate,
            MetricSignal::ContradictionMissedRate,
            MetricSignal::TestsRecommendedVsNeeded,
            MetricSignal::WorkflowSuccessAfterFirstPlan,
        ],
        render_mode: Some(MetricRenderMode::Full),
    };
    assert_round_trip(&args);
    let payload = serde_json::to_value(&args).expect("serialize");
    assert_eq!(payload["render_mode"].as_str(), Some("full"));
}

#[test]
fn get_event_trace_request_and_page_round_trip() {
    let args = GetEventTraceArgs {
        task_id: Some("task".to_string()),
        session_id: Some("session".to_string()),
        workspace_id: None,
        kinds: vec![EventKind::ToolCalled, EventKind::ToolResult],
        since: None,
        until: None,
        cursor: Some(encode_cursor(3)),
        limit: Some(25),
        render_mode: Some(EventTraceRenderMode::Diagnostic),
    };
    assert_round_trip(&args);
    let page = EventTracePage {
        scope: EventTraceScope {
            kind: "task".to_string(),
            value: "task".to_string(),
        },
        render_mode: EventTraceRenderMode::Compact,
        cursor: None,
        next_cursor: None,
        events: vec![EventTraceEntry {
            event_id: "evt".to_string(),
            expansion_handle: "handle".to_string(),
            kind: "tool_called".to_string(),
            actor: "tool:get_event_trace".to_string(),
            timestamp: lattice_core::Utc::now(),
            workspace_id: "workspace".to_string(),
            branch: "main".to_string(),
            session_id: "session".to_string(),
            task_id: Some("task".to_string()),
            summary: "summary".to_string(),
            references: vec!["ref".to_string()],
            payload: None,
            payload_hash: None,
            spilled_payload_row_id: None,
        }],
    };
    assert_round_trip(&page);
}

#[test]
fn verify_explain_args_round_trip_with_legacy_and_structured_memory_id() {
    let structured = VerifyExplainArgs {
        memory_id: MemoryIdInput::Structured(MemoryId {
            workspace_id: "ws".to_string(),
            ulid: "ulid-1".to_string(),
        }),
        mode: VerifyExplainMode::VerifyAndExplain,
        render_mode: VerifyExplainRenderMode::Diagnostic,
    };
    let legacy = VerifyExplainArgs {
        memory_id: MemoryIdInput::Legacy("ulid-2".to_string()),
        mode: VerifyExplainMode::Verify,
        render_mode: VerifyExplainRenderMode::Compact,
    };
    assert_round_trip(&structured);
    assert_round_trip(&legacy);
}

#[test]
fn verify_explain_response_round_trip_includes_optional_diagnostic_trace() {
    let response = VerifyExplainResponse {
        status: lattice_core::verification::VerificationStatus::Verified,
        checks: vec![CheckResult {
            kind: "linked_symbol_missing".to_string(),
            target: "src/x.rs::foo".to_string(),
            outcome: CheckOutcome::Passed,
            evidence_ref: "ref".to_string(),
            detail: "ok".to_string(),
        }],
        confidence_delta: 0.0,
        expansion_handle: "handle".to_string(),
        summary_lines: vec!["passed".to_string()],
        render_mode: VerifyExplainRenderMode::Diagnostic,
        diagnostic_trace: Some(vec!["trace".to_string()]),
        deprecation_warning: None,
    };
    assert_round_trip(&response);
}

#[test]
fn list_memory_conflicts_args_round_trip_for_every_anchor_kind() {
    let memory_anchor = ListMemoryConflictsArgs {
        anchor: ConflictAnchor::Memory(ConflictMemoryIdInput::Legacy("ulid".to_string())),
        render_mode: VerifyExplainRenderMode::Full,
        limit: 25,
        cursor: Some(5),
    };
    let file_anchor = ListMemoryConflictsArgs {
        anchor: ConflictAnchor::File(FileId {
            workspace_id: "ws".to_string(),
            repo_relative_path: "src/x.rs".to_string(),
            content_hash: "0xabc".to_string(),
        }),
        render_mode: VerifyExplainRenderMode::Compact,
        limit: 10,
        cursor: None,
    };
    assert_round_trip(&memory_anchor);
    assert_round_trip(&file_anchor);
}

#[test]
fn list_memory_conflicts_response_round_trip_carries_summary_lines() {
    let response = ListMemoryConflictsResponse {
        anchor: "memory ulid".to_string(),
        conflicts: vec![ConflictRecord {
            source: "src".to_string(),
            target: "tgt".to_string(),
            link_type: "contradicts".to_string(),
            link_strength: 0.42,
            created_by: "assistant".to_string(),
            created_at: 0,
            link_verification_status: lattice_core::verification::VerificationStatus::Verified,
            reason: "explicit".to_string(),
        }],
        total: 1,
        next_cursor: None,
        render_mode: VerifyExplainRenderMode::Full,
        summary_lines: vec!["src contradicts tgt".to_string()],
    };
    assert_round_trip(&response);
}

#[test]
fn task_memory_bundle_and_memory_record_round_trip() {
    let record = MemoryRecord {
        id: "mem-1".to_string(),
        expansion_handle: "handle".to_string(),
        content: "content".to_string(),
        memory_class: MemoryClass::Decision,
        assertion_type: MemoryAssertionType::Decision,
        scope: "repo".to_string(),
        confidence: 0.81,
        confidence_reason: Some("derived".to_string()),
        verification_status: "verified".to_string(),
        trust_status: "trusted".to_string(),
        trust_reason: "verified".to_string(),
        freshness_status: "manual_review".to_string(),
        contradiction_state: "none".to_string(),
        supersession_state: "none".to_string(),
        inclusion_reason: "matches task terms: refresh".to_string(),
        evidence_strength: 0.45,
        linked_files: vec!["src/x.rs".to_string()],
        linked_symbols: vec!["foo".to_string()],
        linked_docs: vec!["docs/x.md#a".to_string()],
        linked_tests: vec!["tests/x.rs::t".to_string()],
        linked_memories: vec!["mem-2".to_string()],
        validity_conditions: vec!["cond".to_string()],
        invalidation_triggers: vec!["trigger".to_string()],
        provenance: Vec::new(),
        evidence: Vec::new(),
        links: Vec::new(),
        access_history: Vec::new(),
        usefulness_scores: Vec::new(),
        source_query: Some("refresh failures".to_string()),
        branch: None,
        refresh_key: None,
        last_verified_at: None,
        last_verified_graph_snapshot_id: None,
        checkout_state: MemoryCheckoutState {
            recorded_head_ref: None,
            recorded_head_oid: None,
            current_head_ref: None,
            current_head_oid: None,
            status: "recorded_unknown".to_string(),
        },
        workspace_conflict: None,
        workspace_path_diagnostic: None,
    };
    let bundle = TaskMemoryBundle {
        task_id: "task".to_string(),
        checkpoint_id: Some(3),
        working_memory_verification_status: "verified".to_string(),
        memories: vec![record],
    };
    assert_round_trip(&bundle);
}

#[test]
fn evolution_proposal_round_trip_keeps_deprecation_field_optional() {
    let with_warning = EvolutionProposal {
        proposal_id: "p".to_string(),
        action: EvolutionAction::Apply,
        source_memory_id: Some("mem".to_string()),
        proposal_kind: "update_memory".to_string(),
        decision: "applied".to_string(),
        prior_state: serde_json::json!({}),
        proposed_state: serde_json::json!({}),
        deprecation_warning: Some("shim".to_string()),
    };
    let without_warning = EvolutionProposal {
        deprecation_warning: None,
        ..with_warning.clone()
    };
    assert_round_trip(&with_warning);
    assert_round_trip(&without_warning);
}

#[test]
fn save_memory_response_round_trip_includes_full_memory_record() {
    let response = SaveMemoryResponse {
        memory_id: "mem-1".to_string(),
        memory: MemoryRecord {
            id: "mem-1".to_string(),
            expansion_handle: "handle".to_string(),
            content: "content".to_string(),
            memory_class: MemoryClass::Constraint,
            assertion_type: MemoryAssertionType::Constraint,
            scope: "repo".to_string(),
            confidence: 0.72,
            confidence_reason: Some("derived".to_string()),
            verification_status: "unverified".to_string(),
            trust_status: "advisory".to_string(),
            trust_reason: "unverified".to_string(),
            freshness_status: "manual_review".to_string(),
            contradiction_state: "none".to_string(),
            supersession_state: "none".to_string(),
            inclusion_reason: "saved by save_memory".to_string(),
            evidence_strength: 0.1,
            linked_files: Vec::new(),
            linked_symbols: Vec::new(),
            linked_docs: Vec::new(),
            linked_tests: Vec::new(),
            linked_memories: Vec::new(),
            validity_conditions: Vec::new(),
            invalidation_triggers: Vec::new(),
            provenance: Vec::new(),
            evidence: Vec::new(),
            links: Vec::new(),
            access_history: Vec::new(),
            usefulness_scores: Vec::new(),
            source_query: None,
            branch: None,
            refresh_key: None,
            last_verified_at: None,
            last_verified_graph_snapshot_id: None,
            checkout_state: MemoryCheckoutState {
                recorded_head_ref: None,
                recorded_head_oid: None,
                current_head_ref: None,
                current_head_oid: None,
                status: "recorded_unknown".to_string(),
            },
            workspace_conflict: None,
            workspace_path_diagnostic: None,
        },
        verification_job_id: "verify-1".to_string(),
    };
    assert_round_trip(&response);
}

#[test]
fn inspect_working_memory_args_deserialize_each_mode() {
    let compact: InspectWorkingMemoryArgs =
        serde_json::from_value(serde_json::json!({"task_id": "task"})).expect("compact mode parse");
    assert_eq!(compact.mode, InspectWorkingMemoryMode::Compact);
    assert!(!compact.include_excluded);
    let diagnostic: InspectWorkingMemoryArgs = serde_json::from_value(
        serde_json::json!({"task_id": "task", "mode": "diagnostic", "include_excluded": true}),
    )
    .expect("diagnostic mode parse");
    assert_eq!(diagnostic.mode, InspectWorkingMemoryMode::Diagnostic);
    assert!(diagnostic.include_excluded);
}

#[test]
fn workflow_bundle_round_trip_keeps_render_choice_and_stable_handles() {
    let bundle = WorkflowBundle {
        overview: "summary".to_string(),
        ranked_pivots: vec![Pivot {
            identity: StableIdentity::LegacyHandle("handle".to_string()),
            kind: "file".to_string(),
            label: "label".to_string(),
            file: Some("src/x.rs".to_string()),
            symbol: None,
            line: Some(10),
            score: 0.9,
            inclusion_reason: "test".to_string(),
            relevance_summary: Some("score 0.90".to_string()),
            relevance_breakdown: None,
            relevance_detail_handle: Some("ctx-rel".to_string()),
            relevance_detail_focus: Some("memory:pivot-0".to_string()),
        }],
        relevant_context: Vec::new(),
        memory_highlights: Vec::new(),
        memory_empty_rationale: Some("no memory yet".to_string()),
        event_episodes: Vec::new(),
        suggested_next_expansion: None,
        stable_handles: vec!["handle".to_string()],
        risks: vec![RiskNote {
            severity: "info".to_string(),
            identity: None,
            message: "msg".to_string(),
            mitigation: "fix".to_string(),
        }],
        render_choice: RenderChoice {
            mode: "compact".to_string(),
            reason: "default".to_string(),
        },
        verification_commands: vec!["cargo test".to_string()],
        workflow_record: WorkflowRecord {
            tool: "prepare_change".to_string(),
            input: "fix login".to_string(),
            resolved_anchors: Vec::new(),
            selected_candidates: Vec::new(),
            excluded_high_scoring_candidates: Vec::new(),
            working_memory_summary: "summary".to_string(),
        },
        structured_payload: serde_json::json!({"key": "value"}),
    };
    let json = serde_json::to_string(&bundle).expect("serialize");
    let restored: WorkflowBundle = serde_json::from_str(&json).expect("workflow bundle round-trip");
    assert_eq!(restored.render_choice.mode, "compact");
    assert_eq!(restored.stable_handles, vec!["handle".to_string()]);
    assert_eq!(
        restored.verification_commands,
        vec!["cargo test".to_string()]
    );
}

#[test]
fn memory_class_and_assertion_type_serde_uses_snake_case_wire_form() {
    let fields = MemoryStructuredFields {
        memory_class: MemoryClass::WorkflowOutcome,
        assertion_type: MemoryAssertionType::WorkflowOutcome,
        verification_status: MemoryVerificationStatus::Unverified,
        freshness_policy: MemoryFreshnessPolicy::ManualReview,
        ..MemoryStructuredFields::default()
    };
    let payload = serde_json::to_value(&fields).expect("serialize");
    assert_eq!(payload["memory_class"].as_str(), Some("workflow_outcome"));
    assert_eq!(payload["assertion_type"].as_str(), Some("workflow_outcome"));
    let constraint = serde_json::to_value(MemoryClass::Constraint).expect("serialize class");
    let assertion =
        serde_json::to_value(MemoryAssertionType::Constraint).expect("serialize assertion_type");
    assert_eq!(constraint.as_str(), Some("constraint"));
    assert_eq!(assertion.as_str(), Some("constraint"));
    let _ = MemoryType::Pattern;
    let _ = MemoryScope::Repo;
}
