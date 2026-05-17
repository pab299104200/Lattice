# Event Log Design

This is the authoritative event log reference for the cognitive workspace successor. It is driven by [## Event Log](../plans/2026-05-16-cognitive-workspace-fork-plan.md#event-log), [## Phase 2: Event Log Substrate](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate), and [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design).

## Append-only invariant

The event log is append-only. Events record workflow history, tool usage, memory lifecycle, verification evidence, and operator corrections. Existing event rows are not rewritten to correct history; corrections are represented by later events that reference the earlier event or affected memory. This invariant makes replay, audit, metrics, and consolidation possible.

Derived graph and memory state may be rebuilt from source files, snapshots, and events. If derived state disagrees with the event log, recovery code treats the event log and compaction snapshots as the audit source.

## Event kinds

The required event kinds from [## Event Log](../plans/2026-05-16-cognitive-workspace-fork-plan.md#event-log) are:

| Event kind | Purpose |
|---|---|
| `AssistantTaskStarted` | Starts a task/session workflow correlation. |
| `ToolCalled` | Records an assistant or operator tool invocation. |
| `ToolResult` | Records completion, failure, summary, and payload reference for a tool invocation. |
| `ContextBundleReturned` | Records a workflow context bundle and its included/excluded candidates. |
| `MemoryRetrieved` | Records durable memory retrieval and inclusion reasons. |
| `MemoryExpanded` | Records focused memory expansion. |
| `PlanCreated` | Records a plan emitted by workflow tooling. |
| `FileRead` | Records file context read into the assistant workflow. |
| `PatchApplied` | Records a code or docs patch operation. |
| `TestRunStarted` | Records verification command start. |
| `TestRunCompleted` | Records verification command result. |
| `DiagnosticObserved` | Records compiler, test, runtime, or validation diagnostics. |
| `UserCorrection` | Records a user correction that should influence retrieval or memory. |
| `UserPreferenceObserved` | Records a user preference with scope discipline. |
| `WorkflowSucceeded` | Records successful task completion and evidence. |
| `WorkflowFailed` | Records failed or blocked task completion and cause. |
| `MemoryCreated` | Records creation of durable memory. |
| `MemoryUpdated` | Records memory refresh, edit, promotion, or metadata update. |
| `MemoryInvalidated` | Records invalidation, expiry, contradiction, or supersession state change. |
| `MemoryConsolidated` | Records consolidation proposal, apply, reject, or summary result. |

## Event envelope

Every event carries:

| Field | Required | Notes |
|---|---:|---|
| `workspace_id` | yes | Workspace-boundary anchor. |
| `branch` | yes when known | Required for branch-scoped replay and verification. |
| `session_id` | yes | Correlates assistant session and operator review. |
| `task_id` | yes when task-scoped | Correlates workflow events and outcomes. |
| `actor` | yes | Assistant, operator, daemon, or system identity. |
| `timestamp` | yes | Monotonic ordering is preserved by storage sequence when clocks tie. |
| `stable_references` | yes | Files, symbols, docs, events, memories, tests, and handles referenced by stable identity. |
| `payload_hash` | yes | Hash of the full payload or canonical compact payload. |
| `compact_payload_summary` | yes | Prompt-safe and review-safe summary. |
| `full_payload_location` | optional | Spillover table row, file, or snapshot reference for large payloads. |

## Payload spillover

Large payloads must not block hot-path event writes or cause unbounded prompt injection. The event row stores a hash, compact summary, and optional spillover pointer. The full payload is stored in a side table or payload store with the same workspace and event identity scoping.

Readers must verify payload hash on retrieval. Failure to retrieve spillover data returns an actionable diagnostic event or error; it must not silently fabricate a partial event.

## Compaction snapshots

Compaction is a daemon-managed background operation described by [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design) and [## Compaction and snapshots](./2026-05-16-cognitive-workspace-architecture.md#compaction-and-snapshots). At configurable intervals the daemon writes a versioned full-state snapshot of graph and memory, then truncates or archives events older than the snapshot boundary.

Snapshot requirements:

- independently readable format
- snapshot version and schema metadata
- workspace and branch scope metadata
- high-water event id and timestamp
- payload integrity hashes
- recovery path that can bootstrap without replaying the full event history

## Replay semantics

Replay reconstructs workflow history, memory lifecycle, and derived state from events after the latest valid snapshot. Replay must be idempotent: reprocessing the same event sequence yields the same derived state. Corrupt or missing payloads produce explicit diagnostics and stop only the affected reconstruction path when safe to do so.

Replay mode is also a consolidation mode; see [Consolidation Design](./2026-05-16-consolidation-design.md#replay-safe-execution).

## Hot-path budgets

[## Phase 2: Event Log Substrate](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate) sets the event-write budget: event capture must add no more than 5ms P99 to hot-path workflow calls such as `prepare_change` and `get_context_capsule`. This budget requires bounded payload summaries, indexed writes, spillover for large payloads, and no synchronous full-history scans.

The budget evidence pattern is documented in [## Phase 2 Budget Evidence](./2026-05-16-event-log-compaction.md#phase-2-budget-evidence).

## Workspace and session scoping

Events are always scoped to a workspace, session, and task when applicable. Query APIs such as `get_event_trace` must require an explicit task, session, or workspace scope and must not leak events across workspaces. Diagnostic modes may include payload hashes and spillover references, but they still follow the same scope filter.
