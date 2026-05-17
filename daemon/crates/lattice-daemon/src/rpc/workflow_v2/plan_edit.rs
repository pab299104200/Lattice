use super::{
    emit_standard_events, empty_memory_rationale, event_episodes, file_identity, memory_highlights,
    render_choice, risks_from_memories, stable_handles, verification_commands, workflow_record,
    ContextItem, Pivot, RiskNote, StableIdentity, WorkflowBundle, WorkflowEventSink,
    WorkflowRequest,
};
use lattice_core::intelligence::PlanEditBundle;
use lattice_core::query::ContextCapsule;
use serde_json::json;

/// Compose the redesigned `plan_edit` bundle with ordered patch steps and
/// stable-identity edit anchors.
pub fn run(
    workspace_id: &str,
    request: &WorkflowRequest,
    plan: &PlanEditBundle,
    capsule: &ContextCapsule,
    sink: &mut dyn WorkflowEventSink,
) -> WorkflowBundle {
    emit_standard_events(sink, "plan_edit");
    let memories = memory_highlights(workspace_id, &capsule.memories, "matched edit plan");
    let files = plan
        .edit_files
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let tests = plan
        .tests
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let steps = ordered_steps(workspace_id, plan);
    let mut risks = risks_from_memories(&memories);
    risks.extend(plan.risks.iter().map(|risk| RiskNote {
        severity: risk.level.clone(),
        identity: Some(StableIdentity::File(file_identity(
            workspace_id,
            &risk.file,
        ))),
        message: risk.reason.clone(),
        mitigation: "Review affected callers and update docs before verification.".to_string(),
    }));

    WorkflowBundle {
        overview: plan.overview.clone(),
        ranked_pivots: pivots_from_plan(workspace_id, plan),
        relevant_context: context_from_plan(workspace_id, plan),
        memory_empty_rationale: empty_memory_rationale(&memories),
        memory_highlights: memories,
        event_episodes: event_episodes(workspace_id, "plan_edit"),
        suggested_next_expansion: None,
        stable_handles: stable_handles(&files),
        risks,
        render_choice: render_choice(request),
        verification_commands: verification_commands(&files, &tests),
        workflow_record: workflow_record("plan_edit", request, &files, &plan.rationale),
        structured_payload: json!({
            "legacy": plan,
            "ordered_edit_steps": steps,
        }),
    }
}

fn ordered_steps(workspace_id: &str, plan: &PlanEditBundle) -> Vec<serde_json::Value> {
    plan.candidate_spans
        .iter()
        .enumerate()
        .map(|(index, span)| {
            json!({
                "order": index + 1,
                "file_identity": file_identity(workspace_id, &span.file),
                "symbol_handle": span.symbol_handle,
                "file": span.file,
                "symbol": span.symbol,
                "line_span": span.line_span,
                "reason": span.reason,
            })
        })
        .collect()
}

fn pivots_from_plan(workspace_id: &str, plan: &PlanEditBundle) -> Vec<Pivot> {
    plan.candidate_spans
        .iter()
        .map(|span| Pivot {
            identity: StableIdentity::File(file_identity(workspace_id, &span.file)),
            kind: "edit_span".to_string(),
            label: span.symbol.clone(),
            file: Some(span.file.clone()),
            symbol: Some(span.symbol.clone()),
            line: Some(span.start_line),
            score: 1.0,
            inclusion_reason: span.reason.clone(),
            relevance_summary: None,
            relevance_breakdown: None,
            relevance_detail_handle: None,
            relevance_detail_focus: None,
        })
        .collect()
}

fn context_from_plan(workspace_id: &str, plan: &PlanEditBundle) -> Vec<ContextItem> {
    plan.supporting_files
        .iter()
        .map(|file| ContextItem {
            identity: StableIdentity::File(file_identity(workspace_id, &file.file)),
            kind: "supporting_file".to_string(),
            label: file.file.clone(),
            file: Some(file.file.clone()),
            summary: file.reasons.join("; "),
            inclusion_reason: file.reasons.join("; "),
        })
        .collect()
}
