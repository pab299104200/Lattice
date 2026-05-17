use super::{
    event_episodes, file_identity, stable_handles, verification_commands, workflow_record,
    ContextItem, Pivot, RenderChoice, RiskNote, StableIdentity, WorkflowBundle,
    WorkflowRenderChoice, WorkflowRequest,
};
use lattice_core::intelligence::TestSelectionReport;

/// Build a workflow-v2 relevant-tests payload.
pub fn build_bundle(
    workspace_id: &str,
    request: &WorkflowRequest,
    report: &TestSelectionReport,
    render_choice: WorkflowRenderChoice,
) -> WorkflowBundle {
    let tests = report
        .tests
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let files = report.source_files.clone();
    let mut risks = report
        .gaps
        .iter()
        .map(|gap| RiskNote {
            severity: "warning".to_string(),
            identity: None,
            message: gap.clone(),
            mitigation: "Broaden the edit anchors or inspect adjacent modules manually."
                .to_string(),
        })
        .collect::<Vec<_>>();
    if report.tests.is_empty() {
        risks.push(RiskNote {
            severity: "error".to_string(),
            identity: None,
            message: "No relevant tests were recovered.".to_string(),
            mitigation: "Run a broader verification sweep and add a regression test.".to_string(),
        });
    }
    WorkflowBundle {
        overview: format!("Selected {} relevant test targets.", report.tests.len()),
        ranked_pivots: report
            .tests
            .iter()
            .map(|test| Pivot {
                identity: StableIdentity::File(file_identity(workspace_id, &test.file)),
                kind: "test".to_string(),
                label: test.file.clone(),
                file: Some(test.file.clone()),
                symbol: None,
                line: None,
                score: test.confidence,
                inclusion_reason: test.reasons.join("; "),
                relevance_summary: None,
                relevance_breakdown: None,
                relevance_detail_handle: None,
                relevance_detail_focus: None,
            })
            .collect(),
        relevant_context: report
            .source_files
            .iter()
            .map(|file| ContextItem {
                identity: StableIdentity::File(file_identity(workspace_id, file)),
                kind: "source_anchor".to_string(),
                label: file.clone(),
                file: Some(file.clone()),
                summary: "source anchor used for test ranking".to_string(),
                inclusion_reason: "file or diff path seeded relevant-test discovery".to_string(),
            })
            .collect(),
        memory_highlights: Vec::new(),
        memory_empty_rationale: Some(
            "Relevant-test discovery does not include memory evidence by default.".to_string(),
        ),
        event_episodes: event_episodes(workspace_id, "find_relevant_tests"),
        suggested_next_expansion: report.tests.first().map(|test| super::ExpansionHint {
            focus: format!("test:{}", test.file),
            reason: "Inspect the top-ranked test before widening the verification surface."
                .to_string(),
        }),
        stable_handles: stable_handles(&tests),
        risks,
        render_choice: RenderChoice {
            mode: format!("{:?}", render_choice).to_lowercase(),
            reason: "Relevant tests default to compact verification guidance.".to_string(),
        },
        verification_commands: verification_commands(&files, &tests),
        workflow_record: workflow_record("find_relevant_tests", request, &tests, &report.rationale),
        structured_payload: serde_json::to_value(report).unwrap_or_default(),
    }
}
