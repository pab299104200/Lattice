# Successor Architecture Overview

This is the front-door architecture reference for the cognitive workspace successor. It is driven by [## Documentation Requirements](../plans/2026-05-16-cognitive-workspace-fork-plan.md#documentation-requirements), [## System Architecture](../plans/2026-05-16-cognitive-workspace-fork-plan.md#system-architecture), and [## Phase 11: Hardening](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-11-hardening). Detailed substrate contracts live in the companion references linked from each section.

## Overview

The successor keeps Lattice's Rust daemon, MCP workflow vocabulary, context handles, workspace boundary protections, and VS Code extension integration, but treats memory, event history, ranking, verification, and review as first-class production substrates. The objective from [## Final Target](../plans/2026-05-16-cognitive-workspace-fork-plan.md#final-target) is an assistant-facing dependency graph and cognitive workspace that can be replayed, audited, evaluated, and corrected across repeated real tasks.

The core operator model is:

- MCP workflow tools return compact, explainable bundles with stable expansion handles.
- Every meaningful workflow action is correlated through append-only events.
- Durable memories are typed claims with evidence, scope, verification, and relationship state.
- Retrieval ranks code, docs, memories, events, and working-memory state through an explainable pipeline.
- The review UI lets humans inspect memory quality, proposals, stale state, event traces, ranking explanations, and daemon health.

The existing Phase 8 MCP contract is [MCP Tool Reference](./2026-05-16-mcp-tool-reference.md#final-tool-list). Compatibility policy is [MCP Compatibility Policy](./2026-05-16-mcp-compatibility-policy.md#backward-compatibility).

## Three substrates

The architecture is organized around three durable substrates and one transient task layer:

- Workspace graph: stable identities for files, symbols, docs, sections, tests, events, memories, and handles; see [## Workspace graph](./2026-05-16-cognitive-workspace-architecture.md#workspace-graph).
- Event log: append-only, replayable workflow history; authoritative details are in [Event Log Design](./2026-05-16-event-log-design.md#append-only-invariant).
- Memory graph: typed, scoped, verified claims and relationships; authoritative details are in [Memory Model Reference](./2026-05-16-memory-model-reference.md#memory-classes).
- Working memory: per-task selected context, hypotheses, plans, exclusions, and checkpoints; see [## Working Memory](../plans/2026-05-16-cognitive-workspace-fork-plan.md#working-memory).

The three durable substrates feed the retrieval engine described in [Retrieval Ranking Design](./2026-05-16-retrieval-ranking-design.md#pipeline). Verification and freshness rules are centralized in [Verification Freshness Design](./2026-05-16-verification-freshness-design.md#verification-checks).

## Identity model

Identity precedes every other contract. Events and memories must reference stable graph identities instead of unstable display names so replay, verification, links, and expansion handles remain meaningful after moves and renames. The identity families are defined in [## Phase 1: Unified Identity Model](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model) and implemented under `daemon/crates/lattice-core/src/identity/`.

Identity resolution must preserve workspace boundaries, report ambiguity instead of choosing arbitrary matches, and add no more than the budget documented in [## Phase 1: Unified Identity Model](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model). MCP payloads may include legacy names for compatibility, but stable identities are the source of truth.

## Storage layout

SQLite remains the local durable store unless a measured constraint proves it insufficient. Logical tables are split by substrate: workspace graph tables, append-only event tables, event payload spillover, memory graph tables, working-memory checkpoints, context handles, consolidation jobs, and verification jobs. The storage policy and migration order are in [Storage Migration Policy](./2026-05-16-storage-migration-policy.md#migration-order).

Storage invariants:

- Append-only events are not edited in place.
- Full event payloads may spill to `event_payloads` while the event row keeps hash and compact summary.
- Derived graph and memory state must be recoverable from source files, snapshots, and events.
- Compaction snapshots are daemon-managed, versioned, and independently readable; see [## Compaction snapshots](./2026-05-16-event-log-design.md#compaction-snapshots).

## Read paths

Assistant read paths should start with high-level MCP workflows from [MCP Tool Reference](./2026-05-16-mcp-tool-reference.md#final-tool-list). The normal read path is:

1. Resolve literal anchors into stable identities.
2. Retrieve graph, doc, memory, event, and working-memory candidates.
3. Rank candidates with reasons and budget controls.
4. Return a compact bundle with expansion handles.
5. Use `expand_context` for focused follow-up instead of broad file dumping.

Operator read paths use the VS Code review UI documented in [Extension Review UI Guide](../operator-guide/2026-05-16-extension-review-ui-guide.md#overview). Review surfaces must show truthful unsupported or not-reported states when the daemon lacks an authoritative payload.

## Write paths

Write paths are event-backed and scope-aware:

- Tool calls and workflow results append events before their results are used for metrics or consolidation.
- `save_memory` creates durable memory with evidence, validity conditions, invalidation triggers, and verification metadata.
- Consolidation creates proposals for LLM-driven or high-scope changes; apply/reject records preserve prior state.
- Verification jobs update memory state when graph, doc, branch, workspace, or time-bound evidence changes.

The authoritative consolidation flow is [Consolidation Design](./2026-05-16-consolidation-design.md#job-types). Memory write semantics are in [Memory Model Reference](./2026-05-16-memory-model-reference.md#memory-record-fields).

## Operational invariants

- Workspace boundaries and memory scopes are enforced on every read, write, event trace, and review payload.
- Stale, contradicted, superseded, expired, or invalidated memory cannot be displayed as normal trusted guidance.
- Compact modes must remain bounded enough for repeated assistant use; diagnostic modes expose ranking, verification, hashes, and excluded candidates where useful.
- LLM-driven consolidation is proposal-only and never rewrites high-scope memory silently.
- Hot paths avoid unbounded graph traversal, payload growth, and event-log scans as required by [## Non-Negotiable Product Properties](../plans/2026-05-16-cognitive-workspace-fork-plan.md#non-negotiable-product-properties).
- Public MCP behavior changes require matching docs, tests, and compatibility handling in the same change.
