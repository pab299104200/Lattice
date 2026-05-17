# Memory Graph Schema

## Classes

This document supersedes the model description in
[2026-04-11-structured-memory.md](2026-04-11-structured-memory.md) for new
cognitive-workspace memory records. The April design remains the legacy
single-table memory store until the T22 migration moves data into this schema.

The authoritative memory classes are:

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

`CounterMemory` is a first-class memory class. It is not just a
`contradicts` edge. Use it when the counter-claim needs its own content,
author, evidence, scope, verification status, and lifecycle. A
`CounterMemory` can itself be verified, invalidated, contradicted, or
superseded.

## Required Fields

Each memory record requires stable id, content, memory class, assertion type,
scope, verification status, confidence and confidence reason, freshness policy,
validity conditions, invalidation triggers, provenance events, evidence
references, linked files, linked symbols, linked docs, linked tests, linked
memories, contradiction links, supersession links, access history, usefulness
scores, and last verified state.

The Rust source of truth is
[classes.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/classes.rs).
The SQLite DDL source of truth is
[schema.sql](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/schema.sql).

## Scopes

`MemoryScope` has five values:

- `session`
- `branch`
- `repo`
- `user`
- `organization`

The `memories` table denormalizes the scope target for index-friendly lookup:
`scope_session_id`, `scope_branch`, `scope_workspace_id`, `scope_user_id`, and
`scope_org_id`. A row must set exactly the target columns required by its
`scope` value. For example, `branch` rows set `scope_branch` and
`scope_workspace_id`; `repo` rows set only `scope_workspace_id`.

Scope enforcement is deny-by-default in retrieval and belongs to T21. This
schema makes the scope target explicit so T21 can enforce it without parsing
freeform content.

## Streams

The memory graph is partitioned into seven typed streams:

- `code_topology`
- `workflow_episodes`
- `failure_patterns`
- `semantic_repo_claims`
- `architecture_decisions`
- `user_team_preferences`
- `docs_and_contract_state`

The Rust source of truth is
[streams.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/streams.rs).
`classify_stream(class, assertion_type)` is deterministic and uses the memory
class as the primary signal:

- `WorkflowOutcome`, `Procedure`, and `OpenQuestion` map to
  `workflow_episodes`.
- `FailurePattern` maps to `failure_patterns`.
- `Decision` and `ArchitectureInvariant` map to
  `architecture_decisions`.
- `Preference` maps to `user_team_preferences`.
- `DocsContract` maps to `docs_and_contract_state`.
- `CounterMemory` normally inherits the stream of the memory it counters. If
  the target stream is not available at classification time, the fallback is
  `semantic_repo_claims` and the runtime emits a warning.
- `Observation`, `Constraint`, `Pattern`, and `AntiPattern` are the ambiguous
  claim/topology classes. `observation` assertions map them to
  `code_topology`; `constraint`, `hypothesis`, `decision`, `counter`, and
  `preference` assertions map them to `semantic_repo_claims`; workflow-shaped
  assertions (`procedure`, `outcome`, `question`) map them to
  `workflow_episodes`.

Each stream has a default policy surface for retrieval:

| Stream | Default scope | Freshness default | Consolidation window | Ranking profile |
| --- | --- | --- | --- | --- |
| `code_topology` | `branch` | `branch_scoped`, TTL 6d, recheck 12h | 7d | `code_topology_v1` |
| `workflow_episodes` | `session` | `session_scoped`, TTL 1d, recheck 4h | 3d | `workflow_episodes_v1` |
| `failure_patterns` | `repo` | `event_triggered`, TTL 30d, recheck 7d | 14d | `failure_patterns_v1` |
| `semantic_repo_claims` | `repo` | `repo_scoped`, TTL 14d, recheck 7d | 21d | `semantic_repo_claims_v1` |
| `architecture_decisions` | `repo` | `repo_scoped`, TTL 90d, recheck 30d | 90d | `architecture_decisions_v1` |
| `user_team_preferences` | `user` | `time_bound`, TTL 30d, recheck 14d | 30d | `user_team_preferences_v1` |
| `docs_and_contract_state` | `repo` | `repo_scoped`, TTL 14d, recheck 7d | 14d | `docs_and_contract_state_v1` |

The ranking profile is a string identifier only. Retrieval logic resolves it in
Phase 4; stream metadata must not embed ranking algorithms directly.

## Scope Enforcement

The Rust source of truth is
[scope.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/scope.rs).

Scope queries are deny-by-default:

- A query without an explicit `ScopeFilter` is underspecified and must not
  return cross-scope data.
- `ScopeFilter::to_sql_predicate()` targets the denormalized scope columns
  (`scope_session_id`, `scope_branch`, `scope_workspace_id`, `scope_user_id`,
  `scope_org_id`) and binds every identifier as a parameter. Formatting
  user-supplied scope ids into SQL is forbidden.
- A `ScopeFilter` can include multiple explicit scopes, for example branch plus
  organization, but organization visibility is opt-in. A branch filter alone
  must not surface organization-scoped rows.
- `ScopeFilter::enforce_subset()` prevents widening. If the session is allowed
  to read only branch-scoped memories for one workspace/branch, adding repo,
  organization, another branch, or another user is a contract violation.
- Internal unscoped query paths must be named explicitly as admin or migration
  paths. Implicit unscoped reads are not allowed.

The legacy `memory::MemoryStore` remains in use while retrieval,
consolidation, and workflow code migrate onto the memory graph. Its enforced
boundary is
[scope_enforcement.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/verification/scope_enforcement.rs):
callers pass the canonical `ScopeFilter` with `workspace_id`, optional branch,
optional organization id, and optional session id. Branch memories require the
same workspace and branch, repo memories require the same workspace,
organization memories require an explicit matching organization id, and session
memories require an explicit matching session id. Store-boundary drops emit
`memory_scope_filtered` audit payloads and a `scope_leak_blocked` warning so
scope-leak attempts are visible during operation.

## Verification Statuses

`VerificationStatus` has eight values:

- `unverified`
- `in_review`
- `verified`
- `stale`
- `contradicted`
- `superseded`
- `expired`
- `invalidated`

The database constrains the column to these wire names. Retrieval and
verification code must treat these as lifecycle states, not display labels.

## Links

`memory_links` is the normalized edge table for contradiction, supersession,
 support, and related graph semantics. Each row stores:

- stable `link_id`
- `source_memory_id`
- `target_kind` plus `target_id`
- `link_type`
- bounded `strength` in `[0, 1]`
- human-readable `reason`
- optional `evidence_event_id`
- `created_by_kind` plus `created_by_detail`
- `created_at`
- `verification_status`

The Rust source of truth is
[links.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/links.rs).
Links replace the denormalized contradiction and supersession JSON blobs as the
authoritative query surface for graph traversal and explanation.

## Evidence

`memory_evidence` stores verification anchors as first-class rows instead of
embedding all provenance into one memory JSON column. Each row stores stable
`evidence_id`, owning `memory_id`, optional `event_id`, `anchor_kind`,
`anchor_json`, `captured_at`, and `captured_by_*`.

`EvidenceAnchor` is a typed enum with these variants:

- `FileSpan { file, byte_start, byte_end, sha256 }`
- `SymbolRef(symbol_id)`
- `DocSection { id, sha256 }`
- `TestResult { test, passed, run_event }`
- `EventReference(event_id)`

The span and doc-section hashes are required for exact re-verification in Phase
7. The Rust source of truth is
[evidence.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/evidence.rs).

## Accesses

`memory_accesses` records retrieval-time inclusion history. Each row stores a
stable `access_id`, owning `memory_id`, `accessed_at`, `accessed_in_event`,
`accessor`, `inclusion_reason`, nullable `was_used`, and nullable
`downstream_outcome_event`.

`was_used` remains nullable on first write because retrieval and downstream
outcome correlation are separate phases. The Rust source of truth is
[accesses.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/accesses.rs).

## Scores

`memory_scores` stores score history instead of a single current value. The
primary key is `(memory_id, score_kind, computed_at)`, which allows multiple
rows per memory and kind while preserving deterministic latest-value lookup.

`ScoreKind` currently includes:

- `usefulness_prior`
- `recent_usefulness`
- `retrieval_accuracy`
- `regression_risk`

Each row stores `value`, `computed_at`, `computed_from_window_secs`, and
`sample_size`. The Rust source of truth is
[scores.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/memory_graph/scores.rs).

## Freshness, Validity, Invalidation

Freshness policy is stored in `freshness_policy_json` and decoded into
`FreshnessPolicy` with `FreshnessKind` values `session_scoped`,
`branch_scoped`, `repo_scoped`, `time_bound`, and `event_triggered`.

Validity conditions are stored in `validity_conditions_json` and decoded into
machine-checkable predicates:

- file exists
- symbol exists
- doc heading exists
- test passes
- time before

Invalidation triggers are stored in `invalidation_triggers_json`. Trigger kinds
are file changed, symbol changed, doc section changed, test failed, and time
expired. Each trigger carries a `StableRef` target so Phase 7 verification can
subscribe deterministically.

All `*_json` columns are untrusted storage. Callers must decode them through
typed Rust structs and return typed errors on decode failure.

## CounterMemory Semantics

`CounterMemory` records are authored claims that a previous memory is wrong or
no longer applicable. They carry their own evidence and validity conditions.

A contradiction link connects two memories. A `CounterMemory` is a record. Both
can exist at the same time: the record explains the counter-claim, while the
link table added by T20 can connect the counter-claim to the memory it
contradicts.

Retrieval must not infer that every `CounterMemory` is verified. It starts with
the same verification lifecycle as any other class and only outranks the prior
claim when verification, scope, confidence, freshness, and link semantics
support that result.

## Lifecycle and transitions

`VerificationStatus` is a state machine, not a freeform tag. The allowed
transitions are:

| From | To |
| --- | --- |
| `unverified` | `in_review`, `verified` |
| `in_review` | `verified`, `stale`, `contradicted`, `expired`, `invalidated` |
| `verified` | `stale`, `contradicted`, `superseded`, `expired`, `invalidated` |
| `stale` | `verified`, `invalidated` |
| `contradicted` | none |
| `superseded` | none |
| `expired` | none |
| `invalidated` | none |

Additional lifecycle rules:

- `superseded` requires `superseded_by` to identify the newer memory.
- Every lifecycle change appends provenance through a lifecycle event in the
  event log.
- Every lifecycle change records evidence that points at the triggering event.
- `MemoryConsolidated` is reserved for Phase 6 consolidation workflows and is
  not used for normal verification-status transitions.

## Replay semantics

The event log is the replay source of truth for memory-graph lifecycle state.
`MemoryCreated`, `MemoryUpdated`, and `MemoryInvalidated` events carry an exact
post-write replay snapshot of the affected memory row plus its normalized
evidence rows and outgoing links.

Replay applies one event at a time and each event snapshot is transactional:

- upsert the `memories` row from the replay snapshot
- replace all `memory_evidence` rows for that memory with the snapshotted set
- replace all outgoing `memory_links` rows for that source memory with the
  snapshotted set

This design makes replay idempotent and bounded. A corrupted replay snapshot
fails that event without leaving a half-applied row/evidence/link state behind.

Snapshot bootstrap must be equivalent to full replay. A midpoint memory snapshot
followed by replay of the tail events must produce the same `memories`,
`memory_links`, and `memory_evidence` contents as replaying the full event log
from an empty database.

## Schema Parity Table

| SQLite column | Rust field |
| --- | --- |
| `memory_id` | `memory_id` |
| `content` | `content` |
| `class` | `class` |
| `assertion_type` | `assertion_type` |
| `scope` | `scope` |
| `scope_session_id` | `scope_session_id` |
| `scope_branch` | `scope_branch` |
| `scope_workspace_id` | `scope_workspace_id` |
| `scope_user_id` | `scope_user_id` |
| `scope_org_id` | `scope_org_id` |
| `verification_status` | `verification_status` |
| `confidence` | `confidence` |
| `confidence_reason` | `confidence_reason` |
| `freshness_policy_json` | `freshness_policy` |
| `validity_conditions_json` | `validity_conditions` |
| `invalidation_triggers_json` | `invalidation_triggers` |
| `provenance_event_ids_json` | `provenance_event_ids` |
| `evidence_references_json` | `evidence_references` |
| `linked_files_json` | `linked_files` |
| `linked_symbols_json` | `linked_symbols` |
| `linked_docs_json` | `linked_docs` |
| `linked_tests_json` | `linked_tests` |
| `linked_memories_json` | `linked_memories` |
| `contradiction_links_json` | `contradiction_links` |
| `supersession_links_json` | `supersession_links` |
| `access_history_json` | `access_history` |
| `last_verified_event_id` | `last_verified_event_id` |
| `last_verified_state` | `last_verified_state` |
| `usefulness_score` | `usefulness_score` |
| `usefulness_score_updated_at` | `usefulness_score_updated_at` |
| `created_at` | `created_at` |
| `created_by` | `created_by` |
| `updated_at` | `updated_at` |
| `updated_by` | `updated_by` |
| `superseded_by` | `superseded_by` |
| `schema_version` | `schema_version` |

Normalized companion tables added by T20:

| SQLite table | Rust type |
| --- | --- |
| `memory_links` | `MemoryLink` |
| `memory_evidence` | `MemoryEvidence` |
| `memory_accesses` | `MemoryAccess` |
| `memory_scores` | `MemoryScore` |

## Migration Plan Reference

T22 owns migration from the legacy `memory/store.rs` single-table shape into
this redesigned memory graph schema. The migration must preserve old memory
content, map legacy memory types into `MemoryClass` and `AssertionType`, seed
freshness and verification fields conservatively, and write provenance event
references from the Phase 2 event log where available.

## Index Inventory

T23 owns the authoritative index inventory for CRUD and transition paths. This
schema intentionally includes denormalized scope columns and scalar lifecycle
columns so T23 can add bounded indexes for scope filtering, verification
status, usefulness ordering, supersession lookup, and migration backfills
without changing the memory contract.
