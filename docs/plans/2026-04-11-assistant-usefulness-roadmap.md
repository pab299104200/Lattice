# Lattice Assistant Usefulness Roadmap

**Date:** 2026-04-11
**Status:** Executed
**Audience:** Lattice maintainers building for coding assistants

Executed across Phases 1-5. This roadmap now serves as the concise source of truth for delivered status, with detailed rationale and implementation notes living in the linked architecture/README updates. Verification was focused and phase-targeted rather than a single full-workspace run.

## Goal

Make Lattice more useful for a coding assistant by shifting it from strong retrieval toward a more deterministic end-to-end coding workflow engine.

Primary success metrics:

- Reduce ambiguous follow-up expansions
- Improve code-first retrieval for natural-language and identifier-heavy coding queries
- Reduce tool calls required to reach an actionable edit plan
- Improve scenario-level debugging support
- Make durable memory more trustworthy and explainable

## Baseline

Current strengths:

- `get_context_capsule`, `prepare_change`, and related workflow tools already compress discovery and planning well
- `expand_context` already supports handle-based follow-up expansion for cached workflow results
- semantic search now has persisted ANN support with SQLite fallback
- memory is persistent, refreshable, stale-aware, and searchable

Current gaps:

- follow-up expansion is still too name-oriented in several paths, which leaves ambiguity when symbol names collide
- semantic indexing still embeds a thin text view of nodes instead of richer coding-assistant-facing context
- workflow planning stops short of exact edit guidance
- behavior-level debugging still requires chaining multiple lower-level tools
- durable memory is still mostly freeform text instead of structured assertions with provenance and contradiction handling

## Working Principles

1. Favor deterministic identities over fuzzy string matching wherever the assistant needs to act on a result.
2. Prefer compact, structured workflow bundles over requiring repeated low-level lookups.
3. Keep retrieval code-first for implementation tasks and doc-first only when the query is explicitly asking for docs.
4. Preserve backward compatibility intentionally when changing tool payloads or follow-up contracts.
5. Measure assistant usefulness in reduced ambiguity, fewer round-trips, and safer code changes, not only in retrieval recall.

## Roadmap

### Phase 1: Stable Follow-Up Handles

Status: `[x]` completed

- `[x]` Add stable symbol and file handles derived from durable graph identity such as file plus byte offset, not only symbol names
- `[x]` Return those handles from `get_context_capsule`, workflow tools, and symbol-bearing helper tools
- `[x]` Update `suggested_expand` to emit stable handles instead of plain `symbol:<name>` targets
- `[x]` Teach `expand_context` to resolve exact handles first and keep fuzzy name matching only as a backward-compatible fallback
- `[x]` Add regression tests for duplicate symbol names across files and repos

Primary files:

- `daemon/crates/lattice-daemon/src/rpc/mcp.rs`
- `daemon/crates/lattice-core/src/intelligence/agent.rs`
- graph model and serialization paths

Expected outcome:

- Follow-up expansion becomes deterministic enough to drive assistant actions without symbol-name ambiguity

### Phase 2: Richer Semantic Indexing

Status: `[x]` completed

- `[x]` Expand embedded text beyond `name + signature` to include concise body summaries, comments, docstrings, error strings, config keys, route names, and other assistant-relevant anchors
- `[x]` Introduce multi-granularity embeddings for symbol-level, file-summary, and doc-section retrieval where it improves ranking
- `[x]` Re-rank semantic candidates with graph, identifier, and query-intent signals before final delivery
- `[x]` Add benchmark queries that reflect real assistant prompts, not only symbol lookup
- `[x]` Track storage growth, rebuild speed, and watcher-sync overhead as the richer index rolls out

Primary files:

- `daemon/crates/lattice-daemon/src/vector_sync.rs`
- `daemon/crates/lattice-core/src/storage/vector_index.rs`
- `daemon/crates/lattice-core/src/storage/usearch_index.rs`
- `daemon/crates/lattice-core/src/query/engine.rs`

Expected outcome:

- Natural-language coding queries map to better implementation anchors without regressing identifier precision

### Phase 3: Patch-Oriented Planning

Status: `[x]` completed

- `[x]` Add a new workflow tool such as `prepare_patch` or `plan_edit`
- `[x]` Return likely edit files, likely symbols, candidate edit spans, affected imports/interfaces/callers, relevant docs, and recommended tests
- `[x]` Integrate `find_relevant_tests`, `impact_from_diff`, stale-doc detection, and memory reuse into one assistant-facing edit bundle
- `[x]` Add compact rendering that is small enough to drive code edits directly
- `[x]` Add golden tests for bug-fix, refactor, and feature-add cases

Primary files:

- `daemon/crates/lattice-daemon/src/rpc/mcp.rs`
- `daemon/crates/lattice-core/src/intelligence/agent.rs`
- `daemon/crates/lattice-core/src/query/engine.rs`

Expected outcome:

- The assistant can move from task statement to edit plan in one call instead of chaining multiple discovery tools

### Phase 4: Scenario Tracing

Status: `[x]` completed

- `[x]` Add a scenario-focused tool such as `trace_scenario` or `explain_execution_path`
- `[x]` Accept behavior descriptions like “why does login fail after refresh” rather than requiring a known symbol
- `[x]` Return likely entrypoints, execution path segments, guards, side effects, failure branches, and relevant tests/docs
- `[x]` Distinguish high-confidence paths from plausible alternatives
- `[x]` Reuse existing call-graph, dependency, docs, and workflow ranking logic instead of creating a parallel retrieval stack

Primary files:

- `daemon/crates/lattice-core/src/intelligence/agent.rs`
- `daemon/crates/lattice-core/src/graph/`
- `daemon/crates/lattice-daemon/src/rpc/mcp.rs`

Expected outcome:

- Behavior-level debugging becomes a first-class assistant workflow instead of a manual composition of symbol and file tools

### Phase 5: Structured Memory

Status: `[x]` completed

- `[x]` Evolve memory rows from mostly freeform text into structured assertions with provenance and evidence
- `[x]` Add fields for assertion type, confidence reason, verification status, supersession, contradiction, and freshness policy
- `[x]` Surface when a recalled memory is contradicted by newer memory or current code state
- `[x]` Prefer verified repo- or branch-scoped workflow outcomes over weaker observations
- `[x]` Add migrations and tests for structured recall, contradiction handling, and refresh behavior

Primary files:

- `daemon/crates/lattice-core/src/memory/store.rs`
- `daemon/crates/lattice-core/src/memory/model.rs`
- workflow memory reuse paths in daemon and core intelligence flows

Expected outcome:

- Memory becomes more trustworthy for repeated coding sessions and less likely to feed stale or weak guidance into later workflows

## Cross-Cutting Work

- `[x]` Add metrics for expansion ambiguity, tool calls per task, semantic hit quality, edit-plan accuracy, and memory contradiction rate
- `[x]` Update MCP schemas, README guidance, and durable design docs when contracts change
- `[x]` Preserve legacy payloads intentionally where clients may already depend on current shapes
- `[x]` Add benchmark fixtures based on real assistant workflows and regressions from actual repo usage

## Suggested Delivery Order

1. Stable follow-up handles
2. Patch-oriented planning
3. Richer semantic indexing
4. Scenario tracing
5. Structured memory

This order maximizes immediate assistant reliability first, then improves workflow leverage, then broadens retrieval and long-session durability.

## Definition Of Done

Each phase is done only when:

- `[x]` core behavior is implemented
- `[x]` failure and fallback paths are covered
- `[x]` MCP/tool contracts are documented
- `[x]` regression tests exist for the new behavior
- `[x]` repo-supported verification has been run and recorded
