# Structured Memory

## Summary

Phase 5 introduces a structured assertion layer on top of the existing durable memory store. The goal is not to replace freeform memory content, but to persist enough verification, provenance, freshness, and relationship metadata that later assistant workflows can reuse memory with an explainable trust order.

The current implementation is split between:

- `daemon/crates/lattice-core/src/memory/model.rs` for the structured-memory types
- `daemon/crates/lattice-core/src/memory/store.rs` for SQLite persistence, additive migration, default derivation, and recall ordering
- `daemon/crates/lattice-core/src/intelligence/agent.rs` for assistant-facing ranking, highlight generation, and compact/full memory bundle shaping

This note describes only behavior verified in those areas. It does not define a new daemon or MCP contract.

## Why This Exists

Before Phase 5, memory rows were durable and searchable, but most trust judgment still had to be inferred from freeform text, confidence, and recency. That makes reuse fragile in longer coding sessions:

- a newer note can be weaker than an older verified one
- an observation can be stale after file or symbol changes
- an old memory can be contradicted or superseded without assistants seeing that clearly
- workflow outputs can reuse memory without telling the assistant why a memory should be trusted

The structured layer makes those states explicit so memory reuse can be both safer and easier to explain.

## Data Model

Each memory row still keeps the original core fields such as `content`, `memory_type`, `scope`, `confidence`, `linked_symbols`, `linked_files`, `workspace_id`, `branch`, `refresh_key`, and stale flags.

Phase 5 adds a parallel structured assertion model:

- `assertion_type`
- `verification_status`
- `confidence_reason`
- `supersedes_memory_id`
- `superseded_by_memory_id`
- `contradicts_memory_ids`
- `contradicted_by_memory_ids`
- `freshness_policy`
- `freshness_policy_detail`
- `provenance`
- `evidence`

The assertion vocabulary is wider than the legacy `memory_type`. In addition to observation, decision, exploration, pattern, and anti-pattern, the structured layer also supports `workflow_outcome` and `constraint`.

The verification lifecycle currently supports:

- `unverified`
- `in_review`
- `verified`
- `stale`
- `contradicted`
- `superseded`

Freshness is also modeled explicitly through `session_scoped`, `branch_scoped`, `repo_scoped`, `time_bound`, and `manual_review`.

## Persistence And Migration

Structured memory is stored in the existing `memories` SQLite table. The schema is extended additively with new scalar columns and JSON columns for contradiction edges, provenance, and evidence.

The migration approach is intentionally low-risk:

- `CREATE TABLE IF NOT EXISTS` defines the full current schema for fresh databases
- older databases are upgraded with additive `ALTER TABLE` statements for each structured column
- new indexes are added for `verification_status` and `superseded_by_memory_id`
- the FTS table is rebuilt after initialization so legacy rows remain searchable

This means old databases continue to open without a destructive rewrite. Legacy rows receive column defaults at the database level. Structured defaults are then derived on later store/update paths when Lattice rewrites a memory row.

## Default Structured Derivation

`MemoryStore::store` resolves structured metadata before persisting a row.

For a row that already has structured fields, the store preserves them and reconciles a few fields with current row state:

- extended `assertion_type` values such as `workflow_outcome` and `constraint` are preserved instead of being reset from the legacy `memory_type`
- legacy-compatible assertion types continue to follow `memory_type`
- scope-derived freshness policies are recomputed from the current scope
- `stale` is forced if `memory.is_stale` is true
- a previously stale row falls back to inferred verification when it is no longer stale
- default provenance and evidence are filled in if missing

For a row with no structured metadata yet, the store derives defaults from the existing row:

- `assertion_type` comes from `memory_type`
- `verification_status` is inferred from row state
- `freshness_policy` comes from scope
- `confidence_reason` is synthesized from a verification-like `source_query` or low confidence
- provenance is seeded from `source_query`, `refresh_key`, or an `assistant_observation` fallback
- evidence is seeded from linked symbols and linked files

The current verification inference is conservative:

- stale rows become `stale`
- rows whose `source_query` contains verification signals such as `verified`, `validated`, `from code`, `from tests`, or `code and tests` become `verified`
- otherwise rows at `confidence >= 0.95` become `in_review`
- everything else remains `unverified`

## Supersession, Contradiction, And Staleness Semantics

Phase 5 now distinguishes three different trust degradations:

### Staleness

Staleness is tied to current code drift, not logical disagreement. When the indexer sees relevant file or symbol changes, `mark_stale_by_file` and `mark_stale_by_symbol` set:

- `is_stale = true`
- `stale_reason`
- `verification_status = 'stale'`

Refreshing, promoting, or directly updating content clears the stale flags. If the previous verification state was only stale, the store drops it back to `unverified` unless stronger structured metadata is reapplied later.

### Supersession

Supersession means a row has been replaced by a newer or stronger memory. `mark_memory_superseded` records the replacing memory id in `superseded_by_memory_id` and sets `verification_status` to `superseded`.

### Contradiction

Contradiction means two memories disagree. `mark_memory_contradicted`:

- appends the newer contradicting id to the older row’s `contradicted_by_memory_ids`
- marks the older row as `contradicted`
- backfills the reverse edge by appending the older id to the newer row’s `contradicts_memory_ids`

These states are intentionally separate. A row can be stale because the code changed, superseded because a better replacement exists, or contradicted because another memory disagrees.

## Recall And Trust Ordering

There are two trust-ordering layers in the current implementation.

### Store-Level Recall

`find_by_refresh_key` is the store’s durable recall path for a scoped memory identity. It prefers memories in this order:

1. `verified`
2. `in_review`
3. `unverified`
4. other fallback states
5. `superseded`
6. `stale`
7. `contradicted`

It then breaks ties by preferring:

- rows that are not superseded
- rows that are not stale
- wider durable scope, with `repo` above `branch` above `session`
- stronger assertion type, with `workflow_outcome` above `constraint` above the legacy assertion kinds
- higher confidence
- newer creation time

This is the current protection against a newer but weaker row displacing an older verified one for the same `refresh_key`.

### Assistant-Facing Ordering

Assistant-facing bundles in `agent.rs` apply a richer ranking to memory values before surfacing them as highlights or compact memory objects.

The ranking score combines:

- effective verification status
- memory scope, with `repo` above `branch` above `session`
- assertion type, with `workflow_outcome` highest and `constraint` next
- counts of provenance and evidence entries
- presence of `confidence_reason`
- presence of supersession or contradiction links

The effective verification status intentionally overrides raw status when stronger trust signals are visible in the value:

- any `contradicted_by_memory_ids` makes the row effectively `contradicted`
- any `superseded_by_memory_id` makes it effectively `superseded`
- `is_stale = true` makes it effectively `stale`
- otherwise the explicit `verification_status` is used

This keeps downstream assistant ranking aligned with the relationship and freshness markers even if a caller passes partially normalized memory values.

## Assistant-Facing Bundle Use

The intelligence layer currently uses structured trust signals in two ways.

First, it emits `memory_highlights` that carry a compact explanation surface:

- truncated content
- legacy memory type
- scope
- stale flag
- assertion type
- effective verification status
- confidence reason
- freshness policy and detail

Those highlights are used directly in workflow overviews. Verified memories are phrased as reusable guidance such as `reuse verified repo workflow outcome`, while stale, superseded, and contradicted memories are phrased as notes instead of reusable advice.

Second, full-mode bundles keep a compact `memories` array that preserves the key structured fields needed for inspection:

- assertion and verification fields
- contradiction and supersession ids
- freshness policy fields
- truncated provenance
- truncated evidence

Compact mode intentionally omits the full `memories` payload and keeps only highlights. Full mode keeps up to two compact memory entries.

The currently verified assistant-facing consumers of this logic are:

- task bundles
- working-set context bundles
- subsystem summaries
- repo playbook summaries

## Explainability Boundary

The structured-memory design is meant to support explainable reuse, not opaque scoring. The current implementation keeps that boundary by exposing the trust factors that changed ranking:

- verification status
- scope
- assertion type
- confidence reason
- freshness policy
- provenance
- evidence
- contradiction and supersession links

That gives assistant workflows a basis for saying why a memory is being reused or why it should be treated only as a warning.

## Current Limits

A few Phase 5 boundaries are still visible in the landed code:

- the base `Memory` struct remains intentionally unstructured; structured fields are persisted and loaded through separate store paths
- legacy rows are migrated additively, but structured provenance and evidence are only fully derived when the row is stored or otherwise updated again
- this note does not freeze any daemon or MCP wire shape for structured memory fields; active daemon-facing payload details should be documented separately once that worker-side contract is settled

## Verification Status

Manual consistency pass only.

Verified against:

- `docs/plans/2026-04-11-assistant-usefulness-roadmap.md`
- `daemon/crates/lattice-core/src/memory/model.rs`
- `daemon/crates/lattice-core/src/memory/store.rs`
- `daemon/crates/lattice-core/src/memory/tests.rs`
- `daemon/crates/lattice-core/src/intelligence/agent.rs`
- `daemon/crates/lattice-core/src/intelligence/agent_tests.rs`

Specifically verified that:

- structured assertion enums and fields exist in the memory model
- SQLite initialization and additive migration create the structured columns and indexes
- default structured metadata is derived from existing row state during store/update paths
- refresh-key recall prefers stronger verified memories over newer weaker ones
- staleness, supersession, and contradiction are stored as distinct states
- assistant-facing memory ordering, highlights, and full-mode compact memory payloads use the structured trust signals described above

No automated tests were run for this docs-only task.
