# Remediation execution tracker

Implementation authority: [storage and agent memory redesign](2026-09-12-storage-and-agent-memory-redesign.md). Evidence: [repository review](../reports/2026-09-12-repository-review.md).

Status: in progress. No release or efficacy acceptance claimed.

Current checkpoint: installer build20b74c84 is verified on PATH; its25 installer
tests and actual six-file CLI exercise passed. Remediation runtime buildcfb306cc
previously passed the following checks. Full supported
Rust suite passed2,248 tests; private smokev4, seven-fixture delivery preflight,
and100-worktree storage18/18 gates passed on this binary. Implementation and
local verification are complete for the recorded remediation changes. Efficacy
and long-plan agent adoption remain open: user requested usage preservation and
will deploy/test with the Beacon agent. Remote CI/native Windows remain unrun.
No shared restart or live-data mutation was performed. Whole-program release
acceptance is not claimed.

## Baseline and safeguards

Started from current working tree, with unrelated modified `.claude/settings.json` and untracked Codex configuration/backup, extension, installer scripts, health plan, and the supplied review/redesign documents. Preserve these. No live data deletion, unrelated worktree modification, or shared-service restart is authorized. Tests use isolated fixtures. Lattice prepare_change returned bootstrap pending; direct source inspection is authoritative.

Verified current defects: R1 unconditional quarantine/fallback; R5 orphan event payloads; R7 existence-only promotion/no-evidence verified; R10 full-ledger exact-ID reads. Remaining findings require implementation-time verification.

## Task ledger

The original ledger below records the initial handoffs. Later dated checkpoints
supersede its intermediate statuses; the latest acceptance ledger is at the end.

| Task | Contract / dependencies | Owner / model | Status | Verification |
|---|---|---|---|---|
| 1a | Classified memory open, exclusive migration owner, unavailable durable writes | memory_open / Sol | implemented; final integrated review pending | 5 availability, owner contention, 3 daemon open tests passed |
| 1b | Separate evidence freshness and observed behavioral proof | trust / Sol | core implemented; all presentation acceptance pending | 40 verification tests passed |
| 2 | Proven historical identity rewrite; depends 1a | memory_open / Sol | identity transaction implemented; relocation/operator contract open | 3 migration, 5 workspace identity, main migration regression passed |
| 3 | Checkout registry, leases, accounting, journaled cache GC | coordinator + memory_open / Sol | lifecycle implemented; user/class budgets and operator surface open | 5 lifecycle tests; 100 real Git worktree churn fixture passed in 4.25 s; indexing completeness requires review |
| 4a | Atomic spill reclamation and bounded physical reclaim | coordinator | implemented; final integrated review pending | 47 event tests passed, 3 ignored; added hash-collision test awaits integration run |
| 4b | Graph-only snapshots and bounded managed snapshot expiry | trust / Sol | core implemented; scheduler integration active | 7 snapshot and 4 replay tests passed |
| 4c | Proven obsolete layout retirement, backup/restore/relocation | coordinator / Sol review | implemented; final review and full integration pending | pinned operator 13/13 and real Git relocation 4/4 passed; unknown artifacts preserved |
| 5a | Transactional graph deltas and shared immutable bodies | object_gc / Sol | delta and inline migration implemented; safe object pins/GC active | 14 graph tests passed before follow-up; independent review found GC, symlink, directory durability gaps |
| 5b | Shared parse/embedding/history objects; resource admission | coordinator + snapshot_finalize / Sol | object reuse and budgets implemented; committed-base startup integration active | embeddings 17 passed/3 ignored; immutable manifests 5 passed; final 1/5/20 measurements pending |
| 6a | Navigation cache and evidence-backed lesson capture | telemetry / Terra | implemented; final focused integration pending | generic edits no longer lessons; digest fixtures updated |
| 6b | Fenced 90/180-day stale/purge, receipts, dependencies | trust / Sol | core implemented; persistent scheduler integration active | 8 core tests including 20 store callers, recall/purge ordering, rollback/reopen, physical reclamation |
| 7 | Indexed applicability, stale discovery, final-content delivery/ACK | delivery_acceptance / Terra | implementation exists; dedicated transport/authority/noise tests active | MCP 118, CLI 8, binary build passed; these do not establish missing receipt cases |
| 8a | Shared traversal and descriptor-safe reads | trust / Sol | implemented; integration milestone pending | security 11, watcher 7, spans 7; daemon runtime/watcher/health focused tests passed |
| 8b | Indexed telemetry and atomic JSONL import | telemetry / Terra | implemented; integration milestone pending | 22 telemetry tests and unavailable status passed |
| 9a | Writable paired agent evaluation | coordinator | pilot demonstrated zero benefit; stricter actual-delivery evidence now implemented | 8 harness tests passed; new representative external run required |
| 9b | CI/artifact policy | ci_artifacts / Luna | implemented; new efficacy harness CI addition open | hook harness, 20 worth-it checks, shell syntax, YAML parse passed; remote CI not run |
| 9c | README and durable documentation cutover | coordinator | dedicated notes written; README open | complete drift check required |
| Review | Independent authority/migration/recovery/GC review | Sol | in progress | concrete object-store findings assigned for correction; final review required |

## Current ownership

One active writer per file. Coordinator owns final source/test integration,
evaluator/runner, documentation, measurements, and this tracker. Sol's canonical
reload and receipt implementation plus independent Sol review are complete;
Luna's storage report handoff was inspected and corrected by the coordinator.
No sub-agent writes are active. The final source is undergoing full supported
verification. Evaluator, runner, and fixtures remain unchanged, and no paired
run is active. Cargo builds use one shared bounded artifact profile. Unrelated
health-engine edits remain preserved and are not attributed to remediation.

No task is accepted solely on an agent completion claim. Full workspace and supported hook/benchmark verification remain required. No percentage reduction is inferred from unit tests.

## Integration updates

- User clarified that multi-user/team support is a future enhancement to consider, not work to implement now. The existing redesign plan records the authenticated service boundary, local SQLite role, and future authority/synchronization decisions without adding a competing architecture program.

- Operator cache plans, backup/restore, explicit relocation, and proven historical artifact retirement are implemented. Independent authority/recovery review is active; no live operator mutation has run.
- Shared parse/history caches and graph body migration are integrated. Embedding membership now uses transactional indexed references and publication leases; a concurrent lock-file creation regression remains under investigation. Vector model identity binding now invalidates old model vectors and fences earlier owners; final tests pending.
- Process/class logical-byte admission reserves source and parsed payloads before materialization. Per-user disk aggregation is integrated; dedicated two-repository fixture is active.
- Snapshot v2 omits memory. Streaming v1 rewrite passed a real 257 MiB fixture with fixed buffers; repository-owned retention descends pinned directories and registers the configured organization store. Agent reports 10 snapshot and 6 scheduler tests passed; coordinator integration review pending.
- Unix managed storage uses descriptor-relative operations. Windows handle-relative implementation exists but is not cross-compiled locally because the target/toolchain is unavailable. Independent review remains required.
- Memory delivery receipt, ACK, stale discovery, and bounded capture replay regressions were added. Full CLI/MCP/hook integration rerun remains required.
- CI includes the efficacy harness tests and hook checks. Remote clean-checkout CI has not run.
- Real paired pilot completed 36 consumer patches (six variants, two trials, three arms), all correct in all arms. Explicit retrieval and automatic briefing had correctness/recurrence deltas 0 with intervals [0,0]. This demonstrates no reduction in repeated mistakes; efficacy gate remains unmet. Report: /tmp/lattice-agent-efficacy-real-20260912.json. Credentials are available; no credential blocker is claimed.
- Full indexed 100-worktree churn and 1/5/20 storage measurements remain required; a dedicated isolated harness is active.

## Verification checkpoints

- Initial full workspace run reached core 1087 passed / 1 failed / 37 ignored; the new trust fixture was corrected and verification 40/40 subsequently passed. This was not a successful full workspace run.
- Later integration builds exposed a missing hook export and moved session ID; both were corrected. A binary build subsequently passed. Re-run after remaining edits.
- Local Codex runner probe succeeded with gpt-5.6-terra and existing authentication. It used a new temporary directory and an ephemeral read-only session. Real paired outcomes remain pending; credentials are not currently a blocked gate.
- No live memory/cache deletion, shared daemon restart, deployment, or unrelated worktree modification has occurred.

- Later full workspace milestone: 1129 passed, 8 failed, 37 ignored. Capture fixtures were updated to require valid observed/resolved evidence; subsequent memory-focused run passed 143 tests. This is not a passing full workspace run.

## Disk-pressure interruption and cleanup

The user explicitly authorized cleanup when the drive reached approximately 200 MiB free. All agents paused builders and private benchmark processes. Process inspection found only the existing shared release daemon; it was left running. Removed `daemon/target/debug/incremental` and generated project debug dependency/test artifacts, preserving third-party build dependencies, debug/release executable paths, live state, and source. Target physical usage fell from 74 GiB to about 6 GiB; free space rose to 68 GiB. Development/test profiles now disable incremental compilation and use line-table debug information. Verification is serialized during the fresh build.

New verified milestones: embedding 18 passed/3 ignored; model identity fencing 1 passed; telemetry 23 passed; indexed memory and related tests 235 passed/3 ignored; actual 10,000-row exact-path/noise regression passed. Efficacy harness contracts 5 passed. Final operator staging and snapshot-paging corrections await rerun. Independent review identified further object-reference GC and registry-root authority gaps; these remain active repairs, not accepted work.

## Current integration and review gates

- Managed SQLite now routes operator/relocation/index connections through pinned directory authority; native WAL/SHM and rollback locking remain SQLite-owned. Independent review found directory-fsync routing, temporary-file authority, partial hook installation, and interoperability coverage defects. Fixes are active; this boundary is not accepted yet. Windows implementation remains locally uncompiled because its Rust target is unavailable.
- Owner locking now has descriptor-relative acquire_in. Three focused contention/symlink/root-replacement tests passed. Operator backup/restore uses that API and pinned staging/registry connections; its new root-replacement integration test awaits execution.
- macOS snapshot paging passed a 65,601-entry reopen fixture with long filenames; snapshot 4,100-entry and interrupted 300-entry fixtures passed. Independent review then found Windows continuation failure and missing mutation/registry-generation fences. These acceptance gaps remain active repairs.
- Efficacy audit found old assisted-arm evidence could accept empty or unrelated successful calls. The harness is being changed to verify captured lesson ID and exact delivered content independently. The previous zero-benefit pilot remains historical evidence with this additional limitation; it is not release acceptance.
- Separate embedding cache opens now share an inode-scoped process coordinator; the concurrent regression now opens eight independent instances. Verification awaits the serialized build slot.
- Disposable build cleanup still leaves approximately 65 GiB free after rebuilding. Shared services remain untouched.

## Worktree reuse acceptance clarification

The user explicitly raised reuse of an original index with only worktree changes
indexed. Code audit verified shared parse/embedding/body objects but found that
cold worktrees still discover/read/hash all sources and materialize complete
checkout graphs and vector accelerators. Existing cache-hit figures do not prove
changes-only indexing or proportional graph memory. The changes-only cold-read path was missing. The plan explicitly keeps
checkout-local resolution/edges and permits admitted graph/ANN accelerators;
sharing a complete resolved base graph is a further optimization, not a
prerequisite for those stated acceptance criteria.

Sol snapshot_finalize now owns the bounded immutable commit-manifest module and
its tests, using the same parsed-cache SQLite transaction for entries and object
pins. Coordinator owns parsed-cache integration. Runtime Git authority, changed-file classification, affected-state publication,
and resource measurements remain dependent implementation and acceptance work. The completed efficacy
repair passed seven Python contract tests in a coordinator rerun; no new external
efficacy outcome has been produced.

## Latest verified milestone and active dependencies

- Broad core run: 1,183 passed / 5 failed / 37 ignored. Three failures were
  memory-highlight fixtures missing required memory IDs; those fixtures now carry
  explicit IDs. Two snapshot retry/double-rewrite defects were fixed and their
  focused interrupted/oversized/vanished-entry regressions passed. This is still
  not a passing full workspace run.
- Memory authority now supports `MemoryStore::open_in` and owner-bound
  `open_store`; 148 memory tests passed. Main's paired primary/engine opens use
  the retained owner directory. Operator/relocation/index authority integrations
  already passed their focused tests.
- Independent review confirmed lazy global SQLite syscall installation raced
  native connections. Installation moved to a platform pre-main initializer;
  explicit validation runs before daemon Tokio construction. Eight focused VFS
  tests and an isolated Windows managed-module cross-check passed. Full Windows
  workspace/runtime verification and final independent review remain gates.
- Snapshot workers now retain one Windows query handle per registered memory
  authority and restart bounded inventory after process restart; daemon tests
  are queued with the graph-only compaction API cutover. No live services changed.
- Immutable commit manifests and parse pins publish in the same SQLite database
  transaction, including a checkout claim. Runtime committed-base reuse is wired
  to both single-workspace startup paths. Checkout-local graph/ANN materialization remains permitted by the plan for
  admitted active views. Fully shared resolved bases are a further optimization;
  affected-state writes and resource measurements remain required. No cold-read
  performance claim is made yet.
- Hook package tests passed, worth-it harness 20/20 passed, standalone installer
  test passed after correcting its two stale timeout expectations. Efficacy
  harness 8/8 passed, including error payload and same-memory content binding.

## September 13 integration checkpoint

- Latest full core milestone: **1,197 passed, 1 failed, 37 ignored**. The remaining
  snapshot mutation fixture expected an empty restarted cursor, while the
  implementation correctly restarts and scans a bounded page in the same call.
  Its assertions now check no deletion, current directory fingerprint, bounded
  progress, eventual completion, and preservation of the newest snapshot.
  This is not yet a passing full workspace result.
- Real linked-worktree regression now creates two Git-linked checkouts and
  checks unchanged parse reuse plus changed-import/deletion graph isolation.
  Source is complete; the regression has not yet run.
- Independent review found historical registry growth above 4,096 rows stalled
  parsed GC. Parsed GC now trusts durable membership and commit pins directly;
  general accounting is being replaced with persisted bounded pages. Read-only
  status will expose incomplete/stale accounting instead of assuming zero.
- GC replay persists the moved cache inode identity, refuses unproven trash,
  and preserves references when a new current cache exists. Old journals with
  no identity require operator recovery and never authorize guessed deletion.
- Relocation reads and resumptions validate identity/checksum/completed journal.
  Additional full-tuple and conditional ownership update regressions are ready.
  Relocation is explicitly operator-attested; checksums do not prove historical
  Git continuity.
- Telemetry legacy migration now commits bounded pages and cursor atomically;
  content-free health aggregation is being made independent of retained event
  count. New migration and aggregate regressions await the serialized build.
- Behavioral observations now require repository, checkout, revision, dirty-state
  generation, freshness, and deterministic latest-result selection. Existing
  generic session check events lack sufficient trusted provenance to certify
  behavior. A bounded daemon-owned check runner/adapter and queued verification
  consumer remain missing acceptance work; unverified output remains truthful.
- No new storage benchmark or paired agent usefulness outcome is claimed.
  Live services and live knowledge remain untouched.

### September 13 subsequent verification and review

- Full core suite: **1,215 passed, 0 failed, 37 ignored**. Full workspace run
  then stopped at daemon library: **319 passed, 8 failed, 1 ignored**
  (`/tmp/lattice-workspace-sep13-final.log`). This supersedes the earlier counts.
- The real linked-worktree reuse/isolation regression passed: three unchanged
  files require zero source reads, hashes, or parses; changed imports and deleted
  files affect only their checkout. Resolved graphs remain checkout-local.
- Bounded persisted inventory and shared-object allocated-byte receipts are
  integrated. Status is read-only and reports incomplete/unknown pressure;
  maintenance advances pages and publication invalidates accounting. Core
  regression coverage passed, including unknown-state preservation.
- Trusted check runner, explicit MCP/CLI adapter, persisted observations, and
  bounded structural verification queue are implemented. Sol independent review
  rejected acceptance pending fixes for older-pass/newer-failure ordering,
  atomic verification persistence, and same-ID memory mutation during execution.
- Current bounded tasks and ownership:

  | Task | Dependency | Owner/model | Status | Verification |
  | --- | --- | --- | --- | --- |
  | Verification observation ordering, atomic status, target-version binding | Runner and adapter | managed_filesystem / Sol | Repairing review findings | Focused concurrency/failure tests and independent Sol review required |
  | Storage budget proof order and worktree lifecycle fixtures | Published inventory contract | Coordinator | Implemented; testing | Daemon library rerun in progress |
  | Delivery ID/status scope/retention failure fixtures | Established public/schema contracts | context_stage_cleanup / Luna | Implemented; integrated | Diff/shell checks passed; Rust rerun in progress |
  | Independent authority/GC and trust review | Integrated changes | budget_finalize / Sol | Reviewing | Concrete trust findings recorded above |
  | Diverse paired efficacy fixture smoke | Fresh verified release binary | efficacy_task_variety / Terra | Waiting for binary | 14 Python harness tests passed; real paired outcomes pending |

- Ninety historical generated logs (376,971,728 bytes) were preserved under
  ignored `.local-artifacts/historical/`; the checked-in SHA-256 inventory records
  every move. This reduces tracked artifacts, not physical disk usage.
- Fresh storage measurements, actual diverse paired agent trials, full workspace
  verification, and private daemon/hook integration remain acceptance gates.
  No shared service has been restarted and no live knowledge has been deleted.

### Latest acceptance ledger (September 13)

| Contract | Dependency | Owner/model | Current status | Verified evidence |
| --- | --- | --- | --- | --- |
| Knowledge open/recovery/identity/expiry and shared-object/graph lifecycle | Plan phases 1–4 | Coordinator + Sol implementations/reviews | Integrated; full rerun pending | Latest completed core run 1,222 passed, 0 failed, 37 ignored |
| Cross-connection verification CAS | Trusted runner + target/observation bindings | managed_filesystem / Sol; budget_finalize / Sol review | Implemented, independent review in progress | Independent two-connection mutation test 1 passed, rollback 1 passed, daemon verification 9 passed |
| Pinned per-user registry, private files, stale-home progress | Repository authority + published inventory | snapshot_finalize / Sol, coordinator integration | Implemented; integrated tests running | Formatting passed; earlier two fixture failures repaired; no passing latest disk run claimed |
| Bounded managed context cache and USearch recovery | Pinned checkout publication | context_stage_cleanup / Luna; coordinator; Sol review | Implemented; integrated tests running | USearch 4 passed, covering torn pair in both directions, malformed/corrupt pair, bounds, owned/foreign staging |
| Public task contracts in paired efficacy fixtures | Independent graders and matched revisions | efficacy_task_variety / Terra | Implemented; actual paired trials pending | 15 Python tests passed; each arm shares committed CONTRACT.md; reference patches excluded from agent input |
| Fresh storage/embedding measurements and real delivery smoke | Passing integration + fresh binary | Coordinator + Terra | Pending | Installed ONNX model and tokenizer checksums verified; no new measurement claimed |
| Hook/CI/artifact contract | Public CLI/MCP and producer-owned artifacts | Coordinator + Luna | Local harnesses passed; clean remote CI pending | Hook package and installer passed; worth-it 20 passed; efficacy harness 15 passed; installer added to CI |

The latest supported workspace rerun is `/tmp/lattice-workspace-sep13-cas.log`.
The preceding full run had 1,222 passing core tests and 328 passing daemon tests,
with two disk fixture failures. Failed/unfinished runs are not acceptance.

Review resolution: aggregate accounting stays explicitly incomplete when a home
is unavailable. Complete proven homes alone may establish a lower bound above
the cap; only their typed, unleased caches may then be reclaimed. This preserves
safe progress without using unknown bytes or foreign authority. Private final
state directory/files use 0700/0600 on Unix. No Windows runtime result is claimed.

### Shutdown and integration review checkpoint

- Full workspace run `/tmp/lattice-workspace-sep13-acceptance.log`: core
  **1,225 passed, 0 failed, 37 ignored**; daemon library **337 passed,
  0 failed, 1 ignored**; binary **491 passed, 13 failed, 1 ignored**.
  Six binary failures required localhost permissions; capture/trust fixtures
  and the Git history failure-stage assertion were corrected afterward.
- Privileged socket rerun passed 24 tests and exposed one genuine shutdown
  cleanup race. Native watcher cancellation now closes its bounded receiver
  before destroying the watcher, preventing a blocked callback teardown deadlock.
- Runtime blocking writers now retain checkout work tracking and index permits
  through completion. Independent Sol review accepted normal shutdown ordering
  but found partial-construction, listener-error, and poisoned-mutex ownership
  gaps. These remain open until repaired and independently reviewed.
- Capture housekeeping now retires transport records without deleting lesson
  knowledge. Transport and tombstone retirement each process at most 256 rows
  per pass. Four focused retention/CAS tests passed; independent bounded
  retention review is in progress.
- Ignored concurrency suite: **7 passed** (`/tmp/lattice-concurrency-sep13.log`).
  Fresh release artifact, storage measurement, and paired efficacy trials remain
  pending. Further runtime tests use private XDG state and private endpoints.

| Bounded task | Dependency | Owner/model | Status | Acceptance evidence |
| --- | --- | --- | --- | --- |
| Shutdown on constructor, listener, and poisoned ownership failures | Runtime work tracker | budget_finalize / Sol | Repairing independent review | Focused failure tests and snapshot_finalize review required |
| Independent transport-retention bounds review | Transport-only retirement | snapshot_finalize / Sol | Reviewing | Concrete defects or bounded acceptance required |
| Trusted runner documentation precision | Pre/post fingerprint implementation | managed_filesystem / Sol | Reviewing | Claims must not imply immutable executable execution |
| Integrated workspace verification | Shutdown repairs | Coordinator | Queued | Private XDG and permitted private listeners |

### Owned runtime work acceptance checkpoint

- Independent Sol review found no remaining shutdown authority blocker after
  two-phase runtime validation, cooperative scheduler shutdown, connection-task
  ownership, and runtime guards across request, watcher, Git, health, query,
  reindex, trusted-check, and consolidation workers. The tracker lives in the
  shared `index_work` module so both library and binary use the same contract.
- Removed the unreachable inline runtime after explicit CLI mode routing.
  `build_workspace_runtime` is now the sole workspace constructor; the
  lightweight stdio proxy does not construct another daemon.
- Admission closure and the final candidate ownership check share the shard-map
  mutex with bootstrap-handle publication and draining. Added a deterministic
  candidate-insertion/victim-shutdown/global-close regression. Full execution
  remains pending; code inspection alone is not the test result.
- Added joined public MCP delivery/expiry acceptance coverage using the core
  production sweep's controlled-clock seam. There is no new public maintenance
  or test-clock verb. The private RPC smoke separately records its real endpoint
  evidence and does not claim to have waited 180 days.
- Shared capture-retirement budget regression passed: **1 passed**; mixed
  backlogs make fair progress within 256 total rows and preserve fresh replay
  fences. Hook/installer and Python harness evidence remains as recorded above.
- The latest full suite is `/tmp/lattice-workspace-sep13-shared-tracker.log`,
  using private XDG directories and permitted private listeners. Earlier
  shutdown runs stopped at compile errors, now corrected; no passing full
  result is yet claimed.
- Prepared a private ONNX Runtime 1.23.2 library from a PyPI wheel, without
  installing into the user's environment. It loaded successfully. Library
  SHA-256: `6c895a1e485b54bdae3cf88e55ab48650a695bf369aab98f21114ace8c35e1fa`.
  Actual inference/storage measurements still require the fresh release binary.

### Passing supported workspace milestone

`/tmp/lattice-workspace-sep13-vm-budget.log` completed successfully:

- Core: **1,231 passed, 0 failed, 37 ignored**.
- Daemon library: **339 passed, 0 failed, 1 ignored**.
- Daemon binary: **512 passed, 0 failed, 1 ignored**.
- Integration target: **7 passed, 0 failed**; remaining empty/doc-test targets passed.

This includes the exact shutdown admission race, public MCP delivery/expiry,
positive authenticated correction capture and replay, navigation-only negative
capture, and lexical recall. Generic edits do not create a lesson. A lesson
requires the sanitized correction summary, observed/resolved error, and check.

Natural-language recall now generates indexed lexical candidates without
requiring every prompt word. Coverage breaks lexical ties before confidence;
exact applicability memberships retain their established priority and ties.
SQLite VM budgeting interrupts broad searches with an actionable error and
clears the progress callback on all exits. Independent Sol review accepted
this behavior and found two final input-edge refinements: cap unique terms
after deduplication and bound total query/token bytes before SQL construction.
Those refinements, the fresh release artifact, and actual storage/efficacy
measurements remain open; this milestone is not whole-program completion.

Fresh shell hook package, installer, and efficacy Python harness runs passed
(`/tmp/lattice-hooks-sep13-final.log`, `/tmp/lattice-install-sep13-final.log`,
`/tmp/lattice-efficacy-harness-sep13-final.log`; Python **15 passed**).

### Release gate and real workflow follow-through

- Latest supported workspace run `/tmp/lattice-workspace-sep13-release-gate.log` passed: core **1,233**, daemon library **339**, daemon binary **512**, integration **7**; zero failures, respectively 37/1/1/0 ignored. Release build passed (`/tmp/lattice-release-sep13.log`), artifact SHA-256 `1d3ecd3cff29b4899bf072c580b1f711b7365b2ed7b751e14fb174171926feae`. This includes the bounded unique-term/input refinements.
- Real private-daemon producer/linked-consumer smoke failed (`/tmp/lattice-private-delivery-smoke-sep13.json`): explicit recall delivered the captured ID/content, but `prepare_change` returned no highlights. Sol `budget_finalize` owns task/anchor candidate composition, removal of unscoped QueryEngine memory, and structured memory-class serialization; Sol `snapshot_finalize` independently reviews authority. Acceptance requires the actual smoke to pass after focused tests and release rebuild.
- The strengthened storage benchmark stopped at its first readiness assertion: CLI `--json` left the workflow renderer at Markdown, hiding symbol evidence inside a summary. Root owns the CLI structured-render fix and regression/docs; actual 100-worktree metrics remain pending. No failed attempt is counted as successful measurement.

- Delivery source repair passed focused public linked-checkout tests in both daemon targets, current/sibling applicability tests in both targets, the graph-only query regression, and CLI JSON tests. Independent Sol review accepted the scoped source, including structured MemoryId receipt extraction. Root is running `/tmp/lattice-workspace-sep13-delivery-final.log`, then the seven opt-in concurrency tests and a fresh release build.
- Evaluation audit review found and assigned actual MCP-envelope/structured-ID parser gaps; no paired run starts until these pass. Producer capture proof and conventional baseline-use auditing now have regression coverage; limitations explicitly exclude concealed process use and adversarial grader containment.
- CI test jobs now install their declared Rust target explicitly, matching build jobs. The storage harness requires all 80 fixture embedding memberships (40 files + 40 functions), rejecting half-published inference; its four contract tests passed.

### Actual-run findings after the passing delivery suite

- `/tmp/lattice-workspace-sep13-delivery-final.log`: **1,234 core, 341 daemon library, 515 daemon binary, 7 integration tests passed**, zero failures (37/1/1 ignored). `/tmp/lattice-concurrency-sep13-final.log`: **7 opt-in concurrency tests passed**. Release build passed, SHA-256 `d370535313294789b155e7d908de550d9b1a73ef363c26c1d5fd812cb6bc8719`.
- Fresh private smoke (`/tmp/lattice-private-delivery-smoke-sep13-final.json`) now actually returns the captured lesson across linked checkouts. It still failed the exact-content harness because the default JSON uses dense `ct`/`mh` fields. The harness now explicitly requests public standard/full JSON. Separately, the actual dense response omitted delivery receipts; Sol budget_finalize owns the final-render receipt correction, recursive suppression/failure handling, and a bounded actual lesson section for default Markdown. Hidden JSON candidates must never be counted as Markdown delivery.
- Independent Sol snapshot_finalize verified and fixed a trust presentation defect: confidence 1.0 was incorrectly mapped to strong evidence/0.92 despite actual evidence strength 0.18. The workflow now derives strength from evidence fields, with missing evidence unverified. Source tests added; Cargo execution pending.
- Real ONNX benchmark (`/tmp/lattice-storage-benchmark-sep13-d370535.log`) stopped at checkout four: initial40 parse memberships and80 embedding memberships published, but its edit did not publish within120seconds. Checkouts1–3 did. Sol watcher_bootstrap owns verified diagnosis and deterministic runtime/index fix; root retains fixture and measurement ownership. This is a failed acceptance run, not a successful 100-worktree measurement.
- Efficacy harness real-wire/audit refinements passed **21 Python tests**. Actual paired model trials remain unrun until fresh delivery acceptance.

### Final-render and watcher acceptance follow-through

- Default Markdown now preserves a complete selected lesson up to512bytes when it fits the response cap, with ID/trust/receipt; explicit small budgets or larger lessons use a recall expansion path. It never restores a session-suppressed ID. Dense and standard JSON bind actual surviving projections; nested payloads are filtered again after token trimming. Authority-qualified IDs take precedence over raw local IDs. Receipt-persistence failure removes memory content and derived summaries. Independent Sol review accepted the source and four focused regressions passed.
- Public `recall` now advertises `include_retention_stale` in the actual eight-verb schema; focused schema regression passed. Confidence/evidence separation regressions passed2/2 and weak-evidence metrics passed1/1. Public capture wording now names `remember`.
- Watcher diagnosis verified two distinct defects: startup scanning preceded native registration, and real native tests exposed `/var` versus `/private/var` path aliases. The latter alone is not claimed to explain the canonical-path benchmark failure. Watchers now establish native/polling coverage before startup scanning, normalize event aliases within canonical authority, and compare metadata for directory/root/rename notifications before processing changed sources. Real native edit, degraded polling edit, selective root event, and real directory rename tests passed **4/4**. The selective fixture excludes64unchanged sources from the content batch. Final independent review and actual100-worktree rerun remain required.
- Root started `/tmp/lattice-workspace-sep13-watcher-final.log` followed by `/tmp/lattice-release-sep13-watcher-final.log`. No success is claimed until these finish. The private smoke now checks genuine default Markdown, exact lesson/ID, attempted-delivery NULL recall timestamp, rejected forged ACK, valid ACK, idempotent ACK replay, and stale exclusion. Python efficacy contracts passed **23**; storage contracts passed **6** including failure reports that cannot overwrite earlier evidence.

### Actual delivery and indexing acceptance checkpoint

- Supported workspace `/tmp/lattice-workspace-sep13-watcher-final.log` passed: core **1,234**, daemon library **347**, daemon binary **525**, integration **7**, zero failures (37/1/1 ignored). Release `/tmp/lattice-release-sep13-watcher-final.log` passed, SHA-256 `8e986bf697f05531aad54a0e1cc2d6f25d68eb44492168ad17e6fd6bca403775`. Later fixes below require a fresh integration run.
- Private smoke `/tmp/lattice-private-delivery-smoke-sep13-ready-ack.json` progressed through actual default Markdown exact-content delivery, linked-checkout JSON briefing/recall, rejected forged ACK, successful exact ACK, and idempotent replay. Overall it **failed** because explicit recall still returned a semantically stale lesson. The next harness preserves partial ACK evidence even when a later gate fails; no failed smoke counts as acceptance.
- Explicit search now uses bounded indexed authority queries for shared and nonshared stores, including checkout scope and semantic freshness. Task recall never lists ambient memories merely because a checkpoint exists and restores a real saved task statement. Four focused regressions passed; independent Sol source review accepted. Actual smoke rerun remains required.
- Watcher native and polling modes now share committed/pending metadata state. Failed read/parse/persistence or inventory does not advance the baseline; successful graph no-ops settle metadata and deletions. Actual retry without another edit and no-op settlement join native alias/rename, selective metadata and symlink coverage: **8 passed**; independent Sol review accepted.
- Conflict status inspection still used a full scoped memory scan and per-row doc lookup. Sol watcher_bootstrap owns bounded indexed conflict inspection with explicit stale visibility and truthful exact totals or budget errors. Independent review remains required.
- Python harness verification: efficacy **24 passed**, storage **6 passed**. Actual 100-worktree/ONNX measurement and diverse paired model trials remain pending; credentials have not been reported unavailable.
- Worktree requirement is recorded in the existing redesign: immutable repository-owned base reuse for unchanged source, changed-file processing, isolated checkout resolution. Current code is not described as a shared resolved base graph. Live data and the shared daemon remain untouched. Latest debug deps are12GiB, release1.9GiB, free disk76GiB.

### Completed 100-worktree measurement and remaining review

- Actual 100-worktree/ONNX run completed on release SHA-256 `6c3b0c4acaf83a30f515e0d1c70fe9cfeda40ded2d5d56dbeb99043c4cb1d9a9`: `/tmp/lattice-storage-benchmark-sep13-real.json`. Every checkout reached initial parsed+80 embedding memberships, then changed-source membership and changed embedding fingerprint, followed by a real symbol/file query. At20worktrees: parsed file reuse92.625%, parsed payload-byte reuse92.735%, embedding object reuse92.625%, embedding byte reuse92.622%. Ready context p50=24.47ms/p95=30.99ms. Plan/apply reclaimed97unleased cache bundles and109,658,112bytes while preserving3active leases. This is a synthetic-fixture measurement, not a total-disk percentage reduction.
- The final embedding snapshot was taken after all worktrees were removed and had zero memberships. A second instrumented run captures the100-worktree snapshot before removal and adds kernel physical I/O/peak footprint, CPU and sampled WAL high-water counters. The first result remains valid for its recorded20-worktree reuse and reclamation; missing measurements are not inferred.
- Actual delivery data revealed repository-qualified recall IDs versus raw remember IDs. The evaluator now requires proven repository authority across raw, structured, qualified, and same-object expansion-handle representations. It rejects foreign, missing, conflicting, and malformed authority. Independent Sol source review accepted; Python25/25 passed. Root checked retained actual remember/prepare/recall payloads through the new matcher successfully. Fresh private ACK/stale smoke and actual paired model trials remain pending.
- Conflict-query focused tests initially passed5core+4library+4binary. Independent Sol/root review found unbounded doc-index backfill, missing reverse structured conflicts, multiple-read snapshot races, and lack of current-checkout applicability. Those are active repairs, not accepted implementation. Root owns the one mcp call-site integration; watcher_bootstrap owns core/query/RPC changes.

### Integrated recall regressions and real smoke success

- Private actual-wire smoke `/tmp/lattice-private-delivery-smoke-sep13-final-wire.json` **passed** capture/default Markdown/linked JSON delivery, forged/exact/replayed ACK, no attempted-delivery renewal, and semantic-stale exclusion on artifact6c3b0c4. Python26/26 passed after correcting the Markdown fixture to the actual renderer identity line and matching single authority receipt.
- Fullworkspace `/tmp/lattice-workspace-sep13-indexed-conflicts.log` passed1,241core tests (37ignored), then failed6daemon-library tests (342passed,1ignored). Three are real replacement regressions: qualified memory attribution, missing changed-file warning metadata, and lost late exact-task identifier in bounded candidate composition. Sol budget_finalize owns repairs; Sol snapshot_finalize reviews. Three expectations require contract updates: qualifiedIDs, default semantic-stale exclusion, and independent retention-stale versus evidence-stale discovery. No full-pass claim applies to this run.
- Final conflict source was independently accepted; post-fix core7/7, daemon library4/4 and binary4/4 passed. This covers reverse structured links, checkout-specific authority, bounded256-row migration progress, stable count/page snapshot, and cleanup after VM interruption.
- The second100-worktree measurement completed with real kernel I/O/CPU/footprint and sampledWAL. Independent measurement review requires exact fixture symbol+file matching, joined nonzero embedding objects, explicit100 completion count, and independent pre/post-apply allocation deltas. Root implemented these with12Python tests passing; final artifact rerun remains required. Existing measured values are retained with their narrower evidence limits.

### Actual feedback authority and storage replacement

- Warning-enabled private smoke `/tmp/lattice-private-delivery-smoke-sep13-event-diagnostic.json` reproduced a real runtime blocker: EventCapture expected the stable repository ID while EventWriter used the checkout pathname, disabling event capture on producer and consumer. Main now uses the proven repository ID; historical path-authority event rows remain immutable audit history, not aliases for new-authority events.
- Actual remembered IDs are UUID-like; the memory identity decoder previously required event-style ULIDs. The memory-specific codec now accepts bounded stable ASCII record IDs while EventId remains ULID-only. Focused UUID/event tests passed; final integrated verification remains pending.
- The old attribution runtime also created raw checkout-local memory_graph.db and memory_attribution.db and required a manually seeded graph node. A passing manually seeded fixture did not establish publicremember feedback. Approved replacement keeps access/retrieval/CAS/metric-replay state in the canonical managedmemories.db, with FKpurge and bounded retirement. No unproven historical graph/index rows are imported or deleted.
- Ownership: Sol watcher_bootstrap implements core MemoryStore journal APIs/schema/tests/docs; Sol budget_finalize implements async daemon orchestration, exact event validation, response diagnostics and public workflow tests; Sol snapshot_finalize independently reviews authority/recovery/concurrency. Root owns Python smoke/evaluation, README/tracker and integration. Exact event lookup replaces the newest10,000-row scan; copied validated event facts must preserve pending feedback across compaction/restart.
- Wholeprogram acceptance and actualpaired efficacy remain open. The earlier private smoke provesdelivery/ACK/staleexclusion, not the newly identified missing feedback workflow.

- The private smoke now requires an actual persisted pending access, a fresh daemon/session resolving it through public `remember`, equal retry, rejected conflicting feedback, and an unchanged recall timestamp. Harness contracts passed **27/27** (`/tmp/lattice-efficacy-harness-sep13-feedback.log`); the real smoke requires the rebuilt integrated artifact and is not yet accepted. Independent review identified metric retry identity, uncited-use counting, pending-outbox expiry visibility, and purge replay fences; writers are resolving these before integration.
- Disk check: debug dependencies **12 GiB**, release artifacts **1.9 GiB**, free space **77 GiB**. No additional cleanup or shared-service action was needed.

### Canonical feedback and telemetry integration

- Core journal source is frozen: exact retrieval replay, atomic terminal claim resolution, copied event authority, FK purge receipts, 256-item compact metric outbox with typed keyset cursor, and repository-scheduled bounded pruning. Focused core attribution tests passed **6/6**, including 260 colon-bearing IDs across outbox pages. Daemon integration and independent final acceptance remain open.
- Root verified telemetry still opened raw checkout-local SQLite and legacy paths despite the plan's repository telemetry contract. It now resolves the shared repository home once, retains a pinned directory, uses ManagedSqlite for database/sidecars and a nofollow legacy file descriptor, and fails visibly on invalid Git authority. Historical linked-checkout ledgers remain preserved audit files. Four new focused authority/replacement/sharing tests await execution; independent Sol source review accepted the change under the existing WorkspaceIdentity contract.
- Repository maintenance now logs journal pruning and emits a warning when metric retries exceed their horizon, with durable dead-letter receipts. Feedback does not renew memory retention.
- Storage harness regression limits were fixed against the completed instrumented macOS baseline before the final run. Completion and acceptance are separate: missing measurements block acceptance and negative reclamation fails it. Python measurement contracts passed **13/13** (`/tmp/lattice-storage-harness-sep13-thresholds.log`); Terra independently reviews this contract. The fresh final artifact measurement and actual paired agent trials remain required.

- Final independent Sol review accepted the canonical journal, event/checkout/session authority, compaction recovery, exactly-once metrics and rotating 256-operation outbox. Core focused coverage passed **7/7**, including a 10,001-row ineligible backlog that exhausts its VM budget before mutation. A real SQLite failure trigger now exercises metrics failure after journal commit and fresh-handler recovery through public verbs; no manually seeded graph is used.
- Managed repository telemetry focused tests passed **32/32** (`/tmp/lattice-telemetry-sep13-managed.log`). The initial run found that empty health reads created a home; the replacement now pins only an existing home for health reads and preserves the prior read-only contract. Writes still create the proven home safely.
- Full workspace `/tmp/lattice-workspace-sep13-feedback-final.log`: **1,250 core passed**, then **347 daemon library passed / 1 failed / 1 ignored**. The single failure was a public MCP fixture still naming removed `search_memory`; root changed it to `recall` with search mode. `/tmp/lattice-workspace-sep13-feedback-accepted.log` is the fresh full rerun; no pass is claimed until completion.
- Independent Terra measurement review required exact100 completion, explicit failed operator exits, verified real embedding evidence, positive WAL samples, consistent failed receipts, and exact edited-file embedding replacement. Root implemented these, including rejection of collapsed or partially changed file/function object keys. Measurement contracts passed **14/14** (`/tmp/lattice-storage-harness-sep13-reviewed.log`). Final benchmark acceptance remains pending the fresh binary.

### Integrated workspace pass

- `/tmp/lattice-workspace-sep13-integrated.log` passed with exit0: **1,250 core**, **348 daemon library**, **530 daemon binary**, and **7 integration tests**, zero failures (37/1/1/0 ignored). This includes the public feedback failure/restart regression and managed repository telemetry. The preceding rerun passed core/library but found a CLI test constructing a nonexistent workspace; its fixture now uses a real private temporary directory. Production identity failure behavior was preserved.
- Seven opt-in concurrency tests are running against this integrated source. Release rebuild, actual private feedback smoke, final calibrated100-worktree measurement, and diverse paired external agent trials remain open.


### Final storage and delivery acceptance; efficacy methodology correction

- Integrated release SHA `e03f8b3ee04d565cb9886b436f6213afccdc156368f48372c5c29a11ca0f5ef0` built successfully. Seven opt-in concurrency tests passed (`/tmp/lattice-concurrency-sep13-integrated.log`). The full workspace pass above remains current.
- Actual warning-clean private smoke `/tmp/lattice-private-delivery-smoke-sep13-final-closed.json` passed: public capture, default Markdown exact lesson/authority receipt, linked-checkout delivery, failed/attempted delivery not renewing retention, forged/replayed ACK rejection/idempotency, fresh-session public feedback and equal retry, conflicting feedback rejection, unchanged recall time, and stale exclusion. Controlled 90/180-day expiry is covered by Rust tests, not fabricated aging in this smoke.
- Actual `/tmp/lattice-storage-benchmark-sep13-measured.json` completed 100 worktrees with real ONNX embeddings and passed all 18 pre-fixed gates. At 20 worktrees, parsed/embedding byte reuse was 92.735%/92.622%; ready-query p95 28.64 ms; independently reclaimed 109,658,112 allocated bytes across 97 unleased bundles, preserving three active leases. See the existing storage benchmark report for measurement scope and limits. Harness tests passed 16/16 without ResourceWarnings.
- The fresh actual Codex producer run `/tmp/lattice-efficacy-runs-sep13/run-g3l9uxtn` failed independent grading before capture: a public fixture failed to specify dictionary versus attribute records. Provider credentials and runner worked. No consumer results or efficacy benefit are claimed.
- Independent efficacy review also found recurrence conflated with general correctness, grader infrastructure errors converted to observed mistakes, and a non-preregistered statistical gate. Corrections are bounded below; the whole-program efficacy gate stays open.

| Task | Dependency | Owner/model | Status | Acceptance evidence |
|---|---|---|---|---|
| Explicit fixture contracts and separately named recurrence decisions | Verified ambiguous producer failure and R11 review | efficacy_fixture_v2 / Terra | Implementing | New module plus reference/base/regression mutant tests required |
| Strict grader failures and preregistered task-cluster efficacy gate | Fixture v2 schema | efficacy_scoring_v2 / Sol | Implementing | Harness/delivery regressions, invalid outcome handling, positive/zero/regression statistics required |
| Real branch-change/conflicting-evidence API contract | Current public remember and runner behavior | efficacy_branch_contract / Terra | Read-only investigation | Exact public interface and authority/delivery feasibility, no benchmark claims |
| Integrate, document, inspect and run valid paired tasks | Three tasks above | coordinator | Pending | Actual paired artifacts and truthful gate outcome required |


### Public lesson evolution: verified recovery gap and ownership split

- The planned changed-branch/conflicting-evidence evaluation exposed a product gap: the eight-verb public surface had no auditable mutation to supersede an old lesson. `remember(kind: evolution)` now routes the existing proposal/apply/reject model; no new public verb is added. Acceptance is pending the recovery correction below.
- Independent Sol review rejected the first integration: a separate event database could commit before memory/proposal/job commit, and replay could materialize that rolled-back update. It also found insufficient proposal/replacement repository/checkout authority and apply-time replacement validation. These are actual recovery/authority defects, not optional evaluation improvements.
- Ownership is split without competing writers: public_memory_evolution / Sol owns daemon MCP schema/routing/proposal validation and daemon tests; evolution_independent_review / Sol now owns core proposal/CAS, deterministic event outbox, event publication/replay and focused core tests; coordinator owns runner/smoke/docs/CI integration. Another independent review is required after implementation.
- Core acceptance requires memory+decision+job+outbox to commit atomically, bounded after-commit publication, stable deduplicated event identity, restart recovery, rejection of changed/deleted/foreign replacement, and replay consistent with committed decisions. Earlier full-suite and delivery passes do not prove this new contract.
- Fixture v2 tests passed 8/8, including separate target/regression outcomes, explicit record contracts, deep mutation checks and both branch revisions. Runner supersession tests passed 5/5, including complete mixed text/structured-response exclusion. Independent review found that obsolete content from an earlier call could be overlooked by checking only a later clean response; evaluator correction now requires raw response evidence and all-call exclusion.


- Follow-up independent Sol review expanded the recovery correction to internal consolidation/review decisions and explicit reversal. All decisions now use the canonical memory transaction and after-commit event outbox. Remaining acceptance requires newly requested causal outage/restart, deletion/replay, historical-event, reversed-create and >10k-event regressions; the earlier six proposal/replay tests do not suffice.
- New audit events are reference-only, with immutable applied/reverted transition and state hash; they no longer duplicate full lesson snapshots. Supersession binds the replacement state hash, not copied replacement content. Existing operator deletion receipts/restore floors fence replay; capture transport retirement remains distinct from knowledge deletion.
- Root rejected a dangerous replay integration discovered during review: replay still cleared its MemoryStore and read only the first10,000 events after that store became canonical proposal authority. The replacement must replay into explicitly owned disposable storage, preserve canonical knowledge, and stream bounded pages. No live replay was run. Whole-program acceptance remains open until this is implemented and verified.
- Public new-handler recovery and65-row backlog metadata regression passed1/1 with an actual EventStore INSERT failure: the decision stayed committed, fresh ordinary status drained the audit once, and a current proposal behind the batch correctly reported pending. Root CLI plumbing now uses the same evolution validator; its new focused test awaits final integrated compilation. Three agents remain assigned to core implementation, daemon branch integration, and independent Sol review.


### Final recovery verification ownership

| Bounded task | Dependency | Owner / model | Status | Required acceptance |
|---|---|---|---|---|
| Transactional proposal/reversal and disposable replay | Verified cross-database and canonical-clear defects | evolution_independent_review / Sol | Implemented; downgrade guard in progress | Versioned new events, old-schema migration, source/target authority and replay fences |
| Independent recovery regressions | Frozen transaction/outbox/replay APIs | evolution_acceptance_tests / Terra | Implementing one new test module | Old pending-row migration, >10k tail event, canonical sentinel/recall-clock preservation, outage/reversal/restart causality, reverted create, historical skips |
| Independent authority/recovery review | Core and daemon implementation | efficacy_scoring_v2 / Sol | Reviewing | Resolve concrete findings; no acceptance based solely on six old tests |
| Trusted branch and ordinary-request audit recovery | Core outbox API | public_memory_evolution / Sol | Focused tests passed | Missing capture, actual non-main branch, fresh-handler recovery,65-row bounded metadata and schema |
| CLI, full workspace, release, private smoke v2, paired efficacy | Tasks above | coordinator | Pending integrated run | Actual artifacts and truthful gate outcome |

New reference-only event semantics require an explicit durable event schema
upgrade: an older binary must reject the upgraded event database rather than
interpret omitted state bodies as an empty-memory transition. This was verified
as a concrete downgrade hazard during independent review and remains a gate.


### Recovery acceptance follow-through

- Root inspected the new six-test recovery module and requested stronger assertions for actual derived tail content, reopened canonical storage, and unchanged `last_recalled_at`, beyond event counts and delivery receipts. Initial focused six-test run passed; strengthened run pending.
- CLI public evolution regression passed 1/1 (`/tmp/lattice-cli-evolution-sep13.log`), including invalid actions, missing deltas, strict public request fields and outcome status.
- Outbox backlog reporting now checks actual post-drain pending rows (`has_more`), including a causally deferred reversal. Root integrated the daemon field change.
- Root review found a concurrent-deletion replay pagination stall and a new constructor panic path; core owner is correcting both. Explicit reversal must also reject later state changes and deletion fences; independent Sol review is checking those paths before freeze.
- User worktree base reuse suggestion is captured in the existing redesign: immutable repository base plus checkout overlay; current parse/embedding reuse must not be described as sharing all resolved graph materialization.

- Strengthened recovery acceptance passed 6/6, now inspecting the derived tail memory, reopening both stores and preserving full sentinel memory plus `last_recalled_at`. Four new negative reversal/replay authority cases remain under independent test ownership.
- Root's public MCP supersession regression passed 1/1 (`/tmp/lattice-supersession-cas-sep13.log`): a replacement changed, invalidated, moved to foreign repository authority, or bound to another checkout after proposal creation cannot supersede the source; decision stays pending and no audit event commits.
- Latest warning-clean Python checks passed 35 evaluator +8 fixture +5 supersession tests. Hook package, installer and worth-it harness tests passed (20 worth-it checks). Scoped diff whitespace check passed.


### Canonical producer integration gate

- Full workspace run `/tmp/lattice-workspace-sep13-evolution-final.log` failed in core: **1,245 passed,17 failed,37 ignored**. Fifteen failures showed proposal producers still writing the former runtime database before canonical decisions; two supersession fixtures lacked required replacement proof. Independent review confirmed production verification and deterministic producers shared these gaps. The earlier focused recovery pass did not establish end-to-end integration.
- Daemon-only run `/tmp/lattice-daemon-sep13-evolution-integration.log` failed library: **350 passed,2 failed,1 ignored**. Session-generated proposals lacked explicit authority evidence; root now binds trusted repository/checkout/EventCapture branch at creation. No daemon binary pass is inferred from this stopped run.
- Sol core owner is replacing producer/decision APIs with explicit canonical MemoryStore and trusted authority. Pending job/proposal creation must be transactional and support a queue connection to the same canonical file without duplicate job inserts or unverified upserts. Verifier evaluation used by the existing atomic verification binding must not create a disposable parallel proposal.
- Root deterministic producers now capture replacement ID/hash, use explicit scan authority, and derive supersession from the structured field alone; redundant backwards `supersedes` links were removed. Terra owns deterministic test adaptation and linked-target reversal regression. Root owns daemon caller integration; Sol independently reviews these changes.

- Terra completed released fixture migrations: existence9/9, incremental5/5, deterministic10/10, evolution acceptance11/11, core lib test compilation passed. Seven opt-in concurrency tests compile but were not rerun in this handoff. New deterministic scope filters leave no foreign-checkout/branch proposal rows; linked-target reversal preserves unrelated graph state.
- Root queued verification now requires explicit checkout authority and filters candidate jobs before claiming. VerificationCommitBinding includes checkout+branch, rechecked under the core immediate transaction alongside target/observation CAS. Independent Sol source review accepted this boundary; new root queued-authority regression awaits execution. The old authority-free verification mutation wrapper was removed.
- Runtime-only APIs were replaced with mandatory canonical store/authority arguments. Root daemon cargo check passed before the latest verification binding changes (`/tmp/lattice-daemon-authority-check-sep13.log`); fresh focused daemon compilation is running.
- Existing episode refresh producers still emitted flat Memory JSON while apply requires a canonical snapshot. A shared canonical episode-state builder is being integrated; no successful episode update/replay is claimed yet.

- Queued verification focused suite passed3/3 (`/tmp/lattice-queue-authority-accepted-sep13.log`). The first new test exposed scope-only exact reads excluding even a valid bound checkout; the store now has an explicitly checkout-aware scoped read used by verify/explain. Foreign jobs remain untouched and current jobs complete without recall renewal.
- Daemon integration rerun `/tmp/lattice-daemon-sep13-canonical-final.log` passed352 library tests with1 failure/1 ignored; the remaining failure was an assertion reading the superseded flat episode-state ID. Root updated it to canonical `memory.id` and extended it to actual episode create→apply→second event window→refresh→public remember apply. This workflow passed1/1 (`/tmp/lattice-episode-refresh-sep13.log`). A new full pass is still required.
- Continued independent Sol source review accepted canonical producers, guarded same-file job promotion, creation-time source/replacement authority within transaction, verification checkout/branch commit checks, canonical review admission, and truthful failure-event branch metadata. Reviewer performed source inspection/diff-check only; full test acceptance remains coordinator-owned.

### Final bounded-expiry and source-selection gates

- Full workspace `/tmp/lattice-workspace-sep13-canonical-full.log`: core1266/1266, daemon library354/354 and CLI integration7/7 passed; daemon binary535 passed,2 failed. Both failures expose captured main-branch facts presented under feature/unknown worker authority. Root owns session-digest source selection and runtime fixtures; filter before provider invocation, retain commit-time authority checks.
- R12 reopened by independent Sol audit: stale marking is unbounded and each purged memory scans every consolidation proposal JSON payload. Required replacement is bounded stale transitions plus transactionally indexed proposal references and restartable row/byte-budget historical backfill. Purge stays blocked until all historical references are accounted for; oversized legacy data must remain preserved with actionable diagnostics.
- Ownership: `efficacy_scoring_v2` (gpt-5.6-sol) implements proposal reference schema/module, narrow canonical validation/init, retention integration and memory-retention docs. `evolution_acceptance_tests` (gpt-5.6-terra) owns a separate retention acceptance test module. `evolution_independent_review` (gpt-5.6-sol) independently reviews migration, recovery, authority and bounded work. Root owns daemon integration, session source filtering, tracker and final acceptance. Implementation and independent tests/review are pending.

- Session source authority focused suite6/6 and runtime16/16 passed. Deeper workflow inspection found oldest-capture claiming was disconnected from newest-fact loading. Root replaced it with exact delivery selection and canonical per-capture completion. Expanded runtime20/20 passed (`/tmp/lattice-session-atomic-recovery-sep13.log`), including actual three-capture provider inputs and branch switch, crash after canonical commit before runtime lease release, rollback of a two-proposal batch on receipt failure, and queue-capacity recovery without discarding pending work.
- Root now bounds scheduler source scans to indexed256-row pages (two at wrap), adds the supporting canonical delivery index, and ignores old runtime watermark completion claims. Fresh cursor-integrated tests pending. Completion receipts cascade with transport; no separate unbounded receipt cleanup exists. R12 staged purge/semantic dependency indexing remains under implementation and independent review.

### Bounded maintenance integration milestone

- R12 now uses indexed proposal/checkpoint dependencies, metadata-first historical payload limits, bounded backfills, ID scan cursors and staged dependency cleanup. Review found a recall-between-pages race; first cleanup now commits an irreversible purge fence, invalidation, receipt and restore floor. Terra's13-test acceptance pass proves rejected ACK/update/replay and reopened cleanup, alongside VM-step bounds and oversized recovery. Additional receipt-counter/admission tests are being finalized.
- Root independently checked bundled SQLite source: a single Count opcode traverses B-tree pages (`sqlite3BtreeCount`), so constant VM steps did not establish bounded receipt accounting. The implementation now maintains an exact singleton count with restartable64-row historical accounting and transactional mutation triggers. Pending review admission similarly uses a maintained indexed projection; no hot admission JSON scan. Independent review remains the acceptance gate.
- Root session suite passed23/23 (`/tmp/lattice-session-final-canonical-sep13.log`): exact capture selection, bounded scheduler cursor, source-state/evidence CAS, atomic multi-proposal receipt transaction, provider-free recovery after commit, terminal completion surviving a separate runtime-state failure, and capacity retry. The new concurrent last-slot regression passed1/1 (`/tmp/lattice-session-concurrent-admission-accepted-sep13.log`); its first fixture attempt lacked a required decision value and was corrected.
- Same-owner continuation scheduling now uses ten seconds (or a shorter configured interval) for unfinished bounded work and returns to the normal interval when complete. Twenty repeated maintenance calls at an unchanged clock cannot make a memory stale early; focused test passed1/1 (`/tmp/lattice-retention-cadence-final-sep13.log`). No additional expiry scheduler or tick-based aging was introduced.
- Full workspace run `/tmp/lattice-workspace-sep13-bounded-final.log` started against the integrated source. Final release, private delivery smoke v2, fresh storage benchmark and actual paired efficacy remain pending. No shared services restarted.

- Independent Sol final source review accepted R12 and session recovery with no remaining blocker. Reviewer actually ran retention acceptance18/18, retention units9/9 and review queue9/9; diff whitespace check passed. Reviewer withdrew an initial constant-VM COUNT inference after root verified actual SQLite B-tree page traversal; maintained counter replacement is the accepted implementation.
- Terra's expanded18-test acceptance includes130-receipt migration over64-row pages/reopen, concurrent insert/delete accounting around the cursor, same-key UPSERT cardinality, pinned receipts across age/count cleanup, and admission selectivity behind1,500 completed plus1,500 foreign proposals. Test-only exact COUNT provides an oracle; VM ceilings are not a hardware wall-clock SLA.
- All seven opt-in hardening concurrency tests passed on integrated source (`/tmp/lattice-concurrency-sep13-bounded-final.log`), including compaction during writes, concurrent writers/read snapshots, memory/link/access/session concurrency, and consolidation replay/idempotent apply. No selected concurrency case remained ignored.


### Integrated verification and remaining measurement repairs

- Full supported workspace run `/tmp/lattice-workspace-sep13-bounded-final.log`
  passed **2,194 tests**: core1,288, daemon library354, daemon binary545, CLI7;
  zero failures and39 opt-in tests ignored. Release build passed, SHA256
  `c0b41e33d6dd5358eff81ac90599b5b7b2919a6cd43beac8a5c7a7d593e7f91d`.
- Private delivery smoke v2 passed against that release, warning-clean:
  `/tmp/lattice-private-delivery-smoke-sep13-bounded-final.json`. It proves
  actual Markdown/JSON delivery, authority-bound acknowledgement, attempted
  delivery versus recall, replay idempotency, canonical feedback and public
  supersession through fresh daemon reads. Controlled-clock Rust tests provide
  expiry evidence; the smoke does not forge elapsed time.
- Serial locally runnable opt-in verification:31 passed, one failed, seven
  unavailable. The seven require absent private corpora (`/home/pete/rmm_server`
  and `/home/pete/cadres/...`). The large-repository run used its default capped
 20-file/1,000-event mode; it does not establish250,000-file performance.
  Logs: `/tmp/lattice-ignored-sep13-*.log` plus the concurrency log above.
- The one failed opt-in scorecard measured internal core reports against a
  historical2,000-byte public-response ceiling. Ranking checks passed, but that
  measurement did not cross the public render boundary. Replacement public
  `tools/call` tests then found a **real new defect**: tiny JSON requested260
  tokens and delivered an estimated805. Current workflow payload nesting was
  not fully handled by the old trimming function. Sol owns a complete public
  budgeting fix; no lowered threshold or passing payload claim is accepted.
- Independent evaluator review found missing receipt enforcement, incomplete
  stale-output inspection, missing artifact/model/settings freeze, incomplete
  writable-file/HEAD checks, briefing-arm contamination and unverified explicit
  recall for controls. Sol is repairing these contracts and regression coverage
  before actual paired trials. Harness tests are not evidence of agent benefit.
- Final storage measurements and paired agent trials wait for the public
  payload fix and frozen artifact. Historical storage results remain valid only
  for their recorded binary; no fresh-result or efficacy claim is made. Native
  Windows execution and remote CI have not run. No shared service restart or
  live-data mutation has occurred.

- Final supporting harness rerun: storage measurement16/16, protected hook
  package, installer idempotency and worth-it20/20 checks passed. Logs are
  `/tmp/lattice-{storage-harness,hooks,install,worth-it}-sep13-final-verification.log`.
  Six current guides resolved all23 local file targets (anchors not checked).
  Generated Rust benchmark outputs are documented under `daemon/target/`;
  checked-in baseline evidence remains unchanged.

- Root independently matched R5 physical-reclamation evidence to the full passing
  suite: `events::store_tests::physical_reclamation_is_bounded_and_reader_backlog_is_visible`
  holds a real SQLite reader, exposes WAL backlog, releases at most the requested
  pages, then verifies the database shrinks after reader release. R6 exact-path
  retrieval behind10,000 newer irrelevant/stale/wrong-branch rows also passed in
  that named full run. These are executed regressions, not source-only inference.
- Initial public payload repair passed its two fixed-fixture tests and nine
  existing workflow tests; root review rejected the claimed final freeze because
  a fixed metadata reserve does not prove the cap after receipts and Markdown
  rendering. Writer is strengthening final-render enforcement and edge-budget
  coverage; `public_budget_review` (gpt-5.6-sol) independently reviews that boundary.

- Public boundary regressions exposed a second UTF-8 panic in remediation-token
  query parsing (`get_task_memory.rs`), beyond the renderer's truncation helper.
  Both fixes are in the response writer's scope and independent Sol review.
  Root and reviewer rejected unconditional removal of full-mode memory
  projections: details that fit with their receipt must survive; budget preview
  must settle the projection before durable receipt binding.
- Evaluator's first repaired50-test pass still rejected the actual public
  receipt's `ack_required` field. Root reproduced this directly with the prior
  private smoke artifact. Validator and tests now require an actual-shaped
  public receipt rather than a narrower invented fixture. Additional control
  preflight/server identity and mutation-freeze regressions remain owner work.

### Public response and evaluator acceptance checkpoint

- Independent Sol final review accepted response budgeting and delivery binding.
  Final wire preview includes receipt and metadata costs; full responses that
  fit preserve their details; dropped lessons leave no returned receipt or
  derived summary. Unicode truncation and remediation-token parsing are safe.
  Writer's focused tests passed: payload3/3, workflow9/9, render7/7, Unicode1/1;
  the obsolete internal-size scorecard is replaced by public wire tests while
  its core ranking assertions remain and passed.
- Root reran the final evaluator contracts:38/38, fixtures8/8, supersession5/5,
  all warnings-as-errors. Logs: `/tmp/lattice-efficacy-{harness,fixtures,supersession}-sep13-frozen-audit.log`.
  Root also verified normalization of the actual smoke artifact's public
  receipt. Model/settings, binary/runner/fixture hashes, writable files and
  matched Git revision are checked before/after invocations; controls enforce
  their assigned intervention. These51 checks prove harness behavior, not
  agent efficacy.
- Root started full workspace integration on frozen production source:
  `/tmp/lattice-workspace-sep13-public-budget-final.log`. Final release, private
  smoke, storage measurement and paired tasks follow; no acceptance outcome
  for those pending runs is inferred.

- First full response-budget integration run passed core1,288 and CLI7, but
  two daemon test cases failed in both targets because they expected full
  audit projections under default output budgets. Tests now explicitly request
  `budget: full, max_tokens: 4000`; result-selection `mode: full` is a separate
  documented control. Their original assertions remain; focused reruns passed.
- Final full workspace rerun **passed2,202 tests**, zero failures,39 ignored:
  core1,288, daemon library358, daemon binary549, CLI7. Log:
  `/tmp/lattice-workspace-sep13-public-budget-accepted.log`. Release rebuild
  started; no fresh binary hash or measurement claimed before completion.
- Root found ordinary Python verification left a `__pycache__` file in the
  earlier actual producer audit. The evaluator's stricter writable guard remains
  unchanged; the common agent environment/prompt is being made bytecode-free
  before the final run. This prevents incidental artifacts rather than hiding
  unapproved edits after execution.

- Release built successfully in38.66 seconds, SHA256
  `1eb5d32d675120f83a2e287857c0f9d121fc943af7c0868f247a8f2458a52396`.
  Private smoke `/tmp/lattice-private-delivery-smoke-sep13-public-budget-final.json`
  failed its JSON identity validator after successfully checking actual default
  Markdown delivery. The raw response carries a qualified external ID inside
  typed `memory_id.ulid`; the harness handled only directly encoded string IDs.
  Exact content and scoped receipts are present in the retained raw responses.
  Sol is correcting typed identity validation with cross-field authority checks;
  independent Sol checks the production representation. No smoke pass or efficacy
  result is claimed for this failed run.

### Context expansion lifecycle gate

- Corrected private smoke passed on release
  `1eb5d32d675120f83a2e287857c0f9d121fc943af7c0868f247a8f2458a52396`:
  `/tmp/lattice-private-delivery-smoke-sep13-public-budget-accepted.json`, empty
  warning log. Qualified IDs inside typed MemoryId are legitimate router output;
  adapter/evaluator now validate their authority instead of rejecting the wrapper.
- Root inspection and independent Sol review then verified a distinct R7/R8/R12
  blocker: public `context(mode: expand)` reads persisted `cached.seed.memories`,
  validates only graph epoch, and returns old payload with no canonical memory
  lookup or delivery receipt. Purge/supersession does not change graph epoch.
  The passing smoke did not test this path and therefore cannot close expiry
  acceptance for expansions.
- Sol `efficacy_scoring_v2` owns reference-only context cache migration and
  canonical scoped expansion/receipts in mcp/context_cache. Terra
  `evolution_acceptance_tests` owns new public lifecycle regressions in a separate
  module. Sol `public_budget_review` independently reviews authority, lifecycle,
  persistence and delivery. Coordinator integrates and reruns release workflows.
  Existing navigation behavior must survive when memory storage is unavailable;
  missing/invalid memory indices must not silently substitute another record.

- Root extended private smoke to protocol v3: actual qualified memory expansion
  and acknowledgement, then rejection of the same predecessor handle after
  public supersession and after restart. It intentionally failed against the
  prior1eb5 release (`/tmp/lattice-private-delivery-smoke-sep13-expansion-baseline.json`):
  the raw response returned cached memory without authority/receipt, and even
  misclassified the qualified target as a symbol before falling back to a cached
  memory. This is executed failure evidence, not just source inference.
- Independent Sol source review accepts the replacement implementation: reference-only
  cache with immediate legacy payload rewrite; exact numeric/qualified reference
  resolution; current scoped canonical read; complete response fit before atomic
  full-state/applicability/lifecycle CAS and receipt persistence. Explicit stale
  discovery remains available through `recall`; expansion has no cached fallback.
  Terra's remaining public lifecycle/restart/budget/scope cases and full integrated
  release verification remain required before this gate closes.

### Expansion final coverage checkpoint

- Root inspected canonical reference resolution, complete-state delivery binding,
  and final fit-before-attempt ordering. Terra's focused public lifecycle run
  passed 8/8 (`/tmp/lattice-context-lifecycle.log`), including actual purge and
  store/cache/handler reopen. Sol reports cache/migration 9/9, atomic CAS 1/1,
  fallback seed-reference 1/1, and navigation compatibility 2/2. Full integration
  remains pending.
- Configured-organization coverage is not blocked by private runtime visibility:
  existing inline tests can inject `SharedMemoryRuntime`. Sol owns that bounded
  test extension. Root's smoke v3 harness contract suite passed 41/41, but the
  fresh release smoke and actual paired outcomes have not yet run.

- Configured-organization public expansion test passed (1/1), proving complete
  canonical lesson/qualified ID, organization receipt and ACK, foreign authority
  rejection and supersession rejection without extra attempts. It exposed raw
  bundle compression dropping qualified identity. Root rejected the first fix's
  positional join: highlight and compressed-memory filters differ, so positions
  are not a safe identity relation. Sol now preserves identity at the core
  compression layer and removes the positional join; independent review and a
  final full rerun remain required.
- Read-only documentation audit mapped R1–R12 to current code/docs. Root corrected
  historical retrieval figures labeled current, snapshot maintenance described
  as future work, and the event schema version. These are documentation fixes,
  not new efficacy or live-data acceptance claims.

### Diagnostic navigation regression found by full integration

- `/tmp/lattice-workspace-sep13-expansion-final.log` completed with core 1,289
  passed, daemon library 369 passed/1 failed, binary 560 passed/1 failed, CLI
  7 passed, 39 ignored. Both failures are the same ranking-detail dereference
  regression, not a passing integration milestone.
- Old ranking diagnostics used synthetic memory payloads. Reference-only lesson
  caching correctly removed those payloads but broke diagnostic expansion. Sol
  owns a separate typed numeric diagnostic cache and public exact `relevance:`
  focus; Terra owns the existing regression adaptation and new public lifecycle
  tests; independent Sol reviews authority and hidden payload risks. Diagnostics
  must carry original retrieval scores only, no lesson content or receipt, and
  memory diagnostics must revalidate current authority/lifecycle.
- Core compression now preserves each value's own qualified memory identity;
  the unsafe positional join is removed. Independent Sol accepts that replacement
  after inspecting the divergent-filter/colliding-local-ID regression.

- Diagnostic replacement passed metrics 8/8 and public relevance lifecycle 3/3
  (`/tmp/lattice-metrics-surface.log`, `/tmp/lattice-context-relevance.log`). Root
  inspected these results and source; independent Sol accepted canonical checks,
  payload-free persistence, exact focus, complete delivered-text fit, and
  successful navigation-handle renewal without memory retention renewal.
- Sol's final cache/migration 10/10, core compression 1/1 and organization 1/1
  passed. Source/docs are frozen. Full workspace rerun started at
  `/tmp/lattice-workspace-sep13-expansion-accepted.log`; no fresh pass claimed yet.

- Final expansion/diagnostic full workspace rerun **passed 2,234 tests**, zero
  failures, 39 ignored: core 1,290, daemon library 373, binary 564, CLI 7. Log:
  `/tmp/lattice-workspace-sep13-expansion-accepted.log`. Release build started;
  final private smoke/storage/paired evaluation remain pending.

- Release build passed in 58.85 seconds; frozen SHA256
  `1080d27b4d11b2706922e1cceb6816f705f38ee8d190d81428ea97d5b8ae527b`.
- Private delivery smoke v3 **passed**, warning log empty:
  `/tmp/lattice-private-delivery-smoke-sep13-expansion-final.json`; audit
  `/var/folders/q2/g_4_z6w95r7chfc5dx6fhnz40000gn/T/lattice-private-delivery-smoke-ikp7ugnh`.
  It proves actual default Markdown and JSON delivery, complete canonical
  qualified expansion with receipt/ACK, forged/replay ACK handling, feedback
  after restart, public supersession and stale handle rejection across restart,
  and CLI idempotent apply. Controlled-clock purge remains separately proven
  by Rust/MCP tests.
- Fresh 100-worktree real-ONNX storage benchmark started on this frozen release
  with the existing fixed policy. No new resource/benefit result claimed before
  completion. Paired-agent trials follow this isolated timing measurement.

- Fresh storage benchmark **completed 100 worktrees and passed all 18 fixed
  checks** on release 1080d27. Raw artifact:
  `/tmp/lattice-storage-benchmark-sep13-expansion-final.json`, warning log empty.
  At 20 checkouts: parsed bytes 92.735%, embedding bytes 92.622% reused; ready
  query p95 32.845 ms; independent allocated GC delta 109,662,208 bytes. This
  differs from apply's 109,727,744-byte estimate; filesystem checkpoint/concurrent
  private writes are included. Cold/edit wall times are slower than the earlier
  run; no overall speedup claim. Luna `final_storage_report` owns the report
  update; root checks it against raw evidence.
- Corrected scripted smoke provenance (no independently graded producer claim)
  and reran v3: `/tmp/lattice-private-delivery-smoke-sep13-expansion-accepted.json`
  passed, warnings empty, same release hash. Prior behavior result is retained,
  not relabeled as agent efficacy. Evaluator suite reran 41/41 passed in 26.373s
  (`/tmp/lattice-efficacy-harness-sep13-final.log`).
- Actual preregistered paired v2 evaluation started: gpt-5.6-terra, low reasoning,
  three matched trials/seven tasks/three arms, frozen binary/runner/harness/fixture
  hashes. Artifacts `/tmp/lattice-efficacy-runs-sep13-v2`; requested final report
  `/tmp/lattice-agent-efficacy-results-sep13-v2.json`. No result claimed before
  execution and independent grading finish.

### Paired-run approval integration correction

- The first fresh paired run stopped truthfully at explicit retrieval. The
  actual MCP record has status failed and error `MCP tool call requires approval,
  but approval policy is never`; the evaluator rejected it. Failed artifact:
  `/tmp/lattice-efficacy-runs-sep13-v2/run-spqc77sc/run-failure.json`. No completed
  comparison or efficacy claim is derived from its partial results.
- The installed CLI documents `--approve-for-me` for workspace-write sandboxing
  plus automatic approval review. Root changed the common producer/consumer
  invocation and preregisters that permission mode; no bypass or ignore-rules
  flags, tool-wide approval overrides, or relaxed delivery validators were added.
  Independent Sol reviewed these boundaries.
- First connectivity probe exposed mutually exclusive CLI flags: approve-for-me
  already selects the sandbox, so the explicit sandbox flag was removed. The
  corrected actual private probe **passed** one public recall under automatic
  review, exit 0, status completed/no error. Log:
  `/tmp/lattice-reviewed-recall-probe-v2.log`; audit
  `/var/folders/q2/g_4_z6w95r7chfc5dx6fhnz40000gn/T/lattice-reviewed-recall-probe-vxzisqmq`.
  This is connectivity evidence, not a paired efficacy outcome.

- Final reviewed-run harness checks passed 41/41 in 26.502 seconds
  (`/tmp/lattice-efficacy-harness-sep13-reviewed-final.log`). Fresh actual paired
  run started with unchanged binary1080d27 and newly frozen common approval mode:
  `/tmp/lattice-efficacy-runs-sep13-reviewed`, output requested at
  `/tmp/lattice-agent-efficacy-results-sep13-reviewed.json`.
- Root checked Luna's report against raw artifacts and corrected two historical
  MiB conversions: e03 100-checkout allocation 168.371 MiB and reclaimed
  104.578 MiB. Prior raw byte values were correct and remain preserved.

- Root verified the restarted run's actual explicit MCP record: `recall`, status
  completed, no error. The first three-arm task finished and the second fixture
  started. This confirms the approval integration is unblocked, not a completed
  efficacy comparison. Final relative-file link check: 17 current documents,
  22 targets, zero missing; anchors were not checked.

## Integrated acceptance ledger — release 1080d27

This ledger describes code and isolated verification. It does not claim that
retained live stores were migrated or that the shared daemon was deployed.

| Review finding | Integrated replacement | Ownership / review | Current evidence |
| --- | --- | --- | --- |
| R1 | Classified open failures; exclusive repository migration/recovery ownership; truthful unavailable durable writes | Sol implementation and independent Sol review; coordinator integration | Full workspace; recovery, contention, rollback and operator fixtures; [recovery](../memory-recovery.md) |
| R2 | Auditable, transactional proven-identity migration; foreign provenance remains isolated | Sol implementation/review | Identity migration, crash/reopen and scope regressions; [migration](../memory-identity-migration.md) |
| R3 | Checkout ownership, leases, bounded accounting, resource admission and resumable reference-aware GC | Coordinator/Sol implementation; independent Sol review | 100 indexed/edited worktrees, 97 unleased bundles reclaimed, 3 active leases preserved; [storage report](../reports/2026-09-12-storage-benchmark.md) |
| R4 | Shared immutable parse/body/embedding/history objects, committed manifests and transactional graph deltas | Coordinator/Sol implementation; independent Sol review | Unchanged cold paths require zero source reads; divergent checkout graph isolation and delta/rebuild equality; 18 storage gates pass |
| R5 | Transactional orphan spill reclamation, graph-only checkpoints and bounded snapshot retirement | Coordinator/Sol implementation/review | Event rollback/concurrency/held-reader physical reclaim; snapshot recovery and scheduler fixtures; [event storage](../event-storage-reclamation.md) |
| R6 | Indexed authority/applicability/lifecycle retrieval before bounded ranking | Terra integration, Sol review, coordinator acceptance | Exact-path lesson survives 10,000 newer irrelevant/stale/wrong-branch rows; public delivery smoke; [retrieval](../memory-retrieval.md) |
| R7 | Distinct acceptance/evidence/behavioral trust; checkout-generation-bound verification and complete-state receipt fencing | Sol implementation/review | Verification CAS/failure/ordering tests; repository and organization public expansion tests; [trust](../memory-trust.md) |
| R8 | Derived navigation/diagnostic caches separated from canonical reusable lessons; atomic capture, feedback and evolution | Coordinator/Sol/Terra implementation; independent Sol review | Capture retry/crash/queue tests; public feedback and supersession across restart; [capture](../lesson-capture.md) |
| R9 | Shared workspace boundary policy and pinned filesystem operations | Sol implementation/review | Traversal, ignored paths, symlink/root replacement, watcher and recovery fixtures; [boundaries](../workspace-boundaries.md) |
| R10 | Indexed SQLite telemetry, bounded imports/retention and content-free summary counters | Terra implementation; coordinator integration | Telemetry failures preserve briefing availability; indexed attribution and restart feedback; [telemetry](../telemetry-storage.md) |
| R11 | Frozen, independently graded paired evaluation; real returned content/identity/receipt requirements | Sol harness/review; coordinator actual execution | Harness 41 pass; private smoke v3 pass; actual three-trial run still active, no efficacy acceptance |
| R12 | One repository-owned 90/180-day recall lifecycle; ACK-only renewal; stale discovery; bounded staged purge and replay/restore fences | Sol implementation/review, Terra public tests, coordinator integration | Controlled-clock purge/reopen, receipt/CAS, fixed-clock repeated-sweep tests and public expansion no-resurrection; [retention](../memory-retention.md) |

Local integrated verification: Rust **2,234 passed, 0 failed, 39 ignored**.
Earlier separate opt-in execution covered 32 locally runnable cases; seven
private-corpus cases remain unavailable, and capped large-repository fixtures
are not claimed as full-scale results. Hook, installer, worth-it, storage-harness,
fixture and supersession checks are recorded above. Remote CI/native Windows
runtime remain unrun. No shared service restart or live operator mutation was
performed. The implementation remains uncommitted with unrelated user changes
preserved; no staging/commit claim is made.

### Empty-result attribution gate found by real consumers

- The reviewed run reached ten consumer responses, then stopped at stale-control
  explicit recall. Raw response had count0/memories[] and a completed MCP call,
  but `memory_attribution.error` reported the core 1–256 accesses invariant.
  The evaluator correctly rejected the error-bearing response. Artifact:
  `/tmp/lattice-efficacy-runs-sep13-reviewed/run-v164j70d/run-failure.json`.
- Terra's `attach_memory_attribution` early return applies only to successfully
  extracted empty attributable sets, before runtime checks/retrieval-event/journal
  work. Nonempty failures and the core invariant remain unchanged. Focused public
  empty/stale/unavailable-runtime test1/1 and nonempty attribution test1/1 passed
  (`/tmp/lattice-empty-attribution.log`, `/tmp/lattice-nonempty-attribution.log`).
  Root inspected the guard; independent Sol accepted its boundary.
- Root extended private smoke to v4 with actual pre-capture empty recall and
  zero canonical attribution/access/delivery row checks. Full workspace rerun
  started; fresh release/smoke/storage and complete paired trials follow.

### Seven-fixture transport preflight

- Empty-result replacement full workspace **passed 2,236 tests**, zero failures,
  39 ignored (core1290/library374/binary565/CLI7), log
  `/tmp/lattice-workspace-sep13-empty-final.log`. Release build passed37.07s, SHA
  `1d1d7f712b6d3206b744bd80b6965c1e596e8e1273022816362ea6dfac7fc601`.
- Private smoke v4 **passed**, clean empty recall and zero attribution/access/
  delivery rows included: `/tmp/lattice-private-delivery-smoke-sep13-empty-final.json`.
  Fresh100worktree realONNX storage run passed18/18 fixed gates: queryp95
  53.961ms, reclaimed109,658,112bytes, parsedbyte reuse92.735%/embedding92.622%
  at20; `/tmp/lattice-storage-benchmark-sep13-empty-final.json`. ResourceWarning
  checks are clean; the combined run log contains the normal output-report path.
- Root scripted a private seven-fixture transport check before restarting model
  calls: `/tmp/lattice-fixture-delivery-preflight.py`, report
  `/tmp/lattice-fixture-delivery-preflight.json`. It uses no model calls and makes
  no producer/efficacy claim; capture provenance explicitly says scripted fixture.
  Six cases passed, including 10,000 noise rows and clean stale/control misses.
- Branch-policy prepare_change failed exact-content delivery: a116-character
  corrective lesson became `Revision v2 replaces the earlier export r...` yet
  received a qualifying receipt. The final~1042-token JSON had room under its
  default2600 cap. Existing full-content restoration was Markdown-only. Sol owns
  the cross-render replacement/regressions; independent Sol reviews source and
  evidence. Root will repeat the matrix before any fresh actual paired run.

### Canonical full-content correction

- Root inspection found the first proposed restoration still relied on the
  longest pre-render projection and an ellipsis heuristic. Core highlight and
  compressed-memory builders already truncate at120characters; longer valid
  lessons would be omitted even when the final budget could hold them. Root
  withheld acceptance and assigned canonical scoped content loading, mutation
  fencing, long-content and legitimate-ellipsis regression coverage to Sol.
- Independent evaluator harness42/42 passed26.493s and storage harness16/16
  passed0.362s with ResourceWarnings treated as errors. Logs:
  `/tmp/lattice-efficacy-harness-sep13-content-final.log` and
  `/tmp/lattice-storage-harness-sep13-content-final.log`. These are harness
  checks, not paired agent outcomes. Free disk remains77GiB.

### Canonical delivery integration review

- Sol replaced snippet heuristics with authority-scoped canonical reload and a
  bounded grouped receipt CAS transaction. Payload scorecard5/5 and grouped
  rollback1/1 passed; independent Sol inspected source and accepted authority,
  lifecycle, batch atomicity, and receipt-withholding boundaries. Unreturned
  attempts across separate authorities remain nonqualifying and cannot renew
  retention.
- Root added a separate finalizer test module. Initial3-case run found stale
  candidate prose surviving an eligible trust mutation (2pass/1fail), log
  `/tmp/lattice-canonical-finalizer-sep13.log`. Root now removes memory-derived
  candidate summaries before rendering canonical entries. All3 cases pass in
  `/tmp/lattice-canonical-finalizer-sep13-v2.log`: current trust metadata,
  supersession omission, and graph availability during canonical read failure.
  These are deterministic finalizer interleavings, not transport/model trials.
- Root corrected README's superseded512-byte inline limit and the diagnostic
  handle-lifetime documentation: successful diagnostic expansion renews only
  navigation lifetime, never memory retention. Canonical workflow semantics are
  documented in the render contract and retention note. Full workspace is
  running: `/tmp/lattice-workspace-sep13-canonical-final.log`.
- Latest measured storage report now leads1d1d7f7 evidence. Root checked byte/MiB
  arithmetic and corrected historical1080 storage to168.758MiB. The report
  explicitly remains measured-release evidence while this correction is under
  integration, not final program or efficacy acceptance.

### Full-suite canonical follow-through

- Restricted full execution exposed2product regressions in both daemon targets
  plus12socket-permission fixture failures. Root restored safe navigation
  handle/focus fields during canonical rebuilding and the existing degraded
  all-missing response contract, keeping the regression assertions unchanged.
  Log `/tmp/lattice-workspace-sep13-canonical-final.log` is a failed run.
- Supported execution with private loopback fixtures then passed2,246 tests:
  core1290/library379/binary570/CLI7, zero failures,39ignored. Log
  `/tmp/lattice-workspace-sep13-canonical-accepted.log`.
- Independent review found foreign-qualified candidates lacked the missing
  marker. Root added it to both authority rejection branches and empty-content
  branches, plus a focused foreignrepo/foreignorg finalizer regression. Sol
  accepted source; final full rerun is
  `/tmp/lattice-workspace-sep13-canonical-freeze.log`. No source edits are active.

- Final frozen source full workspace passed **2,248 tests**, zero failures,
  39ignored: core1290/library380/binary571/CLI7. Log
  `/tmp/lattice-workspace-sep13-canonical-freeze.log`. Four new finalizer
  interleaving/authority regressions are included in both daemon targets.
  Release build log: `/tmp/lattice-release-sep13-canonical-freeze.log`.

### Final binary and usage constraint

- Release build passed in1m03s. SHA256:
  `cfb306cc811ef03c0e87ef4f9deffb440e59113f057a2c8ef6a16ce3784a2b3b`.
- Private delivery smokev4 PASS:
  `/tmp/lattice-private-delivery-smoke-sep13-canonical-freeze.json`, audit
  `/var/folders/q2/g_4_z6w95r7chfc5dx6fhnz40000gn/T/lattice-private-delivery-smoke-utvoeg98`.
- Scripted seven-fixture public delivery preflight PASS7/7, zero model calls:
  `/tmp/lattice-fixture-delivery-preflight-content-final.json`, audit
  `/var/folders/q2/g_4_z6w95r7chfc5dx6fhnz40000gn/T/lattice-fixture-delivery-preflight-xq9r302x`.
  Exact branch correction is now delivered; the earlier failed6/7 artifact
  remains preserved. These are delivery checks, not agent usefulness outcomes.
- User reported little usage remaining and requested speed. Coordinator is
  finishing the final storage measurement and handoff, holding the fresh87-call
  paired agent evaluation to preserve usage. No credential blocker is claimed.
  R11 efficacy and whole-program release acceptance remain incomplete.

- User requested a compiled build on PATH for deployment and testing by the
  Beacon agent. Verified `command -v lattice` resolves
  `/Users/pete/.local/bin/lattice`, already symlinked to this checkout's
  `daemon/target/release/lattice`. Both resolve to SHA256cfb306cc above.
  Artifact format is Mach-O64-bit arm64 (macOS Apple Silicon); a Linux Beacon
  host requires a native build from source. `--version` is unsupported and
  returned CLI usage; executable identity was verified by checksum instead.
  No PATH installation mutation, shared restart, or deployment was needed or
  performed. Beacon testing remains user-owned, not claimed completed here.

### Final storage and agent-workflow handoff

- Final100-worktree storage PASS18/18: queryp95 41.865ms, parsed-byte reuse
  92.735% and embedding92.622% at20, independently reclaimed109,682,688bytes
  from97 unleased bundles with3 active checkouts retained. Artifact:
  `/tmp/lattice-storage-benchmark-sep13-canonical-freeze.json`. Report updated
  with machine-derived metrics and previous measurements retained.
- User clarified long-running agents must use Lattice during their own work.
  Verified hooks and checkpoint support do not detect arbitrary task boundaries.
  Added durable AGENTS workflow and docs/agent-workflow.md with a100-task
  Beacon acceptance exercise: direct calls distinguished from hooks, scoped
  refresh/recovery, independently verified corrections, and trace inspection.
  This is an instruction/acceptance contract, not a claim of enforced adoption.
- New documentation relative links passed (6documents/23targets); no binary
  source changed after final build. Remaining tests belong to the explicit
  usage-constrained Beacon handoff; no benchmark benefit is fabricated.

### User-authorized complete project installation

- User requested `lattice install --workspace <project>` install MCP, hooks,
  and AGENTS.md/CLAUDE.md workflow instructions together. This extension is
  in progress; the earlier cfb306cc binary does not support the new default.
- Fixed interface: install_project(workspace, InstallPaths) returns changed
  paths; root owns CLI/default selection, docs, binary/public acceptance.
  Sol efficacy_scoring_v2 owns new install_project.rs and TOML dependency;
  independent Sol public_budget_review reviews filesystem/config boundaries.
- Complete default creates/reconciles six files; explicit old targets retain
  focused behavior. Official Codex MCP docs verified project .codex/config.toml
  requires trustedproject; installer must preserve existing approvalpolicy.
- Review rejected initial marker, symlink, staging-cleanup and concurrency
  gaps. Replacement uses pinned SecureDir operations and a separate bounded
  installer lock, not the live repository-memory owner lock. Final tests and
  release binary remain pending; no live Beacon installation has been run.

### Complete installer accepted and built

- Root integrated the default/all CLI route, six-file registration, and verified
  both hook targets plus MCP through existing `--verify` fixtures. Root closed
  staging identity cleanup, exact partial-publication reporting, and pinned
  preflight reads after the first review. Independent Sol accepted final source.
- Final focused installer suite:25passed/0failed, log
  `/tmp/lattice-project-install-tests-final.log`. Includes malformed config and
  markers, symlink state/targets, concurrent target creation, partial retry,
  existing memory-owner lock coexistence, and default CLI verification.
- Release build passed41.38s, `/tmp/lattice-project-install-release.log`.
  Current PATH binary SHA256:
  `20b74c8485a1365489963f3b0aebf49af42dc4cdbb77702d297670868684cd81`.
- Actual PATH `lattice install --workspace <private project with spaces>`
  passed six-file verification, repeat byte-idempotency, unrelated settings/
  instructions preservation, and malformed-preflight no-change checks.
  Script `/tmp/lattice-install-project-e2e.py`; audit
  `/var/folders/q2/g_4_z6w95r7chfc5dx6fhnz40000gn/T/lattice-install-project-e2e-10o7s0t6`.
  No live Beacon files or shared daemon were modified. User can retry the
  exact requested command. Earlier storage/delivery measurements remain
  attributed to cfb306cc; this installer-only build is not claimed to have
  rerun the100-worktree or agent-efficacy benchmark.

### Authorized live daemon cutover and Beacon verification

- User explicitly authorized restarting the shared daemon after status/process
  inspection identified PID80791, startedSeptember3, serving an older index
  containing gitignored .agent-work paths. Sent SIGTERM only to that daemon.
  Rebuilt daemon now runs as PID40469, startedSeptember13 10:53:41 local, using
  installed build20b74c84. No live memory deletion or manual cache removal ran.
- Beacon completed the fresh index: ready,2,290 graph files,26,483nodes,
  71,686edges; no parse failures, no partial flag, no queued/active jobs,
  healthy graph storage. Read-only SQLite verification found zero .agent-work
  file-index rows and zero .agent-work graph nodes. Canonical memory is available.
  Captured status: /tmp/lattice-beacon-status-after-restart.json; startup log:
  /tmp/lattice-shared-restart-sep13.log.
