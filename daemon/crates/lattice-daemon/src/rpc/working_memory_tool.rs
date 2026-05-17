use lattice_core::working_memory::{CheckpointId, WorkingMemoryState, WorkingMemorySummary};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InspectWorkingMemoryArgs {
    pub task_id: String,
    #[serde(default)]
    pub mode: InspectWorkingMemoryMode,
    #[serde(default)]
    pub include_excluded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectWorkingMemoryMode {
    #[default]
    Compact,
    Diagnostic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InspectWorkingMemoryCompactResponse {
    pub task_id: String,
    pub mode: &'static str,
    pub checkpoint_id: Option<CheckpointId>,
    pub expansion_handle: String,
    pub summary: WorkingMemorySummary,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InspectWorkingMemoryDiagnosticResponse {
    pub task_id: String,
    pub mode: &'static str,
    pub checkpoint_id: Option<CheckpointId>,
    pub expansion_handle: String,
    pub state: WorkingMemoryState,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "inspect_working_memory",
        "description": "Inspect the current working-memory state for a task, including compact summary and diagnostic state views with an expansion handle.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id whose working memory should be inspected"
                },
                "mode": {
                    "type": "string",
                    "description": "Response mode: 'compact' (default) returns a summary; 'diagnostic' returns the full state snapshot",
                    "enum": ["compact", "diagnostic"],
                    "default": "compact"
                },
                "include_excluded": {
                    "type": "boolean",
                    "description": "When true, include excluded memories and reasons in diagnostic output",
                    "default": false
                }
            },
            "required": ["task_id"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<InspectWorkingMemoryArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid inspect_working_memory arguments: {error}"))
}

pub fn compact_response_value(
    task_id: String,
    checkpoint_id: Option<CheckpointId>,
    expansion_handle: String,
    summary: WorkingMemorySummary,
) -> Result<Value, String> {
    serde_json::to_value(InspectWorkingMemoryCompactResponse {
        task_id,
        mode: "compact",
        checkpoint_id,
        expansion_handle,
        summary,
    })
    .map_err(|error| format!("Failed to serialize compact working memory response: {error}"))
}

pub fn diagnostic_response_value(
    task_id: String,
    checkpoint_id: Option<CheckpointId>,
    expansion_handle: String,
    state: WorkingMemoryState,
) -> Result<Value, String> {
    serde_json::to_value(InspectWorkingMemoryDiagnosticResponse {
        task_id,
        mode: "diagnostic",
        checkpoint_id,
        expansion_handle,
        state,
    })
    .map_err(|error| format!("Failed to serialize diagnostic working memory response: {error}"))
}
