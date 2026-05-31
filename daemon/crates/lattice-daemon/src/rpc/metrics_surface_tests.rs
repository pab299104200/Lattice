use std::time::Duration;

use lattice_core::identity::{EventId, MemoryId};
use lattice_core::metrics::{
    MemorySurfaceRecord, MetricSampleScope, MetricScope, MetricScopeKind, MetricSignal,
    MetricsCollector,
};
use lattice_core::verification::VerificationStatus;
use serde_json::json;

use super::context_cache::ContextHandleCache;
use super::memory_v2::get_memory_metrics::{GetMemoryMetricsArgs, MetricRenderMode};
use super::metrics_surface::{detail_payload, MetricsSurface, MetricsSurfaceError};
use super::workflow_v2::{
    ContextItem, MemoryHighlight, Pivot, RenderChoice, StableIdentity, WorkflowBundle,
    WorkflowRecord,
};

#[test]
fn serde_round_trip_preserves_surface_request_and_response() {
    let args = GetMemoryMetricsArgs {
        scope: Some(MetricScopeKind::Session),
        time_range: None,
        signals: MetricSignal::ALL.to_vec(),
        render_mode: Some(MetricRenderMode::Diagnostic),
    };
    let encoded = serde_json::to_string(&args).expect("serialize args");
    let decoded: GetMemoryMetricsArgs = serde_json::from_str(&encoded).expect("deserialize args");
    assert_eq!(decoded.signals, MetricSignal::ALL.to_vec());

    let surface = fixture_surface();
    let report = surface.build_call_relevance_report(
        "ctx-main",
        "prepare_change",
        Some("session".to_string()),
        &workflow_bundle_fixture(),
    );
    let encoded = serde_json::to_string(&report).expect("serialize report");
    let decoded = serde_json::from_str::<super::metrics_surface::CallRelevanceReport>(&encoded)
        .expect("deserialize report");
    assert_eq!(decoded.tool_name, "prepare_change");
    assert_eq!(decoded.pivots.len(), 1);
}

#[test]
fn collect_returns_every_required_metric_signal_variant() {
    let surface = fixture_surface();
    let values = surface.collect(MetricScope::session("session-main"), &MetricSignal::ALL);
    assert_eq!(values.len(), MetricSignal::ALL.len());
    let returned: Vec<MetricSignal> = values.iter().map(|value| value.signal).collect();
    assert_eq!(returned, MetricSignal::ALL.to_vec());
}

#[test]
fn collect_reports_honest_null_with_reason_when_evidence_is_missing() {
    let surface = MetricsSurface::new("workspace-main", MetricsCollector::new(), None);
    let values = surface.collect(
        MetricScope::session("session-main"),
        &[MetricSignal::MemoryInclusionPrecision],
    );
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].value, None);
    assert_eq!(values[0].sample_count, 0);
    assert!(values[0]
        .reason_if_null
        .as_deref()
        .is_some_and(|reason| reason.contains("no retrieved memories matched")));
}

#[test]
fn call_relevance_exposes_every_ranking_signal_column() {
    let bundle = workflow_bundle_fixture();
    let mut surface = fixture_surface();
    let report = surface.build_call_relevance_report(
        "ctx-main",
        "prepare_change",
        Some("session".to_string()),
        &bundle,
    );
    surface.record_call(report);
    let restored = surface
        .collect_for_call("ctx-main", "prepare_change")
        .expect("call report");
    let payload = serde_json::to_value(&restored.pivots[0].breakdown).expect("serialize");
    for key in [
        "task_type_compatibility",
        "graph_proximity_to_anchors",
        "exact_identifier_match",
        "semantic_similarity",
        "verification_status",
        "freshness",
        "scope",
        "evidence_strength",
        "contradiction_supersession_state",
        "past_usefulness",
        "recent_successful_reuse",
        "user_preference_compatibility",
        "token_cost",
    ] {
        assert!(
            payload["ranking_signals"].get(key).is_some(),
            "missing ranking signal column {key} in {payload:?}"
        );
    }
}

#[test]
fn compact_mode_digest_is_bounded() {
    let surface = fixture_surface();
    let report = surface.build_call_relevance_report(
        "ctx-main",
        "prepare_change",
        Some("session".to_string()),
        &workflow_bundle_fixture(),
    );
    let digest = surface.summarize_for_compact_mode(&report.pivots[0].breakdown);
    assert!(
        digest.len() <= 160,
        "digest exceeded bound: {}",
        digest.len()
    );
}

#[test]
fn diagnostic_expansion_handle_dereferences_to_full_breakdown() {
    let surface = fixture_surface();
    let report = surface.build_call_relevance_report(
        "ctx-main",
        "prepare_change",
        Some("session".to_string()),
        &workflow_bundle_fixture(),
    );
    let mut cache = ContextHandleCache::new_with_limits(8, Duration::from_secs(60));
    let detail = detail_payload(
        &report.pivots[0].label,
        &report.pivots[0].pivot_key,
        "pivot",
        &report.pivots[0].inclusion_reason,
        &report.pivots[0].breakdown,
    );
    let handle = cache.insert(
        "relevance_detail",
        lattice_core::intelligence::ExpandContextSeed {
            query: Some("login".to_string()),
            files: vec!["src/lib.rs".to_string()],
            symbols: vec!["login".to_string()],
            tests: Vec::new(),
            memories: vec![detail],
        },
        "workspace-main",
        "session-main",
        1,
    );
    let cached = cache.get(&handle.legacy_handle).expect("cached seed");
    let expanded = lattice_core::intelligence::expand_context(
        &lattice_core::graph::model::CodeGraph::default(),
        &cached.seed,
        "memory:pivot:0",
        800,
    );
    let relevance = expanded
        .memories
        .first()
        .and_then(|item| item.get("relevance"))
        .expect("relevance payload");
    assert!(relevance.get("ranking_signals").is_some());
}

#[test]
fn collect_for_call_rejects_cross_workspace_and_scope_reports() {
    let mut surface = fixture_surface();
    let mut cross_workspace = surface.build_call_relevance_report(
        "ctx-workspace",
        "prepare_change",
        Some("session".to_string()),
        &workflow_bundle_fixture(),
    );
    cross_workspace.workspace_id = "workspace-other".to_string();
    surface.record_call(cross_workspace);
    let error = surface
        .collect_for_call("ctx-workspace", "prepare_change")
        .expect_err("cross-workspace should fail");
    assert!(matches!(
        error,
        MetricsSurfaceError::CrossWorkspaceQuery { .. }
    ));

    let mut surface = fixture_surface();
    let mut cross_scope = surface.build_call_relevance_report(
        "ctx-scope",
        "prepare_change",
        Some("session".to_string()),
        &workflow_bundle_fixture(),
    );
    cross_scope.allowed_memory_scopes = vec!["session".to_string()];
    cross_scope.memories[0].scope = "repo".to_string();
    surface.record_call(cross_scope);
    let error = surface
        .collect_for_call("ctx-scope", "prepare_change")
        .expect_err("cross-scope should fail");
    assert!(matches!(error, MetricsSurfaceError::CrossScopeQuery { .. }));
}

fn fixture_surface() -> MetricsSurface {
    MetricsSurface::new("workspace-main", fixture_collector(), None)
}

fn fixture_collector() -> MetricsCollector {
    MetricsCollector::new().with_memory_surface_records(vec![MemorySurfaceRecord {
        scope: MetricSampleScope {
            workspace_id: "workspace-main".to_string(),
            branch: Some("main".to_string()),
            session_id: Some("session-main".to_string()),
            user_id: None,
            organization_id: None,
            task_id: Some("ctx-main".to_string()),
            observed_at: lattice_core::Utc::now(),
        },
        retrieval_event_id: EventId {
            workspace_id: "workspace-main".to_string(),
            ulid: "evt-1".to_string(),
        },
        memory_id: MemoryId {
            workspace_id: "workspace-main".to_string(),
            ulid: "mem-1".to_string(),
        },
        verification_status: VerificationStatus::Verified,
        stale_label_surfaced: false,
        contradiction_link_present: false,
        contradiction_surfaced: false,
        used_downstream: true,
        reused_later: true,
    }])
}

fn workflow_bundle_fixture() -> WorkflowBundle {
    WorkflowBundle {
        overview: "Auth workflow".to_string(),
        ranked_pivots: vec![Pivot {
            identity: StableIdentity::LegacyHandle("symbol:login".to_string()),
            kind: "symbol".to_string(),
            label: "login".to_string(),
            file: Some("src/lib.rs".to_string()),
            symbol: Some("login".to_string()),
            line: Some(42),
            score: 0.91,
            inclusion_reason: "exact symbol from the task".to_string(),
            relevance_summary: None,
            relevance_breakdown: None,
            relevance_detail_handle: None,
            relevance_detail_focus: None,
        }],
        relevant_context: vec![ContextItem {
            identity: StableIdentity::LegacyHandle("file:src/auth.rs".to_string()),
            kind: "file".to_string(),
            label: "src/auth.rs".to_string(),
            file: Some("src/auth.rs".to_string()),
            summary: "related auth helper".to_string(),
            inclusion_reason: "same auth path".to_string(),
        }],
        memory_highlights: vec![MemoryHighlight {
            memory_id: MemoryId {
                workspace_id: "workspace-main".to_string(),
                ulid: "mem-1".to_string(),
            },
            content: "Previous auth fix required updating token validation.".to_string(),
            memory_type: "workflow_outcome".to_string(),
            scope: "repo".to_string(),
            inclusion_reason: "captured prior auth fix".to_string(),
            evidence_strength: "strong".to_string(),
            verification_status: "verified".to_string(),
            trust_status: "trusted".to_string(),
            trust_reason: "verified".to_string(),
            risk_domains: vec!["security".to_string()],
            requires_reverification: false,
            reverification_reason: "high_risk_verified_with_evidence".to_string(),
            freshness_status: "fresh".to_string(),
            contradiction_state: "none".to_string(),
            expansion_target: "memory:mem-1".to_string(),
            stale_label: None,
            recheck_commands: vec!["cd daemon && cargo test auth".to_string()],
            relevance_summary: None,
            relevance_breakdown: None,
            relevance_detail_handle: None,
            relevance_detail_focus: None,
        }],
        memory_empty_rationale: None,
        event_episodes: Vec::new(),
        suggested_next_expansion: None,
        stable_handles: vec!["file_id:src/lib.rs".to_string()],
        risks: Vec::new(),
        render_choice: RenderChoice {
            mode: "diagnostic".to_string(),
            reason: "fixture".to_string(),
        },
        verification_commands: vec!["cd daemon && cargo test --workspace".to_string()],
        workflow_record: WorkflowRecord {
            tool: "prepare_change".to_string(),
            input: "fix login".to_string(),
            resolved_anchors: Vec::new(),
            selected_candidates: vec!["src/lib.rs".to_string()],
            excluded_high_scoring_candidates: Vec::new(),
            working_memory_summary: "fixture".to_string(),
        },
        structured_payload: json!({"fixture": true}),
    }
}
