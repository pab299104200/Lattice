# Extension Review UI Guide

This guide documents the Phase 10 review UI. It is driven by [## Human Review Surface](../plans/2026-05-16-cognitive-workspace-fork-plan.md#human-review-surface), [## Phase 10: Human Review And Extension UX](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-10-human-review-and-extension-ux), and [## Documentation Requirements](../plans/2026-05-16-cognitive-workspace-fork-plan.md#documentation-requirements).

## Overview

The review UI is a VS Code extension panel for inspecting and correcting durable memory, proposal queues, stale state, event traces, retrieval explanations, and daemon health. It is a review and trust surface, not the primary assistant interface.

Phase 10 implementation paths:

- T71 shell and bridge: `extension/src/review/reviewPanel.ts`, `extension/src/review/rpcBridge.ts`, `extension/src/review/rpcPayloads.ts`, `extension/src/review/i18n/en.json`
- T72 memory inbox: `extension/src/review/memoryInbox.ts`, `extension/src/review/components/MemoryRow.ts`
- T73 queues and dialogs: `extension/src/review/promotionQueue.ts`, `extension/src/review/contradictionQueue.ts`, `extension/src/review/components/ProposalDialog.ts`
- T74 stale and evidence: `extension/src/review/staleView.ts`, `extension/src/review/evidenceInspector.ts`
- T75 event trace and retrieval explanation: `extension/src/review/eventTraceView.ts`, `extension/src/review/retrievalExplanationView.ts`
- T76 health views: `extension/src/review/consolidationQueueView.ts`, `extension/src/review/indexingHealthView.ts`, `extension/src/review/workspaceGraphHealthView.ts`

## Opening the review panel

Open the panel from the Lattice VS Code extension command registered by `extension/src/extension.ts`. The panel uses the daemon bridge in `extension/src/review/rpcBridge.ts`; if the daemon does not respond, inspect binary deployment from [Operator Guide](./2026-05-16-operator-guide.md#installation).

## Memory inbox

The memory inbox lists memory records with class, scope, verification status, freshness state, contradiction or supersession labels, evidence counts, and review actions. It is backed by memory MCP surfaces such as `list_observations`, `search_memory`, `get_task_memory`, and `verify_explain_memory` as available through the bridge.

Use the inbox to inspect new or changed memory before promotion to higher scopes.

## Promotion queue and contradiction queue

The promotion queue shows proposed memory promotions or durable writes. The contradiction queue shows proposed conflicts, counter-claims, supersessions, and invalidations. Queue actions use the proposal flow in [Consolidation Design](../architecture/2026-05-16-consolidation-design.md#proposal-apply-and-reject-flow).

Accepting a proposal must apply a scoped memory evolution and append event evidence. Rejecting must preserve the proposal and rejection reason for audit.

## Stale view and evidence inspector

The stale view lists memory whose linked files, symbols, docs, tests, branch scope, time-bound validity, or exact-span evidence no longer verifies. The evidence inspector shows the underlying memory detail, linked artifacts, provenance events, and verification explanation.

Use these views before refreshing, invalidating, or promoting stale memory. Stale memory can be shown for review, but must not appear as normal trusted guidance.

## Event trace and retrieval explanation

The event trace view pages through scoped task, session, or workspace events from `get_event_trace`. It is used to inspect workflow history, proposal provenance, tool calls, outcomes, and payload summaries.

The retrieval explanation view is the operator view for ranking diagnostics from [Retrieval Ranking Design](../architecture/2026-05-16-retrieval-ranking-design.md#diagnostic-mode). When the daemon cannot provide authoritative ranking data for a request id, the UI must render a truthful unsupported or not-reported state instead of inventing explanation data.

## Consolidation, indexing, and graph health

Health views summarize consolidation queue state, indexing health, and workspace graph health. They should distinguish authoritative counters from not-reported fields. Cross-link from health rows to event traces or stale edges when evidence exists.

Use these views when workflows miss context, memory proposals stop appearing, indexing is partial, or graph counts are inconsistent with the workspace.

## Accept/reject workflow

1. Open the relevant inbox or queue item.
2. Inspect content, scope, confidence, evidence, linked artifacts, and provenance.
3. Check stale, contradiction, supersession, and verification labels.
4. Use event trace or retrieval explanation when the source is unclear.
5. Accept only when the proposed state is supported in the declared scope.
6. Reject with a concrete reason when evidence is weak, stale, out of scope, or contradicted.

Accept/reject results should be visible in the queue state and event trace without requiring direct SQLite inspection.
