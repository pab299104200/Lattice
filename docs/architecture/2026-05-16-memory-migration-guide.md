# Memory Migration Guide

## Overview

T22 migrates legacy rows from the existing `memories` table into the Phase 3
memory graph schema: `memories`, `memory_links`, `memory_evidence`,
`memory_accesses`, and `memory_scores`.

The live legacy schema in `daemon/crates/lattice-core/src/memory/store.rs`
contains 29 persisted columns plus SQLite `rowid`. Older inventory notes refer
to a wider experimental table; this guide is authoritative for the schema the
daemon currently creates and reads.

The importer keeps source and destination SQLite connections separate. The
operator default is:

- source: `<workspace>/.lattice/memories.db`
- destination: `<workspace>/.lattice/memory_graph.db`

Each destination row is tagged with `created_by` or actor detail text beginning
with `MigrationFrom { source_row_id: ... }`, so migrated data can be separated
from runtime writes during audits.

## Column mapping

| Legacy column | Destination | Transform | Null/default behavior | Drop rationale |
|---|---|---|---|---|
| `rowid` | `migration_progress.source_row_id`; provenance actor detail | Preserved as the restart/idempotence cursor and source provenance id. | Required by SQLite. | Not dropped. |
| `id` | `memories.memory_id` | Encoded as `memory:{workspace_id}/{id}`. | Required. | Not dropped. |
| `session_id` | `memories.scope_session_id`; `memory_accesses.accessed_in_event` synthetic id input | Used when the resolved scope is `session`; empty becomes `legacy-session`. | Empty string becomes `legacy-session` for session-scoped rows. | Not dropped. |
| `content` | `memories.content` | Copied verbatim. | Required. | Not dropped. |
| `memory_type` | `memories.class` | `observation`, `decision`, `pattern`, `anti_pattern` map directly; `exploration` maps to `open_question`. | Unknown values skip the row with `RowMappingFailed`. | Not dropped. |
| `scope` | `memories.scope` and scope discriminator columns | `session`, `branch`, `repo` map directly. Empty falls back to stream default. | Unknown values can fall back from `freshness_policy`; otherwise row is skipped. | Not dropped. |
| `confidence` | `memories.confidence`, `memories.usefulness_score`, `memory_scores.value` | Clamped to `[0, 1]`; initial usefulness prior uses the same value. | SQLite default is `1.0`; importer still writes an explicit value. | Not dropped. |
| `linked_symbols` | `memories.linked_symbols_json`, `memory_links` | JSON string array becomes legacy `SymbolId` values under `legacy-symbols`; each symbol gets an `applies_to` link. | Invalid JSON skips the row. Empty array produces no symbol links. | Not dropped. |
| `linked_files` | `memories.linked_files_json`, `memory_links` | JSON string array becomes `FileId` values with `content_hash = "legacy"`; each file gets an `applies_to` link. | Invalid JSON skips the row. Empty array produces no file links. | Not dropped. |
| `workspace_id` | `MemoryId.workspace_id`; `scope_workspace_id`; linked identity workspace fields | Copied into all stable identities. | Missing value becomes `legacy`. | Not dropped. |
| `branch` | `memories.scope_branch` | Used only for branch-scoped rows. | Missing branch becomes `legacy` for branch scope. | Not dropped. |
| `refresh_key` | Dropped | No direct memory graph column exists; freshness and retrieval are represented by class, stream, scope, links, scores, and evidence. | Null accepted. | Dropped because it was a legacy lookup cache key, not memory content or evidence. |
| `source_query` | `memories.confidence_reason` fallback | Used when `confidence_reason` is null. | Null falls through to stale reason or migration default. | Not dropped. |
| `assertion_type` | `memories.assertion_type`; stream classification input | Legacy `workflow_outcome` becomes `outcome`; `pattern`, `anti_pattern`, and `exploration` become `observation`; known Phase 3 assertion strings map directly. | Unknown values use the class default only when class is known. | Not dropped. |
| `verification_status` | `memories.verification_status`; `memory_links.verification_status` | Known statuses map directly. | Unknown values skip the row. | Not dropped. |
| `confidence_reason` | `memories.confidence_reason` | Copied when present. | Fallback order: `source_query`, `stale_reason`, migration guide reference. | Not dropped. |
| `supersedes_memory_id` | `memories.supersession_links_json`, `memory_links` | Encoded as a memory identity and a `supersedes` link. | Null produces no supersession link. | Not dropped. |
| `superseded_by_memory_id` | `memories.superseded_by` | Encoded as `memory:{workspace_id}/{id}`. | Null remains null. | Not dropped. |
| `contradicts_memory_ids` | `memories.linked_memories_json`, `memories.contradiction_links_json`, `memory_links` | JSON string array becomes memory references and `contradicts` links. | Invalid JSON skips the row. Empty array produces no contradiction links. | Not dropped. |
| `contradicted_by_memory_ids` | Dropped | Reverse contradiction edges are reconstructable from migrated `contradicts` links. | Invalid content does not affect migration. | Dropped to avoid storing duplicated inverse relationship state. |
| `freshness_policy` | `memories.freshness_policy_json`; scope fallback | Legacy enum maps to `FreshnessPolicy.kind`; `time_bound` receives a 30 day TTL and 7 day recheck interval. | Unknown values use the stream default policy. | Not dropped. |
| `freshness_policy_detail` | Dropped | The legacy column is free text with no enforced contract. | Null accepted. | Dropped because the Phase 3 policy object stores typed freshness semantics. |
| `provenance_json` | Dropped as a direct column; replaced by migration actor detail | Legacy provenance shapes were unconstrained. | Invalid content does not affect migration. | Dropped because every migrated row now has uniform source-row provenance. |
| `evidence_json` | `memories.evidence_references_json`, `memory_evidence` | JSON array is preserved in references JSON; each entry becomes `memory_evidence` with a synthetic event-reference anchor. | Invalid JSON skips the row. Empty array produces no evidence rows. | Not dropped. |
| `created_at` | `memories.created_at`, derived graph row timestamps, `memory_scores.computed_at` | Copied as epoch seconds. | Required. | Not dropped. |
| `last_accessed` | `memories.updated_at`, `memory_accesses.accessed_at`, `access_history_json` | `updated_at` is `max(created_at, last_accessed)`. | Required. | Not dropped. |
| `access_count` | `memory_accesses`, `memory_scores.sample_size`, `access_history_json` | Positive values create one summarized access row and score sample size. | `0` creates no access row. Negative values are treated as `0` for score sample size. | Not dropped. |
| `is_stale` | `memories.verification_status` | Non-zero forces `stale`. | Default `0`. | Not dropped. |
| `stale_reason` | `memories.confidence_reason` fallback | Used after `source_query` when no confidence reason exists. | Null falls through to migration default. | Not dropped. |
| `is_invalidated` | `MigrationReport.skipped_rows` | Non-zero rows are reported as skipped. | Default `0`. | Not copied because invalidated rows are not active memories. |

## Idempotence

The destination database owns `migration_progress`:

```sql
CREATE TABLE IF NOT EXISTS migration_progress (
  source_row_id INTEGER PRIMARY KEY,
  dest_memory_id TEXT NULL,
  status TEXT NOT NULL CHECK(status IN ('migrated', 'skipped')),
  migrated_at INTEGER NOT NULL,
  schema_version INTEGER NOT NULL
);
```

`MemoryMigrator::run` reads rows after the highest migrated `source_row_id`.
Each successful row is inserted into all destination tables inside the batch
transaction before its progress row is written. A second successful run sees
the progress rows and inserts nothing.

If a destination memory already exists for a source row that has no progress
record, the importer returns `IdempotenceConflict { source_row_id }`. Operators
must inspect the destination row instead of allowing silent overwrite.

## Dry-run

`--dry-run` performs the same row reads and mapping as `--apply`, then inserts
mapped rows into an in-memory validation database initialized with the memory
graph schema. It returns the same counts and skipped-row report as an apply run
but writes nothing to the destination path.

Dry-run is intended to be run immediately before apply:

```bash
lattice memory-migrate --dry-run --workspace /path/to/repo
lattice memory-migrate --apply --workspace /path/to/repo
```

## CLI usage

Default workspace paths:

```bash
lattice memory-migrate --dry-run --workspace /path/to/repo
lattice memory-migrate --apply --workspace /path/to/repo
```

Explicit database paths:

```bash
lattice memory-migrate --dry-run \
  --source /path/to/.lattice/memories.db \
  --dest /path/to/.lattice/memory_graph.db \
  --batch-size 500
```

Exactly one of `--dry-run` or `--apply` is required. `--batch-size` must be a
positive integer.

## Recovery from partial migration

Batches commit atomically. If the process stops mid-batch, SQLite rolls that
batch back and leaves prior committed batches intact. Re-run the same command;
the importer resumes after the highest migrated `source_row_id` recorded in
`migration_progress`.

Rows that cannot be mapped are recorded as skipped in the report and in
`migration_progress`. They do not halt migration of later valid rows. Fix the
source row and remove its skipped progress row before retrying if it must be
migrated.

## Rollback procedure

Rollback follows `docs/architecture/2026-05-16-storage-migration-policy.md`
`## Rollback`: stop the daemon, preserve the legacy `memories.db`, remove or
quarantine the destination `memory_graph.db`, and restart on the legacy reader.
Because the importer does not mutate the source database, rollback does not
require restoring source rows from backup unless an operator manually modified
the source database outside the migration command.
