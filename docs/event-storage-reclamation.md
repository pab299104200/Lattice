# Event payload reclamation

Part of the [storage remediation program](plans/2026-09-12-storage-and-agent-memory-redesign.md).

The current event schema (version 4) retains the version 3 spill-membership indexes and deletes a payload in the same transaction that removes its last event reference. The writer inserts the payload and envelope atomically; it no longer caches spill-row identifiers that compaction can invalidate. Failed envelope insertion rolls back new payloads. Compaction also removes at most 256 preexisting orphan payload rows per invocation. Shared payloads survive while any event references them.

New file stores use incremental auto-vacuum. `EventStore::reclaim_free_pages` permits 1–4096 pages per call and uses a passive WAL checkpoint. Its result separates released database pages from pending WAL frames; a held reader can prevent immediate physical reclamation. It never performs a full database-copy vacuum. Existing stores without incremental auto-vacuum return an explicit offline-conversion requirement. Operators must establish backup and peak-space budgets before converting historical files. No live store has been converted or reclaimed by this implementation session.

Event payload compaction is separate from snapshot-retirement authority and the
repository-owned [memory retention scheduler](memory-retention.md). The payload
fix alone does not imply that historical snapshots containing memory have expired
or that all event storage is byte bounded. Snapshot retirement is described below.

Verification: event-focused Rust suite passed 47 tests (3 ignored), including failed transaction rollback, shared payload preservation, reappend, concurrent writers/compaction, and held-reader physical reclamation. These tests use temporary stores.

## Snapshot and memory boundary

Snapshot format 2 is a graph replay checkpoint: it contains graph nodes, edges, and the durable event-row cursor, and its memory collection is always empty. Durable repository memory is owned by the memory SQLite store and its separately authorized offline backup workflow. Compaction cannot restore or preserve memory from a graph snapshot.

Readers continue to recognize format 1 snapshots for graph replay, but memory embedded in historical files is never imported. Managed inventory accepts only the exact `snapshot-<event-row>-<timestamp>.bin` shape; unknown names are outside automatic deletion authority.

Historical format 1 snapshots may contain memory payloads and must expire within the configured backup-retention horizon. Deletion is a bounded maintenance action: retain a verified graph checkpoint, inspect at most 4,096 directory entries per invocation, and remove at most eight files under the normal 64 MiB work budget. One oversized file is admitted when it is the first candidate so a large historical copy cannot be deferred forever. Snapshot files are ordered by their numeric event-row and timestamp fields for expiry, rotation, and corrupt-snapshot fallback. Reads, atomic replacement, and deletion operate relative to a pinned, no-follow directory descriptor. Routine maintenance never uses a full unbounded vacuum. No live snapshot deletion was performed by this change.

Persisted daemon maintenance reads native directory pages of at most 256 entries
and may span any number of invocations. Its cursor includes a directory mutation
stamp; a changed directory restarts inventory before deletion can rely on the
old cursor. Deletions performed by the sweep deliberately force another stable
inventory. The daemon also captures the SQLite checkout-membership generation
across the complete root-plus-checkout cycle and revalidates the root directory
before allowing memory purge. Windows owner workers retain one active native
directory handle across pages. A process restart or changed directory discards
the handle-bound cookie and safely restarts bounded inventory from the beginning.
