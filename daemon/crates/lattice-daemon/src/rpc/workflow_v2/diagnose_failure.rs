use super::{
    emit_standard_events, empty_memory_rationale, event_episodes, file_identity, memory_highlights,
    render_choice, stable_handles, verification_commands, workflow_record, ContextItem,
    ExpansionHint, Pivot, StableIdentity, WorkflowBundle, WorkflowEventSink, WorkflowRequest,
};
use lattice_core::intelligence::FailureDiagnosis;
use serde_json::json;

/// Compose the redesigned `diagnose_failure` bundle with failure-pattern memory,
/// prior failure episodes, likely culprit symbols, and a prepare-change follow-up.
pub fn run(
    workspace_id: &str,
    request: &WorkflowRequest,
    diagnosis: &FailureDiagnosis,
    memories: &[serde_json::Value],
    sink: &mut dyn WorkflowEventSink,
) -> WorkflowBundle {
    emit_standard_events(sink, "diagnose_failure");
    let memory_highlights = memory_highlights(workspace_id, memories, "matched failure pattern");
    let files = diagnosis.extracted_files.clone();
    let tests = diagnosis
        .tests
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();

    WorkflowBundle {
        overview: diagnosis.overview.clone(),
        ranked_pivots: suspect_pivots(workspace_id, diagnosis),
        relevant_context: related_context(workspace_id, diagnosis),
        memory_empty_rationale: empty_memory_rationale(&memory_highlights),
        memory_highlights,
        event_episodes: event_episodes(workspace_id, "diagnose_failure"),
        suggested_next_expansion: prepare_change_hint(diagnosis),
        stable_handles: stable_handles(&files),
        risks: Vec::new(),
        render_choice: render_choice(request),
        verification_commands: verification_commands(&files, &tests),
        workflow_record: workflow_record("diagnose_failure", request, &files, &diagnosis.rationale),
        structured_payload: json!(diagnosis),
    }
}

fn suspect_pivots(workspace_id: &str, diagnosis: &FailureDiagnosis) -> Vec<Pivot> {
    diagnosis
        .suspects
        .iter()
        .map(|suspect| Pivot {
            identity: StableIdentity::File(file_identity(workspace_id, &suspect.file)),
            kind: "likely_culprit".to_string(),
            label: suspect.symbol.clone(),
            file: Some(suspect.file.clone()),
            symbol: Some(suspect.symbol.clone()),
            line: Some(suspect.line),
            score: suspect.score,
            inclusion_reason: suspect.evidence.join("; "),
            relevance_summary: None,
            relevance_breakdown: None,
            relevance_detail_handle: None,
            relevance_detail_focus: None,
        })
        .collect()
}

fn related_context(workspace_id: &str, diagnosis: &FailureDiagnosis) -> Vec<ContextItem> {
    diagnosis
        .related_symbols
        .iter()
        .map(|symbol| ContextItem {
            identity: StableIdentity::File(file_identity(workspace_id, &symbol.file)),
            kind: "related_symbol".to_string(),
            label: symbol.symbol.clone(),
            file: Some(symbol.file.clone()),
            summary: symbol.evidence.join("; "),
            inclusion_reason: symbol.evidence.join("; "),
        })
        .collect()
}

fn prepare_change_hint(diagnosis: &FailureDiagnosis) -> Option<ExpansionHint> {
    let suspect = diagnosis.suspects.first()?;
    Some(ExpansionHint {
        focus: suspect
            .symbol_handle
            .clone()
            .unwrap_or_else(|| format!("symbol:{}", suspect.symbol)),
        reason: "Use this suspect as the anchor for a prepare_change follow-up.".to_string(),
    })
}
