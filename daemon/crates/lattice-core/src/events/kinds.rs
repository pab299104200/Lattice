use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::events::DocSectionId;
use crate::identity::{ContextHandleId, EventId, FileId, Identity, MemoryId, SymbolId};

/// Event kinds defined by `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
/// `### 3. Event Log`:
/// `AssistantTaskStarted`, `ToolCalled`, `ToolResult`, `ContextBundleReturned`,
/// `MemoryRetrieved`, `MemoryExpanded`, `PlanCreated`, `FileRead`, `PatchApplied`,
/// `TestRunStarted`, `TestRunCompleted`, `DiagnosticObserved`, `UserCorrection`,
/// `UserPreferenceObserved`, `WorkflowSucceeded`, `WorkflowFailed`, `MemoryCreated`,
/// `MemoryUpdated`, `MemoryInvalidated`, `MemoryConsolidated`.
///
/// `ConsolidationFailed` is the Phase 6 extension required by
/// `docs/plans/2026-05-16-cognitive-workspace-fork-build/tasks/T42.md`
/// after T11's event-kind list because the `## 6. Consolidation Engine`
/// "LLM-driven consolidation" constraints require failed or malformed LLM
/// responses to emit a consolidation failure event.
///
/// `MemoryScopeFiltered` is the Phase 7 extension required by
/// `docs/plans/2026-05-16-cognitive-workspace-fork-build/tasks/T50.md`
/// because `## Risks — Scope Leakage` requires store-boundary filtering to
/// be auditable.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    AssistantTaskStarted,
    ToolCalled,
    ToolResult,
    ContextBundleReturned,
    MemoryRetrieved,
    MemoryExpanded,
    PlanCreated,
    FileRead,
    PatchApplied,
    TestRunStarted,
    TestRunCompleted,
    DiagnosticObserved,
    UserCorrection,
    UserPreferenceObserved,
    WorkflowSucceeded,
    WorkflowFailed,
    MemoryCreated,
    MemoryUpdated,
    MemoryInvalidated,
    MemoryConsolidated,
    ConsolidationFailed,
    MemoryScopeFiltered,
}

impl EventKind {
    /// Return the canonical snake_case wire name used in storage.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::AssistantTaskStarted => "assistant_task_started",
            EventKind::ToolCalled => "tool_called",
            EventKind::ToolResult => "tool_result",
            EventKind::ContextBundleReturned => "context_bundle_returned",
            EventKind::MemoryRetrieved => "memory_retrieved",
            EventKind::MemoryExpanded => "memory_expanded",
            EventKind::PlanCreated => "plan_created",
            EventKind::FileRead => "file_read",
            EventKind::PatchApplied => "patch_applied",
            EventKind::TestRunStarted => "test_run_started",
            EventKind::TestRunCompleted => "test_run_completed",
            EventKind::DiagnosticObserved => "diagnostic_observed",
            EventKind::UserCorrection => "user_correction",
            EventKind::UserPreferenceObserved => "user_preference_observed",
            EventKind::WorkflowSucceeded => "workflow_succeeded",
            EventKind::WorkflowFailed => "workflow_failed",
            EventKind::MemoryCreated => "memory_created",
            EventKind::MemoryUpdated => "memory_updated",
            EventKind::MemoryInvalidated => "memory_invalidated",
            EventKind::MemoryConsolidated => "memory_consolidated",
            EventKind::ConsolidationFailed => "consolidation_failed",
            EventKind::MemoryScopeFiltered => "memory_scope_filtered",
        }
    }
}

impl FromStr for EventKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "assistant_task_started" => Ok(EventKind::AssistantTaskStarted),
            "tool_called" => Ok(EventKind::ToolCalled),
            "tool_result" => Ok(EventKind::ToolResult),
            "context_bundle_returned" => Ok(EventKind::ContextBundleReturned),
            "memory_retrieved" => Ok(EventKind::MemoryRetrieved),
            "memory_expanded" => Ok(EventKind::MemoryExpanded),
            "plan_created" => Ok(EventKind::PlanCreated),
            "file_read" => Ok(EventKind::FileRead),
            "patch_applied" => Ok(EventKind::PatchApplied),
            "test_run_started" => Ok(EventKind::TestRunStarted),
            "test_run_completed" => Ok(EventKind::TestRunCompleted),
            "diagnostic_observed" => Ok(EventKind::DiagnosticObserved),
            "user_correction" => Ok(EventKind::UserCorrection),
            "user_preference_observed" => Ok(EventKind::UserPreferenceObserved),
            "workflow_succeeded" => Ok(EventKind::WorkflowSucceeded),
            "workflow_failed" => Ok(EventKind::WorkflowFailed),
            "memory_created" => Ok(EventKind::MemoryCreated),
            "memory_updated" => Ok(EventKind::MemoryUpdated),
            "memory_invalidated" => Ok(EventKind::MemoryInvalidated),
            "memory_consolidated" => Ok(EventKind::MemoryConsolidated),
            "consolidation_failed" => Ok(EventKind::ConsolidationFailed),
            "memory_scope_filtered" => Ok(EventKind::MemoryScopeFiltered),
            other => Err(format!("unknown event kind `{other}`")),
        }
    }
}

/// Tool result status recorded in event payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Succeeded,
    Failed,
    Partial,
}

/// Test run terminal status recorded in event payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestRunStatus {
    Passed,
    Failed,
    Cancelled,
}

/// Diagnostic severity normalized for the event stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// Payload for `assistant_task_started`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantTaskStartedPayload {
    /// Context handle created for the task, when available.
    pub context_handle_id: Option<ContextHandleId>,
    /// Prior events used to seed the task context.
    pub seed_event_ids: Vec<EventId>,
    /// Existing memories surfaced before the task started.
    pub initial_memory_ids: Vec<MemoryId>,
    /// Compact objective for the task start event.
    pub objective: String,
}

/// Payload for `tool_called`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCalledPayload {
    /// Unique call id for matching request and result events.
    pub call_id: String,
    /// Tool name invoked by the assistant or daemon.
    pub tool_name: String,
    /// Context handle supplied to the tool call.
    pub context_handle_id: Option<ContextHandleId>,
    /// Event that requested the tool call, when chained.
    pub source_event_id: Option<EventId>,
    /// Compact summary of the call input.
    pub input_summary: String,
}

/// Payload for `tool_result`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultPayload {
    /// Tool call id that this result satisfies.
    pub call_id: String,
    /// Tool name that produced the result.
    pub tool_name: String,
    /// Terminal status for the tool call.
    pub status: ToolResultStatus,
    /// Event id of the originating tool call.
    pub tool_call_event_id: Option<EventId>,
    /// Context handle produced by the tool, when any.
    pub output_context_handle_id: Option<ContextHandleId>,
    /// Memories materialized directly by the tool result.
    pub created_memory_ids: Vec<MemoryId>,
    /// Compact summary of the tool output.
    pub output_summary: String,
}

/// Payload for `context_bundle_returned`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleReturnedPayload {
    /// Stable handle for the returned bundle.
    pub context_handle_id: ContextHandleId,
    /// Event that requested the bundle.
    pub source_event_id: Option<EventId>,
    /// Files included in the returned bundle.
    pub file_ids: Vec<FileId>,
    /// Symbols included in the returned bundle.
    pub symbol_ids: Vec<SymbolId>,
    /// Document sections included in the bundle.
    pub doc_section_ids: Vec<DocSectionId>,
    /// Memories included in the bundle.
    pub memory_ids: Vec<MemoryId>,
    /// Approximate token budget of the returned bundle.
    pub token_estimate: u32,
}

/// Payload for `memory_retrieved`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRetrievedPayload {
    /// Query or task string used to retrieve memories.
    pub retrieval_query: String,
    /// Context handle associated with the retrieval.
    pub context_handle_id: Option<ContextHandleId>,
    /// Memories returned to the assistant.
    pub memory_ids: Vec<MemoryId>,
    /// Evidence events cited by the retrieval layer.
    pub supporting_event_ids: Vec<EventId>,
    /// Included context added to working memory by the operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included_context: Vec<IncludedContextDelta>,
    /// Excluded context produced by the operation and why it was excluded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_context: Vec<ExcludedContextDelta>,
}

/// Payload for `memory_expanded`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExpandedPayload {
    /// Memory whose neighborhood was expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<MemoryId>,
    /// Event that requested the expansion.
    pub source_event_id: Option<EventId>,
    /// Additional memories surfaced by expansion.
    pub linked_memory_ids: Vec<MemoryId>,
    /// Symbols surfaced by expansion.
    pub linked_symbol_ids: Vec<SymbolId>,
    /// Document sections surfaced by expansion.
    pub linked_doc_section_ids: Vec<DocSectionId>,
    /// Identity that anchored the expansion when it was not a memory id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion_identity: Option<Identity>,
    /// Included context added to working memory by the expansion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included_context: Vec<IncludedContextDelta>,
    /// Excluded context produced during expansion and why it was excluded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_context: Vec<ExcludedContextDelta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncludedContextDelta {
    pub identity: Identity,
    pub headline: String,
    pub inclusion_reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedContextDelta {
    pub identity: Identity,
    pub headline: String,
    pub exclusion_reason: String,
}

/// Payload for `plan_created`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCreatedPayload {
    /// Context handle attached to the plan output.
    pub context_handle_id: Option<ContextHandleId>,
    /// Event that triggered plan creation.
    pub source_event_id: Option<EventId>,
    /// Memory ids cited while creating the plan.
    pub memory_ids: Vec<MemoryId>,
    /// Number of plan steps produced.
    pub step_count: u32,
    /// Compact summary of the plan body.
    pub plan_summary: String,
}

/// Payload for `file_read`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReadPayload {
    /// File read by the assistant, daemon, or tool.
    pub file_id: FileId,
    /// Event that triggered the file read.
    pub source_event_id: Option<EventId>,
    /// Optional first byte offset included in the read.
    pub byte_start: Option<u32>,
    /// Optional exclusive end byte offset included in the read.
    pub byte_end: Option<u32>,
    /// Compact purpose statement for the read.
    pub reason: String,
}

/// Payload for `patch_applied`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchAppliedPayload {
    /// Stable patch or edit operation id.
    pub patch_id: String,
    /// Event that produced the patch.
    pub source_event_id: Option<EventId>,
    /// Files changed by the patch.
    pub file_ids: Vec<FileId>,
    /// Symbols changed by the patch.
    pub symbol_ids: Vec<SymbolId>,
    /// Number of inserted lines in the patch.
    pub lines_added: u32,
    /// Number of removed lines in the patch.
    pub lines_removed: u32,
}

/// Payload for `test_run_started`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRunStartedPayload {
    /// Stable test run id for matching start and completion.
    pub run_id: String,
    /// Event that triggered the test run.
    pub source_event_id: Option<EventId>,
    /// Files targeted by the test run.
    pub file_ids: Vec<FileId>,
    /// Symbols targeted by the test run.
    pub symbol_ids: Vec<SymbolId>,
    /// Command executed for the test run.
    pub command: String,
}

/// Payload for `test_run_completed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRunCompletedPayload {
    /// Stable id of the completed test run.
    pub run_id: String,
    /// Event id of the corresponding start event.
    pub started_event_id: Option<EventId>,
    /// Terminal status for the run.
    pub status: TestRunStatus,
    /// Number of passed tests.
    pub passed: u32,
    /// Number of failed tests.
    pub failed: u32,
    /// Number of skipped tests.
    pub skipped: u32,
    /// Diagnostics emitted during the run.
    pub diagnostic_event_ids: Vec<EventId>,
}

/// Payload for `diagnostic_observed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticObservedPayload {
    /// Stable diagnostic id emitted by the reporter.
    pub diagnostic_id: String,
    /// Event that observed or produced the diagnostic.
    pub source_event_id: Option<EventId>,
    /// File associated with the diagnostic.
    pub file_id: FileId,
    /// Symbol associated with the diagnostic, when resolved.
    pub symbol_id: Option<SymbolId>,
    /// Severity level of the diagnostic.
    pub severity: DiagnosticSeverity,
    /// Human-readable diagnostic summary.
    pub message: String,
}

/// Payload for `user_correction`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCorrectionPayload {
    /// Event directly corrected by the user.
    pub corrected_event_id: EventId,
    /// Files the user corrected or selected instead.
    pub file_ids: Vec<FileId>,
    /// Memories explicitly superseded by the correction.
    pub superseded_memory_ids: Vec<MemoryId>,
    /// Compact summary of the correction.
    pub correction_summary: String,
}

/// Payload for `user_preference_observed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPreferenceObservedPayload {
    /// Canonical preference key observed from user behavior.
    pub preference_key: String,
    /// Normalized preference value or setting.
    pub preference_value: String,
    /// Event from which the preference was inferred.
    pub observed_from_event_id: Option<EventId>,
    /// Memory record capturing the durable preference, when any.
    pub memory_id: Option<MemoryId>,
}

/// Payload for `workflow_succeeded`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSucceededPayload {
    /// Workflow name or bundle that succeeded.
    pub workflow_name: String,
    /// Event that closed the workflow successfully.
    pub terminal_event_id: Option<EventId>,
    /// Output handle produced by the workflow.
    pub output_context_handle_id: Option<ContextHandleId>,
    /// Memory ids created or strengthened by the workflow.
    pub memory_ids: Vec<MemoryId>,
    /// Compact success summary.
    pub result_summary: String,
}

/// Payload for `workflow_failed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowFailedPayload {
    /// Workflow name or bundle that failed.
    pub workflow_name: String,
    /// Event that terminated the workflow.
    pub terminal_event_id: Option<EventId>,
    /// Diagnostics explaining the failure.
    pub diagnostic_event_ids: Vec<EventId>,
    /// Whether the failure is safe to retry automatically.
    pub retryable: bool,
    /// Compact failure summary.
    pub failure_summary: String,
}

/// Payload for `memory_created`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCreatedPayload {
    /// Memory created by the workflow.
    pub memory_id: MemoryId,
    /// Memory class written into the memory graph.
    pub class: String,
    /// Deterministic stream classification for the created memory.
    pub stream: String,
    /// Scope name for the created memory.
    pub scope: String,
    /// Caller-supplied idempotency key used for dedupe.
    pub idempotency_key: String,
    /// Event that requested the memory creation.
    pub source_event_id: Option<EventId>,
    /// Evidence events attached to the new memory.
    pub evidence_event_ids: Vec<EventId>,
    /// Symbols linked to the created memory.
    pub symbol_ids: Vec<SymbolId>,
    /// Document sections linked to the created memory.
    pub doc_section_ids: Vec<DocSectionId>,
    /// Optional exact post-write snapshot for replayable memory-graph rebuilds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_snapshot_json: Option<String>,
}

/// Payload for `memory_updated`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUpdatedPayload {
    /// Memory updated by the workflow.
    pub memory_id: MemoryId,
    /// Prior event that recorded the older memory state.
    pub previous_event_id: Option<EventId>,
    /// Evidence events attached to the update.
    pub evidence_event_ids: Vec<EventId>,
    /// Optional memory id explicitly superseded by the update.
    pub superseded_memory_id: Option<MemoryId>,
    /// Compact summary of the update.
    pub update_summary: String,
    /// Optional exact post-write snapshot for replayable memory-graph rebuilds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_snapshot_json: Option<String>,
}

/// Payload for `memory_invalidated`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryInvalidatedPayload {
    /// Memory invalidated by the workflow.
    pub memory_id: MemoryId,
    /// Event that caused the invalidation.
    pub invalidated_by_event_id: Option<EventId>,
    /// Contradicting memories that justify invalidation.
    pub contradicting_memory_ids: Vec<MemoryId>,
    /// Compact invalidation reason.
    pub reason: String,
    /// Optional exact post-write snapshot for replayable memory-graph rebuilds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_snapshot_json: Option<String>,
}

/// Payload for `memory_consolidated`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConsolidatedPayload {
    /// Source memories consumed by consolidation.
    pub source_memory_ids: Vec<MemoryId>,
    /// Consolidated memory emitted by the pass.
    pub consolidated_memory_id: MemoryId,
    /// Source events used as evidence for consolidation.
    pub source_event_ids: Vec<EventId>,
    /// Compact summary of the consolidation result.
    pub consolidation_summary: String,
    /// Proposal whose explicit decision materialized this consolidation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// Immutable transition represented by this event (`applied` or `reverted`).
    /// Events without this field are historical audit records and are not replay authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
    /// Historical inline pre-decision state. New events resolve canonical proposal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_state_json: Option<String>,
    /// Historical inline proposed state. New events resolve canonical proposal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_state_json: Option<String>,
    /// Canonical post-apply state hash recorded at the original decision time.
    #[serde(default)]
    pub post_apply_state_hash: [u8; 32],
    /// Operator or policy identity that decided the proposal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// Optional human-readable explanation recorded with the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
}

/// Payload for `consolidation_failed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationFailedPayload {
    pub job_id: String,
    pub job_kind: String,
    pub mode: String,
    pub model_name: String,
    pub error_kind: String,
    pub error_message: String,
}

/// Payload for `memory_scope_filtered`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryScopeFilteredPayload {
    pub memory_id: MemoryId,
    pub attempted_workspace_id: String,
    pub attempted_branch: Option<String>,
    pub memory_scope: String,
}

/// Typed payload wrapper for every event kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum EventPayload {
    AssistantTaskStarted(AssistantTaskStartedPayload),
    ToolCalled(ToolCalledPayload),
    ToolResult(ToolResultPayload),
    ContextBundleReturned(ContextBundleReturnedPayload),
    MemoryRetrieved(MemoryRetrievedPayload),
    MemoryExpanded(MemoryExpandedPayload),
    PlanCreated(PlanCreatedPayload),
    FileRead(FileReadPayload),
    PatchApplied(PatchAppliedPayload),
    TestRunStarted(TestRunStartedPayload),
    TestRunCompleted(TestRunCompletedPayload),
    DiagnosticObserved(DiagnosticObservedPayload),
    UserCorrection(UserCorrectionPayload),
    UserPreferenceObserved(UserPreferenceObservedPayload),
    WorkflowSucceeded(WorkflowSucceededPayload),
    WorkflowFailed(WorkflowFailedPayload),
    MemoryCreated(MemoryCreatedPayload),
    MemoryUpdated(MemoryUpdatedPayload),
    MemoryInvalidated(MemoryInvalidatedPayload),
    MemoryConsolidated(MemoryConsolidatedPayload),
    ConsolidationFailed(ConsolidationFailedPayload),
    MemoryScopeFiltered(MemoryScopeFilteredPayload),
}

impl EventPayload {
    /// Return the `EventKind` corresponding to this payload variant.
    pub fn kind(&self) -> EventKind {
        match self {
            EventPayload::AssistantTaskStarted(_) => EventKind::AssistantTaskStarted,
            EventPayload::ToolCalled(_) => EventKind::ToolCalled,
            EventPayload::ToolResult(_) => EventKind::ToolResult,
            EventPayload::ContextBundleReturned(_) => EventKind::ContextBundleReturned,
            EventPayload::MemoryRetrieved(_) => EventKind::MemoryRetrieved,
            EventPayload::MemoryExpanded(_) => EventKind::MemoryExpanded,
            EventPayload::PlanCreated(_) => EventKind::PlanCreated,
            EventPayload::FileRead(_) => EventKind::FileRead,
            EventPayload::PatchApplied(_) => EventKind::PatchApplied,
            EventPayload::TestRunStarted(_) => EventKind::TestRunStarted,
            EventPayload::TestRunCompleted(_) => EventKind::TestRunCompleted,
            EventPayload::DiagnosticObserved(_) => EventKind::DiagnosticObserved,
            EventPayload::UserCorrection(_) => EventKind::UserCorrection,
            EventPayload::UserPreferenceObserved(_) => EventKind::UserPreferenceObserved,
            EventPayload::WorkflowSucceeded(_) => EventKind::WorkflowSucceeded,
            EventPayload::WorkflowFailed(_) => EventKind::WorkflowFailed,
            EventPayload::MemoryCreated(_) => EventKind::MemoryCreated,
            EventPayload::MemoryUpdated(_) => EventKind::MemoryUpdated,
            EventPayload::MemoryInvalidated(_) => EventKind::MemoryInvalidated,
            EventPayload::MemoryConsolidated(_) => EventKind::MemoryConsolidated,
            EventPayload::ConsolidationFailed(_) => EventKind::ConsolidationFailed,
            EventPayload::MemoryScopeFiltered(_) => EventKind::MemoryScopeFiltered,
        }
    }
}
