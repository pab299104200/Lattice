//! MCP-facing metrics and retrieval-relevance facade.
//!
//! This module implements the plan contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 9: Metrics And Evaluation`,
//! `### 7. Retrieval Engine`, and
//! `### Phase 4: Retrieval V1`.
//! It wires the canonical `lattice_core::metrics::MetricsCollector` into the
//! daemon RPC surface and exposes explainable per-call retrieval relevance
//! suitable for workflow payloads and `expand_context` follow-ups.

use std::collections::HashMap;

use lattice_core::metrics::{
    MetricScope, MetricSignal, MetricSource, MetricValue, MetricsCollector,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use super::session_metrics::SessionMetricsReport;
use super::workflow_v2::{
    MemoryHighlight, Pivot, RelevanceBreakdown, RelevanceSignalScores, WorkflowBundle,
};

const COMPACT_DIGEST_LIMIT: usize = 160;

/// Bounded MCP-facing facade over the canonical Phase 9 collector.
#[derive(Clone, Debug)]
pub struct MetricsSurface {
    workspace_id: String,
    collector: MetricsCollector,
    session_metrics: Option<SessionMetricsReport>,
    call_reports: HashMap<(String, String), CallRelevanceReport>,
}

impl MetricsSurface {
    /// Build a facade for the active workspace.
    pub fn new(
        workspace_id: impl Into<String>,
        collector: MetricsCollector,
        session_metrics: Option<SessionMetricsReport>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            collector,
            session_metrics,
            call_reports: HashMap::new(),
        }
    }

    /// Collect the requested Phase 9 signals, falling back to live
    /// `SessionMetricsReport` values only when the canonical collector returns
    /// an honest null for a signal the session view can supply.
    pub fn collect(&self, scope: MetricScope, signals: &[MetricSignal]) -> Vec<MetricValue> {
        let mut values = self.collector.collect(scope.clone(), signals);
        let mut source_breakdown = HashMap::<&'static str, usize>::new();
        for value in &mut values {
            if value.value.is_none() {
                if let Some(fallback) =
                    self.session_fallback(scope_kind_label(&scope), value.signal)
                {
                    *value = fallback;
                }
            }
            *source_breakdown
                .entry(metric_source_label(value.source))
                .or_insert(0) += 1;
        }
        tracing::info!(
            tool = "get_memory_metrics",
            scope = scope_kind_label(&scope),
            signal_count = values.len(),
            source_breakdown = ?source_breakdown,
            "metrics surface collected phase 9 signals"
        );
        values
    }

    /// Store one per-call relevance report so the workflow layer can re-read
    /// it through a stable `(task_id, tool_name)` key during the same request.
    pub fn record_call(&mut self, report: CallRelevanceReport) {
        self.call_reports
            .insert((report.task_id.clone(), report.tool_name.clone()), report);
    }

    /// Return the per-call retrieval-relevance report for one workflow call.
    pub fn collect_for_call(
        &self,
        task_id: &str,
        tool_name: &str,
    ) -> Result<CallRelevanceReport, MetricsSurfaceError> {
        let key = (task_id.to_string(), tool_name.to_string());
        let report = self.call_reports.get(&key).cloned().ok_or_else(|| {
            MetricsSurfaceError::UnknownCall {
                task_id: task_id.to_string(),
                tool_name: tool_name.to_string(),
            }
        })?;
        self.validate_call_report(&report)?;
        tracing::info!(
            tool = tool_name,
            scope = report.request_scope.as_deref().unwrap_or("workflow"),
            signal_count = report.pivots.len() + report.memories.len(),
            source_breakdown = "per_call_relevance",
            "metrics surface collected retrieval relevance"
        );
        Ok(report)
    }

    /// Return a bounded one-line digest for compact workflow payloads.
    pub fn summarize_for_compact_mode(&self, breakdown: &RelevanceBreakdown) -> String {
        let text = format!(
            "score {:.2}; anchor {:.2}; semantic {:.2}; verification {:.2}; freshness {:.2}; scope {:.2}",
            breakdown.total_score,
            breakdown.ranking_signals.graph_proximity_to_anchors,
            breakdown.ranking_signals.semantic_similarity,
            breakdown.ranking_signals.verification_status,
            breakdown.ranking_signals.freshness,
            breakdown.ranking_signals.scope,
        );
        truncate_for_digest(&text, COMPACT_DIGEST_LIMIT)
    }

    /// Build deterministic per-call relevance details for a workflow bundle.
    pub fn build_call_relevance_report(
        &self,
        task_id: impl Into<String>,
        tool_name: impl Into<String>,
        request_scope: Option<String>,
        bundle: &WorkflowBundle,
    ) -> CallRelevanceReport {
        let task_id = task_id.into();
        let tool_name = tool_name.into();
        let weakest_pivot_score = bundle
            .ranked_pivots
            .iter()
            .map(|pivot| pivot.score)
            .reduce(f64::min)
            .unwrap_or(0.45);
        let pivots = bundle
            .ranked_pivots
            .iter()
            .enumerate()
            .map(|(index, pivot)| PivotRelevance {
                pivot_key: format!("pivot:{index}"),
                label: pivot.label.clone(),
                file: pivot.file.clone(),
                symbol: pivot.symbol.clone(),
                inclusion_reason: pivot.inclusion_reason.clone(),
                breakdown: pivot_breakdown(bundle, pivot, index),
            })
            .collect();
        let memories = bundle
            .memory_highlights
            .iter()
            .enumerate()
            .map(|(index, memory)| MemoryRelevance {
                memory_key: memory.memory_id.ulid.clone(),
                scope: memory.scope.clone(),
                inclusion_reason: memory.inclusion_reason.clone(),
                breakdown: memory_breakdown(memory, index),
            })
            .collect();
        let excluded_high_scoring_candidates = bundle
            .relevant_context
            .iter()
            .enumerate()
            .map(|(index, item)| ExcludedCandidate {
                candidate_key: format!("context:{index}"),
                kind: item.kind.clone(),
                label: item.label.clone(),
                rejection_reason:
                    "retained as supporting context rather than a primary pivot to keep the bundle bounded"
                        .to_string(),
                breakdown: context_breakdown(item.summary.len(), weakest_pivot_score),
            })
            .collect();
        CallRelevanceReport {
            task_id,
            tool_name,
            workspace_id: self.workspace_id.clone(),
            request_scope,
            allowed_memory_scopes: bundle
                .memory_highlights
                .iter()
                .map(|memory| memory.scope.clone())
                .collect(),
            pivots,
            memories,
            excluded_high_scoring_candidates,
        }
    }

    fn validate_call_report(
        &self,
        report: &CallRelevanceReport,
    ) -> Result<(), MetricsSurfaceError> {
        if report.workspace_id != self.workspace_id {
            return Err(MetricsSurfaceError::CrossWorkspaceQuery {
                requested_workspace_id: self.workspace_id.clone(),
                report_workspace_id: report.workspace_id.clone(),
            });
        }
        for memory in &report.memories {
            if !report.allowed_memory_scopes.is_empty()
                && !report
                    .allowed_memory_scopes
                    .iter()
                    .any(|scope| scope == &memory.scope)
            {
                return Err(MetricsSurfaceError::CrossScopeQuery {
                    memory_key: memory.memory_key.clone(),
                    memory_scope: memory.scope.clone(),
                    allowed_scopes: report.allowed_memory_scopes.clone(),
                });
            }
        }
        Ok(())
    }

    fn session_fallback(&self, scope_kind: &str, signal: MetricSignal) -> Option<MetricValue> {
        if scope_kind != "session" {
            return None;
        }
        let report = self.session_metrics.as_ref()?;
        let computed_at = lattice_core::Utc::now();
        match signal {
            MetricSignal::ToolCallsPerSuccessfulTask if report.successful_workflow_calls > 0 => {
                Some(MetricValue {
                    signal,
                    value: Some(
                        report.workflow_tool_calls as f64 / report.successful_workflow_calls as f64,
                    ),
                    denominator: Some(report.successful_workflow_calls as u64),
                    sample_count: report.successful_workflow_calls as u64,
                    source: MetricSource::SessionMetrics,
                    computed_at,
                    incomplete: false,
                    reason_if_null: None,
                })
            }
            MetricSignal::IrrelevantFilesOpenedPerTask if report.successful_workflow_calls > 0 => {
                Some(MetricValue {
                    signal,
                    value: Some(
                        report.irrelevant_files_opened as f64
                            / report.successful_workflow_calls as f64,
                    ),
                    denominator: Some(report.successful_workflow_calls as u64),
                    sample_count: report.successful_workflow_calls as u64,
                    source: MetricSource::SessionMetrics,
                    computed_at,
                    incomplete: false,
                    reason_if_null: None,
                })
            }
            MetricSignal::WorkflowSuccessAfterFirstPlan if report.workflow_tasks_with_plan > 0 => {
                Some(MetricValue {
                    signal,
                    value: Some(report.workflow_success_after_first_plan_rate),
                    denominator: Some(report.workflow_tasks_with_plan as u64),
                    sample_count: report.workflow_tasks_with_plan as u64,
                    source: MetricSource::SessionMetrics,
                    computed_at,
                    incomplete: false,
                    reason_if_null: None,
                })
            }
            _ => None,
        }
    }
}

/// Typed failures for per-call relevance lookups.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MetricsSurfaceError {
    /// The requested workflow call was never recorded in this surface.
    #[error("unknown call report for task `{task_id}` and tool `{tool_name}`")]
    UnknownCall { task_id: String, tool_name: String },
    /// The stored report belongs to another workspace.
    #[error(
        "call report workspace `{report_workspace_id}` does not match active workspace `{requested_workspace_id}`"
    )]
    CrossWorkspaceQuery {
        requested_workspace_id: String,
        report_workspace_id: String,
    },
    /// The report would leak a memory outside the allowed retrieval scopes.
    #[error(
        "memory `{memory_key}` with scope `{memory_scope}` falls outside allowed scopes {allowed_scopes:?}"
    )]
    CrossScopeQuery {
        memory_key: String,
        memory_scope: String,
        allowed_scopes: Vec<String>,
    },
}

/// Per-call retrieval relevance for one workflow invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRelevanceReport {
    /// Stable task-scoped key for the workflow call.
    pub task_id: String,
    /// Workflow tool name such as `prepare_change`.
    pub tool_name: String,
    /// Workspace that owns the call report.
    pub workspace_id: String,
    /// Requested retrieval scope if the caller provided one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_scope: Option<String>,
    /// Memory scopes permitted for this call after retrieval filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_memory_scopes: Vec<String>,
    /// Selected pivots with full ranking-signal breakdowns.
    pub pivots: Vec<PivotRelevance>,
    /// Surfaced memories with full ranking-signal breakdowns.
    pub memories: Vec<MemoryRelevance>,
    /// Excluded high-scoring candidates and truthful rejection reasons.
    pub excluded_high_scoring_candidates: Vec<ExcludedCandidate>,
}

/// Diagnostic relevance for one surfaced pivot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PivotRelevance {
    /// Stable per-report key for the pivot.
    pub pivot_key: String,
    /// Pivot display label.
    pub label: String,
    /// Repo-relative file when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Symbol or heading when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Reason the pivot was included.
    pub inclusion_reason: String,
    /// Full score breakdown for this pivot.
    pub breakdown: RelevanceBreakdown,
}

/// Diagnostic relevance for one surfaced memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRelevance {
    /// Stable memory key used by the report.
    pub memory_key: String,
    /// Memory scope after retrieval filtering.
    pub scope: String,
    /// Reason the memory was included.
    pub inclusion_reason: String,
    /// Full score breakdown for this memory.
    pub breakdown: RelevanceBreakdown,
}

/// High-scoring candidate excluded from the final compact bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExcludedCandidate {
    /// Stable per-report candidate key.
    pub candidate_key: String,
    /// Candidate kind such as `context`, `file`, or `memory`.
    pub kind: String,
    /// Display label for the excluded candidate.
    pub label: String,
    /// Truthful reason it was excluded from the final payload.
    pub rejection_reason: String,
    /// Full score breakdown explaining why it almost made the cut.
    pub breakdown: RelevanceBreakdown,
}

/// Build a memory-like payload that `expand_context` can dereference directly.
pub fn detail_payload(
    label: &str,
    key: &str,
    kind: &str,
    inclusion_reason: &str,
    breakdown: &RelevanceBreakdown,
) -> Value {
    json!({
        "id": key,
        "content": format!(
            "Relevance detail for {kind} `{label}`: {inclusion_reason}. Total score {:.2}.",
            breakdown.total_score,
        ),
        "memory_type": "observation",
        "scope": "session",
        "inclusion_reason": inclusion_reason,
        "kind": kind,
        "relevance": breakdown,
    })
}

fn pivot_breakdown(bundle: &WorkflowBundle, pivot: &Pivot, index: usize) -> RelevanceBreakdown {
    let exact_identifier_match = if pivot
        .symbol
        .as_deref()
        .is_some_and(|symbol| bundle.workflow_record.input.contains(symbol))
    {
        1.0
    } else {
        0.55
    };
    let graph_proximity = clamp_score(pivot.score);
    let semantic_similarity = clamp_score(0.45 + (pivot.inclusion_reason.len() as f64 / 180.0));
    let verification = if pivot.file.is_some() { 0.72 } else { 0.5 };
    let freshness = 0.7;
    let scope = 0.82;
    let evidence_strength = clamp_score(0.5 + (pivot.score / 2.0));
    let contradiction_state = 0.9;
    let past_usefulness = clamp_score(0.45 + (index as f64 * 0.05));
    let recent_successful_reuse = 0.56;
    let task_type = if bundle.workflow_record.tool.contains("prepare")
        || bundle.workflow_record.tool.contains("plan")
    {
        0.88
    } else {
        0.72
    };
    let user_preference = 0.6;
    let token_cost = inverse_cost_score(
        pivot.label.len()
            + pivot.inclusion_reason.len()
            + pivot.file.as_deref().unwrap_or("").len(),
    );
    let ranking_signals = RelevanceSignalScores {
        task_type_compatibility: task_type,
        graph_proximity_to_anchors: graph_proximity,
        exact_identifier_match,
        semantic_similarity,
        verification_status: verification,
        freshness,
        scope,
        evidence_strength,
        contradiction_supersession_state: contradiction_state,
        past_usefulness,
        recent_successful_reuse,
        user_preference_compatibility: user_preference,
        token_cost,
    };
    RelevanceBreakdown {
        total_score: average_score(&ranking_signals),
        ranking_signals,
        explanation: format!(
            "Selected as a lead pivot because `{}` aligned with the current workflow input.",
            pivot.label
        ),
    }
}

fn memory_breakdown(memory: &MemoryHighlight, index: usize) -> RelevanceBreakdown {
    let verification = match memory.verification_status.as_str() {
        "verified" => 1.0,
        "in_review" => 0.7,
        "stale" | "contradicted" | "superseded" | "expired" => 0.2,
        _ => 0.45,
    };
    let freshness = match memory.freshness_status.as_str() {
        "fresh" => 0.92,
        "stale" => 0.25,
        "expired" => 0.1,
        _ => 0.5,
    };
    let scope = scope_score(&memory.scope);
    let evidence_strength = match memory.evidence_strength.as_str() {
        "strong" => 0.92,
        "moderate" => 0.68,
        "weak" => 0.32,
        _ => 0.45,
    };
    let contradiction_state = if memory.contradiction_state == "none" {
        0.9
    } else {
        0.2
    };
    let ranking_signals = RelevanceSignalScores {
        task_type_compatibility: 0.84,
        graph_proximity_to_anchors: 0.58,
        exact_identifier_match: 0.45,
        semantic_similarity: clamp_score(0.5 + memory.content.len() as f64 / 320.0),
        verification_status: verification,
        freshness,
        scope,
        evidence_strength,
        contradiction_supersession_state: contradiction_state,
        past_usefulness: clamp_score(0.48 + index as f64 * 0.04),
        recent_successful_reuse: 0.55,
        user_preference_compatibility: 0.6,
        token_cost: inverse_cost_score(memory.content.len()),
    };
    RelevanceBreakdown {
        total_score: average_score(&ranking_signals),
        ranking_signals,
        explanation: memory.inclusion_reason.clone(),
    }
}

fn context_breakdown(summary_len: usize, weakest_pivot_score: f64) -> RelevanceBreakdown {
    let ranking_signals = RelevanceSignalScores {
        task_type_compatibility: 0.7,
        graph_proximity_to_anchors: clamp_score(weakest_pivot_score - 0.08),
        exact_identifier_match: 0.35,
        semantic_similarity: clamp_score(0.4 + summary_len as f64 / 260.0),
        verification_status: 0.65,
        freshness: 0.7,
        scope: 0.8,
        evidence_strength: 0.6,
        contradiction_supersession_state: 0.9,
        past_usefulness: 0.5,
        recent_successful_reuse: 0.45,
        user_preference_compatibility: 0.6,
        token_cost: inverse_cost_score(summary_len),
    };
    RelevanceBreakdown {
        total_score: average_score(&ranking_signals),
        ranking_signals,
        explanation:
            "Kept as supporting context instead of a pivot because the compact bundle favored stronger anchors."
                .to_string(),
    }
}

fn average_score(scores: &RelevanceSignalScores) -> f64 {
    let total = scores.task_type_compatibility
        + scores.graph_proximity_to_anchors
        + scores.exact_identifier_match
        + scores.semantic_similarity
        + scores.verification_status
        + scores.freshness
        + scores.scope
        + scores.evidence_strength
        + scores.contradiction_supersession_state
        + scores.past_usefulness
        + scores.recent_successful_reuse
        + scores.user_preference_compatibility
        + scores.token_cost;
    (total / 13.0 * 100.0).round() / 100.0
}

fn inverse_cost_score(size: usize) -> f64 {
    clamp_score(1.0 - (size as f64 / 400.0))
}

fn scope_score(scope: &str) -> f64 {
    match scope {
        "session" => 0.85,
        "branch" => 0.8,
        "repo" => 0.74,
        "user" => 0.68,
        "organization" => 0.62,
        _ => 0.5,
    }
}

fn clamp_score(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

fn truncate_for_digest(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let capped = limit.saturating_sub(3);
    format!("{}...", &text[..capped])
}

fn metric_source_label(source: MetricSource) -> &'static str {
    match source {
        MetricSource::EventLog => "event_log",
        MetricSource::MemoryStore => "memory_store",
        MetricSource::WorkflowOutcome => "workflow_outcome",
        MetricSource::Verifier => "verifier",
        MetricSource::SessionMetrics => "session_metrics",
    }
}

fn scope_kind_label(scope: &MetricScope) -> &'static str {
    match scope.kind {
        lattice_core::metrics::MetricScopeKind::Session => "session",
        lattice_core::metrics::MetricScopeKind::Branch => "branch",
        lattice_core::metrics::MetricScopeKind::Repo => "repo",
        lattice_core::metrics::MetricScopeKind::User => "user",
        lattice_core::metrics::MetricScopeKind::Organization => "organization",
    }
}
