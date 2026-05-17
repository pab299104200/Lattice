use super::{
    event_episodes, file_identity, stable_handles, workflow_record, ContextItem, Pivot,
    RenderChoice, RiskNote, StableIdentity, WorkflowBundle, WorkflowRenderChoice, WorkflowRequest,
};
use lattice_core::intelligence::DocsCapsule;

/// Build a workflow-v2 docs capsule payload.
pub fn build_bundle(
    workspace_id: &str,
    request: &WorkflowRequest,
    report: &DocsCapsule,
    render_choice: WorkflowRenderChoice,
) -> WorkflowBundle {
    let files = report
        .docs
        .iter()
        .map(|doc| doc.file.clone())
        .collect::<Vec<_>>();
    let risks = if report.docs.is_empty() {
        vec![RiskNote {
            severity: "warning".to_string(),
            identity: None,
            message: "No documentation pivots matched the query.".to_string(),
            mitigation: "Broaden the query or anchor it with a file or symbol.".to_string(),
        }]
    } else {
        Vec::new()
    };
    WorkflowBundle {
        overview: format!(
            "Selected {} relevant documentation sections.",
            report.docs.len()
        ),
        ranked_pivots: report
            .docs
            .iter()
            .map(|doc| Pivot {
                identity: StableIdentity::LegacyHandle(format!("doc:{}#{}", doc.file, doc.line)),
                kind: "doc_primary".to_string(),
                label: doc.symbol.clone(),
                file: Some(doc.file.clone()),
                symbol: Some(doc.symbol.clone()),
                line: Some(doc.line),
                score: doc.score,
                inclusion_reason: doc.reasons.join("; "),
                relevance_summary: None,
                relevance_breakdown: None,
                relevance_detail_handle: None,
                relevance_detail_focus: None,
            })
            .collect(),
        relevant_context: report
            .related_symbols
            .iter()
            .map(|symbol| ContextItem {
                identity: StableIdentity::File(file_identity(workspace_id, &symbol.file)),
                kind: "code_referenced".to_string(),
                label: symbol.symbol.clone(),
                file: Some(symbol.file.clone()),
                summary: format!("mentioned from {}", symbol.mentioned_from.join(", ")),
                inclusion_reason: format!("doc section references code symbol {}", symbol.symbol),
            })
            .collect(),
        memory_highlights: Vec::new(),
        memory_empty_rationale: Some(
            "Docs capsule does not include memory evidence by default.".to_string(),
        ),
        event_episodes: event_episodes(workspace_id, "get_docs_capsule"),
        suggested_next_expansion: report.docs.first().map(|doc| super::ExpansionHint {
            focus: format!("file:{}", doc.file),
            reason: "Expand the top-ranked doc section before reading broader prose.".to_string(),
        }),
        stable_handles: stable_handles(&files),
        risks,
        render_choice: RenderChoice {
            mode: format!("{:?}", render_choice).to_lowercase(),
            reason: "Docs capsule prefers authoritative markdown pivots.".to_string(),
        },
        verification_commands: Vec::new(),
        workflow_record: workflow_record("get_docs_capsule", request, &files, &[]),
        structured_payload: serde_json::to_value(report).unwrap_or_default(),
    }
}
