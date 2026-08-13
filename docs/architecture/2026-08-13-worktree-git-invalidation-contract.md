# Worktree Git Invalidation Contract

**Status:** binding implementation contract for recovery-workplan C2
**Depends on:** [worktree storage architecture](2026-08-12-worktree-storage-architecture.md)

## Decision

A Git metadata event is not a source-file change. It may cause a full
workspace invalidation only after Lattice proves that the watched checkout's
resolved `HEAD` changed. Metadata belonging to a sibling checkout must have no
effect on this checkout's epoch, graph, index work, or readiness state.

The relevant identity is the **checkout**, not the common Git directory. A
primary checkout and all linked worktrees have one common Git directory, but
each has an individual Git directory and an individual `HEAD`. C1's resolved
workspace identity must therefore expose these canonical, validated paths:

- `checkout_root` — source-content boundary;
- `checkout_git_dir` — the primary `.git` directory or a linked worktree's
  target from its `.git` pointer file;
- `common_git_dir` — shared refs/object store; and
- `repository_id` / `checkout_id` — as defined by C1.

The watcher must receive that object; it must not rediscover `.git` or infer a
worktree relationship from string prefixes. All path comparisons use
canonical paths and component-aware containment, never substring matching.

## Checkout state

Maintain one mutex-protected `ObservedCheckoutHead` per watcher:

```text
ObservedCheckoutHead {
  ref_name: Option<RefName>,       // None means detached or unborn
  target: HeadTarget,              // Unborn | Resolved(ObjectId)
  observed_at: Instant,
}
```

`HeadTarget` is read with `git2` from the repository opened at
`checkout_root`. For a symbolic `HEAD`, resolve through the repository's ref
database, including packed refs. For a detached `HEAD`, use the direct object
id. An unborn branch is an explicit state, not an error. The `ref_name` and
the resolved object id are both part of equality: moving from one ref to
another at the same commit is still a checkout transition.

The initial state is captured only after watches have been installed. It is
the baseline for future decisions; watcher startup itself never invalidates a
workspace. After every successful comparison, replace the baseline with the
new state, including when it is equal.

If Git cannot yield a coherent state during a transition (for example, while
rebase metadata is being rewritten), retain the last successful baseline and
schedule bounded retries with debounce. Do not claim a full invalidation on
the uncertain event. Once a state can be read, compare it with that baseline.
If retry is exhausted, publish actionable watcher health (`git_state=unknown`,
last error, retry count) and retry on a later owned Git event. It is never
permitted to turn uncertainty or an unrelated ref update into a whole-workspace
epoch bump.

## Event classification and decision

First deduplicate the debounced event paths by canonical path. Classify each
path before applying any batch-size rule:

| Event kind | Classification | Action |
| --- | --- | --- |
| Indexable regular file beneath `checkout_root` | `Source` | Queue incremental upsert/removal. |
| Twenty or more distinct `Source` paths in one debounce window | `SourceStorm` | Preserve existing full-workspace invalidation behaviour. |
| `<common_git_dir>/worktrees/**` (including a primary checkout's `.git/worktrees/**`) | `SiblingWorktreeState` | Ignore completely: do not read HEAD, do not enqueue index work, do not advance the epoch. |
| A ref not equal to this checkout's currently resolved symbolic ref | `UnrelatedRef` | Ignore. |
| The checkout's own `HEAD`, its active ref, `packed-refs`, `ORIG_HEAD`, merge/rebase state, or a configured per-worktree control path | `OwnedGitState` | Coalesce and compare `ObservedCheckoutHead`; invalidate only if unequal. |
| `.git/index`, reflogs, object packs, config, hooks, lock files, or any other Git path not listed above | `IgnoredGitState` | Ignore for graph invalidation and indexing. |
| Outside the checkout boundary | `OutsideWorkspace` | Ignore and log only at trace level. |

An `OwnedGitState` event is a request to *observe*, not evidence that the
checkout changed. After debounce, read `new_head` and apply this exact rule:

```text
head_changed = new_head != last_successful_head
invalidate_workspace = source_path_count >= 20 || head_changed
```

The full invalidation happens at most once per coalesced batch. A batch may
also contain normal source updates; those are prepared and indexed normally.
Git-only batches whose head is unchanged must short-circuit before acquiring
the index-work permit. The source-storm count excludes every Git path, so a
burst of worktree administration events can never cross the threshold.

This intentionally handles the required transitions:

- branch switch: symbolic ref name changes, even if its object id happens to
  match the former branch;
- `reset`, `rebase`, or forced current-ref movement: target object id changes;
- detached-HEAD checkout: ref/target representation changes; and
- rebase completion after transient unreadability: the settled target differs
  from the retained baseline.

It intentionally does **not** invalidate for staging, committing without a
checkout-content transition, a sibling worktree commit, remote-tracking ref
updates, reflog updates, or unrelated local branch updates. Source events
remain sufficient to incrementally index edits made before a normal commit.

## Watch topology

The watcher has two explicit topologies:

1. Recursively watch `checkout_root` for source changes. Events under Git
   metadata are routed through the classifier above; this is necessary for a
   primary checkout whose `.git` lives below its root.
2. Add narrow, non-recursive watches for the linked checkout's own control
   paths that lie outside `checkout_root`: its private `HEAD`, merge/rebase
   metadata, and its currently resolved symbolic-ref file (if loose), plus the
   common `packed-refs` file. Reconfigure the active-ref watch after every
   successful head transition.

Never recursively watch `common_git_dir`, `common_git_dir/refs`, or
`common_git_dir/worktrees`. An exact watch of the current symbolic ref is
allowed so that a reset of this checkout is observable; a watch of a ref
directory is not. In the primary checkout, events under `.git/worktrees/**`
are still ignored before state observation. In a linked checkout, the target
of `.git` is its private directory under `common_git_dir/worktrees/<name>`;
that exact directory is owned by this watcher, while sibling directories are
not.

All watch registrations are tracked with their purpose and canonical target.
On a failed registration, preserve source watching, report the missing Git
control watch in watcher health, and use the next observed source/owned event
to refresh state. Do not broaden to a recursive common-directory watch as a
fallback.

## Implementation boundary

Replace the boolean `is_git_state_path` / `should_invalidate_workspace`
combination with a pure classifier plus a state comparator. The pure portion
accepts `WorkspaceIdentity`, the last observed head, and normalized paths, and
returns a `ChangeBatch` containing `source_paths`, `owned_git_event`, ignored
counts by reason, and `requires_head_observation`. The async watcher owns the
Git read, retry schedule, epoch mutation, and watch reconfiguration.

`prepare_change_batch` accepts only `source_paths`; it must never receive a
Git path. This prevents accidental parsing or metrics attribution of metadata.
The existing >=20 source batch policy remains unchanged for actual checkout
file storms. `indexing` is set only after the batch has a source operation or
a proven head transition, and cleared through the existing completion/error
path.

Expose bounded counters in index status and tracing: owned Git observations,
head transitions, unchanged owned observations, ignored sibling-worktree
events, ignored unrelated-ref events, and Git-state read failures. These are
diagnostics, not adoption metrics. They make a purported reindex storm
auditable without retaining unbounded event paths.

## Required verification

Use a fake head reader and fake watch backend for pure/unit coverage; use
`git2` fixture repositories for integration coverage, not shell Git commands.

1. Classifier tests prove `.git/worktrees/sibling/HEAD`, sibling index files,
   and twenty such paths produce no head read, no source paths, and no epoch
   change.
2. An unrelated branch-ref update produces no invalidation. A `packed-refs`
   event with an unchanged resolved current head also produces no invalidation.
3. A current checkout branch switch invalidates exactly once; switching to a
   different ref at the same object id is covered.
4. Current-ref reset, detached checkout, and a rebase state that is initially
   unreadable then settles at a different object id each invalidate exactly
   once. An unreadable state that never settles reports health and does not
   manufacture an epoch.
5. A >=20-source-file batch still invalidates once and submits one index batch;
   mixed source plus ignored sibling metadata preserves that count and submits
   no metadata paths.
6. Topology tests assert a linked worktree registers its private `HEAD` and
   exact active-ref/`packed-refs` watches, never a recursive common Git
   directory or a sibling worktree path. A ref change re-arms the exact ref
   watch.
7. A fixture primary repository plus a linked worktree sends the watcher the
   metadata events from a sibling commit. Assert zero workspace epoch changes,
   zero graph rebuilds, and zero index-work acquisitions in the primary
   watcher. Then perform an own branch switch/reset and assert one epoch and
   one reindex cycle. Record both counts in the test assertion, not only the
   final graph shape.

The C2 acceptance result is a real linked-worktree regression test proving a
sibling commit causes zero full reindexes of the main checkout, alongside the
unit transition matrix above. C3 may later replace the full graph rebuild
mechanism; it must preserve this invalidation decision contract.

## C2 acceptance evidence

The daemon-owned watcher tests implement this acceptance surface without
invoking a shell `git` executable. Their fixture uses `git2` to initialize a
repository, create same-target `main` and `feature` branches, and add a real
linked `sibling` worktree. The test then passes the actual private-worktree
`HEAD` path and shared sibling branch-ref path to the primary watcher.

`real_git2_linked_worktree_churn_does_not_invalidate_primary_checkout`
asserts all of the following with explicit counters:

- a sibling worktree commit/ref update leaves the primary epoch unchanged,
  leaves its graph snapshot unchanged, and acquires zero index-work permits;
- an unrelated local ref event and an unchanged `packed-refs` event remain
  no-ops;
- a same-object-id branch switch, an active-ref reset, and a detached-HEAD
  transition each advance exactly one epoch and complete exactly one index
  work cycle, even when the corresponding metadata event is delivered twice.

`git_head_rebase_retry_is_bounded_and_never_manufactures_an_epoch` makes the
primary `HEAD` temporarily unavailable during a synthetic rebase rewrite. It
settles on the single bounded retry and invalidates once when that settled HEAD
differs. A permanently unavailable `HEAD` performs exactly that one retry,
records `git_state=unknown`, its retry count, and an actionable reason in
watcher health, while leaving both epoch and index-work counts unchanged.

`mixed_source_and_sibling_worktree_metadata_uses_only_source_threshold`
combines nineteen indexable source files with more than twenty sibling
worktree metadata paths. It confirms the batch remains an incremental source
batch: exactly nineteen files are indexed, one ordinary index-work cycle runs,
and no workspace epoch is advanced.
