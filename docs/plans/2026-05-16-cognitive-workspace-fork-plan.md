# Cognitive Workspace Fork Plan

**Date:** 2026-05-16
**Status:** Proposed
**Audience:** Lattice maintainers considering a fork or major successor architecture
**Source Context:** Lattice current code graph, workflow tools, structured memory, A-Mem, AgeMem, ViLoMem, and SRMA-style memory/reflection concepts

## Goal

Build a mature assistant-facing workspace cognition system from Lattice's strongest primitives, without constraining the design to an incremental Lattice feature extension.

The target system should let coding agents understand a codebase, act on it, remember what happened, consolidate useful experience, verify whether memories still hold, and retrieve the right context with inspectable reasons.

The core product should be a local MCP server and daemon that exposes a unified engineering intelligence layer:

- code graph
- document graph
- event log
- typed memory graph
- workflow engine
- consolidation engine
- verification engine
- explainable retrieval and ranking
- assistant usefulness metrics

The bar is not "RAG over a repo." The bar is a trustworthy, auditable, long-running engineering memory system for agents.

## Design Thesis

The correct abstraction is not memory attached to a code graph. The correct abstraction is a workspace cognition substrate with three first-class sources of truth:

1. **Workspace graph:** files, symbols, docs, tests, dependencies, routes, schemas, build targets, runtime surfaces, and ownership boundaries.
2. **Event log:** every assistant action, tool call, context bundle, edit, diagnostic, test result, user correction, deployment step, and outcome.
3. **Memory graph:** durable typed claims over the workspace and event history, with evidence, freshness, contradiction, supersession, use history, and retrieval explanations.

These substrates should share identity, storage, and ranking primitives. A memory is not just text. It is a claim with evidence and validity conditions.

## Research Concepts To Carry Forward

### A-Mem

A-Mem contributes the most directly useful memory model:

- atomic memory notes
- generated context, tags, and keywords
- embeddings for semantic access
- explicit memory-to-memory links
- memory evolution as new notes arrive

Adopt the structure and link evolution. Do not adopt silent uncontrolled mutation. In this system, memory evolution should produce auditable proposals or transactional updates with provenance.

### AgeMem

AgeMem is most useful as an operation and evaluation model:

- separate short-term and long-term memory actions
- `ADD`, `UPDATE`, and `DELETE` for durable memory
- `RETRIEVE`, `SUMMARY`, and `FILTER` for working memory
- delayed usefulness signals for memory policy quality

Do not start with reinforcement learning. First build deterministic event capture, retrieval telemetry, and offline usefulness scoring. Policy learning can only be credible after the substrate records enough real outcomes.

### ViLoMem

ViLoMem's transferable idea is dual-stream memory and retrieval. For code agents, generalize that to multiple typed memory streams:

- code topology
- workflow episodes
- failure patterns
- semantic repo claims
- architecture decisions
- user/team preferences
- docs and contract state

Each stream should have its own ranking rules, freshness semantics, and consolidation policy.

### SRMA-Style Reflection

Use SRMA-like concepts as quality metrics:

- reflection quality
- consolidation quality
- retention quality
- drift detection
- retrieval alignment

Do not build opaque autonomous self-rewriting memory. Reflection should be inspectable, testable, and reversible.

## Non-Negotiable Product Properties

- Local-first by default.
- Deterministic identities for every file, symbol, doc section, event, and memory.
- Every durable memory has evidence.
- Every retrieved memory has an inclusion reason.
- Every stale or contradicted memory is surfaced as such, not hidden behind recency.
- Every workflow bundle is compact by default with deliberate expansion handles.
- Every background consolidation pass is recoverable, replayable, and observable.
- Every public MCP contract is documented and regression-tested.
- No silent broad workspace reads that bypass ignore rules or workspace boundaries.
- No unbounded graph traversal, payload growth, or event-log scans on hot paths.

## System Architecture

### 1. Ingestion Layer

Responsibilities:

- watch workspace changes
- parse code
- parse docs
- parse config and build files
- ingest test results and diagnostics
- capture assistant event traces
- normalize all inputs into stable records

Required inputs:

- source files
- Markdown docs
- repo instruction files
- test files and test manifests
- package/build metadata
- compiler diagnostics
- test runner output
- MCP tool calls and responses
- assistant edits and generated diffs
- user corrections and accepted/rejected outcomes

The ingestion layer should never directly decide what context is important. It produces normalized facts and events.

### 2. Workspace Graph

The workspace graph stores code, docs, tests, and operational surfaces.

Primary node families:

- `File`
- `Directory`
- `Symbol`
- `Type`
- `Module`
- `Test`
- `Document`
- `Section`
- `ConfigKey`
- `Command`
- `Route`
- `Schema`
- `Package`
- `BuildTarget`
- `RuntimeSurface`

Primary edge families:

- `contains`
- `imports`
- `calls`
- `implements`
- `extends`
- `type_ref`
- `tested_by`
- `documents`
- `mentions`
- `depends_on`
- `configured_by`
- `generated_by`
- `co_changed_with`
- `stale_against`

The graph must support exact identity lookup, bounded neighborhood traversal, impact queries, doc/code backlinks, and freshness queries.

### 3. Event Log

The event log is append-only and replayable. It is the substrate that makes memory consolidation and usefulness measurement possible.

Required event types:

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

Events should carry:

- workspace id
- branch
- session id
- task id
- actor
- timestamp
- stable references to files, symbols, docs, events, and memories
- payload hash
- compact payload summary
- optional full payload location

The full event stream should not be injected into prompts. It exists for retrieval, audit, replay, metrics, and consolidation.

### 4. Memory Graph

Memory should be stored as typed claims and episodes, not loose notes.

Primary memory classes:

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
- `CounterMemory` — a memory record that explicitly asserts a prior memory is wrong or no longer applicable. Distinct from a `contradicts` link: a link connects two existing memories; a CounterMemory is a first-class record authored by an agent or user that carries its own evidence, scope, and verification status and can itself be superseded or invalidated. Use CounterMemory when the counter-claim needs its own provenance and lifecycle, not just an edge annotation.

Each memory record requires:

- stable id
- content
- memory class
- assertion type
- scope: session, branch, repo, user, organization
- verification status
- confidence and confidence reason
- freshness policy
- validity conditions
- invalidation triggers
- provenance events
- evidence references
- linked files
- linked symbols
- linked docs
- linked tests
- linked memories
- contradiction links
- supersession links
- access history
- usefulness scores
- last verified state

Memory links should be first-class records:

- source memory
- target memory or graph node
- link type
- strength
- reason
- evidence event
- created by
- created at
- verification status

Required link types:

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

### 5. Working Memory

Working memory is per-task and short-lived. It should be explicit rather than implicit prompt accumulation.

Working memory contains:

- task statement
- interpreted intent
- active files and symbols
- active hypotheses
- active failures
- current plan
- selected memories
- excluded memories and reasons
- budget decisions
- unresolved questions
- verification status

Required operations:

- `retrieve`
- `summarize`
- `filter`
- `pin`
- `evict`
- `expand`
- `compress`
- `checkpoint`

Working memory is not durable by itself. Durable value is extracted through event-backed consolidation.

### 6. Consolidation Engine

The consolidation engine turns event traces into better memory.

Consolidation jobs:

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

Consolidation modes:

- synchronous post-task consolidation for small task traces
- background scheduled consolidation for larger histories
- manual review mode for high-impact repo or organization memories
- replay mode for rebuilding memory state from the event log

No consolidation job may silently rewrite high-scope memory without preserving provenance and prior state.

LLM-driven consolidation:

Several consolidation jobs (episode summary generation, procedure extraction, contradiction detection, failure-pattern extraction) cannot be implemented deterministically and require LLM inference. These jobs must follow additional constraints:

- each LLM-driven job produces a proposal record, not a direct write; proposals require explicit apply or reject before altering durable memory
- failed or malformed LLM responses must leave the prior memory state unchanged and emit a consolidation failure event
- LLM-driven jobs run only in background or manual review modes, never on the synchronous post-task hot path
- each job records its model, prompt hash, and response hash as part of the proposal provenance so outputs are auditable and reproducible
- consolidation queue depth must be bounded; when the queue is full, new jobs are dropped with a log warning rather than stalling the daemon
- cost and latency budgets for LLM consolidation should be documented per job type before Phase 6 begins; jobs exceeding budget must fall back to deterministic approximations or skip with a stale flag

### 7. Retrieval Engine

Retrieval must be task-aware and explainable.

Pipeline:

1. Parse user task and classify intent.
2. Extract literal anchors: paths, symbols, errors, commands, APIs, config keys.
3. Resolve anchors into graph identities.
4. Retrieve graph candidates.
5. Retrieve memory candidates from typed streams.
6. Retrieve relevant event episodes.
7. Expand through bounded graph and memory neighborhoods.
8. Score candidates.
9. Deduplicate and compress.
10. Return a compact bundle with inclusion reasons and expansion handles.

Candidate sources:

- exact path/symbol lookup
- code graph traversal
- doc backlinks/outgoing links
- FTS
- embeddings
- event similarity
- memory links
- workflow similarity
- recent active working memory

Ranking signals:

- task-type compatibility
- graph proximity to anchors
- exact identifier match
- semantic similarity
- verification status
- freshness
- scope
- evidence strength
- contradiction/supersession state
- past usefulness
- recent successful reuse
- user preference compatibility
- token cost

The ranker should expose scores and reasons in diagnostic mode. Compact mode should include only enough explanation for an assistant to trust the result.

### 8. Verification Engine

The verification engine keeps memory honest.

Verification checks:

- linked files still exist
- linked symbols still exist
- cited docs still exist
- linked tests still exist
- evidence text still matches when exact spans were captured
- implementation still matches memory claim where deterministic checks are possible
- contradicted/superseded states remain coherent
- branch-scoped memory is not leaking into unrelated branches
- time-bound memory has expired

Verification outputs:

- `verified`
- `unverified`
- `in_review`
- `stale`
- `contradicted`
- `superseded`
- `expired`
- `invalidated`

Verification must be incremental and tied to workspace changes. Large repos cannot tolerate full rescans for every memory.

### 9. MCP Surface

The MCP contract should expose high-level assistant workflows rather than forcing clients to compose low-level graph calls.

Required workflow tools:

- `get_context_capsule`
- `prepare_change`
- `plan_edit`
- `trace_scenario`
- `diagnose_failure`
- `find_relevant_tests`
- `impact_from_diff`
- `get_docs_capsule`
- `get_backlinks`
- `expand_context`
- `get_working_set_context`
- `get_repo_playbook`
- `search_symbols`
- `search_memory`
- `record_workflow_outcome`

New or redesigned memory tools:

- `get_task_memory`
- `save_memory`
- `propose_memory_evolution`
- `apply_memory_evolution`
- `verify_memory`
- `explain_memory`
- `list_memory_conflicts`
- `consolidate_session`
- `get_memory_metrics`
- `get_event_trace`

Tool surface discipline: ten new memory tools is a meaningful cognitive load for clients. Before shipping Phase 8, audit whether `propose_memory_evolution` + `apply_memory_evolution` can collapse into a single tool with an `action` parameter, and whether `verify_memory` + `explain_memory` can be unified. The goal is the smallest surface that covers all assistant workflows. Consolidate before stabilizing the MCP contract.

Every tool response should support:

- compact rendering
- full structured JSON
- context handles
- stable expansion targets
- budget controls
- diagnostic explanations where useful

### 10. Human Review Surface

Agents are the primary client, but durable memory needs human inspectability.

Required operator views:

- memory inbox
- proposed promotions
- proposed contradictions
- stale memory list
- memory evidence view
- event trace view
- retrieval explanation view
- usefulness metrics
- workspace graph health
- indexing health
- consolidation queue

This can be a VS Code extension panel or local web UI. It should be a review and trust surface, not the center of the product.

## Storage Design

Use SQLite as the durable local store unless proven insufficient. Split logical tables cleanly.

Core tables:

- `workspaces`
- `files`
- `symbols`
- `documents`
- `sections`
- `tests`
- `graph_edges`
- `events`
- `event_payloads`
- `memories`
- `memory_links`
- `memory_evidence`
- `memory_accesses`
- `memory_scores`
- `working_memory_checkpoints`
- `context_handles`
- `consolidation_jobs`
- `verification_jobs`

Indexes:

- graph identity indexes
- file path indexes
- symbol name/path indexes
- doc section indexes
- event task/session indexes
- memory scope/type/status indexes
- memory link source/target indexes
- freshness and verification indexes
- FTS over code summary, docs, memory, and event summaries
- vector indexes over symbols, doc sections, memories, and event episodes

The event log should be append-only. Derived graph and memory state can be rebuilt from source files and events.

Event log compaction: append-only storage will grow unboundedly for long-running repos. The storage design must include a compaction snapshot strategy: at configurable intervals, write a full-state snapshot of graph and memory to a separate file, then truncate the event log to events after the snapshot timestamp. The snapshot format must be versioned and independently readable so the daemon can bootstrap from a snapshot without replaying the full history. Compaction must be a daemon-managed background operation, not a user-initiated manual step.

## Fork Strategy

This should be a fork only if the implementation needs to break major internal assumptions. Preserve useful Lattice assets.

Carry forward:

- Rust daemon structure
- MCP handling experience
- workspace boundary protections
- existing workflow tool vocabulary
- graph parsing and indexing foundation
- context handles
- compact/full render modes
- structured memory groundwork
- test discipline around daemon behavior
- VS Code extension integration pattern

Replace or redesign:

- memory retrieval as keyword search
- memory as mostly isolated rows
- workflow metrics as secondary side data
- event capture as implicit logs instead of a first-class substrate
- ranking paths that cannot explain why context was included
- any storage path that cannot be replayed or verified

## Implementation Plan

### Phase 0: Fork Foundation

Deliverables:

- new repo or long-lived branch with clear successor name
- architecture docs copied and revised
- crate/module boundary plan
- storage migration policy
- compatibility policy for existing MCP tools
- baseline benchmark suite from current Lattice workflows

Fork decision gate:

Phase 0 ends with an explicit fork/extend decision before any structural code changes begin. Fork if any of the following are true:

- the memory graph schema changes require breaking the existing `memories` table in a way that cannot be migrated without a full rebuild
- the identity model requires changing the primary key or graph edge representation in a way that invalidates existing indexed data
- the event log requires a new storage file or WAL mode incompatible with the current daemon's SQLite usage
- two or more of the above apply and sharing a codebase would require sustained compatibility shims across all phases

If none of those conditions hold, extend in place on a long-lived branch and skip the repo split. Document the decision and the evidence for it before Phase 1 begins.

Verification:

- current Lattice daemon tests pass in the fork before major changes
- current MCP workflow fixtures run unchanged or with documented compatibility shims

### Phase 1: Unified Identity Model

Identity must precede the event log. An event that references a file, symbol, or doc by unstable name cannot be reliably replayed or used for consolidation. This phase establishes the identity substrate that all subsequent phases depend on.

Deliverables:

- stable ids for files, symbols, docs, sections, events, memories, and context handles
- identity resolver for paths, symbols, headings, tests, and event refs
- explicit identity serialization in MCP payloads
- compatibility support for legacy name-based references
- tests for duplicate symbol names, renamed files, moved sections, and branch changes

Definition of done:

- every workflow output can be expanded through a stable identity
- ambiguous names return ambiguity diagnostics instead of arbitrary matches
- identity resolution adds no more than 2ms P99 to hot-path tool calls

### Phase 2: Event Log Substrate

Deliverables:

- event model
- append-only SQLite event tables
- event writer API
- event reader/query API
- event payload hashing and optional payload spillover
- MCP/tool-call event capture using stable identities from Phase 1
- workflow task/session correlation
- tests for ordering, replay, corruption handling, and workspace scoping

Definition of done:

- every high-level MCP workflow records task, retrieval, response, and outcome events
- event log can replay enough data to reconstruct workflow history
- failure paths emit useful events without hiding errors
- event write adds no more than 5ms P99 to hot-path tool calls (`prepare_change`, `get_context_capsule`); large payloads spill to a side table rather than blocking the writer

### Phase 3: Memory Graph Storage

Deliverables:

- redesigned memory schema
- first-class memory links
- first-class memory evidence
- memory access/use history
- contradiction and supersession as graph edges
- memory stream/type taxonomy
- migration/import path from existing Lattice memory rows
- tests for memory creation, linking, contradiction, supersession, stale state, and replay

Definition of done:

- memory can represent atomic notes, claims, workflow outcomes, procedures, preferences, and counter-memory
- every memory can explain its evidence and relationship state

### Phase 4: Retrieval V1

Deliverables:

- task intent classifier
- anchor extractor and resolver
- hybrid candidate retrieval from graph, docs, memories, and events
- scoring model with graph proximity, semantic similarity, freshness, scope, verification, and usefulness
- compact response shaper
- diagnostic response mode with ranking reasons
- benchmark suite for realistic assistant tasks

Scope constraint: Phase 4 retrieval must not depend on the formal working memory state model defined in Phase 5. Retrieval returns bundles into the calling context; working memory formalization is layered on top in Phase 5. Design the retrieval output schema to be forward-compatible with working memory checkpoints without requiring a Phase 4 rewrite.

Definition of done:

- memory retrieval no longer depends on keyword search as the primary path
- workflow tools return memory with inclusion reasons and stable expansion handles
- irrelevant memory rate is measured and regressed

### Phase 5: Working Memory

Deliverables:

- explicit per-task working memory state
- operations for retrieve, summarize, filter, pin, evict, expand, compress, and checkpoint
- MCP surface for inspecting current working memory
- automatic event capture for included and excluded context
- tests for token budgets, pinned context, eviction, and checkpoint restore

Definition of done:

- workflow tools can show what context is active, why it is active, and what was intentionally excluded
- context management is no longer just prompt accumulation

### Phase 6: Consolidation Engine

Deliverables:

- session consolidation jobs
- episode summary generation
- memory promotion rules
- duplicate detection
- contradiction detection
- supersession proposal
- procedure extraction from repeated successful workflows
- failure-pattern extraction from repeated diagnostics
- manual review queue for high-scope memory changes
- replay-safe job execution

Definition of done:

- completed tasks produce useful episode memories
- repeated successful traces produce procedural memories
- consolidation is auditable, reversible, and test-covered

### Phase 7: Verification And Freshness

Deliverables:

- incremental memory verification against graph changes
- file/symbol/doc/test existence checks
- exact-span evidence validation where available
- branch and workspace scope enforcement
- time-bound expiry
- stale memory surfacing in workflow bundles
- verification jobs and operator diagnostics

Definition of done:

- stale or contradicted memory cannot appear as normal trusted guidance
- verification results are explainable and queryable

### Phase 8: Workflow Engine V2

Deliverables:

- redesigned versions of context, change planning, scenario tracing, failure diagnosis, docs retrieval, test selection, impact analysis, and playbook generation
- workflow composition over graph, event, memory, and working-memory substrates
- one-call edit-planning bundles with code, docs, tests, memory, risks, and verification commands
- workflow outcome recording integrated by default

Definition of done:

- common coding tasks require fewer manual discovery calls
- workflows produce enough context to implement and verify changes without broad file dumping

### Phase 9: Metrics And Evaluation

Deliverables:

- retrieval relevance metrics
- memory usefulness metrics
- context token efficiency metrics
- stale/contradicted memory surfacing metrics
- test-pass-after-plan metrics
- repeated-failure reduction metrics
- benchmark tasks with golden expected anchors
- regression dashboard or CLI report

Required metrics:

- tool calls per successful task
- irrelevant files opened per task
- relevant anchor recall
- memory inclusion precision
- memory later-used rate
- stale memory surfaced rate
- contradiction missed rate
- tests recommended versus tests needed
- workflow success after first plan

Definition of done:

- changes to retrieval, memory, and workflow behavior can be evaluated without relying on anecdote

### Phase 10: Human Review And Extension UX

Deliverables:

- VS Code or local web review UI
- memory inbox
- promotion queue
- contradiction queue
- stale memory view
- event trace view
- retrieval explanation view
- consolidation job health
- indexing health
- one-click accept/reject for memory evolution proposals

Definition of done:

- operators can understand and correct durable memory without reading raw SQLite rows

### Phase 11: Hardening

Deliverables:

- large-repo performance tests
- concurrency tests
- recovery tests
- corrupted-event handling
- partial-index handling
- workspace-boundary tests
- migration tests
- MCP schema compatibility tests
- documentation for setup, operations, memory semantics, and workflow behavior

Definition of done:

- the system is production-grade for repeated agent use, not only successful on a toy repo

## MCP Tool Contract Principles

Every assistant-facing tool should return:

- `overview`
- ranked pivots
- relevant context
- memory highlights
- event episodes where relevant
- suggested next expansion
- stable handles
- risks or uncertainty
- compact/full render choice
- structured payload

Every memory-bearing response should identify:

- why the memory was included
- evidence strength
- verification status
- freshness status
- contradiction/supersession state
- scope
- expansion target

Every workflow tool should record:

- inputs
- resolved anchors
- selected candidates
- excluded high-scoring candidates where useful
- response summary
- downstream use events

## Documentation Requirements

Required docs:

- successor architecture overview
- MCP contract reference
- memory model reference
- event log design
- consolidation design
- retrieval/ranking design
- verification/freshness design
- operator guide
- migration guide from Lattice
- benchmark/evaluation guide
- extension review UI guide

Docs must be updated in the same change as public behavior changes.

## Testing Requirements

Required test families:

- storage migration tests
- event append/replay tests
- graph identity tests
- parser tests
- memory link tests
- contradiction/supersession tests
- stale verification tests
- retrieval ranking golden tests
- workflow golden tests
- MCP schema tests
- compact/full render tests
- concurrency tests
- large fixture performance tests
- extension compile/UI smoke tests

No phase is complete until its failure paths are tested.

## Measurable Success Criteria

The fork should beat current Lattice on:

- lower tool calls per coding task
- lower irrelevant context inclusion
- higher relevant file/symbol recall
- higher memory reuse precision
- lower stale memory surfacing
- better repeated-failure avoidance
- better test recommendation accuracy
- better user correction retention
- faster diagnosis from failures
- more explainable context selection

Initial targets:

- 30 percent fewer discovery tool calls on benchmark tasks
- 40 percent fewer irrelevant file reads
- 80 percent memory inclusion precision on curated memory benchmarks
- zero trusted display of known contradicted memory
- zero trusted display of known stale memory without stale label
- 90 percent correct relevant-test recommendation on curated tasks

## Risks

### Opaque Memory Mutation

Risk: automatic consolidation corrupts durable repo knowledge.

Control: event-backed proposals, reversible updates, provenance, review queue for high-scope changes, and replay.

### Ranking Complexity

Risk: ranking becomes hard to reason about.

Control: keep ranker feature-based and inspectable before any learned policy. Expose diagnostic scores.

### Event Log Growth

Risk: event storage becomes too large or slow.

Control: append compact summaries on hot paths, spill large payloads, index by task/session/workspace, and support compaction snapshots.

### Stale Memory Leakage

Risk: agents trust outdated memory.

Control: freshness indexes, graph-change-triggered verification, stale labels in all memory surfaces, and tests that stale memory cannot rank as trusted.

### Scope Leakage

Risk: user/org/branch memories leak into wrong workspaces.

Control: scope-aware queries, enforced filters in store APIs, and negative tests.

### Overfitting To Current Repo

Risk: benchmarks pass only for Lattice-like Rust/TypeScript repos.

Control: fixture repos across languages, repo sizes, and documentation styles.

## First Implementation Slice

The first real slice should be event log plus memory graph, not UI.

Implement in this order:

1. Event model and append-only store.
2. MCP workflow event capture.
3. Memory graph schema and migration/import from existing rows.
4. Memory access/use history.
5. Retrieval V1 with graph-aware memory ranking.
6. Compact memory inclusion reasons in existing workflow tools.
7. Session consolidation into episode memories.
8. Verification and stale surfacing tests.

This slice creates the substrate that all later intelligence depends on. Building UI or learned policy before this substrate would create debt instead of capability.

## Final Target

The final system should make a coding agent feel like it is operating in a workspace that remembers accurately:

- what the repo contains
- why the repo is shaped that way
- what was tried before
- what failed before
- what tests prove behavior
- what memories are trustworthy
- what memories are stale
- what context matters now
- what should be done next

That is the measurable leap over a code graph plus ad hoc memory store.
