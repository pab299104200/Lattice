# Crate Boundary Plan

This plan is the binding Phase 0 crate and module layout for the cognitive workspace branch. It follows the in-place extension decision in [2026-05-16-fork-or-extend-decision.md](./2026-05-16-fork-or-extend-decision.md) and the substrate overview in [2026-05-16-cognitive-workspace-architecture.md](./2026-05-16-cognitive-workspace-architecture.md).

The plan cites the fork spec's [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design) and [## Fork Strategy](../plans/2026-05-16-cognitive-workspace-fork-plan.md#fork-strategy) headings directly. It also preserves the current storage/search contract described in [2026-04-11-storage-and-search-backends.md#Contract-Notes](./2026-04-11-storage-and-search-backends.md#contract-notes) instead of duplicating that note.

## Decision

Phase 1 through Phase 7 will land as modules inside `lattice-core`, not as immediate sibling crates. The reserved crate names are still part of the contract: `lattice-identity`, `lattice-events`, `lattice-memory`, `lattice-retrieval`, `lattice-working-memory`, `lattice-consolidation`, and `lattice-verification`.

The in-core choice is deliberate:

- `lattice-core` already owns the graph, storage, query, memory, indexing, and workflow types that these substrates must share.
- The downstream Phase 1 and Phase 2 task files already target `daemon/crates/lattice-core/src/identity/` and `daemon/crates/lattice-core/src/events/`.
- Keeping the substrates in one crate avoids premature public-API stabilization while identities, events, memory links, and ranking contracts are still being shaped.
- The module boundaries below are extraction-ready. A later extraction to the reserved crate names is allowed only after the public API, storage migrations, and MCP compatibility shims are stable and a review proves no dependency cycle would be introduced.

## `lattice-core::identity`

Reserved crate name: `lattice-identity`.

Purpose: Own stable typed identities for files, directories, symbols, docs, sections, tests, events, memories, context handles, and runtime surfaces. It generalizes the existing `SymbolId` basis without replacing the current graph primary key.

Public API surface:

- identity value types and typed id encodings
- identity parser and serializer for MCP payloads
- resolver traits for path, symbol, heading, test, memory, event, and handle references
- ambiguity diagnostics for duplicate or stale names
- compatibility helpers for legacy name-based references

Allowed dependencies:

- `lattice-core::symbols`
- `lattice-core::graph`
- `lattice-core::storage`
- `lattice-core::workspace`
- `lattice-core::parser` for doc section and test identity extraction

Spec sections implemented: [### Phase 1: Unified Identity Model](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model), [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), and [## Fork Strategy](../plans/2026-05-16-cognitive-workspace-fork-plan.md#fork-strategy).

Boundary rationale: Identity sits below every later substrate, so it must not depend on events, memory, retrieval, working memory, consolidation, or verification. That keeps the dependency graph acyclic and preserves current graph and memory compatibility.

## `lattice-core::events`

Reserved crate name: `lattice-events`.

Purpose: Own the append-only event log, event payload hashing, replay readers, event compaction metadata, and workflow/session correlation. It records assistant and workflow activity through stable identities from `identity`.

Public API surface:

- event kind taxonomy
- event envelope and referenced-identity fields
- append-only writer API
- bounded reader/query API
- replay cursors and snapshot coordination types
- payload hashing and spillover payload references

Allowed dependencies:

- `lattice-core::identity`
- `lattice-core::storage`
- `lattice-core::workspace`
- `lattice-core::memory` only through narrow memory-id references, not memory retrieval

Spec sections implemented: [### Phase 2: Event Log Substrate](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate), [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), and [## Fork Strategy](../plans/2026-05-16-cognitive-workspace-fork-plan.md#fork-strategy).

Boundary rationale: Events depend on identity and storage but remain below memory consolidation and retrieval. Replay readers may produce graph or memory rebuild inputs; they do not call high-level workflow code.

## `lattice-core::memory_graph`

Reserved crate name: `lattice-memory`.

Purpose: Own normalized memory classes, links, evidence, accesses, scores, contradiction/supersession semantics, and migration adapters from the existing `lattice-core::memory` row model.

Public API surface:

- memory class taxonomy including `CounterMemory`
- memory link and evidence models
- memory access and score writers
- migration/backfill helpers from existing `memories` rows
- trust, freshness, contradiction, and supersession queries
- compatibility adapters for current observation-oriented MCP tools

Allowed dependencies:

- `lattice-core::identity`
- `lattice-core::events`
- `lattice-core::memory`
- `lattice-core::storage`
- `lattice-core::workspace`

Spec sections implemented: [### Phase 3: Memory Graph Storage](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-3-memory-graph-storage), [### 4. Memory Graph](../plans/2026-05-16-cognitive-workspace-fork-plan.md#4-memory-graph), and [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design).

Boundary rationale: The module wraps the existing memory store instead of forking it. Existing `memories` rows remain the compatibility table while normalized side tables become the first-class model for new behavior.

## `lattice-core::retrieval`

Reserved crate name: `lattice-retrieval`.

Purpose: Own Retrieval V1: task intent, anchor extraction, hybrid candidate retrieval, explainable scoring, compact/full response shaping, and diagnostic ranking reasons across graph, docs, memories, and events.

Public API surface:

- task-intent classifier
- anchor extractor and resolver integration
- candidate model across graph, docs, events, and memories
- scoring configuration and score explanations
- compact and diagnostic bundle shapers
- benchmark fixture entry points

Allowed dependencies:

- `lattice-core::identity`
- `lattice-core::events`
- `lattice-core::memory_graph`
- `lattice-core::graph`
- `lattice-core::query`
- `lattice-core::storage`
- `lattice-core::intelligence` only through adapter traits during migration

Spec sections implemented: [### Phase 4: Retrieval V1](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-4-retrieval-v1), [## MCP Tool Contract Principles](../plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-tool-contract-principles), and [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design).

Boundary rationale: Retrieval is above graph, event, memory, and identity storage. It must not be called by those lower modules, or the shared substrates would become cyclic and hard to test.

## `lattice-core::working_memory`

Reserved crate name: `lattice-working-memory`.

Purpose: Own explicit per-task working memory state, included/excluded context bookkeeping, pins, evictions, expansion state, compression state, and checkpoint restore.

Public API surface:

- task working-memory state model
- operations for retrieve, summarize, filter, pin, evict, expand, compress, and checkpoint
- checkpoint serializer and restore reader
- context-handle integration types
- MCP inspection payload models

Allowed dependencies:

- `lattice-core::identity`
- `lattice-core::events`
- `lattice-core::retrieval`
- `lattice-core::storage`
- `lattice-core::intelligence` only through workflow adapters

Spec sections implemented: [### Phase 5: Working Memory](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-5-working-memory), [## MCP Surface](../plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-surface), and [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design).

Boundary rationale: Working memory consumes retrieval results and emits events. It is task-local state, so it must not own durable memory graph semantics or identity resolution rules.

## `lattice-core::consolidation`

Reserved crate name: `lattice-consolidation`.

Purpose: Own background consolidation jobs, episode summaries, memory promotion proposals, duplicate detection, contradiction detection, supersession proposals, procedure extraction, failure-pattern extraction, and review-queue handoff.

Public API surface:

- consolidation job model and lifecycle
- replay-safe job runner traits
- deterministic proposal generation helpers
- LLM proposal metadata with model, prompt hash, and response hash
- review queue payload types
- reversible job outcome records
- replay drivers that rebuild memory state from `MemoryConsolidated` events and verify `post_apply_state_hash` against the reconstructed durable state

Allowed dependencies:

- `lattice-core::identity`
- `lattice-core::events`
- `lattice-core::memory_graph`
- `lattice-core::retrieval`
- `lattice-core::storage`
- `lattice-core::workspace`

Spec sections implemented: [### Phase 6: Consolidation Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-6-consolidation-engine), [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), and the event compaction requirement under [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design).

Boundary rationale: Consolidation is background work over events and memories. It may propose memory changes but must not silently bypass memory graph validation or verification.

## `lattice-core::verification`

Reserved crate name: `lattice-verification`.

Purpose: Own incremental memory verification, file/symbol/doc/test existence checks, exact-span evidence validation, branch/workspace scope enforcement, expiry, stale-memory surfacing, and verification diagnostics.

Public API surface:

- verification job model and lifecycle
- evidence validators for identity-backed references
- freshness and expiry evaluators
- stale, contradicted, superseded, and invalidated status transitions
- operator diagnostic payloads
- incremental graph-change verification hooks

Allowed dependencies:

- `lattice-core::identity`
- `lattice-core::events`
- `lattice-core::memory_graph`
- `lattice-core::graph`
- `lattice-core::storage`
- `lattice-core::workspace`

Spec sections implemented: [### Phase 7: Verification And Freshness](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-7-verification-and-freshness), [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), and the trust rules in [### 4. Memory Graph](../plans/2026-05-16-cognitive-workspace-fork-plan.md#4-memory-graph).

Boundary rationale: Verification consumes graph facts and memory evidence. It writes verification results and events, but retrieval only reads those results; retrieval must not decide trust state itself.

## Dependency graph

The Phase 1 through Phase 7 module graph is directed and acyclic:

```text
lattice-daemon
  -> lattice-core::working_memory
  -> lattice-core::retrieval
  -> lattice-core::consolidation
  -> lattice-core::verification
  -> lattice-core::events
  -> lattice-core::memory_graph
  -> lattice-core::identity

lattice-core::consolidation
  -> lattice-core::retrieval
  -> lattice-core::memory_graph
  -> lattice-core::events
  -> lattice-core::identity

lattice-core::working_memory
  -> lattice-core::retrieval
  -> lattice-core::events
  -> lattice-core::identity

lattice-core::retrieval
  -> lattice-core::memory_graph
  -> lattice-core::events
  -> lattice-core::identity

lattice-core::verification
  -> lattice-core::memory_graph
  -> lattice-core::events
  -> lattice-core::identity

lattice-core::memory_graph
  -> lattice-core::events
  -> lattice-core::identity

lattice-core::events
  -> lattice-core::identity

lattice-core::identity
  -> existing lattice-core graph, parser, storage, symbols, and workspace modules
```

No lower substrate may call a higher substrate. In particular, `identity` must not call `events`; `events` must not call `retrieval`, `working_memory`, `consolidation`, or `verification`; and `memory_graph` must not call `retrieval`.

## Test boundary

Each new module carries its own test modules under `src/` per the convention in [context.md#2-carry-forward-inventory-what-lattice-already-gives-us](../plans/2026-05-16-cognitive-workspace-fork-build/context.md#2-carry-forward-inventory-what-lattice-already-gives-us). Expected paths include:

- `daemon/crates/lattice-core/src/identity/tests.rs`
- `daemon/crates/lattice-core/src/events/tests.rs`
- `daemon/crates/lattice-core/src/memory_graph/tests.rs`
- `daemon/crates/lattice-core/src/retrieval/tests.rs`
- `daemon/crates/lattice-core/src/working_memory/tests.rs`
- `daemon/crates/lattice-core/src/consolidation/tests.rs`
- `daemon/crates/lattice-core/src/verification/tests.rs`

Module tests own local invariants. Cross-module behavior belongs in the highest module that composes the behavior, with daemon/MCP compatibility tests only at the RPC boundary.

## Extraction criteria

A module may move to its reserved crate name only when all of these are true:

- its public API has stopped changing for one full numbered phase
- all storage migrations it owns are documented in the storage migration policy
- all MCP compatibility shims affected by the move are documented and tested
- the resulting Cargo graph remains acyclic
- the crate has its own focused test target and does not require `lattice-daemon` to validate core invariants
