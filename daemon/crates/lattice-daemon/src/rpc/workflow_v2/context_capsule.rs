use super::{
    empty_memory_rationale, event_episodes, file_identity, memory_highlights, risks_from_memories,
    stable_handles, verification_commands, workflow_record, ContextItem, Pivot, StableIdentity,
    WorkflowBundle, WorkflowRenderChoice, WorkflowRequest,
};
use lattice_core::graph::model::CodeGraph;
use lattice_core::query::ContextCapsule;

/// Build a workflow-v2 context capsule payload.
pub fn build_bundle(
    graph: &CodeGraph,
    workspace_id: &str,
    request: &WorkflowRequest,
    capsule: &ContextCapsule,
    render_choice: WorkflowRenderChoice,
) -> WorkflowBundle {
    let memories = memory_highlights(workspace_id, &capsule.memories, "matched context query");
    let mut risks = risks_from_memories(&memories);
    if capsule.pivots.is_empty() {
        risks.push(super::RiskNote {
            severity: "warning".to_string(),
            identity: None,
            message: "No primary pivots matched the query.".to_string(),
            mitigation: "Broaden the query or expand from a concrete file or symbol anchor."
                .to_string(),
        });
    }
    let files = capsule
        .pivots
        .iter()
        .map(|pivot| pivot.file.clone())
        .chain(capsule.context.iter().map(|item| item.file.clone()))
        .collect::<Vec<_>>();
    WorkflowBundle {
        overview: format!(
            "Found {} ranked pivots and {} supporting context items.",
            capsule.pivots.len(),
            capsule.context.len()
        ),
        ranked_pivots: capsule
            .pivots
            .iter()
            .map(|pivot| Pivot {
                identity: StableIdentity::LegacyHandle(format!("symbol:{}", pivot.symbol)),
                kind: "symbol".to_string(),
                label: pivot.symbol.clone(),
                file: Some(pivot.file.clone()),
                symbol: Some(pivot.symbol.clone()),
                line: Some(pivot.line),
                score: pivot.score,
                inclusion_reason: pivot.reason.clone(),
                relevance_summary: None,
                relevance_breakdown: None,
                relevance_detail_handle: None,
                relevance_detail_focus: None,
            })
            .collect(),
        relevant_context: capsule
            .context
            .iter()
            .map(|item| ContextItem {
                identity: StableIdentity::File(file_identity(workspace_id, &item.file)),
                kind: "context".to_string(),
                label: item.symbol.clone(),
                file: Some(item.file.clone()),
                summary: item.skeleton.clone(),
                inclusion_reason: item.relationship.clone(),
            })
            .collect(),
        memory_highlights: memories.clone(),
        memory_empty_rationale: empty_memory_rationale(&memories),
        event_episodes: event_episodes(workspace_id, "get_context_capsule"),
        suggested_next_expansion: capsule.pivots.first().map(|pivot| super::ExpansionHint {
            focus: format!("symbol:{}", pivot.symbol),
            reason: "Expand the lead pivot before broad file reads.".to_string(),
        }),
        stable_handles: stable_handles(&files),
        risks,
        render_choice: super::RenderChoice {
            mode: format!("{:?}", render_choice).to_lowercase(),
            reason: render_choice_reason(render_choice),
        },
        verification_commands: verification_commands(&files, &related_test_files(graph, &files)),
        workflow_record: workflow_record("get_context_capsule", request, &files, &[]),
        structured_payload: serde_json::to_value(capsule).unwrap_or_default(),
    }
}

fn render_choice_reason(render_choice: WorkflowRenderChoice) -> String {
    match render_choice {
        WorkflowRenderChoice::Focused => "Focused capsule for bounded first-pass retrieval",
        WorkflowRenderChoice::Compact => "Compact capsule is the default discovery shape",
        WorkflowRenderChoice::Full => "Full mode requested by caller",
        WorkflowRenderChoice::Diagnostic => "Diagnostic mode requested by caller",
    }
    .to_string()
}

fn related_test_files(graph: &CodeGraph, files: &[String]) -> Vec<String> {
    let mut tests = Vec::new();
    for node in graph.all_nodes() {
        if files.iter().any(|file| file == &node.file) {
            for (dependent, _edge) in graph.get_dependents(&node.id) {
                if dependent.file.contains("test") || dependent.file.contains("spec") {
                    tests.push(dependent.file.clone());
                }
            }
        }
    }
    tests.sort();
    tests.dedup();
    tests
}
