# Graph storage

Each checkout owns its graph database and file manifest under its disposable
cache directory. Graph nodes, edges, module digests, the generation epoch, and
the exact indexed-file manifest commit in one SQLite transaction. A failed
manifest or graph write leaves the previously published generation available.
Watcher publication occurs only after that transaction succeeds.

Registered checkout graph and vector databases retain the same managed cache
directory capability used by repository accounting. SQLite files and sidecars
open through that capability, and the inventory generation is invalidated
before the first durable size-changing write. Replacing a cache or repository
pathname cannot split the payload authority from the registry being
invalidated.

Symbol bodies are immutable repository-shared content objects. Their key is the
SHA-256 digest of the exact UTF-8 body bytes. Checkout node rows retain symbol
identity, signature, source span, and the object key; the inline body column is
empty for new repository-backed generations. Identical bodies across worktrees
therefore occupy one object file.

Object writes finish and sync before the checkout transaction publishes their
keys. This ordering does not assume atomic transactions across SQLite and the
filesystem: interruption may leave an unreferenced immutable object, but cannot
publish a reference before its object exists. Loading verifies the object's hash
and returns a storage error for a missing, corrupt, or non-UTF-8 object. Lattice
does not silently substitute an empty body or replace the graph in that case.

Opening an older graph with repository object storage stages every inline body,
then transactionally writes all object references and clears the inline values.
A staging or transaction failure aborts opening instead of leaving two runtime
representations authoritative. This is a schema transition within the existing
graph database, not a parallel graph schema.

Shared body objects use a repository publication lock and an indexed reference
database. A checkout has an explicit owner ID; no graph pathname grants object
authority. A writer takes a shared lock and transactionally pins both the prior
committed set and the complete proposed set before staging bodies. It keeps that
lock through the graph transaction, then supplies its already-open graph SQLite
connection so the reference index can stream the actual committed epoch and
`nodes.body_hash` rows into the new membership. This closes both crash windows:
a pre-commit interruption retains the prior and proposed pins conservatively,
while a post-commit interruption is reconciled from that checkout's own graph
connection when `GraphStore` next opens.

Repository maintenance takes the exclusive publication lock and selects at
most the configured number of indexed objects having neither a committed
reference nor a publication pin. It never constructs an all-live set or scans
every manifest and shard. Unknown pre-index object files remain preserved until
ownership is proved. Candidate deletion is journaled before identity-checked
`unlinkat`; restart treats a missing already-deleted file as replay completion.
Interrupted staging filenames are indexed and reclaimed through the same bound.
A checkout's references retire only after lifecycle GC has proved and completed
the cache-directory move into repository trash. Readers hold the shared lock
while opening and validating an object, so concurrent GC cannot remove a body
being hydrated.

Object files and reference-index state are written transactionally or to temporary files, synced,
atomically renamed with descriptor-relative operations, and followed by a
parent-directory descriptor sync. SQLite opens use `SQLITE_OPEN_NOFOLLOW`;
object and metadata opens use `openat` with `O_NOFOLLOW` on Unix.
Missing, malformed, and hash-mismatched objects remain explicit storage errors.

Context-handle persistence is a disposable navigation cache. Managed loads use
its pinned checkout authority, reject symlinks and invalid data, and cap reads
at 8 MiB. An unreadable or oversized cache is reported and left unloaded;
clients must obtain a fresh handle. Managed publication never creates directories
through a replacement pathname, and failed staging cleanup preserves foreign
file identities.
