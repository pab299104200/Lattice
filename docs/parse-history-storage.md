# Parse and history cache storage

Lattice keeps two repository-shared derived caches outside checkout graph
generations. They are disposable: a missing or corrupt entry causes a local
rebuild, never a deletion of a durable graph, memory, event, or evidence store.

## Parsed files

`parsed-cache.db` keys path-free parser output by content hash, language,
parser version, schema version, and cache configuration. A checkout receives a
membership only after its graph and file-index manifest commit. Failed graph
publication therefore cannot pin a parse object.

Each successful manifest replaces the checkout's complete membership instead
of appending to it. Repository maintenance first moves an idle checkout's
cache bundle through the registry's durable trash journal. Only after that
move does the bounded parsed-cache collector use the remaining cache
directories as live manifest owners and remove unreferenced objects. It scans
at most 4,096 checkout records and deletes at most the configured cache batch.

If `parsed-cache.db` fails integrity validation, runtime parsing uses a local
in-memory cache. The corrupt shared file remains available for diagnosis; the
collector logs a deferred maintenance result and does not replace it.

## Commit manifests

`parsed-cache.db` also has an immutable commit-manifest contract for committed
base-content reuse. A manifest identity contains the proven repository ID, Git
object format, full commit object ID, and the parser, schema, and configuration
identities. Each entry contains a normalized relative path, Git mode, blob
object ID, content hash, and parse-object key. Repository and commit identity
come from an authorized Git runtime; the storage layer does not execute Git,
infer identity from a remote, or share data across repositories.

Publication validates the configured entry bound, path and object-ID formats,
and every referenced parse row. It writes the generation, entries, parse pins,
count, digest, and completeness marker in one immediate transaction on the
same managed SQLite connection as the parse objects. Lookups expose only a
complete generation and are bounded by the requested path count. A missing or
incompatible parse object, conflicting manifest, invalid identity, interrupted
transaction, or wrong parser configuration is a miss or failed publication;
partial state never becomes lookup authority.

The runtime opens the checkout as a Git repository, requires its canonical
common directory and directory identity to match the repository authority,
resolves its exact HEAD tree and object format, and inventories regular tracked blobs by relative path,
mode, and object ID. It marks staged, modified, deleted, renamed, untracked,
assume-unchanged, and skip-worktree paths unsafe. A complete manifest is pinned
for the checkout before any lookup. Only entries whose path, mode, and blob ID
match this inventory may supply a parsed file without reading or hashing its
worktree source. Reuse records each securely opened source's inode, device,
size, and nanosecond modification time and securely reopens it before graph
publication. Only one validation descriptor is needed at a time. Paths
affected by `autocrlf`, `core.eol`, `eol`, `filter`, `ident`, or
`working-tree-encoding` do not reuse committed bytes; Lattice inspects filter
attributes but never executes filter commands. A missing manifest, invalid parse object, ambiguous Git state,
or changed path falls back to the normal secure source read and hash.

The first proven clean indexing pass may stage a new commit manifest. Before
staging an entry, runtime compares the bytes it parsed with the named Git blob.
It publishes the complete manifest and checkout claim atomically only after the
checkout graph commits, and only after rechecking HEAD, the index identity and
metadata, status paths, and special index flags. A race or failed proof leaves
the previous graph and manifest authority intact and makes the next pass read
sources normally.

Checkout base claims keep complete generations and their parse pins live.
Retirement first makes an unclaimed generation unavailable to lookup or new
claims, then removes entry rows and parse pins across bounded transactions. A
generation larger than one collection budget therefore makes durable progress
instead of blocking younger generations forever. Checkout registration and
cache retirement bind and release claims at their established transactional
lifecycle boundaries.

This reuse removes source reads, content hashing, and parsing for proven
unchanged files in a cold checkout. Directory traversal and source metadata
checks still run, and each checkout still builds its own resolved graph and
query/vector indexes. Those mutable products are deliberately not shared;
branch-specific resolution remains isolated. An edited import or deleted file
therefore changes only that checkout's graph even when its other files came
from a shared commit manifest. Full commit-tree inventory is
currently bounded by the manifest entry limit, and repositories beyond that
limit use the safe source path.

## Git history

`history-object-cache.db` stores complete Git-mining snapshots. The cache key
contains the ordered, bounded commit window, every mining limit, and the
aggregation version. Branches with distinct windows cannot share a result,
even when their current tips or file paths look similar.

After publishing a newly mined snapshot, the runtime removes at most 128
entries not accessed for seven days. Active snapshots are revalidated against
their exact window and limits before use; an invalid row is treated as a miss
and mining continues locally. Cache cleanup is bounded and never changes the
checkout graph generation or Git repository state.
