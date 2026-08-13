# Worktree Storage Architecture

**Status:** binding design for recovery workplan C1

## Decision

Git worktrees are separate **checkouts** of one canonical **repository**. Lattice
must preserve that distinction everywhere it chooses an identity, a storage
path, a cache key, or a verification context.

The current checkout root remains the boundary for reading source files,
watching changes, resolving relative paths, and publishing a graph snapshot.
The canonical repository identity is the boundary for repository-scoped memory
and shared durable metadata. A sibling worktree must never be treated as a
second repository merely because its `.git` entry is a pointer file.

## Identity resolution

For every requested checkout root, resolve Git identity with the equivalent of
`git rev-parse --git-common-dir` and `git rev-parse --show-toplevel`:

1. Canonicalize the checkout root without following a path outside the allowed
   workspace boundary.
2. Discover Git from that root. A `.git` directory and a `.git` `gitdir:`
   pointer file are both valid Git checkouts.
3. Resolve the common Git directory. Its canonical path is the repository
   identity input; it is shared by the primary checkout and every worktree.
4. Derive `RepositoryId` deterministically from that common directory (and
   repository format/version where necessary), never from a checkout path.
5. Derive `CheckoutId` separately from the canonical checkout root plus the
   repository identity. It distinguishes content and verification context, but
   it is not a memory-scope identity.

If Git discovery fails, retain the existing standalone-workspace behavior: the
canonical workspace root is both repository and checkout identity. Do not
invent a cross-directory identity from matching basenames, remotes, or source
contents. Symlinked roots must resolve to one canonical identity, and invalid
or unreadable `gitdir:` pointers must return an actionable workspace error.

## Storage layout and authority

For a Git repository, use a repository-owned Lattice directory resolved from
the primary checkout associated with the common Git directory. Worktrees must
not create independent `<worktree>/.lattice/` stores.

| Data | Key / location | Why |
|---|---|---|
| Repository memories | canonical repository `.lattice/memories.db`, keyed by `RepositoryId` | Repo, branch, and session knowledge belongs to one repository, independent of checkout location. |
| Organization memories | `~/.lattice/shared/memories.db`, owned by the shared-memory router | This is distinct authority; C1 neither aliases nor duplicates it. |
| Checkout graph snapshot | `graph.db` namespaced by `CheckoutId` under canonical repository storage | A graph represents one checkout's files and cannot be shared across divergent branches. |
| Parsed-file cache | canonical repository cache keyed by parser version, schema version, and content hash | Identical files across worktrees reuse parsing safely. |
| Context handles, watcher state, vector state | keyed by `CheckoutId` unless explicitly repository-scoped | They may describe different live contents, paths, or graph epochs. |

The memory store records `RepositoryId` for scope enforcement. It may record
`CheckoutId` and the checkout path as evidence/diagnostic context, but must not
use either to authorize repository memory access. A verification result is valid
only for the checkout snapshot that produced it; a result from another worktree
is advisory until reverified.

Graph database paths must be deterministic and collision-resistant. Use a
stable encoded `CheckoutId`, not a branch name: branch names are mutable,
detached HEAD has no branch name, and two worktrees may check out the same
branch only under abnormal repository states. This replaces the prior implicit
one-`.lattice`-per-checkout behavior; no compatibility path may continue to
write worktree-local memory stores after C1 lands.

## Content-addressed parsed-file cache

The parsed-file cache is the only graph-adjacent state shared across checkouts.
Its cache key includes at minimum:

- the byte-level stable content hash;
- normalized parser/language identity and parser version;
- Lattice parsed-file schema version; and
- every parse-affecting configuration or feature flag.

The file path, branch, checkout root, mtime, and file size are validation or
lookup aids, not authority for a cache hit. Cache values contain no
checkout-relative edges, resolved imports, graph node identifiers, or absolute
paths that could leak one checkout's topology into another. Cache corruption,
schema mismatch, or an incomplete entry is a cache miss and must be recorded in
status/diagnostics; it must never produce a partial graph as a hit.

The C1 implementation must measure a real-worktree cold start. On a fixture
where at least 90% of indexed source bytes are unchanged, it must demonstrate
at least 90% parsed-file cache reuse and prove graph equivalence to a full
parse. The requirement is a cache-hit measurement, not an assumption based on
file count.

## Concurrency, atomicity, and recovery

Multiple daemon shards may open the same repository `memories.db`. SQLite WAL
is required, with the existing bounded busy timeout, foreign keys, and bounded
auto-checkpoint configuration applied on every connection. Database mutations
remain short transactions; callers must not hold a write transaction while
parsing, watching, network I/O, or graph construction. `SQLITE_BUSY` after the
bounded retry/timeout is an actionable operation failure, never a silent
fallback to a worktree-local database.

Repository memory writes are serialized by SQLite, but memory scope checks use
the resolved `RepositoryId` at both query and post-query enforcement boundaries.
The shared parsed cache permits concurrent read/write access only through its
transactional store API. A writer must publish a complete cache row atomically;
readers either observe the prior complete row or miss and parse locally.

Each `CheckoutId` graph state has independent watcher/invalidation epochs and
an independent graph write lock. A commit, ref update, or index update in a
sibling worktree must not invalidate this checkout merely because both share a
common Git directory. C2 owns the precise watcher predicate required to uphold
that rule.

Backup and recovery treat SQLite database, WAL, and SHM files as one live set
or use SQLite's online backup API. `memories.db` is durable authority and is
never deleted to repair a reconstructable graph/cache failure. Graph snapshots
and parsed-file cache entries are reconstructable per their respective keys.

## Implementation contracts and tests

C1 implementation must expose one resolved identity/storage object to the CLI,
daemon registry, storage bootstrap, memory router, watcher, and status output;
those call sites must not independently reinterpret `.git` pointer files.

Required integration coverage uses a real fixture repository and linked
worktree, rather than mocked `.git` strings, and proves all of the following:

1. Primary checkout and worktree resolve to one `RepositoryId` and two distinct
   `CheckoutId` values.
2. Both read and write the same repository `memories.db`; a repo-scoped memory
   saved in either is visible in the other without cross-repository leakage.
3. Graph storage is distinct per checkout and a divergent source file cannot
   surface the primary checkout's graph nodes or verification result.
4. A cold worktree scan reuses at least 90% of parsed-file cache entries when
   source content is unchanged, with graph equivalence to a full rebuild.
5. Concurrent repository-memory writes from two checkout shards preserve all
   committed rows and surface bounded lock failures truthfully.
6. Malformed pointer files, missing common directories, and non-Git roots have
   deterministic, actionable outcomes.

## Dependencies

C1 is the identity prerequisite for C2's worktree-safe invalidation and D2's
repository memory routing. D2 may use injected `RepositoryId` values in unit
tests before C1 implementation, but production routing must consume C1's
resolver and must not retain a path-derived fallback. C1 does not alter
organization-store authority or shared-memory ranking; those remain D1/D2
responsibilities.
