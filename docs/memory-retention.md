# Recall-based memory retention

Memory lifetime is based on acknowledged delivery to an authenticated caller. Querying, ranking, internal reads, failed response delivery, and queued delivery attempts do not extend it. An attempt records an exact delivery ID, repository, session, payload hash, and canonical memory set. Acknowledgement must reproduce that binding within the seven-day replay window.


Workflow delivery reloads canonical content and trust metadata before rendering,
then compares complete snapshots inside the receipt transaction. Shortened
candidate text never qualifies as a complete lesson. If content and proof cannot
fit, both are withheld. Failed or unreturned attempts do not renew retention.

Persisted context handles contain references only. Expanding a referenced
memory checks current authority, checkout applicability, lifecycle state, and
the complete serialized trust snapshot in the same transaction that records
the delivery attempt. If the complete lesson and receipt cannot fit the
requested expansion budget, the call fails without a receipt or handle
renewal.

The default policy marks memory retention-stale after 90 days without acknowledged recall and purges it after 180 days. Historical rows receive one persisted 30-day migration grace. `LATTICE_MEMORY_STALE_SECS`, `LATTICE_MEMORY_PURGE_SECS`, `LATTICE_MEMORY_SWEEP_SECS`, `LATTICE_MEMORY_RECEIPT_SECS`, `LATTICE_MEMORY_MAX_RECEIPTS`, and `LATTICE_MEMORY_RETENTION_BATCH` configure the policy; invalid configurations fail startup. Default retrieval adds `retention_stale = 0`; explicit stale discovery may omit it and must label the result stale.

A persisted deadline admits one transactional sweep across concurrent callers. Recall acknowledgement uses an immediate transaction too, so purge and recall serialize. Purge deletes dependent verification state, capture commits, proposal snapshots containing the memory ID, and only checkpoints indexed as containing that memory. Foreign checkpoints and evidence owned by other memories remain.

Retention migration and collection use durable, restartable pages. Stale and
purge eligibility scans advance by memory ID and examine at most the configured
retention batch per transaction. Dependent proposal and working-memory
checkpoint deletion is limited to 32 rows of each kind per candidate per pass;
final removal of the memory payload follows only after those dependencies are
gone. Expired delivery and receipt cleanup uses the same retention batch bound.
The first dependency page commits an irreversible `purge_pending` fence,
marks the memory invalidated, records its deletion receipt, and advances the
restore floor. Reads hide that row, acknowledgements cannot renew it, generic
updates cannot mutate it, and replay cannot replace it. Pending receipts do not
age out. Restarted maintenance prioritizes these fenced rows and finishes their
bounded dependency pages regardless of later wall-clock or recall activity.

Consolidation proposals maintain a semantic memory-reference index on every
insert and payload update. The index includes proposal targets, canonical
memory state IDs, source and replacement IDs, links, supersession, and
contradiction relationships. Ordinary strings such as branch names, status
labels, and narrative evidence are not dependencies. Proposal payloads are
limited to 1 MiB and 4,096 indexed memory references. Existing proposal rows
are backfilled in transactions of at most 64 rows and 4 MiB; checkpoint
references use the same row and byte budgets. Reads of historical payloads are
capped before decoding. A malformed or oversized historical row records its
identity and blocks purge without deleting audit data. Deleting or repairing
that row clears the block on the next maintenance pass. The daemon schedules
bounded continuation work after 10 seconds, or sooner when configured, while a
backfill or staged purge remains incomplete. Continuations do not advance the
configured sweep age or grace clocks.

Deletion-receipt cardinality is also migrated through a durable 64-row cursor.
Insert and delete triggers maintain the exact count across concurrent writes;
sweeps and health reports read the singleton counter instead of traversing the
receipt B-tree. Retention work pauses while that count migration is incomplete.
Health exposes `receipts_complete`; until it is true, `receipts` is the durable
lower bound counted so far rather than an exact total.

Deletion receipts are bounded by age and count and live at least as long as accepted replay. Every purge advances a durable restore floor. Snapshot restoration must call `validate_restore_time` before inserting memory, including after receipts expire. Session digest capture rejects input outside the same replay horizon.

Health distinguishes logical payload bytes deleted from physical database bytes reclaimed. Row deletion does not shrink SQLite allocation, so physical reclamation stays zero and `reclamation_pending` stays true until separately measured database compaction reports it.

The daemon persists proven memory-store owners in its versioned state registry. Repository stores must be anchored by the adjacent storage registry whose canonical home matches the store directory; an arbitrary user-owned SQLite file is not accepted. A separately configured organization database is accepted only when its canonical path exactly matches `LATTICE_SHARED_MEMORY_PATH`, `[memory].shared_store_path` in the operator-owned `~/.lattice/config.toml`, or the documented default shared path. The shared-memory runtime registers that owner after successfully opening it. The registry is a regular non-symlink file bounded to 1 MiB and 1,024 records. Registration takes a cross-process file lock and replaces a mode-0600 registry atomically after syncing file and directory. Startup registration also discovers valid prior entries, rejects missing, renamed, and symlink store paths, and starts at most one worker per canonical owner. Each interval acquires the repository memory lock, honors the store's persisted sweep deadline, performs at most 256 incremental-vacuum pages, and expires at most eight recognized snapshots within a normal 64 MiB work budget and 30-day horizon. One oversized first candidate is admitted so it cannot be deferred forever. Oversized legacy snapshots are hash-verified and rewritten through fixed 64 KiB buffers, preserving graph bytes and the event cursor while dropping copied memory before atomic publication. Unknown snapshot names are inventoried and preserved.

Snapshot retirement precedes authoritative row purge. Any snapshot validation,
I/O, or space failure leaves the memory row retryable. Persisted snapshot
maintenance uses opaque, typed directory cursors. On macOS,
Lattice records the kernel block offset plus the number of records consumed from
that block and replays at most one fixed 64 KiB block after restart. It never
derives offsets from directory record sizes. Inventory and deletion are separate
phases: deletion starts only after a complete inventory proves the numerically
newest graph checkpoint, and a partial or interrupted cycle keeps the memory
purge fence closed. Each inventory page examines at most 256 entries; cursors
have no ordinal ceiling, so directories beyond 65,536 entries continue making
bounded progress. Checkout registry paging similarly persists its position
between bounded maintenance runs. Each directory cursor is paired with the
pinned directory identity and nanosecond modification stamp. A changed stamp,
vanished entry, or malformed cursor restarts inventory and leaves the purge
fence closed; deletion begins again only after a stable complete inventory.
The root snapshot stamp remains part of the proof while checkout pages advance
and is revalidated at the end.

Repository checkout membership has a separate monotonic generation maintained
by SQLite triggers for insert, delete, checkout-identity change, and repository
owner change. Maintenance captures that generation before paging and compares
it again before purge. A mismatch restarts the entire root and checkout cycle,
so an entry inserted lexically before the persisted cursor cannot be skipped.
Heartbeat-only updates do not invalidate membership inventory.

Windows native directory continuations are handle-bound. Each owner worker
therefore retains at most one active snapshot-directory handle across bounded
maintenance calls (the root or current checkout). A stable directory continues
on that handle without an entry ceiling. Daemon restart, directory replacement,
mutation, or checkout switch discards a persisted Windows cookie and begins a
new bounded full inventory; purge stays closed until that new cycle completes.
