# Storage Migration Policy

This policy is binding for Phase 1 through Phase 7 storage introductions on the cognitive workspace branch. It follows the in-place extension decision in [2026-05-16-fork-or-extend-decision.md](./2026-05-16-fork-or-extend-decision.md) and the substrate overview in [2026-05-16-cognitive-workspace-architecture.md](./2026-05-16-cognitive-workspace-architecture.md).

The policy cites the fork spec's [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design) and [## Fork Strategy](../plans/2026-05-16-cognitive-workspace-fork-plan.md#fork-strategy) headings directly. It also keeps the existing SQLite, FTS, vector, and compatibility-source-of-truth guidance in [2026-04-11-storage-and-search-backends.md#Contract-Notes](./2026-04-11-storage-and-search-backends.md#contract-notes) as the lower-level storage reference.

## Policy

SQLite remains the durable local store unless a later phase proves insufficiency with benchmark and recovery evidence. New schema is additive by default, workspace-scoped where relevant, and introduced in phase order. Existing `graph.db`, `memories.db`, vector indexes, context handles, and current MCP-compatible memory rows must remain readable after upgrade.

Every migration implementation must provide:

- a monotonic migration id with phase prefix, such as `p2_001_events`
- an idempotent forward operation
- a checked rollback operation or an explicit unsafe-rollback classification
- temp-database migration tests for fresh install, upgrade from existing data, and repeated open
- schema parity tests for public storage models where applicable
- logging that names the migration id, workspace, database file, outcome, and failure detail

The operator rollback command shape is reserved now so later phases do not invent incompatible controls:

```bash
lattice storage rollback --workspace <workspace-root> --migration <migration-id> --archive-newer
```

`--archive-newer` is required for rollback of additive phase tables. It copies rows or files introduced after the rollback target to `.lattice/rollback-archive/<timestamp>/` before destructive schema removal.

## Migration order

The order below is chronological and binding. A later phase may add indexes for its own tables in the same migration group, but it may not introduce an earlier phase's source-of-truth table out of order.

| Order | Phase | Migration id | Schema introduction | Database target | Owner module |
|---|---|---|---|---|---|
| 1 | Phase 1 | `p1_001_identity_workspaces` | `workspaces` | `.lattice/graph.db` or shared metadata database chosen by implementation | `lattice-core::identity` |
| 2 | Phase 1 | `p1_002_identity_files` | `files` identity table or compatibility view over current `file_index` | `.lattice/graph.db` | `lattice-core::identity` |
| 3 | Phase 1 | `p1_003_identity_symbols` | `symbols` identity table or compatibility view over current `nodes` | `.lattice/graph.db` | `lattice-core::identity` |
| 4 | Phase 1 | `p1_004_identity_documents` | `documents` | `.lattice/graph.db` | `lattice-core::identity` |
| 5 | Phase 1 | `p1_005_identity_sections` | `sections` | `.lattice/graph.db` | `lattice-core::identity` |
| 6 | Phase 1 | `p1_006_identity_tests` | `tests` | `.lattice/graph.db` | `lattice-core::identity` |
| 7 | Phase 1 | `p1_007_identity_context_handles` | stable handle identity metadata for existing context handles | `.lattice/graph.db` or `.lattice/handles.db` if handle persistence is separated | `lattice-core::identity` |
| 8 | Phase 2 | `p2_001_events` | `events`, append-only triggers, and `event_payloads` applied atomically as event schema version 1 | `.lattice/events.db` | `lattice-core::events` |
| 9 | Phase 2 | `p2_002_event_payloads` | Reserved logical component for payload spillover; implemented by `p2_001_events` because event rows foreign-key directly to spilled payload rows on first writer use | `.lattice/events.db` | `lattice-core::events` |
| 10 | Phase 3 | `p3_001_memory_links` | `memory_links` | `.lattice/memories.db` | `lattice-core::memory_graph` |
| 11 | Phase 3 | `p3_002_memory_evidence` | `memory_evidence` | `.lattice/memories.db` | `lattice-core::memory_graph` |
| 12 | Phase 3 | `p3_003_memory_accesses` | `memory_accesses` | `.lattice/memories.db` | `lattice-core::memory_graph` |
| 13 | Phase 3 | `p3_004_memory_scores` | `memory_scores` | `.lattice/memories.db` | `lattice-core::memory_graph` |
| 14 | Phase 5 | `p5_001_working_memory_checkpoints` | `working_memory_checkpoints` | `.lattice/working-memory.db` or shared workflow database chosen by implementation | `lattice-core::working_memory` |
| 15 | Phase 5 | `p5_002_context_handles` | `context_handles` durable checkpoint references | `.lattice/working-memory.db` or `.lattice/handles.db` | `lattice-core::working_memory` |
| 16 | Phase 6 | `p6_001_consolidation_jobs` | `consolidation_jobs` | `.lattice/consolidation.db` or shared workflow database chosen by implementation | `lattice-core::consolidation` |
| 17 | Phase 7 | `p7_001_verification_jobs` | `verification_jobs` | `.lattice/verification.db` or shared workflow database chosen by implementation | `lattice-core::verification` |

Phase 4 retrieval introduces no source-of-truth tables in this policy. It may add derived indexes, FTS tables, vector indexes, or ranking metadata only as rebuildable accelerators. Those accelerators must never become the only source of truth for graph, event, memory, or working-memory state.

### Memory store migration and FTS recovery

`memories.db` records its additive column upgrades in `memory_schema_migrations`. An upgrade records a migration only after its schema operation succeeds; a recorded migration whose expected column is absent is an explicit open failure, not a silently ignored error. The derived `memories_fts` index has a persisted dirty bit in `memory_fts_state`: normal writes update the affected row incrementally, while an interrupted write leaves the bit set and causes one source-of-truth rebuild on the next open. FTS matches are ordered by SQLite `bm25` relevance, with creation time used only as a deterministic tie-breaker.

## Rollback

Rollback classes:

- `none`: rollback removes only empty tables, views, indexes, or metadata and preserves all user/workflow data
- `archived`: rollback removes schema after copying new rows or files into the rollback archive
- `destructive`: rollback cannot preserve behavior or complete data fidelity; operator must restore a backup or keep the newer binary

| Migration id | Inverse operation | Data-loss class | Operator command |
|---|---|---|---|
| `p1_001_identity_workspaces` | Drop the identity-owned `workspaces` table or view after archiving rows if any exist. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p1_001_identity_workspaces --archive-newer` |
| `p1_002_identity_files` | Drop the `files` identity table or compatibility view; preserve current `file_index`. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p1_002_identity_files --archive-newer` |
| `p1_003_identity_symbols` | Drop the `symbols` identity table or compatibility view; preserve current `nodes` primary key data. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p1_003_identity_symbols --archive-newer` |
| `p1_004_identity_documents` | Drop `documents` after archiving rows; derived document indexes can be rebuilt. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p1_004_identity_documents --archive-newer` |
| `p1_005_identity_sections` | Drop `sections` after archiving rows; derived section indexes can be rebuilt. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p1_005_identity_sections --archive-newer` |
| `p1_006_identity_tests` | Drop `tests` after archiving rows; test identity can be regenerated from workspace indexing. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p1_006_identity_tests --archive-newer` |
| `p1_007_identity_context_handles` | Drop stable handle identity metadata after archiving rows; live in-memory handles expire normally. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p1_007_identity_context_handles --archive-newer` |
| `p2_001_events` | Archive `.lattice/events.db` event and payload rows, then drop the event tables or move the whole database to the rollback archive. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p2_001_events --archive-newer` |
| `p2_002_event_payloads` | Compatibility alias for the payload-spillover portion of `p2_001_events`; rollback targets the same archived `.lattice/events.db` payload rows. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p2_002_event_payloads --archive-newer` |
| `p3_001_memory_links` | Archive normalized links, drop `memory_links`, and keep legacy JSON relationship columns in `memories`. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p3_001_memory_links --archive-newer` |
| `p3_002_memory_evidence` | Archive normalized evidence, drop `memory_evidence`, and keep legacy `evidence_json` and `provenance_json`. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p3_002_memory_evidence --archive-newer` |
| `p3_003_memory_accesses` | Archive detailed access rows, drop `memory_accesses`, and keep aggregate `last_accessed` and `access_count`. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p3_003_memory_accesses --archive-newer` |
| `p3_004_memory_scores` | Archive score history, drop `memory_scores`, and keep compatibility `confidence` fields. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p3_004_memory_scores --archive-newer` |
| `p5_001_working_memory_checkpoints` | Archive checkpoint rows and drop `working_memory_checkpoints`; active tasks lose checkpoint restore. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p5_001_working_memory_checkpoints --archive-newer` |
| `p5_002_context_handles` | Archive durable handle rows and drop `context_handles`; legacy volatile handles continue to expire. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p5_002_context_handles --archive-newer` |
| `p6_001_consolidation_jobs` | Archive job rows and proposal payloads, then drop `consolidation_jobs`; queued consolidation work must be re-created under the newer binary if restored. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p6_001_consolidation_jobs --archive-newer` |
| `p7_001_verification_jobs` | Archive job rows and diagnostics, then drop `verification_jobs`; already-written memory verification status remains in memory records unless separately rolled back. | `archived` | `lattice storage rollback --workspace <workspace-root> --migration p7_001_verification_jobs --archive-newer` |

No listed migration is allowed to be `destructive` by design. If implementation discovers that a migration cannot be rolled back with archive preservation, the migration must be split or the phase review must mark it explicitly as `destructive` before it ships. Destructive rollback requires stopping the daemon, restoring the previous binary, and restoring a backup of the affected `.lattice/` database file.

## Compaction snapshot policy

Event log compaction is daemon-managed background work, not a user-invoked cleanup path. The daemon owns the schedule, performs compaction only at safe checkpoints, and records the compaction event in the event log.

Snapshot rules:

- Snapshots live outside the hot event tables, under a versioned path such as `.lattice/snapshots/<format-version>/<snapshot-id>/`.
- Each snapshot includes a manifest with format version, schema versions, workspace id, branch, highest included event id, graph checksum, memory checksum, creation time, daemon version, and payload hashes.
- The snapshot format is independently readable. Bootstrap can load the latest valid snapshot and replay only events after the snapshot's highest included event id.
- Snapshot validation happens before event truncation. Failed validation leaves the event log untouched and logs the failure with snapshot id and reason.
- Truncation removes only events included in a validated snapshot and only after the manifest and payload files have been flushed.
- Payload spillover files are reference-counted or manifest-owned so compaction cannot orphan payloads required by post-snapshot events.
- Operators may configure compaction interval and retention. They may not trigger ad hoc compaction that bypasses validation or replay checks.

Rollback interaction:

- Rolling back `p2_001_events` archives snapshots with the event database.
- Rolling back later memory, working-memory, consolidation, or verification migrations must not delete snapshots that are needed to replay event history.
- A restored older binary may ignore snapshot files, but it must not require deleting `graph.db` or `memories.db`.

## Compatibility constraints

Migration implementations must preserve the MCP compatibility rules in [2026-05-16-mcp-compatibility-policy.md#Backward-compatibility](./2026-05-16-mcp-compatibility-policy.md#backward-compatibility). Existing tool names, compact/full render modes, and legacy memory rows remain parseable during the compatibility window.

Current storage behavior from [2026-04-11-storage-and-search-backends.md#Memory-Search](./2026-04-11-storage-and-search-backends.md#memory-search) and [2026-04-11-storage-and-search-backends.md#Contract-Notes](./2026-04-11-storage-and-search-backends.md#contract-notes) remains valid: SQLite tables are the source of truth, FTS/vector indexes are rebuildable accelerators, and MCP response shapes do not change merely because storage is richer.
