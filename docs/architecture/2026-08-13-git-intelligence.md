# Git Intelligence

## Decision

Lattice derives repository-history signals from a bounded Git commit window and
persists one atomic, queryable snapshot in the workspace `graph.db`. The immutable
commit object id is the unit of reuse. The active checkout identity and resolved
`HEAD` select which persisted commits belong to the current window.

Git intelligence is an additive ranking and explanation input. It does not create
dependency edges, prove that a change is correct, or establish human ownership.
When history is unavailable, incomplete, or stale, consumers must report that
state and continue without the signal. Unknown history is never scored as zero
risk.

This replaces watcher-derived `edit_count` hotspots and watcher-batch co-change
counts. Those session-local counters describe tool activity, not repository
history, and keeping them as a second hidden ranking authority would make results
neither stable nor explainable.

## Component boundary

`lattice-core::git_intelligence` owns pure, deterministic aggregation. It accepts
newest-first `CommitSample` values and returns a `GitIntelligenceSnapshot`; it has
no process, filesystem, database, clock, or Git dependency. Fixtures can therefore
prove the same input produces the same ordered output on every platform.

A thin `git2` adapter owns repository discovery, reachable-commit traversal, and
first-parent tree diffing. It intentionally collects file paths only: it does
not parse historical blobs, resolve semantic renames, or apply mailmap
normalization. A graph store adapter owns schema creation, commit reuse,
snapshot publication, and reads. The daemon coordinates those adapters and
schedules refreshes. Retrieval, `impact`, and hooks only read the published
snapshot; they never traverse Git on a request path.

The adapter records only data required for aggregation:

- immutable commit object id and its bounded traversal position;
- commit subject and the author identity exposed by Git; and
- canonical repository-relative changed paths and change kind.

Commit bodies, raw blobs, arbitrary diff content, and historical symbol
observations are not persisted. The adapter's author string is an attribution
signal only; it is not mailmap-normalized and is not an ownership declaration.

## Signal semantics

- A file or symbol hotspot is the number of distinct commits in the active window
  that touched it. Repeated hunks, rename deltas, and duplicate adapter entries in
  one commit count once.
- A file co-change count is the number of distinct commits in which both canonical
  paths changed. Pairs are unordered and stored in lexical order.
- Bug-fix density is the number of fix-shaped subjects divided by all included
  commits touching the file. It is persisted as integer per-mille, avoiding
  floating-point drift.
- Author count is the number of distinct known identities in the active window.
  Bus factor is the fewest known contributors accounting for a strict majority of
  attributed touches. It is absent if any relevant commit has unknown authorship.
- The adapter does not enable similarity-based rename detection. A Git rename
  therefore contributes the old and new paths reported by the tree diff; this
  is an intentional file-history limitation, not a claim of semantic lineage.

The subject classifier uses the small, case-insensitive vocabulary `fix`, `bug`,
`hotfix`, `patch`, and `regression` with token boundaries. It is deliberately an
explainable heuristic, not an issue-tracker or defect prediction system.

## Persistence model

The graph database stores normalized evidence separately from the published
snapshot. Exact table names may follow graph-store conventions, but the following
keys and constraints are contractual:

| Record | Key | Required contents |
| --- | --- | --- |
| repository state | workspace identity | active generation, resolved head OID, limits/version, refreshed time, freshness and completeness |
| commit key | repository identity + commit OID | immutable identity for window membership and replay-safe cleanup |
| file observation | repository identity + commit OID + path | change kind and optional rename source |
| symbol observation | repository identity + commit OID + stable symbol key | canonical current path |
| window membership | generation + ordinal | commit OID, newest first |
| file/symbol aggregate | generation + stable key | hotspot, fix, and attribution fields |
| co-change aggregate | generation + ordered path pair | distinct commit count |

Foreign keys cascade from commit evidence and generation membership. Unique keys
make replaying the same commit idempotent. All stored paths pass the same workspace
path canonicalizer used by indexing; absolute paths, `..` traversal, NULs, and
paths outside the repository are rejected before persistence.

The aggregation version and effective limits are part of repository state. A
version or limit change forces reaggregation of the bounded window even when
`HEAD` is unchanged. A force-push, branch switch, rebase, or history truncation
replaces window membership with the commits reachable from the newly resolved
head. Commit evidence may be reused by OID, but unreachable evidence must not
remain visible through the active generation.

Generation rows make completeness explicit. `complete` means every eligible
commit in the selected window was aggregated within the declared limits.
`degraded` includes the precise exclusion counters and reason. `stale` identifies
the last successfully published head after a refresh failure. No consumer may
infer freshness solely from the presence of aggregate rows.

## Refresh and publication transaction

The runtime opens the Git-intelligence store with the workspace and hydrates the
read handle from its active generation, if one exists. It does not mine during
open. The watcher schedules refresh after it observes the owned checkout's
`HEAD`; bursts are latest-wins and share the index-work coordinator, so at most
one refresh per workspace runs. Ordinary working-tree edits and sibling-worktree
control files do not cause mining.

Refresh has two phases:

1. Outside the database write transaction, traverse the bounded commit window
   from the requested checkout head and deterministically aggregate a candidate
   generation. The current runtime performs a bounded read on each refresh;
   commit keys are reused only for persisted membership and cleanup, not as a
   shortcut around diff extraction. A latest-wins request superseding the worker
   causes its candidate to be discarded instead of publishing mixed history.
2. In one SQLite `BEGIN IMMEDIATE` transaction, upsert immutable commit evidence,
   insert the candidate membership and aggregates, mark its completeness, switch
   the repository state's active-generation pointer, and retire the prior
   generation. Commit publication is all-or-nothing. Orphan evidence and retired
   generations are garbage-collected only after the new pointer is durable.

If traversal, serialization, or SQLite work fails, publication is skipped and the
previous generation remains in the store; the read handle is marked `stale` by
the watcher until a later refresh succeeds. A repository with no commits
publishes an explicit complete empty generation. A missing or unreadable
repository has no fabricated zero-valued snapshot: consumers expose
`availability: unavailable` when no generation can be hydrated.

Daemon shutdown may cancel mining before publication. Publication itself is a
short, non-cancellable transaction. Refresh is replay-safe: processing the same
head twice produces the same active aggregates and does not double-count commits.

## Resource bounds

All bounds are enforced before allocation grows with untrusted repository history:

| Resource | Default / hard behavior |
| --- | --- |
| history window | 500 commits; operator values are clamped to `0..=500`; zero explicitly disables mining |
| changed paths per commit | 20,000 normalized paths; an over-wide commit is recorded as excluded rather than partially counted |
| symbols per commit | not mined by the Git adapter; symbol rows are retained only for pure-miner fixtures/API compatibility and are empty for runtime history |
| co-change width | commits touching more than 256 files contribute hotspots but no co-change pairs and increment an exclusion counter |
| unique co-change pairs | 250,000 per generation; overflow makes co-change unavailable for the entire generation rather than publishing a biased partial set |
| adapter work | bounded by the commit/path/co-change limits and serialized through the index-work coordinator; failures preserve the prior generation as stale |
| retained generations | active plus one prior generation until successful publication cleanup |

Input is normalized and deduplicated before applying a bound. Ordered maps and
lexical tie-breaks make output byte-stable. Arithmetic uses checked integer
operations and saturating presentation conversions. Limits and exclusion counters
are exposed in status and response metadata so callers can distinguish a complete
absence of signal from bounded omission.

## Consumer contracts

### Retrieval ranking

Retrieval may add a bounded history feature only after semantic relevance,
workspace eligibility, and dependency reachability have selected candidates. File
hotspot, exact stable-symbol hotspot, and bug-fix density are normalized against
the active window; they cannot admit an otherwise ineligible result or outweigh
the primary relevance score. Equal scores use the existing deterministic path,
symbol, and line tie-breaks.

Every history contribution included in a result explanation names the signal,
observed count or per-mille ratio, window size, and snapshot head. If the snapshot
is unavailable, stale, or degraded, ranking omits the feature while metadata
reports the corresponding availability. Author identities and bus-factor values
are not used as relevance boosts.

### `impact`

Within each existing graph-distance and impact-severity tier, `impact` orders
dependents by descending symbol hotspot, then file hotspot, then the existing
stable tie-break. History never invents a dependency.

`impact` also emits a separate `missing_cochange_partners` section: paths that
co-changed with a current-diff path but are absent from that diff. Entries include
the source diff path, partner path, distinct commit count, window size, and head
OID. They are ordered by count descending and paths ascending, require at least two
supporting commits, are deduplicated, and are capped at ten. The section is omitted
with explicit completeness metadata when co-change is unavailable; no graph edge
is persisted from this advisory relationship.

### PostToolUse hook

The hook passes successfully edited, workspace-scoped paths to one bounded
`impact` request. The MCP result includes at most five file warnings when a
path's hotspot is at or above the computed top-decile cutoff among nonzero file
hotspots. The integration scripts extract those marked warnings into one
non-blocking `Git History Warning` section. They emit nothing for failed tool
operations, paths outside the workspace, unavailable/stale/degraded history, or
repositories without a cutoff. Hook failure remains best-effort and exits `0`
quickly when the daemon is unavailable.

## Removal of pseudo-hotspots

Landing the history-backed consumers requires deleting, in the same implementation
change, every session-local authority that can be mistaken for Git history:

- `ChangeTracker.edit_counts`, `get_hotspot_score`, watcher-batch co-change state,
  their tests, and ranking call sites in `intelligence/mod.rs`;
- `Node.edit_count`, its graph-store column and migration, and MCP fields derived
  from it unless a field is explicitly redefined to read Git intelligence; and
- comments, docs, fixtures, and response metadata that describe watcher save counts
  as hotspots or co-change evidence.

Session change chronology may remain for thrashing and dead-end diagnostics only
if it is named and surfaced as session-local behavior. Watcher edit events remain
valid for invalidation and adoption telemetry. Neither may feed repository-history
ranking, `impact`, or hotspot warnings. There is no compatibility alias or fallback
from a missing Git snapshot to `edit_count`.

## Observability and recovery

MCP workflow payloads expose the read-handle generation and compact metadata:
availability, sampled window size, and head commit id. The report retained in
`graph.db` carries aggregation limits and exclusion counters for diagnostics.
Consumers suppress history whenever the handle is not `available`, so a missing
or stale generation cannot be mistaken for zero risk.

The runtime validates the store while opening a workspace and publishes an
existing generation as stale until a refresh succeeds. Store-open or generation
validation errors fail runtime construction rather than silently creating an
empty snapshot. A failed refresh leaves the prior generation untouched; the
next checkout-head event retries the same bounded path. There is no unbounded
repair mode or request-time Git fallback.

## Acceptance tests

The implementation is complete only when the following layers are covered:

1. Pure miner fixtures prove stable ordering, commit/path/symbol deduplication,
   unsafe-path rejection, rename semantics, bug-fix token boundaries, unknown
   authorship, integer ratios, history limits, wide-commit behavior, co-change
   overflow behavior, and identical snapshots for replayed input.
2. `git2` adapter tests build temporary repositories covering an initial commit,
   merge history, rename, deleted file, mailmap identity, shallow history, detached
   head, unborn head, and a commit exceeding each adapter bound.
3. Graph-store tests prove commit-OID idempotence, generation isolation, atomic
   pointer swap, rollback preservation, version-triggered reaggregation, orphan
   cleanup, empty repository publication, and recovery from corrupt derived rows.
4. Watcher/coordinator tests prove owned-ref events are coalesced, ordinary edits
   and sibling-worktree refs do not mine history, a head change during mining
   cannot publish mixed evidence, and a queued retry converges on the latest head.
5. Retrieval tests prove history is a bounded secondary feature, explanations cite
   the correct snapshot, and stale/degraded history has no ranking effect.
6. `impact` tests prove hotspot ordering within an existing severity tier and the
   deterministic, deduplicated, ten-entry list of co-change partners missing from
   the diff.
7. Hook tests prove top-decile boundaries, the five-file output cap, no warning on
   failed edits or incomplete snapshots, workspace rejection, and best-effort exit
   behavior when the daemon is unavailable.
8. Migration and contract tests prove all watcher `edit_count`/pseudo-co-change
   authorities are absent from runtime schemas, ranking, MCP responses, docs, and
   fixtures. A full `cargo test --workspace` run remains green.

Performance fixtures must additionally demonstrate that the 500-commit refresh
respects the declared allocation and deadline bounds on a large repository and
that steady-state retrieval, `impact`, and hook reads perform no Git traversal.
