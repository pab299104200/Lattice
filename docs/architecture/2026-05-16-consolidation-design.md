# Consolidation Design

This is the authoritative consolidation engine reference. It implements [## Consolidation Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#consolidation-engine), [## Phase 6: Consolidation Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-6-consolidation-engine), and the LLM budget policy in [Consolidation LLM Budgets](./2026-05-16-consolidation-llm-budgets.md#per-job-budgets).

## Job types

Consolidation turns event traces into better durable memory. Supported job types are:

- create episode summaries from completed tasks
- promote repeated successful workflow traces into procedures
- promote recurring failures into failure patterns
- promote verified implementation facts into semantic repo memory
- detect duplicate memories
- detect contradictions
- detect supersession candidates
- demote unused or low-value memories
- mark stale memories after graph changes
- refresh memories whose evidence still matches current code
- propose docs updates when memory and docs diverge

Every job records provenance, source events, scope, mode, and outcome. Jobs that alter durable memory must emit memory lifecycle events described in [Event Log Design](./2026-05-16-event-log-design.md#event-kinds).

## Modes

Consolidation runs in four modes:

- synchronous: small post-task traces only; deterministic and bounded.
- background: scheduled or queue-driven larger histories.
- manual-review: high-impact repo, user, or organization memory changes.
- replay: rebuilding memory state from event history and snapshots.

Synchronous mode is not allowed to perform LLM calls or long-running scans. Background and manual-review modes may enqueue proposal-producing LLM work within documented queue and cost budgets.

## LLM-driven consolidation

LLM-driven jobs include episode summary generation, procedure extraction, contradiction detection, and failure-pattern extraction. Per [## Consolidation Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#consolidation-engine), they obey these constraints:

- produce proposal records, not direct durable-memory writes
- preserve prior memory state on failed or malformed responses
- run only in background or manual-review mode
- record model, prompt hash, response hash, source events, and cost metadata
- enforce bounded queue depth; when full, drop new jobs with a warning and event evidence rather than blocking the daemon
- follow the per-job budgets in [## Per-job budgets](./2026-05-16-consolidation-llm-budgets.md#per-job-budgets)

Deterministic approximations may mark a memory stale or create a low-confidence proposal when an LLM job is skipped, but they must not pretend to have completed the inference.

## Proposal apply and reject flow

Proposals are first-class review items. The canonical MCP action is `propose_memory_evolution` from [MCP Tool Reference](./2026-05-16-mcp-tool-reference.md#final-tool-list). Apply and reject operations must record actor, timestamp, reason, source proposal, previous memory state, and resulting memory state.

Apply flow:

1. Load proposal and source memory under workspace and scope checks.
2. Validate proposal state, target memory, and conflict relationships.
3. Apply creation, update, supersession, invalidation, or link changes in one transaction.
4. Append memory lifecycle and consolidation events.
5. Return an expansion handle for review.

Reject flow:

1. Load proposal under workspace and scope checks.
2. Record rejection reason and actor.
3. Preserve the proposal for audit.
4. Append a consolidation event without mutating the target memory.

## Reversibility

No consolidation job may silently rewrite high-scope memory. Reversibility requires preserved prior state, event-backed provenance, contradiction and supersession links instead of destructive edits, and operator-visible proposal history. Invalidated memory remains available for audit and conflict explanation, but it is excluded from trusted retrieval.

Rollback of an applied proposal is represented as a new memory evolution proposal that restores or supersedes state with explicit evidence.

## Replay-safe execution

Replay mode rebuilds consolidation-derived state from the event log and snapshots. Jobs must be idempotent: rerunning a deterministic job over the same event range should produce the same proposed state or detect an existing equivalent proposal. LLM-backed proposals use prompt and response hashes to avoid duplicating previously reviewed inference.

Replay must respect workspace, branch, and scope filters. It must not promote session memory into higher scopes unless the original apply event or review event proves that promotion occurred.
