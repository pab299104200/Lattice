//! Workflow-outcome capture for Phase 8 MCP tools.
//!
//! Bound to `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 8: Workflow Engine V2`,
//! `## MCP Tool Contract Principles`,
//! `## Event Log`, and
//! `## Phase 2: Event Log Substrate`.
//!
//! "Every workflow tool should record:
//!
//! - inputs
//! - resolved anchors
//! - selected candidates
//! - excluded high-scoring candidates where useful
//! - response summary
//! - downstream use events"

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use lattice_core::events::StableRef;
use lattice_core::identity::{ContextHandleId, EventId, FileId, MemoryId, SymbolId};
use serde_json::{json, Value};

use crate::rpc::event_capture::EventCapture;
use crate::rpc::event_capture_support::{compact_text, memory_ids_from_value, parse_tool_payload};
use crate::rpc::session_metrics::SessionMetrics;

#[derive(Debug, Default)]
struct RecorderState {
    handle_origins: HashMap<String, EventId>,
    memory_origins: HashMap<String, EventId>,
}

#[derive(Debug, Clone)]
struct PreparedOutcome {
    input_summary: String,
    resolved_anchors: Vec<StableRef>,
    selected_candidates: Vec<String>,
    excluded_candidates: Vec<String>,
    response_summary: String,
    downstream_references: Vec<StableRef>,
    output_handle: Option<String>,
    output_handle_id: Option<ContextHandleId>,
    output_memory_ids: Vec<MemoryId>,
    success: bool,
    retryable: bool,
    had_plan: bool,
    irrelevant_files_opened: usize,
}

#[derive(Debug, Default)]
pub struct WorkflowOutcomeRecorder {
    state: Mutex<RecorderState>,
}

impl WorkflowOutcomeRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn should_record(tool: &str) -> bool {
        matches!(
            tool,
            "prepare_change"
                | "plan_edit"
                | "trace_scenario"
                | "diagnose_failure"
                | "get_context_capsule"
                | "get_docs_capsule"
                | "find_relevant_tests"
                | "impact_from_diff"
                | "expand_context"
                | "get_task_memory"
                | "save_memory"
                | "propose_memory_evolution"
                | "apply_memory_evolution"
                | "verify_explain_memory"
                | "verify_memory"
                | "explain_memory"
                | "list_memory_conflicts"
                | "consolidate_session"
                | "get_memory_metrics"
                | "record_workflow_outcome"
        )
    }

    pub fn record(
        &self,
        capture: &EventCapture,
        metrics: &mut SessionMetrics,
        tool: &str,
        arguments: &Value,
        result: &Result<Value, (i32, String)>,
        terminal_event: &EventId,
    ) -> Result<EventId, String> {
        let started_at = Instant::now();
        let prepared = self.prepare(tool, arguments, result)?;
        let summary = summarize_outcome(&prepared);
        tracing::info!(
            tool,
            task_id = capture
                .current_task_id()
                .as_ref()
                .map(|value| value.value.as_str())
                .unwrap_or(""),
            session_id = capture.session_id().value.as_str(),
            outcome = if prepared.success {
                "success"
            } else {
                "failure"
            },
            excluded_count = prepared.excluded_candidates.len(),
            latency_ms = started_at.elapsed().as_millis() as u64,
            "workflow outcome recorded"
        );
        if prepared.had_plan {
            capture
                .record_plan_created(tool.to_string(), &[])
                .map_err(|error| error.to_string())?;
        }
        let event_id = capture
            .record_workflow_outcome_detailed(
                prepared.success,
                tool,
                &summary,
                std::slice::from_ref(terminal_event),
                &prepared
                    .resolved_anchors
                    .iter()
                    .chain(prepared.downstream_references.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
                prepared.output_handle_id.clone(),
                &prepared.output_memory_ids,
                prepared.retryable,
            )
            .map_err(|error| error.to_string())?;
        self.remember_origins(&prepared, &event_id);
        metrics.record_workflow_outcome(
            prepared.success,
            prepared.had_plan,
            prepared.irrelevant_files_opened,
        );
        Ok(event_id)
    }

    fn prepare(
        &self,
        tool: &str,
        arguments: &Value,
        result: &Result<Value, (i32, String)>,
    ) -> Result<PreparedOutcome, String> {
        let payload = result.as_ref().ok().and_then(parse_tool_payload);
        let success = result.is_ok();
        let output_memory_ids = payload
            .as_ref()
            .map(|value| memory_ids_from_value(value, &workspace_id(arguments, result)))
            .unwrap_or_default();
        let output_handle = payload
            .as_ref()
            .and_then(|value| value.get("context_handle").and_then(Value::as_str))
            .map(ToString::to_string);
        let output_handle_id = payload
            .as_ref()
            .and_then(|value| value.get("context_handle_identity"))
            .and_then(|value| value.get("fields"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        let resolved_anchors = collect_resolved_anchors(arguments, payload.as_ref());
        let selected_candidates = collect_selected_candidates(tool, payload.as_ref());
        let excluded_candidates =
            collect_excluded_candidates(payload.as_ref(), &selected_candidates);
        let downstream_references = self.collect_downstream_references(arguments);
        let irrelevant_files_opened = excluded_candidates
            .iter()
            .filter(|candidate| candidate.contains('/'))
            .count();
        Ok(PreparedOutcome {
            input_summary: input_summary(tool, arguments, payload.as_ref()),
            resolved_anchors,
            selected_candidates,
            excluded_candidates,
            response_summary: response_summary(tool, payload.as_ref(), result),
            downstream_references,
            output_handle,
            output_handle_id,
            output_memory_ids,
            success,
            retryable: false,
            had_plan: success
                && matches!(
                    tool,
                    "prepare_change" | "plan_edit" | "trace_scenario" | "diagnose_failure"
                ),
            irrelevant_files_opened,
        })
    }

    fn collect_downstream_references(&self, arguments: &Value) -> Vec<StableRef> {
        let mut refs = Vec::new();
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return refs,
        };
        if let Some(handle) = arguments.get("handle").and_then(Value::as_str) {
            if let Some(event_id) = state.handle_origins.get(handle) {
                refs.push(StableRef::EventRef(event_id.clone()));
            }
        }
        for memory_id in collect_argument_memory_ids(arguments) {
            if let Some(event_id) = state.memory_origins.get(&memory_id) {
                refs.push(StableRef::EventRef(event_id.clone()));
            }
        }
        refs
    }

    fn remember_origins(&self, prepared: &PreparedOutcome, event_id: &EventId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(handle) = prepared.output_handle.as_ref() {
            state
                .handle_origins
                .insert(handle.clone(), event_id.clone());
        }
        for memory_id in &prepared.output_memory_ids {
            state
                .memory_origins
                .insert(memory_id.ulid.clone(), event_id.clone());
        }
    }
}

fn workspace_id(arguments: &Value, result: &Result<Value, (i32, String)>) -> String {
    arguments
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            result
                .as_ref()
                .ok()
                .and_then(parse_tool_payload)
                .as_ref()
                .and_then(|value| value.get("memory_id"))
                .and_then(Value::as_object)
                .and_then(|value| value.get("workspace_id"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_default()
}

fn input_summary(tool: &str, arguments: &Value, payload: Option<&Value>) -> String {
    payload
        .and_then(|value| value.get("workflow_record"))
        .and_then(|value| value.get("input"))
        .and_then(Value::as_str)
        .or_else(|| {
            [
                "query",
                "scenario",
                "task",
                "focus",
                "content",
                "session_id",
            ]
            .iter()
            .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        })
        .map(ToString::to_string)
        .unwrap_or_else(|| compact_text(&format!("{tool}:{}", arguments), 220))
}

fn response_summary(
    tool: &str,
    payload: Option<&Value>,
    result: &Result<Value, (i32, String)>,
) -> String {
    if let Some(payload) = payload {
        if let Some(overview) = payload.get("overview").and_then(Value::as_str) {
            return overview.to_string();
        }
        if let Some(summary) = payload.get("summary_lines").and_then(Value::as_array) {
            let joined = summary
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ");
            if !joined.is_empty() {
                return joined;
            }
        }
        if let Some(status) = payload.get("status").and_then(Value::as_str) {
            return format!("{tool}:{status}");
        }
    }
    match result {
        Ok(value) => compact_text(&value.to_string(), 260),
        Err((code, message)) => format!("error {code}: {}", compact_text(message, 220)),
    }
}

fn collect_resolved_anchors(arguments: &Value, payload: Option<&Value>) -> Vec<StableRef> {
    let mut refs = Vec::new();
    if let Some(items) = payload
        .and_then(|value| value.get("workflow_record"))
        .and_then(|value| value.get("resolved_anchors"))
        .and_then(Value::as_array)
    {
        refs.extend(items.iter().filter_map(stable_ref_from_identity));
    }
    refs.extend(parse_file_refs(arguments, "entry_files"));
    refs.extend(parse_symbol_refs(arguments, "entry_symbols"));
    dedupe_refs(refs)
}

fn collect_selected_candidates(tool: &str, payload: Option<&Value>) -> Vec<String> {
    if let Some(candidates) = payload
        .and_then(|value| value.get("workflow_record"))
        .and_then(|value| value.get("selected_candidates"))
        .and_then(Value::as_array)
    {
        let selected: Vec<_> = candidates
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        if !selected.is_empty() {
            return selected;
        }
    }
    match tool {
        "save_memory" => payload
            .and_then(|value| value.get("memory_id"))
            .and_then(Value::as_str)
            .map(|value| vec![format!("memory:{value}")])
            .unwrap_or_default(),
        "verify_explain_memory" | "verify_memory" | "explain_memory" => payload
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .map(|value| vec![format!("verification:{value}")])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn collect_excluded_candidates(
    payload: Option<&Value>,
    selected_candidates: &[String],
) -> Vec<String> {
    if let Some(candidates) = payload
        .and_then(|value| value.get("workflow_record"))
        .and_then(|value| value.get("excluded_high_scoring_candidates"))
        .and_then(Value::as_array)
    {
        let excluded: Vec<_> = candidates
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        if !excluded.is_empty() {
            return excluded;
        }
    }
    let selected: HashSet<_> = selected_candidates.iter().cloned().collect();
    payload
        .and_then(|value| value.get("ranked_pivots"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let file = item.get("file").and_then(Value::as_str)?;
                    if selected.contains(file) {
                        return None;
                    }
                    Some(format!(
                        "{file} rejected: ranked pivot was not selected for the compact result"
                    ))
                })
                .take(3)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_file_refs(arguments: &Value, key: &str) -> Vec<StableRef> {
    arguments
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|file| {
            StableRef::FileRef(FileId {
                workspace_id: "workspace".to_string(),
                repo_relative_path: file.to_string(),
                content_hash: "unknown".to_string(),
            })
        })
        .collect()
}

fn parse_symbol_refs(arguments: &Value, key: &str) -> Vec<StableRef> {
    arguments
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|symbol| {
            StableRef::SymbolRef(SymbolId {
                file: FileId {
                    workspace_id: "workspace".to_string(),
                    repo_relative_path: String::new(),
                    content_hash: "unknown".to_string(),
                },
                qualified_name: symbol.to_string(),
                byte_offset: 0,
                kind: "symbol".to_string(),
            })
        })
        .collect()
}

fn stable_ref_from_identity(value: &Value) -> Option<StableRef> {
    let kind = value.get("kind")?.as_str()?;
    let encoded = value.get("value")?;
    match kind {
        "File" => serde_json::from_value::<FileId>(encoded.clone())
            .ok()
            .map(StableRef::FileRef),
        "Symbol" => serde_json::from_value::<SymbolId>(encoded.clone())
            .ok()
            .map(StableRef::SymbolRef),
        "Memory" => serde_json::from_value::<MemoryId>(encoded.clone())
            .ok()
            .map(StableRef::MemoryRef),
        "Event" => serde_json::from_value::<EventId>(encoded.clone())
            .ok()
            .map(StableRef::EventRef),
        "ContextHandle" => serde_json::from_value::<ContextHandleId>(encoded.clone())
            .ok()
            .map(StableRef::ContextHandleRef),
        _ => None,
    }
}

fn collect_argument_memory_ids(arguments: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    collect_argument_memory_ids_inner(arguments, &mut ids);
    ids.sort();
    ids.dedup();
    ids
}

fn collect_argument_memory_ids_inner(value: &Value, ids: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(id) = object.get("memory_id") {
                if let Some(value) = id.as_str() {
                    ids.push(value.to_string());
                } else if let Some(ulid) = id.get("ulid").and_then(Value::as_str) {
                    ids.push(ulid.to_string());
                }
            }
            for key in ["linked_memories", "memory_ids"] {
                if let Some(items) = object.get(key).and_then(Value::as_array) {
                    ids.extend(
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string),
                    );
                }
            }
            for child in object.values() {
                collect_argument_memory_ids_inner(child, ids);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_argument_memory_ids_inner(child, ids);
            }
        }
        _ => {}
    }
}

fn dedupe_refs(items: Vec<StableRef>) -> Vec<StableRef> {
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        let key = format!("{item:?}");
        if seen.insert(key) {
            deduped.push(item);
        }
    }
    deduped
}

fn summarize_outcome(prepared: &PreparedOutcome) -> String {
    json!({
        "input": compact_text(&prepared.input_summary, 180),
        "anchors": prepared
            .resolved_anchors
            .iter()
            .map(|item| format!("{item:?}"))
            .collect::<Vec<_>>(),
        "selected": prepared.selected_candidates,
        "excluded": prepared.excluded_candidates,
        "summary": compact_text(&prepared.response_summary, 220),
        "downstream_use_events": prepared
            .downstream_references
            .iter()
            .filter_map(|item| match item {
                StableRef::EventRef(event_id) => Some(event_id.ulid.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}
