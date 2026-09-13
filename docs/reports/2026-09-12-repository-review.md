# Lattice repository review — 2026-09-12

**Reviewed revision:** `70f50ec` on `master`, including the working-tree state present during review.
**Outcome:** redesign storage lifecycle and memory delivery around repository-owned services, bounded disposable caches, and evidence-backed lessons.
**Implementation plan:** [Repository storage and agent memory redesign](../plans/2026-09-12-storage-and-agent-memory-redesign.md).

This is a review and proposed implementation program, not a claim that the redesign has shipped. No runtime code, retained memory, or cache was changed by the review. Existing user changes in client configuration, extension files, installation scripts, and the health plan were left intact. Tests created their normal temporary/build artifacts. Initial CLI probes contacted the daemon and could initiate indexing; subsequent database inspection used SQLite read-only connections.

## Assessment

Lattice has useful foundations: real Git-worktree identity tests, a shared parsed-file cache, scoped memory routing, bounded query responses, authenticated hook sessions, atomic graph publication machinery, recovery tests, and a substantial passing Rust suite. Replacing all of this with a new implementation would discard valuable contracts.

The principal architectural problem is that **sharing identity and parsing is not the same as sharing storage or managing its lifetime**. Every checkout still owns substantial materialized state; parsed content versions have no eviction; full graph persistence rewrites the graph; retained historical stores coexist with the new layout. Resource control limits resident shard count but does not establish a repository disk budget.

The principal memory problem is more serious than ranking alone. The retained database contains mostly generated navigation summaries, not verified lessons; most historical records have identities that no longer match the current repository. Hooks narrow the candidate set before considering the task and omit the trust metadata that the router computes. This cannot yet support a strong claim that memory reduces repeated mistakes.

The user's required lifecycle is also explicit: **memories that are not recalled must become stale and then be purged**. The linked plan includes recall-based expiry, actual delivery attribution, physical payload reclamation, and protection against resurrection from old captures or snapshots. Its 90-day stale / 180-day purge thresholds are proposed configurable defaults, not current behavior or user-specified durations.

## Evidence and limits

Reviewed the CLI/proxy/shard runtime, identity and storage bootstrap, parser/indexer/watcher paths, graph/vector persistence, events and snapshots, memory capture/routing/verification/consolidation, hook presentation and attribution, workflow composition, benchmark harness, CI configuration, and architecture/operator documentation. This was a repository-wide architectural and defect review, not an exhaustive proof of every function or a penetration test. Findings below distinguish retained-state observations from code-derived risks.

Other repositories' `.lattice` folders had already been deleted. Their former total size, checkout counts, WAL growth, and data composition cannot be reconstructed from this checkout. Lattice's own retained state remained available. A live daemon may have been writing during inspection; separate databases were not read as one globally atomic snapshot. No historical multi-worktree storage benchmark was available from this inspection.

### Disk snapshot

Initial `du` measurements are approximate allocated sizes, not a post-cleanup inventory:

| Component | Observed size | Interpretation |
|---|---:|---|
| `daemon/target` | 41 GiB | Rust artifacts, distinct from Lattice runtime data |
| `daemon/target/debug/incremental` | 15 GiB | Incremental compilation cache |
| `daemon/target/debug/deps` | 23 GiB | Debug dependencies and executables |
| Main `.lattice` | 289 MiB | Current and historical runtime stores |
| `.lattice/checkouts` | 128 MiB | Two opaque checkout namespaces; only one worktree was listed by Git |
| Root `.lattice/graph.db` | 58.6 MiB logical file size | Historical root layout retained beside checkout layout |
| `.lattice/parsed-cache.db` | 43.2 MiB logical file size | 717 content-addressed entries, no free SQLite pages |
| `.lattice/snapshots` | 15 MiB | Historical snapshot directory |
| `daemon/.lattice` | 18 MiB | Additional nested local state; ownership not established |
| Historical build-plan logs | 360 MiB | Agent-run logs under `docs/plans`, distinct from indexing data |
| User-level model cache | 87 MiB | Already shared at user level |

The two checkout graph databases contained 7,451 and 21,628 nodes. The larger graph file was 80.0 MiB, including about 7.8 MiB of free pages. Every inspected `vectors.db` contained **zero vectors**. Embedding duplication is therefore an architectural scaling concern, not the measured cause of this checkout's footprint. The retained root `events.db` had 187 events and no spilled payloads; event spill leakage below is likewise a code finding, not the measured local cause.

### Memory snapshot

Read-only aggregates from `.lattice/memories.db`:

| Measurement | Value |
|---|---:|
| Memories | 73 |
| Verification status | 69 `in_review`, 4 `unverified`, 0 verified |
| Memory class | 69 patterns, 3 observations, 1 workflow outcome |
| Scope | 70 branch (`master`), 3 session, 0 repository, 0 organization |
| Stored repository identity | 18 checkout-path IDs, 53 Git-directory-path IDs, 2 current hashed IDs |
| Structured memory-access rows | 0 |
| Sum of older `access_count` counters | 7 |
| Consolidation jobs / proposals | 0 / 0 |
| Verification jobs | 3, all queued |
| Session digest deliveries / capture commits | 2 / 1 |

Sample pattern records begin with “Subsystem playbook” and describe ranked entry points for prior queries. The recent workflow outcome says that a session edited two files. These are observations of this database, not assumptions about all users. Empty access/consolidation tables do not prove that no historical memory was ever useful: older counters exist, and other telemetry may use separate stores. They do demonstrate that this database does not substantiate an established verified learning loop.

## Findings, ordered by priority

### R1 — P0: transient memory-open errors enter destructive recovery

`daemon/crates/lattice-daemon/src/main.rs:1184` routes **any** persistent open error through quarantine and fresh database creation. `main.rs:1231` and `main.rs:1246` move the DB/WAL/SHM files individually. There is no corruption classification or repository-wide exclusive recovery lease at this boundary. Failure to recover falls back to in-memory storage.

A lock timeout, failed migration, or access error must not replace durable authority. With multiple shards, a still-open connection can retain the old database while another shard opens the replacement, fragmenting future writes. This is a code-derived failure scenario; no such race was induced on the user's data. Quarantining files preserves artifacts but does not preserve continuous memory availability or guarantee a coherent live SQLite backup.

**Required change:** typed open failures, bounded contention retry, one repository memory owner, offline/exclusive corruption recovery, explicit degraded durability, and refusal to acknowledge a durable write to an ephemeral fallback.

### R2 — P0: historical memory identity migration is incomplete in retained data

`daemon/crates/lattice-daemon/src/workspace_identity.rs:40` derives `RepositoryId` from the canonical common-Git-directory hash. The current ID was verified independently against that path. Only two retained records use it. `daemon/crates/lattice-core/src/memory/store.rs:1155` applies authority in SQL; `verification/scope_enforcement.rs:108` requires matching repository IDs for branch/repository records.

There are **68 branch memories with older IDs**. Those records cannot satisfy the current repository scope even while the checkout remains on `master`. Another three old-ID records are session-scoped; their applicability depends on session identity. The inspected startup path does not repair this retained state.

**Required change:** a one-time, auditable identity migration using proven Git ownership, preserving record IDs and branch/session restrictions. Unknown provenance stays isolated for explicit resolution. Do not weaken runtime scope predicates or keep path aliases as parallel authority.

### R3 — P1: no complete lifecycle or byte budget for checkout caches

`workspace_identity.rs:67` creates checkout namespaces. `socket_server.rs:1120` evicts idle runtimes and shuts them down without reclaiming their disk directories. `storage/parsed_file_cache.rs:13` and `:145` create/upsert content versions without expiry, access accounting, reference tracking, or pruning.

Checkout churn accumulates directories; file edits accumulate parse versions. An idle-shard cap does not bound either. Two checkout directories with one listed worktree are evidence of unmatched retained state, not sufficient proof that either directory is safe to delete: the current ID also depends on the requested root, including subdirectory workspaces.

**Required change:** registered checkout ownership, activity leases, byte accounting, configurable cache budgets, resumable GC, and explicit separation of durable memory from reclaimable data.

### R4 — P1: checkout materialization and full graph saves amplify writes

`main.rs:594` and `:603` open checkout-owned event and graph stores; `main.rs:1144` selects checkout-owned vector storage. `main.rs:638` opens Git-history facts in that same graph database. `storage/graph_store.rs:346` regenerates digests, deletes all graph nodes/edges/digests, and reinserts the entire graph. `lattice-daemon/src/watcher.rs:644` calls that path when publishing watcher updates.

This preserves transactionality, but a small source change can generate whole-graph writes, WAL traffic, and duplicate bodies across checkouts. Git-history facts tied to identical commit inputs are duplicated too. The live footprint confirms substantial node bodies, edge indexes, digests, and co-change data; it does not establish the write-amplification ratio.

**Required change:** shared immutable content and fact objects, checkout membership/overlay manifests, and transactional graph deltas. Do not share one mutable graph across divergent worktrees. Embedding cache keys must include the full embedding-input and model identity, not merely source bytes.

### R5 — P1: event compaction leaves spilled payload rows and duplicates snapshots

`events/store.rs:352` deletes events but never deletes newly unreferenced `event_payloads`. The schema's foreign key points from events to payloads; deleting a child does not delete its parent. Repository-wide search found no payload deletion path. Large payload rows can therefore outlive their events indefinitely.

`events/compaction.rs:99` retains five snapshots by default. `:147` writes the graph and memory into each snapshot; `events/snapshot.rs:100` and `:240` capture those states. As checkout stores multiply, so can full graph snapshots and copies of repository memory. Count retention is not a byte budget. SQLite row deletion also does not necessarily return allocated file space to the filesystem.

**Required change:** transactional orphan-payload collection, tested physical reclamation, byte-bounded transient event retention, and repository-owned memory checkpoints separate from rebuildable graph caches. Preserve evidence/replay dependencies before deleting any historical event stream.

### R6 — P1: hooks retrieve recent candidates before matching the task

`hook_session_route.rs:618` calls `recall(None, 64)`. `memory/router.rs:385` caps per-store oversampling at 64; `memory/store.rs:1195` orders a query without terms by creation time. Prompt/path relevance is computed afterward at `hook_session_route.rs:839`.

An older exact-path lesson can never win if it falls outside that initial window. Lifecycle filtering also happens after the store limit in the router, so recent ineligible rows can crowd out eligible knowledge. Increasing 64 delays the problem without solving retrieval.

**Required change:** authority- and lifecycle-filtered indexed candidate queries for exact path/symbol, failure signature, applicable constraints, and lexical relevance; bounded merging afterward. Add fixtures where the correct lesson is older than thousands of irrelevant records.

### R7 — P1: hook rendering strips trust; verification is not proof of correctness

The router exposes effective verification and cross-repository advisory reasons at `memory/router.rs:470`. `hook_session_route.rs:935` renders IDs, classes, and content, omitting those trust signals. `memory/router.rs:593` allows unverified and in-review records, although stale/contradicted/superseded records are excluded. A post-tool path match can consequently render a forceful warning without showing its evidentiary limits.

`verification/existence.rs:226` checks file, symbol, doc-section, and test existence, optionally validating exact spans. Unsupported evidence kinds are skipped. `:645` aggregates existence outcomes; this does not execute a linked test or establish that the claim is correct. No evidence plus a passing scope verdict can also lead to an all-verified existence result. This distinction must survive the public response and promotion logic.

**Required change:** separate evidence freshness from behavioral validation, preserve uncertainty in every renderer, reject unsupported/no-evidence promotion to behavioral proof, and bind checks to the querying checkout's content generation.

### R8 — P1: automatic navigation summaries consume the durable memory channel

`rpc/mcp.rs:4329` automatically upserts a subsystem playbook after computing a summary. `:5300` stores/refreshed patterns with confidence `0.95`; branch scope is preferred. The content formatter at `:11335` describes ranked files and symbols, rather than an observed failure and validated correction.

The retained 69 in-review pattern records are consistent with this path. A branch-scoped generated summary will not teach agents on unrelated feature branches, and a query-derived confidence constant is not evidence. Automatically widening all these records to repository scope would make the noise more pervasive.

**Required change:** cache generated navigation summaries as derived context; reserve durable learning for decisions, constraints, procedures, and failure/correction records with provenance and applicability. Promote only the reusable assertion, not the entire session or branch transcript.

### R9 — P1: index traversal follows symlinks without proving workspace containment

`runtime_support.rs:545` recurses using `path.is_dir()` and later reads candidates after `fs::metadata`; both follow symlinks. It checks the textual repository-relative path, not the canonical target. `indexer/mod.rs:551` has a separate recursive scanner with the same traversal shape. Neither inspected recursion tracks visited directory identities. A symlink to an external source tree or an ancestor can introduce out-of-bound reads or repeated traversal. The runtime filter also loads only root ignore files (`security/mod.rs:16`), rather than implementing hierarchical ignore discovery.

This is source-confirmed behavior, not a live exploit test. Default excluded directory names do not establish a workspace boundary. Some filters are also overbroad (for example `lib`) and can omit genuine source.

**Required change:** one audited traversal/reading policy for startup, reindex, watcher, and verification. Honor nested ignores; use explicit symlink policy and canonical containment; prevent cycles; surface unreadable paths as coverage gaps. Test these paths end to end rather than only testing filter predicates.

### R10 — P2: metrics perform ledger-wide work on the hook path

`adoption_metrics.rs:565` reads the complete JSONL ledger to deduplicate each exact-ID event while holding an exclusive file lock. `:648` compacts by reading and rewriting retained events. A new `AdoptionMetricsStore` has fresh compaction state (`:317`); hooks construct one per presentation (`hook_session_route.rs:583`). The once-per-instance/day optimization therefore does not guarantee once-per-day work across hook requests.

**Required change:** indexed uniqueness and bounded retention in a repository telemetry store, separate from the critical presentation transaction. Preserve replay-safe attribution, but do not make an unavailable metrics ledger suppress useful memory delivery.

### R11 — P2: utility evaluation does not measure mistake prevention

`tools/lattice-worth-it-benchmark.sh:52` defines five read-only tasks: implementation lookup, blast radius, diagnosis, decision recall, and health risk. The harness validates runner output and evidence vocabulary; its tests are useful contract tests. It does not run a learning sequence where a first agent's validated correction prevents a second agent's mistake on a fresh branch.

The tracked CI workflow runs Rust tests/builds but does not run hook or worth-it harness tests. It also expects an extension directory that is untracked in this checkout and masks extension lint failure with `|| true`. A passing local Rust suite is therefore not a complete clean-checkout release signal.

**Required change:** clean-checkout CI plus longitudinal, independently judged editing tasks. Track correct changes, recurrence, misleading memory, tokens, and latency, separately from tool calls or explicit use claims.

### R12 — P1: existing inactivity cleanup archives payloads and runs per shard

`memory/store.rs:2453` decays old memories' confidence by an amount per invocation. `:2472` calls its operation “prune (archive)”: it removes FTS rows and sets `is_invalidated`, but leaves the memory payload in the database. `main.rs:854` starts an hourly loop in each workspace runtime, calling decay for seven-day-old access timestamps and pruning low-confidence records older than 30 days. Both errors are converted to zero affected rows. With multiple checkout runtimes sharing `memories.db`, the same records can be decayed multiple times per interval. This is not a deterministic repository-owned stale/purge clock.

`verification/expiry.rs:62` also handles explicit time-bound expiry through proposals; it does not implement inactivity-based physical deletion. The existing pieces therefore do not satisfy the requested recall-based lifecycle. Old generic access counters cannot be assumed equivalent to successful delivery to an agent.

**Required change:** replace per-shard confidence decay/archive authority with one repository-owned retention scheduler, explicit last-recall timestamps, separate retention/evidence staleness, bounded physical purge, and visible failures. Include multi-worktree tests proving that the number of shards cannot accelerate expiry.

## Additional design constraints

- Keep the eight public retrieval/workflow verbs. Storage inspection/maintenance belongs in deliberate CLI/operator contracts and additive `status` information, not a proliferation of MCP tools.
- Unify policy at the service layer before splitting files. `rpc/mcp.rs` is 16,280 lines and `memory/store.rs` is 4,637 lines, but file length alone is not a defect. The material problem is multiple capture/retrieval/rendering paths making different trust and persistence decisions.
- Preserve the current protections: organization authority from trusted configuration, checkout-local verification, exact replay IDs, bounded response shaping, and explicit partial/indexing state.
- Shared storage must also handle common Git directories not named `.git`. `workspace_identity.rs:113` falls back to the checkout top level for unusual/bare layouts, which can give identical repository IDs different physical storage homes. Include real fixtures in the identity redesign.
- Rust build artifacts and historical agent logs need their own retention policy. Lattice cache GC must never start deleting arbitrary `target` directories or run logs as if it owns them.

## Verification performed

`cargo test --workspace --offline` passed with local socket access: **1,781 passed, 39 ignored, zero failed** (core 1,078; daemon library 269; daemon binary 427; CLI integration 7). Doc-test targets contained zero tests. The first sandboxed attempt failed six local-server tests on socket permission errors; all six passed with socket access.

`bash integrations/codex/tests/hooks_test.sh` passed its protected Codex/Claude hook package checks. `bash tools/tests/worth-it-benchmark_test.sh` passed **20 checks**. These are harness tests, not an actual paired agent efficacy run. Ignored benchmarks and cross-platform CI were not run. No live storage was deleted, migrated, compacted, or deliberately corrupted.

The redesign's new regressions and resource/agent acceptance gates are specified in the linked implementation plan. Existing passing tests do not cover away the findings above.
