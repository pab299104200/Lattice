use super::{
    event_episodes, file_identity, stable_handles, verification_commands, workflow_record,
    ContextItem, Pivot, RenderChoice, RiskNote, StableIdentity, WorkflowBundle,
    WorkflowRenderChoice, WorkflowRequest,
};
use lattice_core::graph::model::EdgeKind;
use lattice_core::intelligence::DiffImpactReport;

/// Build a workflow-v2 diff-impact payload.
pub fn build_bundle(
    graph: &lattice_core::graph::model::CodeGraph,
    workspace_id: &str,
    request: &WorkflowRequest,
    report: &DiffImpactReport,
    render_choice: WorkflowRenderChoice,
) -> WorkflowBundle {
    let files = report
        .changed_files
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let tests = report
        .tests
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let mut relevant_context = report
        .affected_symbols
        .iter()
        .map(|symbol| ContextItem {
            identity: StableIdentity::LegacyHandle(
                symbol
                    .symbol_handle
                    .clone()
                    .unwrap_or_else(|| format!("symbol:{}", symbol.symbol)),
            ),
            kind: "affected_symbol".to_string(),
            label: symbol.symbol.clone(),
            file: Some(symbol.file.clone()),
            summary: format!("{} affected by diff traversal", symbol.symbol),
            inclusion_reason: symbol.via.join("; "),
        })
        .collect::<Vec<_>>();
    relevant_context.extend(affected_docs(graph, workspace_id, report));
    let mut risks = report
        .risks
        .iter()
        .map(|risk| RiskNote {
            severity: risk.level.clone(),
            identity: Some(StableIdentity::File(file_identity(
                workspace_id,
                &risk.file,
            ))),
            message: risk.reason.clone(),
            mitigation: "Inspect the impacted dependents and rerun the recommended tests."
                .to_string(),
        })
        .collect::<Vec<_>>();
    if report.changed_symbols.is_empty() {
        risks.push(RiskNote {
            severity: "warning".to_string(),
            identity: None,
            message: "Diff lines did not map onto indexed symbols.".to_string(),
            mitigation: "Review changed files directly and verify indexing coverage.".to_string(),
        });
    }
    WorkflowBundle {
        overview: format!(
            "Diff touches {} files and {} changed symbols.",
            report.changed_files.len(),
            report.changed_symbols.len()
        ),
        ranked_pivots: report
            .changed_symbols
            .iter()
            .map(|symbol| Pivot {
                identity: StableIdentity::LegacyHandle(
                    symbol
                        .symbol_handle
                        .clone()
                        .unwrap_or_else(|| format!("symbol:{}", symbol.symbol)),
                ),
                kind: "changed_symbol".to_string(),
                label: symbol.symbol.clone(),
                file: Some(symbol.file.clone()),
                symbol: Some(symbol.symbol.clone()),
                line: Some(symbol.line),
                score: symbol.impact_count as f64,
                inclusion_reason: symbol.reasons.join("; "),
                relevance_summary: None,
                relevance_breakdown: None,
                relevance_detail_handle: None,
                relevance_detail_focus: None,
            })
            .collect(),
        relevant_context,
        memory_highlights: Vec::new(),
        memory_empty_rationale: Some(
            "Diff impact does not include memory evidence by default.".to_string(),
        ),
        event_episodes: event_episodes(workspace_id, "impact_from_diff"),
        suggested_next_expansion: report.suggested_expand.as_ref().map(|hint| {
            super::ExpansionHint {
                focus: hint.focus.clone(),
                reason: hint.reason.clone(),
            }
        }),
        stable_handles: stable_handles(&files),
        risks,
        render_choice: RenderChoice {
            mode: format!("{:?}", render_choice).to_lowercase(),
            reason: "Diff impact traversal stays bounded on hot paths.".to_string(),
        },
        verification_commands: verification_commands(&files, &tests),
        workflow_record: workflow_record("impact_from_diff", request, &files, &report.rationale),
        structured_payload: serde_json::to_value(report).unwrap_or_default(),
    }
}

fn affected_docs(
    graph: &lattice_core::graph::model::CodeGraph,
    workspace_id: &str,
    report: &DiffImpactReport,
) -> Vec<ContextItem> {
    let mut docs = Vec::new();
    for changed in &report.changed_symbols {
        for node in graph.all_nodes() {
            if !node.file.ends_with(".md") {
                continue;
            }
            let mentions_changed =
                graph
                    .get_dependencies(&node.id)
                    .into_iter()
                    .any(|(dep, edge)| {
                        dep.file == changed.file
                            && dep.name == changed.symbol
                            && edge == EdgeKind::Mentions
                    });
            if mentions_changed {
                docs.push(ContextItem {
                    identity: StableIdentity::File(file_identity(workspace_id, &node.file)),
                    kind: "affected_doc".to_string(),
                    label: node.name.clone(),
                    file: Some(node.file.clone()),
                    summary: "markdown section references a changed symbol".to_string(),
                    inclusion_reason: format!(
                        "documentation mentions changed symbol {}",
                        changed.symbol
                    ),
                });
            }
        }
    }
    docs
}
