# Fork Or Extend Decision

## Decision

Extend Lattice in place on a long-lived branch. The Phase 0 fork gate in `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` says to fork only when one of the named breaking conditions is true, and to extend in place when none hold (`docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:566`, `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:577`, `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:579`, `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:586`).

Current code evidence does not require a repo split. The memory graph work can preserve and migrate the existing `memories` rows, the identity model can be generalized without invalidating the current graph primary key or edge representation, and a new append-only event log can coexist with the daemon's current SQLite/WAL usage.

## Branch or repo name

Use branch `feat/cognitive-workspace`.

No new repository is required for Phase 0. If a later task proposes a contract-breaking storage or identity replacement, it must reopen this decision with fresh evidence before making structural changes.

## Evidence

### Phase 0 fork gate source

The binding source is `## Phase 0: Fork Foundation` in `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:566`. The fork gate lists three concrete fork conditions and one aggregate compatibility-shim condition at `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:579` through `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:586`.

### Existing memory table

`MemoryStore::initialize` creates the current `memories` table in `daemon/crates/lattice-core/src/memory/store.rs:47` through `daemon/crates/lattice-core/src/memory/store.rs:81`. The table columns are:

| Column | Current purpose | Phase 3 implication |
|---|---|---|
| `id` | Text primary key for the memory row. | Preserve. Memory identity can be wrapped in the unified identity layer without rewriting existing rows. |
| `session_id` | Session ownership. | Preserve. Event/session correlation can reference it directly or through new event tables. |
| `content` | Freeform claim text. | Preserve as the human-readable assertion body. |
| `memory_type` | Legacy memory type. | Preserve and map into the expanded class taxonomy. New class values are additive. |
| `scope` | Session, branch, or repo durability scope. | Preserve. Scope enforcement helpers can validate existing values. |
| `confidence` | Numeric confidence. | Preserve. New score history belongs in `memory_scores`. |
| `linked_symbols` | JSON list of symbol references. | Preserve as compatibility data; first-class links move to `memory_links` additively. |
| `linked_files` | JSON list of file references. | Preserve as compatibility data; first-class links move to `memory_links` additively. |
| `workspace_id` | Optional workspace scope. | Preserve and enforce more strictly. |
| `branch` | Optional branch scope. | Preserve and enforce more strictly. |
| `refresh_key` | Durable recall key. | Preserve. |
| `source_query` | Query or workflow source note. | Preserve as provenance input. |
| `assertion_type` | Structured assertion type. | Preserve and extend for Phase 3 classes such as `CounterMemory`. |
| `verification_status` | Trust lifecycle state. | Preserve. Phase 7 can add stronger verifier transitions without row rebuild. |
| `confidence_reason` | Explanation for confidence. | Preserve. |
| `supersedes_memory_id` | Forward supersession relation. | Preserve as compatibility; first-class relation edges can be mirrored in `memory_links`. |
| `superseded_by_memory_id` | Reverse supersession relation. | Preserve as compatibility; index already exists. |
| `contradicts_memory_ids` | JSON list of contradicted memories. | Preserve as compatibility; first-class contradiction edges can be mirrored in `memory_links`. |
| `contradicted_by_memory_ids` | JSON reverse contradiction list. | Preserve as compatibility; first-class contradiction edges can be mirrored in `memory_links`. |
| `freshness_policy` | Freshness lifecycle policy. | Preserve. |
| `freshness_policy_detail` | Optional freshness detail. | Preserve. |
| `provenance_json` | JSON provenance entries. | Preserve as compatibility; normalized evidence/provenance can be copied into `memory_evidence`. |
| `evidence_json` | JSON evidence entries. | Preserve as compatibility; normalized evidence can be copied into `memory_evidence`. |
| `created_at` | Creation timestamp. | Preserve. |
| `last_accessed` | Last recall/access timestamp. | Preserve; access history can be added in `memory_accesses`. |
| `access_count` | Aggregate recall count. | Preserve; detailed access records can be added in `memory_accesses`. |
| `is_stale` | Legacy stale flag. | Preserve as compatibility with `verification_status`. |
| `stale_reason` | Stale explanation. | Preserve. |
| `is_invalidated` | Soft invalidation flag. | Preserve. |

The store already applies additive migrations for older schemas with `ALTER TABLE ... ADD COLUMN` statements, including structured assertion, verification, contradiction, freshness, provenance, and evidence fields (`daemon/crates/lattice-core/src/memory/store.rs:92` through `daemon/crates/lattice-core/src/memory/store.rs:156`). It also creates memory indexes and FTS5 without destructive rebuild of the base table (`daemon/crates/lattice-core/src/memory/store.rs:158` through `daemon/crates/lattice-core/src/memory/store.rs:189`).

The structured-memory architecture note confirms the same design intent: `## Data Model` keeps existing core fields while adding trust metadata, and `## Persistence And Migration` states that structured memory is stored in the existing `memories` SQLite table with additive scalar and JSON columns (`docs/architecture/2026-04-11-structured-memory.md:26`, `docs/architecture/2026-04-11-structured-memory.md:28`, `docs/architecture/2026-04-11-structured-memory.md:57`, `docs/architecture/2026-04-11-structured-memory.md:59`, `docs/architecture/2026-04-11-structured-memory.md:68`). The same note already names verification, freshness, evidence, provenance, contradiction, and supersession as exposed trust signals (`docs/architecture/2026-04-11-structured-memory.md:99`, `docs/architecture/2026-04-11-structured-memory.md:113`, `docs/architecture/2026-04-11-structured-memory.md:117`, `docs/architecture/2026-04-11-structured-memory.md:210`).

Phase 3 fields are therefore additive:

| Phase 3 requirement | Additive or breaking | Evidence and implication |
|---|---|---|
| Redesigned memory schema | Additive | Keep `memories` as the compatibility table; add normalized tables and optional columns with defaults. Existing rows migrate forward. |
| First-class memory links | Additive | Add `memory_links`; backfill from `linked_symbols`, `linked_files`, `supersedes_memory_id`, `superseded_by_memory_id`, `contradicts_memory_ids`, and `contradicted_by_memory_ids`. |
| First-class memory evidence | Additive | Add `memory_evidence`; backfill from `evidence_json` and `provenance_json`. |
| Memory access/use history | Additive | Add `memory_accesses`; seed aggregate state from `last_accessed` and `access_count`. |
| Memory scores | Additive | Add `memory_scores`; leave `confidence` as compatibility scalar. |
| Memory stream/type taxonomy including `CounterMemory` | Additive | Extend `assertion_type` or add a class column with a default; preserve `memory_type` for legacy callers. |
| Contradiction and supersession as graph edges | Additive | Store canonical edges in `memory_links` while preserving existing relation columns until compatibility policy retires them. |

No Phase 3 requirement forces dropping, renaming, or changing the primary key of `memories`.

### Existing graph identity and edge representation

The graph identity basis is already explicit. `SymbolId` is `{ file, name, byte_offset }` in `daemon/crates/lattice-core/src/symbols.rs:6` through `daemon/crates/lattice-core/src/symbols.rs:15`, and stable handles serialize those fields in `daemon/crates/lattice-core/src/symbols.rs:17` through `daemon/crates/lattice-core/src/symbols.rs:32`. The stable-follow-up-handles note names `## Durable Identity Basis` and says the repository already has this identity basis (`docs/architecture/2026-04-11-stable-follow-up-handles.md:19`, `docs/architecture/2026-04-11-stable-follow-up-handles.md:21`, `docs/architecture/2026-04-11-stable-follow-up-handles.md:23`, `docs/architecture/2026-04-11-stable-follow-up-handles.md:26`).

The in-memory graph uses `petgraph::DiGraph<GraphNode, EdgeKind>` plus a `HashMap<SymbolId, NodeIndex>` index (`daemon/crates/lattice-core/src/graph/model.rs:64` through `daemon/crates/lattice-core/src/graph/model.rs:69`). Its edge family is an enum of relationship kinds, not a hard-coded storage key shape (`daemon/crates/lattice-core/src/graph/model.rs:8` through `daemon/crates/lattice-core/src/graph/model.rs:20`). SQLite stores graph nodes under primary key `(file, name, byte_offset)` and stores edges by from/to triples plus `kind` (`daemon/crates/lattice-core/src/storage/schema.rs:1` through `daemon/crates/lattice-core/src/storage/schema.rs:26`). Loading reconstructs `SymbolId` values for nodes and edges from those same persisted triples (`daemon/crates/lattice-core/src/storage/graph_store.rs:327` through `daemon/crates/lattice-core/src/storage/graph_store.rs:331`, `daemon/crates/lattice-core/src/storage/graph_store.rs:390` through `daemon/crates/lattice-core/src/storage/graph_store.rs:401`).

The Phase 1 identity model can add typed identities for files, docs, sections, events, memories, and handles around this existing symbol identity. That is an additive generalization, not a required replacement of the current node primary key or edge row representation.

### Existing SQLite and WAL behavior

Current graph storage opens a file-backed SQLite database and enables WAL mode (`daemon/crates/lattice-core/src/storage/graph_store.rs:29` through `daemon/crates/lattice-core/src/storage/graph_store.rs:40`). Current memory storage does the same through `configure_connection`, with a busy timeout, WAL mode, auto-checkpointing, journal size limit, and passive checkpoint (`daemon/crates/lattice-core/src/memory/store.rs:21` through `daemon/crates/lattice-core/src/memory/store.rs:31`, `daemon/crates/lattice-core/src/memory/store.rs:1678` through `daemon/crates/lattice-core/src/memory/store.rs:1696`).

The storage/search backend note keeps SQLite as durable source of truth for graph and memories, keeps `memories.db` as the source of truth for rows and metadata, and documents runtime storage expansion alongside existing files (`docs/architecture/2026-04-11-storage-and-search-backends.md:3`, `docs/architecture/2026-04-11-storage-and-search-backends.md:5`, `docs/architecture/2026-04-11-storage-and-search-backends.md:43`, `docs/architecture/2026-04-11-storage-and-search-backends.md:45`, `docs/architecture/2026-04-11-storage-and-search-backends.md:53`, `docs/architecture/2026-04-11-storage-and-search-backends.md:56`).

An append-only `.lattice/events.db` can use the same file-backed SQLite pattern and WAL pragmas as `graph.db` and `memories.db`. A separate events database does not force incompatible WAL mode because SQLite WAL is per database file, and the existing daemon already tolerates multiple SQLite-backed stores.

## Gate conditions

| Gate condition | Verdict | Evidence |
|---|---|---|
| Memory graph schema changes require breaking the existing `memories` table in a way that cannot be migrated without a full rebuild. | Unmet | The current table already contains structured memory, verification, freshness, evidence, provenance, contradiction, and supersession fields. Existing migrations are additive, and Phase 3 normalized tables can be added alongside current rows. |
| Identity model requires changing the primary key or graph edge representation in a way that invalidates existing indexed data. | Unmet | Current node identity is already the stable `(file, name, byte_offset)` triple in code and SQLite. Phase 1 can wrap/generalize that identity while preserving current graph indexes and edge rows. |
| Event log requires a new storage file or WAL mode incompatible with the current daemon's SQLite usage. | Unmet | The daemon already uses separate SQLite stores and WAL mode for graph and memory databases. A new `.lattice/events.db` can follow the same connection policy. |
| Two or more of the above apply and sharing a codebase would require sustained compatibility shims across all phases. | Unmet | None of the three concrete fork gates is met. Compatibility work is normal migration and MCP surface discipline, not sustained cross-repo shim maintenance. |

## Migration implication

Existing graph, memory, vector, and handle data must be preserved. The extension path must not require deleting `.lattice/graph.db`, `.lattice/memories.db`, vector index files, or cached context handles as a condition of upgrade.

Memory migration strategy:

- Preserve every existing `memories` row and its `id`.
- Add normalized Phase 3 tables beside `memories`.
- Backfill `memory_links`, `memory_evidence`, `memory_accesses`, and `memory_scores` from existing compatibility fields.
- Keep compatibility reads from legacy columns until the MCP compatibility policy defines a retirement path.

Graph and identity migration strategy:

- Preserve `nodes` primary key `(file, name, byte_offset)` and `edges` from/to triple representation.
- Add typed identity wrappers and resolver tables/indexes around existing identities.
- Rebuild derived indexes only when parser or schema versions require it; do not force a full workspace graph rebuild merely because identity wrappers exist.

Event migration strategy:

- Create `.lattice/events.db` as a new append-only SQLite database.
- Use the same WAL connection discipline as graph and memory stores unless Phase 2 proves a different setting is required and documents that as a contract change.
- Treat event state as additive; old workspaces start with an empty event log and retain existing graph/memory data.

Rollback path:

- If Phase 1 through Phase 3 changes must be rolled back, stop the daemon, restore the previous binary, and leave existing `graph.db` and `memories.db` in place.
- Ignore or archive additive files and tables introduced by the cognitive workspace branch, especially `.lattice/events.db` and normalized memory side tables.
- Because this decision forbids destructive replacement of current primary keys and memory rows, rollback does not require reconstructing existing Lattice data from the new event log.

## Downstream binding implications

This decision binds the following direct Phase 0 and Phase 1 tasks:

- `T02` — Phase 0 successor architecture overview and compatibility policy must target an in-place branch, not a new repo.
- `T03` — Phase 0 crate/module boundary plan and storage migration policy must preserve current crate ownership and migration compatibility.
- `T04` — Phase 0 baseline benchmark capture must measure current Lattice workflows before branch changes.
- `R05` — Foundation review must evaluate the branch-based foundation, not a repo split.
- `T06` — Phase 1 identity type system must generalize current `SymbolId` identity without replacing it destructively.
- `T07` — Phase 1 identity resolver must keep backward-compatible resolution for existing graph and handle data.
- `T08` — Phase 1 identity serialization must include a legacy name compatibility shim.
- `T09` — Phase 1 identity tests must prove duplicate-name, rename, move, branch-change, and P99 behavior without invalidating existing indexes.
- `R10` — Phase 1 backend review must enforce the in-place compatibility contract.

It also binds all later tasks transitively because `tasks.json` chains Phase 2 through Phase 11 after the Phase 1 review. Later storage, retrieval, memory, verification, MCP, extension, metrics, hardening, and documentation tasks must treat this as an in-place successor architecture unless this document is explicitly superseded.
