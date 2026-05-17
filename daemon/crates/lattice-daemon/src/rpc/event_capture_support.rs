use lattice_core::events::{
    CompactSummary, DiagnosticSeverity, EventStoreError, EventWriteError, StableRef, TestRunStatus,
    ToolResultStatus,
};
use lattice_core::identity::{ContextHandleId, EventId, FileId, MemoryId, SymbolId};
use serde_json::Value;
use thiserror::Error;

pub type PlanId = String;
// T15 defines the capture helper contract for later memory and verification wiring.
#[allow(dead_code)]
pub type PatchId = String;

// T15 defines the capture helper contract for later preference consolidation wiring.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreferenceScope {
    Session,
    Workspace,
    Repo,
}

// T15 defines the capture helper contract for later preference consolidation wiring.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreferenceValue {
    pub key: String,
    pub value: String,
}

// T15 defines the capture helper contract for later diagnostic surfaces.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticRecord {
    pub diagnostic_id: String,
    pub file_id: FileId,
    pub symbol_id: Option<SymbolId>,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

// T15 defines the capture helper contract for later test-run capture surfaces.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRunPair {
    pub run_id: String,
    pub file_ids: Vec<FileId>,
    pub symbol_ids: Vec<SymbolId>,
    pub command: String,
    pub status: TestRunStatus,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Success(Value),
    Error { code: i32, message: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EventCaptureError {
    #[error("event capture workspace `{actual}` does not match writer workspace `{expected}`")]
    WorkspaceMismatch { expected: String, actual: String },
}

pub(super) fn compact_summary(summary: String) -> Result<CompactSummary, EventWriteError> {
    CompactSummary::new(compact_text(&summary, 500)).map_err(|error| {
        EventStoreError::EnvelopeInvalid {
            reason: error.to_string(),
        }
        .into()
    })
}

pub(super) fn lock_error<T>(_error: std::sync::PoisonError<T>) -> EventWriteError {
    EventStoreError::EnvelopeInvalid {
        reason: "event capture lock was poisoned".to_string(),
    }
    .into()
}

pub(crate) fn compact_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub(super) fn tool_result_status(outcome: &ToolOutcome) -> ToolResultStatus {
    match outcome {
        ToolOutcome::Success(_) => ToolResultStatus::Succeeded,
        ToolOutcome::Error { .. } => ToolResultStatus::Failed,
    }
}

pub(super) fn status_label(outcome: &ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Success(_) => "succeeded",
        ToolOutcome::Error { .. } => "failed",
    }
}

pub(super) fn outcome_summary(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Success(value) => compact_text(&value.to_string(), 300),
        ToolOutcome::Error { code, message } => {
            format!("error {code}: {}", compact_text(message, 260))
        }
    }
}

pub(super) fn workflow_summary(success: bool, workflow_name: &str) -> String {
    if success {
        format!("Workflow succeeded: {workflow_name}")
    } else {
        format!("Workflow failed: {workflow_name}")
    }
}

pub(super) fn output_context_handle_id(outcome: &ToolOutcome) -> Option<ContextHandleId> {
    match outcome {
        ToolOutcome::Success(value) => parse_tool_payload(value)
            .as_ref()
            .and_then(context_handle_id_from_value),
        ToolOutcome::Error { .. } => None,
    }
}

pub(super) fn created_memory_ids(outcome: &ToolOutcome, workspace_id: &str) -> Vec<MemoryId> {
    match outcome {
        ToolOutcome::Success(value) => parse_tool_payload(value)
            .as_ref()
            .map(|payload| direct_memory_ids(payload, workspace_id))
            .unwrap_or_default(),
        ToolOutcome::Error { .. } => Vec::new(),
    }
}

pub(crate) fn parse_tool_payload(value: &Value) -> Option<Value> {
    value
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .find_map(parse_payload_text)
}

pub(super) fn context_handle_id_from_value(value: &Value) -> Option<ContextHandleId> {
    let fields = value.get("context_handle_identity")?.get("fields")?;
    serde_json::from_value(fields.clone()).ok()
}

pub(crate) fn memory_ids_from_value(value: &Value, workspace_id: &str) -> Vec<MemoryId> {
    let mut ids = Vec::new();
    collect_memory_ids(value, workspace_id, &mut ids);
    ids.sort_by(|a, b| a.ulid.cmp(&b.ulid));
    ids.dedup_by(|a, b| a.ulid == b.ulid && a.workspace_id == b.workspace_id);
    ids
}

pub(super) fn call_id_from_parent(tool: &str, parent: &EventId) -> String {
    format!("{}:{tool}", parent.ulid)
}

pub(super) fn file_refs(anchors: &[StableRef]) -> Vec<FileId> {
    anchors
        .iter()
        .filter_map(|item| match item {
            StableRef::FileRef(file_id) => Some(file_id.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn symbol_refs(anchors: &[StableRef]) -> Vec<SymbolId> {
    anchors
        .iter()
        .filter_map(|item| match item {
            StableRef::SymbolRef(symbol_id) => Some(symbol_id.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn doc_section_refs(anchors: &[StableRef]) -> Vec<lattice_core::events::DocSectionId> {
    anchors
        .iter()
        .filter_map(|item| match item {
            StableRef::DocSectionRef(section_id) => Some(section_id.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn memory_refs(anchors: &[StableRef]) -> Vec<MemoryId> {
    anchors
        .iter()
        .filter_map(|item| match item {
            StableRef::MemoryRef(memory_id) => Some(memory_id.clone()),
            _ => None,
        })
        .collect()
}

fn parse_payload_text(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .or_else(|| {
            let (_, rest) = text.split_once("\n\n### Structured Payload\n```json\n")?;
            serde_json::from_str(rest.strip_suffix("\n```")?).ok()
        })
        .or_else(|| {
            let (_, payload) = text.rsplit_once("<!-- lattice-metrics: ")?;
            serde_json::from_str(payload.strip_suffix(" -->")?).ok()
        })
}

fn direct_memory_ids(value: &Value, workspace_id: &str) -> Vec<MemoryId> {
    ["id", "memory_id"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(|id| memory_id(workspace_id, id))
        .collect()
}

fn collect_memory_ids(value: &Value, workspace_id: &str, ids: &mut Vec<MemoryId>) {
    match value {
        Value::Object(object) => {
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                if object.contains_key("content") || object.contains_key("memory_type") {
                    ids.push(memory_id(workspace_id, id));
                }
            }
            for child in object.values() {
                collect_memory_ids(child, workspace_id, ids);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_memory_ids(child, workspace_id, ids);
            }
        }
        _ => {}
    }
}

fn memory_id(workspace_id: &str, id: &str) -> MemoryId {
    MemoryId {
        workspace_id: workspace_id.to_string(),
        ulid: id.to_string(),
    }
}
