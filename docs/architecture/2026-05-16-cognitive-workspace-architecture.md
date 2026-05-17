# Cognitive Workspace Architecture

## Summary

This document is the Phase 0 successor architecture overview required by [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation). It translates the design contract from [## Design Thesis](../plans/2026-05-16-cognitive-workspace-fork-plan.md#design-thesis), [## System Architecture](../plans/2026-05-16-cognitive-workspace-fork-plan.md#system-architecture), and [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design) into an in-place branch architecture for `feat/cognitive-workspace`, consistent with [docs/architecture/2026-05-16-fork-or-extend-decision.md#Decision](./2026-05-16-fork-or-extend-decision.md#decision).

The binding model is a shared cognition substrate with three first-class sources of truth:

1. workspace graph
2. event log
3. memory graph

These substrates must share identity, storage, and ranking primitives. Phase sequencing can add new crates and tables over time, but it must not let each substrate drift into its own incompatible naming, storage, or retrieval contract.

The MCP compatibility implications of this architecture are defined in [docs/architecture/2026-05-16-mcp-compatibility-policy.md](./2026-05-16-mcp-compatibility-policy.md).

## Workspace graph

Per [## System Architecture](../plans/2026-05-16-cognitive-workspace-fork-plan.md#system-architecture), the workspace graph remains the source of truth for code, docs, tests, and operational surfaces. The current implementation already owns most of the graph substrate in `daemon/crates/lattice-core/src/graph`, `src/storage`, `src/indexer`, and `src/query`; later phases may split some ownership into planned crates such as `lattice-identity`, but the graph contract remains continuous.

### Node families

| Node family | Current or planned owner | Notes |
|---|---|---|
| `File` | Current: `lattice-core::graph`, `lattice-core::storage`, `lattice-core::watcher` | Existing file indexing and storage stay authoritative. |
| `Directory` | Current: `lattice-core::workspace`; planned stronger identity in `lattice-identity` | Currently implicit in workspace and path handling; Phase 1 should make directory identity explicit where useful. |
| `Symbol` | Current: `lattice-core::symbols`, `lattice-core::graph`, `lattice-core::storage` | Existing `SymbolId { file, name, byte_offset }` stays the durable basis. |
| `Type` | Current: `lattice-core::graph` via symbol parsing; planned richer type identity in `lattice-identity` | Remains graph-backed rather than a separate storage system. |
| `Module` | Current: `lattice-core::graph` and parser/indexer paths | Module boundaries continue to derive from language-specific graph ingestion. |
| `Test` | Current: workflow/intelligence and graph query surfaces; planned stronger first-class storage in graph tables | Existing test discovery remains, but `Test` identity must become explicit for verification and retrieval. |
| `Document` | Current: query/intelligence/doc capsules; planned stronger graph persistence | Markdown and durable docs stay part of the same graph substrate, not a parallel doc-only index. |
| `Section` | Current: docs retrieval flows; planned stronger identity in `lattice-identity` and graph storage | Stable section identity is required for follow-up expansion and stale-doc detection. |
| `ConfigKey` | Current: parser/query extraction where available; planned broader ingestion coverage | Remains a graph node because retrieval must connect config facts to code, docs, and workflows. |
| `Command` | Planned ingestion in graph and event-linked storage | Commands need graph identity because workflows, procedures, and diagnostics cite them repeatedly. |
| `Route` | Planned graph/indexer extension | Route nodes belong in the workspace graph because docs, tests, and runtime surfaces link to them. |
| `Schema` | Planned graph/indexer extension | Schema nodes cover API, config, and storage contracts needed by impact analysis and docs checks. |
| `Package` | Current workspace/build metadata ingestion; planned stronger graph persistence | Package ownership stays tied to workspace/build parsing rather than memory. |
| `BuildTarget` | Planned graph/indexer extension | Build targets are graph facts used by tests, diagnostics, and procedures. |
| `RuntimeSurface` | Planned graph plus event references | Runtime surfaces connect code, routes, commands, diagnostics, and operations. |

### Edge families

| Edge family | Current or planned owner | Notes |
|---|---|---|
| `contains` | Current graph/store | Core file, symbol, and doc containment stays graph-native. |
| `imports` | Current graph/store | Already part of code graph semantics. |
| `calls` | Current graph/store | Already part of code graph semantics. |
| `implements` | Current graph/store | Existing type and symbol relations extend here. |
| `extends` | Current graph/store | Existing type and symbol relations extend here. |
| `type_ref` | Current graph/store | Type reference edges stay in graph, not memory. |
| `tested_by` | Current workflows; planned first-class graph/storage edge | Needed for deterministic test recommendation and verification. |
| `documents` | Current docs/workflow logic; planned first-class graph/storage edge | Connects doc sections to code and schema nodes. |
| `mentions` | Current docs/workflow logic; planned first-class graph/storage edge | Enables backlinks, stale-doc detection, and retrieval evidence. |
| `depends_on` | Current graph/store | Remains the default impact-analysis edge family. |
| `configured_by` | Planned graph/storage edge | Connects config keys, schemas, and runtime surfaces. |
| `generated_by` | Planned graph/storage edge | Tracks generated assets and machine-produced files. |
| `co_changed_with` | Planned event-derived graph edge | Materialized from event history and diffs, but stored as graph state for bounded traversal. |
| `stale_against` | Planned verification-derived graph edge | Marks docs or memories whose claims have drifted against workspace facts. |

The graph continues to answer exact identity lookup, bounded neighborhood traversal, impact analysis, backlinks, and freshness queries. What changes is breadth and identity discipline, not the fundamental responsibility boundary.

## Event log

Per [## System Architecture](../plans/2026-05-16-cognitive-workspace-fork-plan.md#system-architecture), [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), and [### Phase 2: Event Log Substrate](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate), the event log becomes the append-only record of assistant and workflow activity.

### Required event types

The event taxonomy is fixed at Phase 0 so later phases do not drift:

- `AssistantTaskStarted`
- `ToolCalled`
- `ToolResult`
- `ContextBundleReturned`
- `MemoryRetrieved`
- `MemoryExpanded`
- `PlanCreated`
- `FileRead`
- `PatchApplied`
- `TestRunStarted`
- `TestRunCompleted`
- `DiagnosticObserved`
- `UserCorrection`
- `UserPreferenceObserved`
- `WorkflowSucceeded`
- `WorkflowFailed`
- `MemoryCreated`
- `MemoryUpdated`
- `MemoryInvalidated`
- `MemoryConsolidated`

### Invariants

- The event log is append-only. Updates happen by appending new events, never by mutating historical rows in place.
- Events carry stable workspace, branch, session, task, actor, and referenced-identity metadata so replay remains deterministic across graph and memory evolution.
- Large payloads are hash-addressed and may spill to side storage, but every event row still carries a compact summary and payload hash.
- Event history is for audit, replay, metrics, consolidation, and retrieval. It is not a prompt dump surface.
- `MemoryConsolidated` payloads preserve the proposal id, prior state, proposed state, and a canonical `post_apply_state_hash` so replay can re-apply event data without re-running scanners or LLM calls and still detect divergence deterministically.

### Compaction and snapshots

The append-only invariant does not permit unbounded hot-path replay. Per [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), compaction is handled through versioned snapshots:

- the daemon writes a full graph-and-memory snapshot at configured compaction points
- the snapshot is independently readable and versioned
- the live event log is truncated only to post-snapshot history
- bootstrap may start from the newest valid snapshot plus post-snapshot replay

Compaction is therefore a state-transfer optimization, not a rewrite of history semantics.

### Performance budget

Phase 2 establishes a hard hot-path budget: event writing adds no more than 5 ms P99 to `prepare_change` and `get_context_capsule`, per [### Phase 2: Event Log Substrate](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate). That budget binds later event-schema, storage, and payload-shaping decisions. If a new event field or serialization path threatens the budget, it must move to spillover payload storage, asynchronous enrichment, or derived tables rather than slowing the write path.

### Writer behavior

The Phase 2 writer computes a canonical payload hash over sorted-key JSON and stores it as a stable `sha256:<hex>` identifier. Payloads at or below the writer's inline ceiling stay in `events.payload_inline`; larger payloads spill into `event_payloads` by content hash so repeated large payloads dedupe instead of re-inflating the hot path.

Writer calls are workspace-bound at the API boundary: a caller may omit `workspace_id` and inherit the store's workspace, but it may not override that workspace silently. Synchronous flush mode is reserved for tests and durability-sensitive verification; daemon hot-path use defaults to batched WAL checkpointing so event commits stay bounded.

### Reader behavior

The Phase 2 reader is intentionally scope-bound. Every query must anchor on `task_id`, `session_id`, or the `workspace_id + branch` pair; unscoped reads are rejected before SQLite runs. Queries default to 1000 rows and reject requests above 10,000 rows so retrieval, verification, and metrics workflows cannot accidentally turn the event log into a full-scan substrate.

Reader reconstruction always returns full `EventEnvelope` values, regardless of whether the payload lived inline or in `event_payloads`. On read, the loader recomputes the payload hash from the stored bytes and fails the query if the bytes no longer match the envelope hash, preserving replay and audit integrity instead of silently serving corrupted payloads.

### Ownership

Current and planned ownership is intentionally split:

- current daemon integration: `daemon/crates/lattice-daemon/src/rpc` records tool and workflow boundaries
- planned storage and APIs: `lattice-events` or equivalent Phase 2 module owns append-only tables, readers, writers, and replay helpers
- shared identity: `lattice-identity` or equivalent Phase 1 module resolves referenced files, symbols, docs, sections, memories, and handles

## Memory graph

Per [## Design Thesis](../plans/2026-05-16-cognitive-workspace-fork-plan.md#design-thesis) and [### 4. Memory Graph](../plans/2026-05-16-cognitive-workspace-fork-plan.md#4-memory-graph), durable memory is a typed claim graph over workspace and event history, not an isolated table of note rows.

### Memory classes

The planned first-class memory classes are:

- `Observation`
- `Decision`
- `Constraint`
- `Pattern`
- `AntiPattern`
- `WorkflowOutcome`
- `FailurePattern`
- `Procedure`
- `Preference`
- `ArchitectureInvariant`
- `DocsContract`
- `OpenQuestion`
- `CounterMemory`

Current structured memory already covers several trust and relationship fields in `memories`; later phases normalize that into richer graph storage without breaking existing rows, as required by [docs/architecture/2026-05-16-fork-or-extend-decision.md#Migration-implication](./2026-05-16-fork-or-extend-decision.md#migration-implication).

### Link types

Memory links are first-class records, not JSON-only annotations. The planned link vocabulary from [### 4. Memory Graph](../plans/2026-05-16-cognitive-workspace-fork-plan.md#4-memory-graph) is:

- `supports`
- `contradicts`
- `supersedes`
- `refines`
- `generalizes`
- `specializes`
- `co_occurs_with`
- `derived_from`
- `applies_to`
- `validated_by`
- `invalidated_by`

Each link carries source, target, strength, reason, evidence event, creator, creation time, and verification status.

### Evidence model

Every durable memory requires evidence. The minimum evidence model is shared across direct memory writes, workflow outcome recording, consolidation proposals, and verification:

- provenance events identify where the claim came from
- evidence references point at files, symbols, docs, tests, events, or exact spans
- verification status describes current trust state
- freshness policy and validity conditions explain when the claim should be rechecked
- contradiction and supersession state remain explicit instead of hidden in ranking
- usefulness and access history stay queryable for retrieval and evaluation

Current groundwork for this model already exists in [docs/architecture/2026-04-11-structured-memory.md#Data-Model](./2026-04-11-structured-memory.md#data-model), [#Persistence-And-Migration](./2026-04-11-structured-memory.md#persistence-and-migration), and [#Explainability-Boundary](./2026-04-11-structured-memory.md#explainability-boundary).

### `CounterMemory` distinction

`CounterMemory` is a first-class memory record asserting that another memory is wrong, stale in meaning, or no longer applicable. That is a stronger construct than a `contradicts` link:

- a `contradicts` link expresses a relationship between two existing memories
- a `CounterMemory` record is its own claim with evidence, scope, verification, and lifecycle
- a `CounterMemory` can itself be contradicted, superseded, refreshed, or invalidated

Use a contradiction edge when the relationship is enough. Use `CounterMemory` when the counter-claim must be retrieved, reviewed, and verified as a standalone fact.

### Ownership

- current compatibility layer: `lattice-core::memory`
- current assistant-facing ranking and shaping: `lattice-core::intelligence`
- planned normalized storage and APIs: `lattice-memory`
- planned verification and consolidation integration: Phase 6 and Phase 7 modules over shared memory and event identities

Phase 7 verifier jobs use the shared workflow database and write `verification_jobs` rows, but verifier outcomes still flow through the proposal runtime rather than direct memory mutation. `mark_verified`, `mark_stale`, and `mark_invalidated` remain proposal kinds so replay, review, and event emission keep one mutation contract instead of giving verification a parallel write path. The `verification_jobs.reason` column remains `TEXT`, but mismatch diagnostics are allowed to store structured JSON payloads such as span-mismatch details so the review surface can distinguish citation drift from missing anchors without inventing a second diagnostics table.

Phase 7 freshness metadata also stays on the memory row as additive compatibility fields rather than a parallel verification-state table. `expires_at` gives time-bound memories an explicit expiry threshold, while `last_verified_graph_snapshot_id` records which post-index graph snapshot last re-validated the memory. The incremental verifier uses those fields together with `memory_evidence` reference narrowing so graph deltas verify only impacted memories, and successful verification still lands through proposal application instead of direct writes.

## Shared substrate primitives

The three substrates are separate sources of truth, but they are not allowed to invent separate foundational primitives.

### Identity

Identity is shared first. Files, symbols, docs, sections, events, memories, tests, and context handles need stable ids that can cross graph rows, event rows, memory evidence, and MCP payloads. Phase 1 formalizes that on top of the current `SymbolId` basis described in [docs/architecture/2026-04-11-stable-follow-up-handles.md#Durable-Identity-Basis](./2026-04-11-stable-follow-up-handles.md#durable-identity-basis).

### Storage

Storage is shared second. Per [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), SQLite remains the durable local store. The substrates may use separate logical tables and, where justified, separate database files such as `.lattice/events.db`, but they must share migration discipline, replayability, workspace scoping, and snapshot compatibility. Derived indexes, FTS, and vector indexes remain replaceable accelerators rather than hidden sources of truth.

### Ranking and retrieval

Ranking is shared third. Retrieval across graph, events, and memories must use one explainable scoring model family with task compatibility, identity anchors, proximity, verification, freshness, contradiction state, usefulness, and token cost as first-class signals. Memory ranking cannot ignore graph proximity, and graph ranking cannot ignore validated memory or event evidence. This is the core operational consequence of [## Design Thesis](../plans/2026-05-16-cognitive-workspace-fork-plan.md#design-thesis).

### Shared MCP contract consequences

Because the substrates share primitives, assistant-facing workflows keep one bounded contract:

- compact and full render modes stay consistent
- context handles and stable expansion targets remain portable across graph, event, and memory-backed tools
- diagnostic explanations use the same trust and inclusion vocabulary
- backward compatibility is enforced at the tool boundary, not left to each substrate team

The detailed compatibility rules live in [docs/architecture/2026-05-16-mcp-compatibility-policy.md#Backward-compatibility](./2026-05-16-mcp-compatibility-policy.md#backward-compatibility).

## Reference to existing docs

This overview is the successor umbrella document. It does not replace the verified lower-level notes below; it binds them together and points later phases at the right detailed contract.

- [docs/architecture/2026-04-11-structured-memory.md#Summary](./2026-04-11-structured-memory.md#summary)
- [docs/architecture/2026-04-11-structured-memory.md#Data-Model](./2026-04-11-structured-memory.md#data-model)
- [docs/architecture/2026-04-11-structured-memory.md#Persistence-And-Migration](./2026-04-11-structured-memory.md#persistence-and-migration)
- [docs/architecture/2026-04-11-stable-follow-up-handles.md#Contract](./2026-04-11-stable-follow-up-handles.md#contract)
- [docs/architecture/2026-04-11-storage-and-search-backends.md#Contract-Notes](./2026-04-11-storage-and-search-backends.md#contract-notes)
- [docs/architecture/2026-04-11-patch-oriented-planning.md#Bundle-Shape](./2026-04-11-patch-oriented-planning.md#bundle-shape)
- [docs/architecture/2026-04-11-scenario-tracing.md#Contract](./2026-04-11-scenario-tracing.md#contract)

Those notes remain authoritative for already-implemented behavior. This document is authoritative for the successor architecture boundary across the three substrates.
