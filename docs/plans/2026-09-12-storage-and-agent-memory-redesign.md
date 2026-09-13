# Repository storage and agent memory redesign

**Date:** 2026-09-12
**Status:** Implementation and acceptance in progress; current evidence and open gates are recorded in the [execution tracker](2026-09-12-remediation-execution.md).
**Evidence:** [Repository review](../reports/2026-09-12-repository-review.md).

## Product contract

Lattice should help an agent identify the right code, understand the consequences of a change, and avoid a previously demonstrated mistake. Success is a correct patch with less investigation and less repetition. Stored memories, tool calls, and generated summaries are supporting measurements, not the outcome.

Retain the graph/query engine, public eight-verb interface, trusted authority boundaries, authenticated hooks, and useful contract tests. Replace the storage and learning paths that violate the contract. Do not add a second permanent runtime model beside the current one.

The implementation order is deliberate: protect memory and recover usable history; bound disk use; remove repeated materialization; repair learning and delivery; then prove value through agent tasks. The review supplies evidence; the steps below define the implementation and acceptance contract. The execution tracker distinguishes completed work from remaining gates.

**User requirement:** memories must become stale and be purged when they are not recalled. Durable storage means surviving crashes and checkout deletion; it does not mean retaining unused memory forever. This applies to learned repository and organization memory, including approved lessons. Permanent team rules belong in reviewed repository instruction files or executable checks, not an implicit exemption from memory expiry.

## Target architecture

```text
Local daemon
  Repository registry + resource coordinator
    Repository service (one per proven Git repository)
      Durable knowledge + evidence + migration journal
      Content-addressed parse, embedding, and history caches
      Checkout registrations, manifests, active leases, GC journal
      Bounded telemetry and transient events
      Checkout views
        content membership + changed-file overlay
        resolved graph edges / query indexes
        verification generation + handles + watcher cursor
  Organization knowledge service (separate configured authority)
```

“One repository service” means one owner inside the daemon with an explicit cross-process lock for migration/recovery. It does not mean holding a SQLite write transaction while indexing or running an external check. MCP proxies stay lightweight; inactive checkouts do not retain full materialized indexes merely because their directory exists.

### Future team service consideration

Team and multi-user support is a future enhancement, explicitly outside this
remediation's implementation scope. Keep SQLite for the local daemon and local
repository state. A future shared service should expose an authenticated API
with explicit organization, repository, and user authorization; clients must
not share a SQLite file over a network filesystem. Select the service database
against measured concurrency and operational requirements; PostgreSQL is a
candidate, not a dependency introduced by this program.

Preserve the separation between local disposable caches and authoritative
knowledge. A future design must define shared repository identity, membership
revocation, promotion permissions, conflict resolution, audit history, and
recall/expiry ownership before synchronizing knowledge. Matching Git remotes
does not establish sharing authority. Current organization-store support does
not claim remote multi-user security or synchronization.

### Storage authority and lifetime

| State | Authority / sharing | Lifetime |
|---|---|---|
| Approved lessons, decisions, evidence, promotion history | Repository durable store | Survives checkout removal/cache reset; expires without recall |
| Organization lessons | Separate trusted organization store | Explicit promotion; same recall-based expiry under its owning authority |
| Parse objects | Repository cache, keyed by source/language/parser/schema/config identity | Disposable under reference-aware budget |
| Embeddings | Repository cache, keyed by exact normalized input and model/tokenizer/preprocessing identity | Disposable; no cross-workspace retrieval authority |
| Git-history facts | Repository cache, keyed by commit set and miner configuration/version | Shared when inputs match |
| Checkout file membership and local resolution | Checkout view | Active lease plus bounded idle retention |
| Materialized ANN and graph query structures | Checkout view or repository index with strict membership filters | Optional bounded accelerator |
| Memory verification | Memory assertion + checkout content generation + verifier identity | Invalidated when relevant evidence changes |
| Captured episodes | Repository-owned, explicitly scoped source evidence | Bounded age/count/bytes; promoted evidence lives only as long as its retained assertion needs it |
| Navigation summaries | Derived context cache | Expire with graph generation; never automatic durable lessons |
| Hook/session telemetry | Repository telemetry | Bounded retention and indexed idempotency |

Physically separate durable knowledge and disposable cache directories. Keep repository memory local to the repository's registered storage home; keep organization data in its separate configured home. Resolve the home once from proven Git identity, including bare/separate-Git-dir layouts. Never infer sharing from matching remotes or folder names. A repository move needs an explicit registry relocation transaction; a path-hash change must not silently orphan its knowledge.

Avoid shared mutable graph authority. A parse object describes content; a checkout manifest gives it a path and version; a checkout resolver binds imports, symbols, and edges. Two identical files may parse identically while resolving imports differently. Embedding search must apply checkout membership before producing a complete top-k result; a global top-k followed by filtering can starve valid results.

**Worktree base reuse:** a new worktree should reuse a proven immutable repository
base for unchanged content and index its changes. The base belongs to the
repository, not to the continued existence of the original worktree. Current
commit manifests eliminate source reads, hashing, and parsing for proven
unchanged paths, while graph resolution and query accelerators remain
checkout-local. A shared resolved base graph with tombstones and edge overrides
would be a further optimization: it must preserve import-resolution isolation,
generation atomicity, and reference-aware base lifetime. Do not describe current
parse/content reuse as eliminating all per-worktree graph materialization.

## Phase 1 — Protect and recover knowledge

**Addresses:** R1, R2, R7. **Primary areas:** `main.rs`, `workspace_identity.rs`, `memory/store.rs`, `memory/router.rs`, verification, memory RPC/status surfaces.

1. Introduce typed errors for busy/locked, access denied, disk full, unsupported schema, corruption, and malformed authority. Retry transient contention within a bounded deadline. Preserve the existing files on non-corruption errors.
2. Give the repository service exclusive control of memory migrations and recovery. Drain/close every relevant connection before any artifact movement. Use a coherent SQLite backup or closed-store operation; a sequence of live DB/WAL/SHM renames is not the recovery protocol.
3. Keep graph-only service available where possible when memory is unavailable. `remember` must return a truthful durability failure; it must never report a durable save backed only by process memory. `status` identifies the unavailable authority, cause, and actionable recovery path.
4. Inventory old identities, schema versions, and locations. Produce an exact migration manifest. For path identities provably owned by this repository, transactionally rewrite authority-bearing records and dependent references while retaining stable record IDs and original provenance. Preserve branch/session applicability. Unknown ownership remains quarantined for review; no broad fallback aliases.
5. Make migration idempotent and crash-resumable. Record row counts and checksums before/after; test rollback/restart and schema-version rejection. Do not change existing lesson scope or treat migration as verification.
6. Represent claim acceptance, evidence freshness, and behavioral validation separately. An existing test file is evidence freshness, not a passing test run. Unsupported/no-evidence checks remain unverified for behavioral claims.

**Acceptance:** concurrent worktree open/write and injected lock/access/full-disk/schema failures never replace valid memory authority. Restart preserves every acknowledged durable write. The retained-state fixture with path/Git-directory/current IDs migrates all provably owned rows once, rejects foreign rows, and preserves IDs, evidence, links, and scope. A referenced but failing test cannot certify a behavioral claim.

## Phase 2 — Establish a bounded storage lifecycle

**Addresses:** R3, R5, historical layout duplication. **Depends on:** Phase 1 ownership/recovery rules.

1. Register repository homes and checkout roots with last-seen metadata, content generation, and activity leases. Reconcile with Git worktree metadata, but never use missing directory visibility alone as permission to delete. Handle offline volumes, moved roots, subdirectory workspaces, locks, and daemon restarts.
2. Add read-only accounting to `status` and a dedicated operator maintenance contract. Report logical/allocated bytes, WAL bytes, SQLite free pages, durable/cache/telemetry classes, active and retained checkouts, unknown historical stores, and estimated reclaimable bytes. Cache accounting itself must be incremental/bounded.
3. Implement a typed GC plan and execution journal. Mark unleased derived data, acquire the repository maintenance lock, revalidate ownership and leases, atomically move candidates into a same-filesystem trash area, then delete in bounded batches. Recovery resumes or restores incomplete steps. Never traverse symlinks out of managed storage; durable memory/evidence and unknown artifacts are excluded from automatic cache GC.
4. Add byte high/low watermarks for repository and user cache totals, plus class budgets and idle retention. Evict idle accelerators and obsolete content before current working data. If active data exceeds budget, stream/rebuild within bounds or report resource-limited partial coverage; never claim a complete result from a silently truncated index.
5. Collect unreferenced event payloads transactionally when events are compacted. Separate durable evidence/checkpoints from transient event history. Test bounded physical reclamation with held readers and insufficient free space; checkpoints and vacuum need explicit peak-space budgets.
6. Audit old root-layout graphs, checkout layouts, nested stores, and snapshots. Migrate needed authority/evidence first, then retire proved-obsolete derived artifacts. Remove old runtime writers as part of the cutover. Do not delete a snapshot just because its filename is old.
7. Publish safe cache reset, knowledge backup/restore, and storage-pressure runbooks. Update README when these public operator contracts ship.

Cache GC and memory expiry are distinct operations: cache GC never deletes knowledge opportunistically to meet a disk watermark; the explicit recall-based memory lifecycle below intentionally purges unused knowledge and its unneeded evidence.

**Acceptance:** a churn harness creates, indexes, edits, closes, and removes 100 worktrees. After the configured grace interval, no unleased checkout cache remains above budget; memory and pinned evidence are unchanged. Active leases, concurrent writes, held WAL readers, symlink attacks, daemon crash, disk-full, and unavailable Git metadata all have tested outcomes. Reclaim estimates are checked against actual allocated bytes.

Suggested initial defaults for evaluation: a 2 GiB repository disposable-cache high watermark, 75% low watermark, and 24-hour idle grace, configurable with an additional user-wide cap. These are proposed starting values, not measured requirements or permission to evict durable data. Validate them against representative Cadres repos before release.

## Phase 3 — Share content and persist only changed state

**Addresses:** R4 and parse/vector scaling. **Depends on:** Phase 2 manifests and GC.

1. Extend the existing path-free parse cache into immutable parse objects referenced by checkout manifests. Give parser/language/schema/config versions a single source of truth. Store symbol bodies once per content object and use spans/references in checkout structures.
2. Add an embedding object cache keyed by the complete embedding input plus pinned model artifact, tokenizer, dimension, normalization, and preprocessing versions. Model switches of equal dimension must still invalidate old objects. Batch identical requests across worktrees through single-flight work.
3. Keep checkout-local import resolution and edges. Persist changed nodes, affected edges, file manifests, and affected module digests in one transaction; publish the associated immutable generation only after it commits. Handle deletions, renames, ref changes, dirty files, and edits arriving during publication.
4. Share Git-history facts when repository commit/miner inputs match. Keep branch-tip selection and checkout-specific health facts separate. Preserve active-generation atomicity and bounded history sampling.
5. Materialize graph/ANN structures only for admitted active views. Enforce memory/byte admission as well as shard-count admission. Keep exact lexical/path retrieval usable if semantic indexing is absent or deferred.
6. Unify startup and watcher publication around one transaction/result contract. Propagate persistence failures; remove ignored save results and distinguish a usable current in-memory result from durable index state.

**Acceptance:** on fixtures with 90% identical source content, reuse at least 90% of eligible parsed and embedding inputs; separately report file and byte reuse. Ten identical worktrees must share identical object keys, and marginal persisted bytes must be attributable to membership/resolution/accelerators rather than repeated bodies or embeddings. Changing one file must write only its affected dependency/digest set, with graph equality to a clean rebuild. Model-change, corrupted-object, interrupted-save, divergent-import, and concurrent-GC tests must pass.

Measure cold/warm indexing wall time, CPU, peak RSS, bytes read/written, WAL peak, physical disk, and query p50/p95 on 1/5/20 worktrees. Set release thresholds against the measured baseline; do not advertise an unmeasured percentage reduction in total disk usage.

## Phase 4 — Make memory an evidence-backed learning loop

**Addresses:** R6–R8, R12. **Can start after:** Phase 1 contracts; independent of shared graph optimization.

Use a focused reusable record within the existing canonical memory schema, rather than adding a parallel memory database/model:

```text
assertion: the decision, constraint, or known failure and correction
trigger: task terms, exact files/symbols, failure signature, applicability
evidence: observed failure, validated correction, check result and revision
scope: repository/branch/session/organization under explicit authority
trust: acceptance state, evidence freshness, behavioral check state
lifecycle: supersession, contradiction, expiry, provenance
```

1. Move automatic subsystem/repository summaries to the derived context cache. Keep historical generated summaries as non-authoritative migration evidence only where needed. Stop assigning durable confidence from ranked navigation output.
2. Capture a completed task's actual failure, cause, fix, and validation. A generic “edited N files” event remains an episode, not a reusable lesson. Keep capture bounded and replay-safe; avoid full transcript storage.
3. Run deterministic deduplication and applicability validation first. Optional LLM consolidation may propose a lesson from evidence, but must not invent successful checks or widen scope. Reusable scope promotion is an explicit reviewed operation with provenance; branch episodes remain branch-local.
4. Query relevant memory using indexed path/symbol membership, failure signatures, applicable constraints, and lexical search under authority/lifecycle predicates **before** source budgets. Merge deterministically and explain why the lesson applies. Do not solve old-memory starvation by scanning all records or raising a global recent-record limit.
5. Deliver a short briefing through `prepare_change` and task hooks before substantive work. Where an integration supports a reliable pre-edit event, supply an advisory relevant check there; otherwise use task-start/prepare-change and truthful post-edit warnings. Never claim an unsupported host can block a mistake.
6. Preserve trust in every presentation: why it applies, freshness, unresolved conflicts, concrete corrective action, and a bounded check where available. High-risk/unverified lessons must read as hypotheses. Suggested commands are not automatically executed from memory content.
7. Capture explicit applied/rejected/superseded feedback and resulting test evidence. Prefer converting stable, enforceable lessons into repository tests, linters, or durable instructions through normal reviewed changes. Memory should bridge the gap, not replace executable safeguards.

### Recall-based memory expiry

Implement this in the repository/organization memory service, independently of optional LLM consolidation:

- Replace the existing per-shard hourly confidence-decay/archive loops. One fenced retention lease per owning store controls each sweep. Expiry depends on timestamps, never on how many shards are running or how often a scheduler retries. Remove the superseded runtime writers rather than letting both policies act on the same records.
- Store `created_at`, `last_recalled_at`, and an explicit retention-stale reason. The idle clock starts at creation until the first recall. Proposed configurable defaults: mark retention-stale after **90 days** without recall; purge after **180 days** without recall. The exact durations are proposed defaults; stale-then-purge is a required behavior.
- A qualifying recall is inclusion of the actual memory content in a bounded result delivered to the agent through `recall`, a workflow bundle, or a hook presentation. Count neither candidates later filtered/truncated nor raw database reads, background verification, backups, consolidation, metrics, or GC. Merely naming a memory ID does not count. A user-facing explicit memory inspection does count.
- Record recall after successful response delivery where the transport supports it; otherwise record separately named delivery attempts and use an explicit delivery acknowledgment. Do not falsely claim that a queued response was seen. Batch updates and deduplicate by delivery ID; failed/retried delivery must not manufacture repeated recall. Agent action/use is a separate signal and is not required to renew retention.
- Retention-stale memories leave automatic hook/default workflow injection. They remain discoverable through a deliberately labeled stale-inclusive `recall` search/inspection until the purge deadline, using the existing public verb with a documented schema option. A qualifying recall resets the inactivity deadline and clears **only** retention staleness. Contradiction, evidence drift, supersession, and failed verification remain unchanged.
- Automatic briefings must first pass task/path applicability and per-session duplicate suppression; there is no periodic “keep memory alive” sweep. Otherwise repetitive irrelevant injection could preserve the whole store forever.
- Purge transactionally removes the unused memory's content, FTS entries, vectors, working-memory cached copies, and exclusively owned evidence. Evidence still referenced by another retained assertion remains with that assertion. Remove or tombstone inbound links without exposing deleted content; a link alone must not immortalize an unused record.
- Keep only a minimal, bounded deletion receipt for replay/idempotency, then expire that receipt too. Define the maximum accepted replay age so deleting receipts cannot let very old capture retries recreate purged memories. Snapshot/event compaction must remove recoverable copies of purged payloads within bounded retention; restore must reapply expiry/deletion state before serving memory. Retained backups have an explicit retention horizon rather than promising immediate erasure from offline backups.
- Serialize purge against recall on the owning store. A recall transaction that renews a record before deletion makes it ineligible; a committed purge makes later handle use return an actionable expired/purged result. No partly deleted memory or dangling FTS/vector hit may surface.
- Migrate usable historical access timestamps with provenance. Do not infer recall from verification or generic mutation timestamps, or reset every old record to “recalled now.” Produce a migration expiry inventory before enabling the sweep; use one explicitly documented rollout grace period for uncertain historical records, not a permanent legacy exception.
- Expose active/retention-stale/purge-due counts, last successful sweep, bytes reclaimed, next deadline, and failure reasons in memory health. For organization memory, record recall in its owning store without leaking the querying repository's private context.

**Expiry acceptance:** use a controlled clock to prove never-recalled and previously recalled transitions, boundary timestamps, explicit stale recall renewal, and eventual physical reclamation. Verify that 1 versus 20 worktree runtimes produces identical expiry timing; internal reads/candidates/retries do not renew retention; recall does not clear evidence staleness; active recall races with purge deterministically; scope remains enforced; shared evidence survives only while needed; and restart, old capture replay, or snapshot restore cannot resurrect purged content. Test this lifecycle through MCP and hooks, not only SQL helpers.

**Acceptance:** an old exact-path lesson remains retrievable behind 10,000 irrelevant observations; stale and wrong-branch records cannot crowd it out. An in-review/cross-repository record is visibly advisory in CLI, MCP, and hook output. A validated lesson from one worktree reaches another only under correct scope, and its check result is re-evaluated against changed code. Duplicate capture, incomplete delivery, malicious memory instructions, and contradiction cases are covered.

## Phase 5 — Unify boundaries, telemetry, and operational behavior

**Addresses:** R9, R10, delivery drift. **May proceed alongside:** Phases 2–4.

1. Replace independent scanners with one policy for ignore discovery, directory exclusion, symlink handling, canonical containment, file-size limits, and file reading. Apply it across cold indexing, reindex, watcher events, and evidence verification. Report coverage gaps instead of silently treating failed reads as no source.
2. Put memory authority, applicability, trust shaping, and write durability into one service used by both hooks and MCP. Split RPC orchestration into small modules only after these ownership boundaries are established; retain one runtime contract.
3. Replace full-ledger exact-ID checks with indexed telemetry writes and bounded retention. Keep telemetry failures observable but decoupled from serving a memory result. An attribution failure must not be converted into a false memory-use success.
4. Expose stage-specific readiness, degraded persistence, GC progress/last error, cache pressure, and learning health. Keep read-only storage/memory health available without requiring a full graph bootstrap.
5. Add failure injection for storage, query deadlines, provider outages, daemon restart, and hook transport. Keep hooks best-effort and bounded; retain existing authenticated session binding.

**Acceptance:** nested ignore, outside-root symlink, symlink cycle, renamed directory, unreadable file, and watcher parity fixtures pass. Memory presentation cost does not grow with retained telemetry row count. A failed telemetry store does not suppress an otherwise valid briefing. CLI/MCP/hook golden cases agree on authority and trust.

## Phase 6 — Prove value to agent teams and ship a clean contract

**Addresses:** R11, build/log footprint, documentation drift.

1. Extend the paired benchmark into isolated writable task fixtures with an independently scored patch/test outcome. Use fixed model/settings, matched starting snapshots, reset sessions, randomized arm order, and multiple trials; retain confidence intervals rather than one favorable run.
2. Include at least: repeat a known mistake on a fresh worktree; apply a valid repository decision on a new branch; reject a stale lesson; resolve conflicting evidence; locate old exact-path knowledge behind noise; and complete an unfamiliar task without memory. Evaluate baseline, explicit Lattice retrieval, and automatic briefing separately.
3. Score correctness/regressions first, then mistake recurrence, misleading advice, tokens, latency, and storage/CPU overhead. Explicit attribution is a useful diagnostic, not proof of causal improvement. No efficiency gain compensates for worse patch correctness.
4. Add the hook and benchmark harness tests to clean-checkout CI. Resolve whether the extension is a supported tracked deliverable, then either include its full reproducible source/dependency contract or remove its stale CI expectations. Enforce lint failures for supported deliverables.
5. Establish a separate development-artifact policy: shared/configured Cargo target caching where toolchain/target/profile/features allow reuse, bounded incremental cache retention, and disposable per-job artifacts where isolation matters. Benchmark lock contention before forcing all concurrent agents into one target directory. Keep build cleanup separate from Lattice-managed cache GC.
6. Move generated run logs out of durable design-document trees; retain compact reviewed results and explicitly selected audit artifacts. Set retention at the producing harness rather than deleting arbitrary user files.
7. Update README, operator recovery/storage guides, memory model/verification notes, worktree architecture, and migration policy together. Clearly mark superseded plans as historical. Keep the public MCP verbs stable; deliberately version any changed wire semantics and migration behavior.

**Release gate:** clean-checkout CI passes; all storage/memory regression fixtures pass; worktree churn remains within configured resource budgets; there is no authority leakage or loss of acknowledged memory before its deliberate expiry/deletion; unused memories become stale and are physically purged on schedule; and paired agent tasks demonstrate reduced repeated mistakes without a correctness regression. If efficacy does not improve, revise capture/delivery before adding more memory classes or background intelligence.

## Concrete implementation sequence

| Change set | Deliverable | Required evidence |
|---|---|---|
| 1 | Classified memory open/recovery and truthful durability | Contention/full-disk/restart regressions |
| 2 | Identity migration and explicit trust distinctions | Historical-ID migration and failing-test evidence fixtures |
| 3 | Storage inventory, repository/checkout registry, safe cache lifecycle | 100-worktree churn and recovery tests |
| 4 | Event spill GC, evidence-safe checkpoints, old-layout retirement | Replay equivalence and physical-byte reclamation |
| 5 | Shared content/embedding/history objects and transactional graph deltas | Worktree reuse and full-rebuild equivalence benchmarks |
| 6 | Navigation-cache separation, reusable lesson capture, recall-based stale/purge lifecycle | Failure/correction/promotion/idempotency and controlled-clock expiry tests |
| 7 | Indexed lesson retrieval and consistent briefing/feedback | Old-lesson, branch, conflict, and trust presentation tests |
| 8 | Shared boundary policy and indexed telemetry | Traversal safety and bounded hook latency tests |
| 9 | Longitudinal agent evaluation, clean CI, artifact policy, documentation cutover | Paired outcomes and complete release evidence |

These are reviewable implementation units, not optional deferred cleanup. Each unit includes its own success/failure tests and documentation; units that change authority or storage must include their migration/recovery path before becoming runtime authority. No unit may be called complete based only on a passing happy-path fixture.
