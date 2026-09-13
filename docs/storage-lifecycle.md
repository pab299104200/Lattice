# Repository storage lifecycle

Lattice stores repository-wide durable state in the repository storage home and
checkout-specific state under `checkouts/<checkout-id>`. The only automatically
reclaimable directory is `checkouts/<checkout-id>/cache`. Its positive allowlist
contains the graph SQLite bundle, vector SQLite and Usearch files, and context
handles. Memory, events, snapshots, and unknown files are not garbage-collected.
Parsed objects are reclaimed only when the parsed-cache database proves that no
checkout membership and no committed-manifest pin references them.

Repository-shared immutable symbol bodies live under `symbol-bodies`, outside
checkout cache GC. See `graph-storage.md` for their publication and verification
contract.

After proven checkout bundle eviction, the durable GC journal releases that
checkout's object references. Other committed references and in-flight publication
pins continue to protect shared bodies. A bounded collector removes only
unreferenced indexed objects under the exclusive object lock. Unknown files and
unproven trash never authorize releasing memberships.

Every live checkout holds an operating-system file lease for its entire runtime.
The registry updates `last_seen` once per minute for inventory, but the file lease
is the authority: a cache cannot be reclaimed while a process still holds it,
even when its heartbeat is stale. Before reclamation, Lattice takes the repository
maintenance lock and rechecks the checkout lease and idle age.

The default cache pressure limit is 2 GiB. Checkout accounting advances through
at most 4,096 registry rows per maintenance call and persists its lexical cursor
and partial accumulator. Totals are published only after a complete traversal of
one stable registry generation. Registration, removal, root changes, and cache
publishers invalidate the snapshot; until a fresh cycle completes, pressure is
reported as unknown and cannot authorize deletion. Published totals older than
60 seconds likewise cannot authorize deletion. When allocated cache bytes exceed it,
inactive checkouts idle for at least 24 hours are selected oldest first until the
projected size is at most 1.5 GiB. Selection and deletion are bounded. Lattice
journals per-checkout accounting for the completed generation and reads eviction
candidates through its `(generation, reclaimable, last_seen, checkout)` index;
execution still rechecks the operating-system lease and source identity. Lattice
journals each cache generation, atomically renames the whole cache directory into
same-filesystem trash, syncs both pinned parent directory descriptors,
and deletes only flat allowlisted files. Startup maintenance resumes interrupted
trash deletion and never deletes a newly-created source cache generation. A
journal row created before trash identity fencing has no durable device/inode
proof; maintenance preserves that trash and its shared-object memberships for
typed operator recovery instead of guessing that the payload is disposable.

The per-user budget registry binds each proven repository home to its stable
directory identity as well as its canonical path and repository ID. Maintenance
retains that verified directory handle through writable registry access. A home
replaced at the same path is rejected before accounting or reclamation. Version
1 path-only budget entries cannot prove identity and fail closed; explicit
registration by an active repository starts a version 2 authority set, and each
other repository must register again before it participates.
The state directory, lock, registry read, and atomic replacement remain beneath
one pinned authority. Registry staging files retain their opened-file identity
through cleanup, and publication syncs both file and directory. An unavailable,
copied, or path-replaced home is skipped without writable access and makes
aggregate accounting incomplete. It does not block reclamation from other
proven homes when their known allocation alone exceeds the configured budget.

On Unix, managed traversal, locking, accounting, rename, and deletion are
relative to directory descriptors pinned at registry open. Each component is
opened with `O_NOFOLLOW|O_DIRECTORY`; entries are identified with no-follow
`fstatat`, and destructive calls use `renameat`/`unlinkat` against those pinned
parents. Enumeration is paged with opaque directory-stream cookies and bounded
before metadata is loaded. Replacing `checkouts/<id>`, `cache`, a trash bundle,
or an object shard with a symlink therefore fails closed and cannot redirect GC
outside the repository home. Windows uses `NtCreateFile` with a pinned parent
handle and `FILE_OPEN_REPARSE_POINT`, handle identity checks, handle-based
rename/disposition, and bounded native directory enumeration. That Windows
implementation passed an isolated Windows managed-module cross-check. Full
Windows workspace and runtime verification remain outstanding.

Storage inventory reports logical and allocated derived bytes, WAL bytes, active
and retained checkout counts, reclaimable bytes, unknown cache artifacts, and
derived files from the historical flat checkout layout. Historical files remain
untouched during live startup. To migrate them, stop all Lattice processes, back
up the repository storage home, verify the files belong to that checkout and are
derived data, then move the allowlisted bundle into its `cache` directory. A
failed verification should leave the files in place for operator review.

To reset derived data, stop all Lattice processes, back up the storage home, and
remove only a checkout's `cache` directory. Never include `events.db`, snapshots,
`memories.db`, `parsed-cache.db`, the registry, leases, or unknown artifacts in a
cache reset. Lattice recreates the cache and rebuilds its graph and vectors on the
next start.

Allocated-byte accounting uses filesystem block counts on Unix. Each checkout
page scans only the fixed positive cache bundle and persists progress in the
repository registry. Read-only status consumes the last fully published snapshot
and never advances or repairs accounting implicitly.

Shared object totals cover managed objects present in the content and embedding
indexes. They do not claim that an unindexed foreign file is managed cache; such
files remain protected and are outside those indexed totals. Checkout-cache
unknown-artifact diagnostics are separate from shared-object receipt completeness. Existing indexed
objects are measured in bounded pages, and a missing or non-regular object is
recorded as an accounting error so later keys still progress while completeness
remains false.

The public read-only inventory and explicit cache plan/apply workflow are
documented in `docs/operator-storage.md`. Dry-run plans are bound to the exact
repository home and candidate set; apply revalidates leases and rejects stale
plans. Knowledge backup and restore use a separate offline contract and never
place durable memory inside the derived-cache GC allowlist.

The per-user registry and lock use user-only file permissions on Unix (0600),
and their Lattice state directory is user-only (0700). An existing XDG state
parent may be readable but must be user-owned and not writable by another user.
Unsafe existing permissions produce an actionable registration failure rather
than silently changing the operator's files. Windows uses retained directory
handles; this host has not validated Windows ACL behavior.

An unavailable registered home leaves the aggregate total explicitly incomplete.
If complete, proven homes alone exceed the configured cap, that lower bound is
sufficient to collect their eligible idle caches. Unknown homes never contribute
guessed bytes or authorize deletion, and unknown accounting cannot justify GC
when the known lower bound is within budget.

Workspace shutdown keeps its checkout lease until runtime-owned indexing and
publication work has actually stopped. Async task cancellation is followed by
joining the task wrappers and a completion barrier for blocking watcher jobs,
because cancelling a Tokio `spawn_blocking` handle does not stop the underlying
filesystem or SQLite writer. The index-work permit is retained by that blocking
job as well, so shard eviction and storage maintenance continue to see the
checkout as busy until the write returns. Finite startup indexing is joined
cooperatively rather than aborted during publication.

Workspace construction validates every configured repository identity and every
Git and health persistence surface before it starts any background worker. A
later-root validation failure therefore cannot strand an earlier-root writer.
Daemon listener exit first stops and joins connection tasks, then drains the
shard map under its mutex and performs runtime shutdown after releasing that
mutex. Shutdown also recovers runtime and bootstrap ownership from poisoned
mutexes; poisoning may degrade request handling, but it cannot silently discard
the handle responsible for releasing storage leases and joining writers.
Request dispatch, explicit reindexing, stale-workspace refresh, trusted checks,
query workers, and session consolidation join the same runtime completion
accounting. Detached MCP tasks register before spawning, and their blocking
children retain both completion and index-work permits. Daemon exit closes new
shard admission under the shard-map mutex before cancelling connection tasks,
so deferred requests and view prewarming cannot repopulate the map after its
shutdown drain.
