# Git Intelligence

## Purpose

Git intelligence provides history-derived signals that static dependency analysis and
session watcher activity cannot provide: file and symbol hotspots, file co-change,
known-author concentration, and bug-fix density. It is a ranking input and an
explanation signal; it must never be presented as a proof that a change is correct
or that a person owns code.

## Boundary

`lattice-core::git_intelligence` is a pure, deterministic aggregation boundary. A
thin adapter owns `git2` repository traversal and converts commits into
`CommitSample` values ordered newest-first. The miner has no process, filesystem,
database, or Git dependency. This makes its limits, fixtures, and results testable
without a live repository.

The adapter reads at most 500 commits by default. It records commit object ids, the
subject, an optional canonical author identity, changed repository-relative paths,
and parser-resolved stable symbols when historical blobs can be resolved. Commit
bodies are deliberately not persisted as intelligence input.

## Signals and semantics

- A hotspot is the number of distinct sampled commits that changed a file or symbol.
- A co-change pair is the number of distinct sampled commits that changed both
  repository-relative files. Pairs are unordered and canonicalized.
- Bug-fix density is fix-shaped subjects divided by all sampled commits touching a
  file, stored as an integer per-mille ratio for deterministic persistence.
- Author count describes distinct known Git identities in the sampled window. Bus
  factor is the fewest known contributors accounting for a strict majority of
  attributed commits; it is absent when any sampled commit has no author identity.
  Missing author data remains unknown (`None`), never zero-risk.

The subject classifier intentionally uses a small, explainable vocabulary (`fix`,
`bug`, `hotfix`, `patch`, and `regression`). It is a heuristic label, not an issue
tracker integration.

## Safety and bounds

The miner rejects absolute and parent-traversal paths, canonicalizes separators,
deduplicates paths and symbols within a commit, ignores replayed commit ids, and
caps input traversal. Results use ordered maps so equal input produces byte-stable
ordering. A top-decile hotspot cutoff is computed only from observed nonzero files.

## Integration contract

The graph-store adapter persists per-commit input/state keyed by immutable commit
id, so refreshes are idempotent and can incrementally process newly reachable
history. On watched Git-state changes it refreshes the bounded window, then exposes
additive history annotations to retrieval and `impact`. `impact` should rank
dependents by hotspot and show co-change partners absent from the current diff.
Post-edit hooks may warn only for files at or above the reported top-decile cutoff.

The old watcher edit-count hotspots are not a substitute for repository history and
must be removed when the history-backed call sites land; retaining both as silent
ranking inputs would make explanations dishonest.

## Verification

The module contains fixture-only unit tests for deterministic aggregation, bounds,
deduplication, unsafe path rejection, unknown authorship, bug-fix labeling, and
co-change counts. Adapter tests must separately exercise a temporary `git2`
repository, persistence idempotence, and refresh behavior.
