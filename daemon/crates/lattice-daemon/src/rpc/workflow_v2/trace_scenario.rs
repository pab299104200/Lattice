use super::{
    emit_standard_events, empty_memory_rationale, event_episodes, file_identity, memory_highlights,
    render_choice, stable_handles, verification_commands, workflow_record, ContextItem, Pivot,
    StableIdentity, WorkflowBundle, WorkflowEventSink, WorkflowRequest,
};
use lattice_core::intelligence::ScenarioTraceBundle;
use serde_json::json;

/// Compose the redesigned `trace_scenario` bundle with call-chain evidence,
/// event episodes, and memory annotations.
pub fn run(
    workspace_id: &str,
    request: &WorkflowRequest,
    trace: &ScenarioTraceBundle,
    memories: &[serde_json::Value],
    sink: &mut dyn WorkflowEventSink,
) -> WorkflowBundle {
    emit_standard_events(sink, "trace_scenario");
    let memory_highlights = memory_highlights(workspace_id, memories, "matched scenario path");
    let files = trace
        .likely_entrypoints
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let tests = trace
        .tests
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();

    WorkflowBundle {
        overview: trace.overview.clone(),
        ranked_pivots: trace_pivots(workspace_id, trace),
        relevant_context: trace_context(workspace_id, trace),
        memory_empty_rationale: empty_memory_rationale(&memory_highlights),
        memory_highlights,
        event_episodes: event_episodes(workspace_id, "trace_scenario"),
        suggested_next_expansion: None,
        stable_handles: stable_handles(&files),
        risks: Vec::new(),
        render_choice: render_choice(request),
        verification_commands: verification_commands(&files, &tests),
        workflow_record: workflow_record("trace_scenario", request, &files, &trace.rationale),
        structured_payload: json!(trace),
    }
}

fn trace_pivots(workspace_id: &str, trace: &ScenarioTraceBundle) -> Vec<Pivot> {
    trace
        .execution_path
        .iter()
        .map(|segment| Pivot {
            identity: StableIdentity::File(file_identity(workspace_id, &segment.to_file)),
            kind: "call_chain_segment".to_string(),
            label: format!("{} -> {}", segment.from_symbol, segment.to_symbol),
            file: Some(segment.to_file.clone()),
            symbol: Some(segment.to_symbol.clone()),
            line: Some(segment.to_line),
            score: segment.score,
            inclusion_reason: segment.rationale.join("; "),
            relevance_summary: None,
            relevance_breakdown: None,
            relevance_detail_handle: None,
            relevance_detail_focus: None,
        })
        .collect()
}

fn trace_context(workspace_id: &str, trace: &ScenarioTraceBundle) -> Vec<ContextItem> {
    trace
        .likely_entrypoints
        .iter()
        .map(|entrypoint| ContextItem {
            identity: StableIdentity::File(file_identity(workspace_id, &entrypoint.file)),
            kind: "entrypoint".to_string(),
            label: entrypoint.symbol.clone(),
            file: Some(entrypoint.file.clone()),
            summary: entrypoint.evidence.join("; "),
            inclusion_reason: entrypoint.evidence.join("; "),
        })
        .collect()
}
