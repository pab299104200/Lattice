//! Paginated event-trace MCP surface for audit, replay, and debugging.
//!
//! This module implements the `## MCP Surface`, `## Event Log`, and
//! `## MCP Tool Contract Principles` contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.
//! Responses are compact by default and expose stable expansion handles for
//! callers that want full payload inspection.

use lattice_core::events::{EventEnvelope, EventKind, PayloadLocation, StableRef};
use lattice_core::identity::{encode_identity, Identity};
use lattice_core::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Arguments for the `get_event_trace` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetEventTraceArgs {
    /// Optional task scope.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Optional session scope.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Optional workspace scope.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Optional event-kind filter.
    #[serde(default)]
    pub kinds: Vec<EventKind>,
    /// Optional inclusive lower bound.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Optional inclusive upper bound.
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Opaque pagination cursor from a prior page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Requested page size.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Response verbosity.
    #[serde(default)]
    pub render_mode: Option<EventTraceRenderMode>,
}

/// Response verbosity for event traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTraceRenderMode {
    Compact,
    Full,
    Diagnostic,
}

impl Default for EventTraceRenderMode {
    fn default() -> Self {
        Self::Compact
    }
}

/// Event-scope discriminator for the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventTraceScope {
    /// Scope kind returned by the page.
    pub kind: String,
    /// Scope value returned by the page.
    pub value: String,
}

/// One event entry in the paginated response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventTraceEntry {
    /// Stable event identity.
    pub event_id: String,
    /// Expansion handle for later payload retrieval.
    pub expansion_handle: String,
    /// Event kind.
    pub kind: String,
    /// Actor attached to the event envelope.
    pub actor: String,
    /// UTC timestamp for the event.
    pub timestamp: DateTime<Utc>,
    /// Workspace id attached to the event.
    pub workspace_id: String,
    /// Branch attached to the event.
    pub branch: String,
    /// Session id attached to the event.
    pub session_id: String,
    /// Task id attached to the event, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// One-line compact summary.
    pub summary: String,
    /// Stable references carried by the event.
    pub references: Vec<String>,
    /// Full payload in full/diagnostic modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Canonical payload hash in diagnostic mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_hash: Option<String>,
    /// Spilled-payload row id in diagnostic mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spilled_payload_row_id: Option<i64>,
}

/// Top-level paginated event trace response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventTracePage {
    /// Scope used for the trace page.
    pub scope: EventTraceScope,
    /// Render mode used to shape entries.
    pub render_mode: EventTraceRenderMode,
    /// Cursor supplied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Cursor for the next page, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Returned event entries.
    pub events: Vec<EventTraceEntry>,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "get_event_trace",
        "description": "Return a paginated, workspace-bounded event trace for a task, session, or workspace with compact, full, or diagnostic rendering.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": {"type": "string"},
                "session_id": {"type": "string"},
                "workspace_id": {"type": "string"},
                "kinds": {"type": "array", "items": {"type": "string"}},
                "since": {"type": "string", "format": "date-time"},
                "until": {"type": "string", "format": "date-time"},
                "cursor": {"type": "string"},
                "limit": {"type": "integer", "default": 25},
                "render_mode": {"type": "string", "enum": ["compact", "full", "diagnostic"], "default": "compact"}
            },
            "required": []
        }
    })
}

pub fn parse_args(args: &Value) -> Result<GetEventTraceArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid get_event_trace arguments: {error}"))
}

pub fn validate_args(args: &GetEventTraceArgs) -> Result<(), String> {
    let scope_count = usize::from(args.task_id.is_some())
        + usize::from(args.session_id.is_some())
        + usize::from(args.workspace_id.is_some());
    if scope_count != 1 {
        return Err(
            "get_event_trace requires exactly one of task_id, session_id, or workspace_id"
                .to_string(),
        );
    }
    if args.limit.unwrap_or(25) == 0 {
        return Err("get_event_trace limit must be greater than zero".to_string());
    }
    Ok(())
}

pub fn encode_cursor(row_id: i64) -> String {
    format!("event-row:{row_id}")
}

pub fn decode_cursor(cursor: &str) -> Result<i64, String> {
    cursor
        .strip_prefix("event-row:")
        .ok_or_else(|| "cursor must start with `event-row:`".to_string())?
        .parse::<i64>()
        .map_err(|error| format!("Invalid event cursor: {error}"))
}

pub fn build_entry(event: &EventEnvelope, render_mode: EventTraceRenderMode) -> EventTraceEntry {
    let payload = match render_mode {
        EventTraceRenderMode::Compact => None,
        EventTraceRenderMode::Full | EventTraceRenderMode::Diagnostic => {
            Some(serde_json::to_value(&event.payload).unwrap_or_else(|_| json!({})))
        }
    };
    let payload_hash = matches!(render_mode, EventTraceRenderMode::Diagnostic)
        .then(|| event.payload_hash.to_string());
    let spilled_payload_row_id = match (&event.payload_location, render_mode) {
        (PayloadLocation::Spilled { row_id }, EventTraceRenderMode::Diagnostic) => Some(*row_id),
        _ => None,
    };
    EventTraceEntry {
        event_id: event.event_id.to_string(),
        expansion_handle: encode_identity(&Identity::Event(event.event_id.clone())),
        kind: event.kind.as_str().to_string(),
        actor: actor_label(&event.actor),
        timestamp: DateTime::from_unix_seconds(event.timestamp.timestamp()),
        workspace_id: event.workspace_id.clone(),
        branch: event.branch.name.clone(),
        session_id: event.session_id.value.clone(),
        task_id: event.task_id.as_ref().map(|value| value.value.clone()),
        summary: event.summary.as_str().to_string(),
        references: event.references.iter().map(reference_label).collect(),
        payload,
        payload_hash,
        spilled_payload_row_id,
    }
}

fn reference_label(reference: &StableRef) -> String {
    serde_json::to_string(reference).unwrap_or_else(|_| "reference".to_string())
}

fn actor_label(actor: &lattice_core::events::Actor) -> String {
    match actor {
        lattice_core::events::Actor::Assistant { model } => format!("assistant:{model}"),
        lattice_core::events::Actor::User => "user".to_string(),
        lattice_core::events::Actor::Tool { name } => format!("tool:{name}"),
        lattice_core::events::Actor::Daemon => "daemon".to_string(),
    }
}
