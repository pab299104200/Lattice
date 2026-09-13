# Resource admission and disposable-cache budgets

Lattice reserves logical bytes before it creates an active checkout view or a
staging index generation. A single process-wide broker accounts for both
classes, so concurrent cold starts and watcher rebuilds cannot independently
overcommit the configured allowance. Dropping a view, completing a rebuild,
cancelling its task, or returning an error releases its reservation through
the reservation guard.

`LATTICE_MATERIALIZATION_BUDGET_BYTES` sets the process daemon allowance
and defaults to 2 GiB. `LATTICE_VIEW_RESERVATION_BYTES` sets the configured
logical admission unit for each resident checkout view and defaults to 256 MiB.
`LATTICE_INDEX_JOB_RESERVATION_BYTES` sets the additional allowance for a
concurrent staging generation and also defaults to 256 MiB. These are logical
admission units, not measured allocations or guarantees about physical RSS.
`LATTICE_VIEW_CLASS_BUDGET_BYTES` and `LATTICE_INDEX_CLASS_BUDGET_BYTES`
optionally cap their respective classes within the process-wide allowance;
both default to the process allowance. The source-payload class shares the
index class cap.

Once traversal has securely opened and measured the candidate files, an index
generation reserves the exact metadata byte total for changed source buffers
before any changed source is read. After parsing, it counts the exact JSON wire
bytes of every parsed-file value without allocating a second serialized copy,
then grows the reservation before graph construction copies node data. These
are reproducible logical input and parsed-payload byte bases, not claims about
allocator overhead or resident memory. The operating system remains the
authority for a hard RSS/address-space limit; configure the daemon's service
manager limit separately when one is required.

Admission first uses the existing idle-view eviction path. If active or
indexing views still consume the allowance, the request receives a truthful
`partial: true` response with a `resource-limited` deferred reason. It does not
publish or describe a truncated graph as complete. The cold status response
reports the limit, reserved and available logical bytes, and reservations by
class. Exact lexical/path service remains available for already published
views; a never-materialized cold view reports its stages as not evaluated.

Repository cache GC remains a separate disk lifecycle. Its repository high
and low watermarks, idle grace, typed journal, and active-lease exclusions are
documented in [storage lifecycle](storage-lifecycle.md). Cache reclamation may
delete only registered disposable cache bundles. It never deletes durable
knowledge or unknown artifacts to satisfy either memory admission or a disk
budget.

`LATTICE_USER_CACHE_BUDGET_BYTES` caps allocated bytes across all proven
repository homes known to the user registry and defaults to 8 GiB.
`LATTICE_DISPOSABLE_CACHE_BUDGET_BYTES` caps the disposable-cache class across
those homes and defaults to 6 GiB. The registry is persisted under
`$XDG_STATE_HOME/lattice` (or `$HOME/.local/state/lattice`), is limited to 1
MiB and 1,024 homes, and accepts a home only after its repository id and
canonical path match the repository storage authority. Accounting uses
allocated filesystem bytes from bounded class inventories. Under aggregate
pressure, collection delegates exclusively to the existing typed checkout
cache GC, which rechecks idle age and active leases and journals each move.
Durable knowledge, telemetry, historical files, and unknown artifacts remain
in the total for honest reporting but are never eviction candidates. If those
protected classes keep the user total above its cap, maintenance remains over
budget rather than deleting them.

Before changing defaults, benchmark cold and warm indexing with 1, 5, and 20
worktrees. Record wall time, CPU, peak RSS, bytes read and written, WAL peak,
physical disk, admitted logical bytes, rejection count, and query p50/p95.
Release limits must cite those measurements; no percentage reduction should
be claimed from the initial defaults.

Disposable-cache writers invalidate repository inventory before changing
allocated bytes and retain a shared accounting fence through publication.
Vector synchronization owns one fence for the whole delete/upsert/accelerator
batch, so inventory advances are excluded once per batch rather than once per
vector. Operations that prove they are no-ops do not invalidate a completed
inventory.
