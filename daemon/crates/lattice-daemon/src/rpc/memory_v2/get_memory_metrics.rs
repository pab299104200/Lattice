//! Assistant-facing memory metrics surface for Phase 9 signals.
//!
//! This module implements the `## MCP Surface`, `## Event Log`, and
//! `## Phase 9: Metrics And Evaluation` contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.
//! It reuses the canonical `lattice_core::metrics` wire types rather than
//! maintaining a parallel RPC-only copy.

pub use lattice_core::metrics::{MetricScopeKind, MetricSignal, MetricTimeRange, MetricValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Arguments for the `get_memory_metrics` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetMemoryMetricsArgs {
    /// Aggregation scope for the requested metrics.
    #[serde(default)]
    pub scope: Option<MetricScopeKind>,
    /// Optional UTC time-range filter.
    #[serde(default)]
    pub time_range: Option<MetricTimeRange>,
    /// Optional subset of signals to return.
    #[serde(default)]
    pub signals: Vec<MetricSignal>,
    /// Response verbosity.
    #[serde(default)]
    pub render_mode: Option<MetricRenderMode>,
}

/// Response verbosity for the metrics payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricRenderMode {
    Compact,
    Full,
    Diagnostic,
}

impl Default for MetricRenderMode {
    fn default() -> Self {
        Self::Compact
    }
}

/// Top-level tool response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSnapshot {
    /// Requested scope.
    pub scope: MetricScopeKind,
    /// Requested render mode.
    pub render_mode: MetricRenderMode,
    /// Signals returned by the tool.
    pub signals: Vec<MetricValue>,
    /// Whether any returned signal was incomplete or still relied on session
    /// fallback data.
    pub incomplete: bool,
    /// Notes about fallback behavior and missing evidence.
    pub notes: Vec<String>,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "get_memory_metrics",
        "description": "Return Phase 9 memory and workflow metrics with explicit per-signal provenance and honest nulls when canonical metrics are unavailable.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "scope": {"type": "string", "enum": ["session", "branch", "repo", "user", "organization"], "default": "session"},
                "time_range": {
                    "type": "object",
                    "properties": {
                        "since": {"type": "string", "format": "date-time"},
                        "until": {"type": "string", "format": "date-time"}
                    }
                },
                "signals": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": [
                            "tool_calls_per_successful_task",
                            "irrelevant_files_opened_per_task",
                            "relevant_anchor_recall",
                            "memory_inclusion_precision",
                            "memory_later_used_rate",
                            "stale_memory_surfaced_rate",
                            "contradiction_missed_rate",
                            "tests_recommended_vs_needed",
                            "workflow_success_after_first_plan"
                        ]
                    }
                },
                "render_mode": {"type": "string", "enum": ["compact", "full", "diagnostic"], "default": "compact"}
            },
            "required": []
        }
    })
}

pub fn parse_args(args: &Value) -> Result<GetMemoryMetricsArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid get_memory_metrics arguments: {error}"))
}

pub fn requested_signals(args: &GetMemoryMetricsArgs) -> Vec<MetricSignal> {
    if args.signals.is_empty() {
        return MetricSignal::ALL.to_vec();
    }
    args.signals.clone()
}
