# Feature Build Plan: 2026-05-16-cognitive-workspace-fork-build

This file is generated from `tasks.json`. Edit `tasks.json` when changing the
machine contract, then rerun the harness to refresh this plan.

## Task Graph

| Task | Status | Type | Model | Depends On | Title |
|---|---|---|---|---|---|
| `T01` | `complete` | `foundation` | `advanced` | - | Phase 0 — Fork-or-extend decision doc with evidence |
| `T02` | `complete` | `docs` | `balanced` | T01 | Phase 0 — Successor architecture overview + compatibility policy |
| `T03` | `complete` | `foundation` | `advanced` | T01, T02 | Phase 0 — Crate/module boundary plan + storage migration policy |
| `T04` | `complete` | `tests` | `balanced` | T01 | Phase 0 — Baseline benchmark capture (current Lattice workflows) |
| `R05` | `complete` | `review` | `advanced` | T01, T02, T03, T04 | Foundation review — Phase 0 decisions and baselines |
| `T06` | `complete` | `backend` | `advanced` | R05 | Phase 1 — Identity type system (File, Symbol, Doc, Section, Event, Memory, Handle) |
| `T07` | `complete` | `backend` | `balanced` | T06 | Phase 1 — Identity resolver with ambiguity diagnostics |
| `T08` | `complete` | `backend` | `balanced` | T07 | Phase 1 — Identity serialization in MCP payloads + legacy name compatibility shim |
| `T09` | `complete` | `tests` | `balanced` | T06, T07, T08 | Phase 1 — Identity resolution tests (renames, moves, duplicate names, branch changes, P99 budget) |
| `R10` | `complete` | `review` | `advanced` | T06, T07, T08, T09 | Backend review — Phase 1 identity substrate |
| `T11` | `complete` | `backend` | `balanced` | R10 | Phase 2 — Event model types (all 20 event kinds + envelope) |
| `T12` | `complete` | `backend` | `advanced` | T11 | Phase 2 — Append-only SQLite event tables + payload spillover + indexes |
| `T13` | `complete` | `backend` | `balanced` | T12 | Phase 2 — Event writer API + payload hashing + workspace scoping |
| `T14` | `complete` | `backend` | `balanced` | T12, T13 | Phase 2 — Event reader/query API (filter by task/session/workspace/branch) |
| `T15` | `complete` | `backend` | `advanced` | T13, T14 | Phase 2 — MCP/tool-call event capture wiring (all workflow tools) |
| `T16` | `complete` | `backend` | `advanced` | T12, T13, T14 | Phase 2 — Event log compaction snapshot + bootstrap-from-snapshot |
| `T17` | `complete` | `tests` | `balanced` | T11, T12, T13, T14, T15, T16 | Phase 2 — Event log tests (ordering, replay, corruption, scoping, P99 ≤5ms hot path) |
| `R18` | `complete` | `review` | `advanced` | T11, T12, T13, T14, T15, T16, T17 | Backend review — Phase 2 event log substrate |
| `T19` | `complete` | `backend` | `advanced` | R18 | Phase 3 — Memory schema redesign (classes incl CounterMemory + required fields) |
| `T20` | `complete` | `backend` | `balanced` | T19 | Phase 3 — memory_links + memory_evidence + memory_accesses + memory_scores tables |
| `T21` | `complete` | `backend` | `balanced` | T19 | Phase 3 — Memory stream taxonomy + scope enforcement helpers (deny-by-default) |
| `T22` | `complete` | `backend` | `advanced` | T19, T20, T21 | Phase 3 — Migration importer from existing Lattice memory rows |
| `T23` | `complete` | `backend` | `balanced` | T19, T20 | Phase 3 — Memory graph CRUD + verification status transitions + idempotent writes |
| `T24` | `complete` | `tests` | `balanced` | T19, T20, T21, T22, T23 | Phase 3 — Memory graph tests (links, contradiction, supersession, stale, replay) |
| `R25` | `complete` | `review` | `advanced` | T19, T20, T21, T22, T23, T24 | Backend review — Phase 3 memory graph storage |
| `R26` | `complete` | `review` | `advanced` | R10, R18, R25 | Contract gate — Identity ↔ Event Log ↔ Memory cross-layer contracts |
| `T27` | `complete` | `backend` | `balanced` | R26 | Phase 4 — Task intent classifier |
| `T28` | `complete` | `backend` | `balanced` | R26, T27 | Phase 4 — Anchor extractor + resolver (paths, symbols, errors, commands, APIs, config keys) |
| `T29` | `complete` | `backend` | `advanced` | T27, T28 | Phase 4 — Hybrid candidate retrieval (graph + docs + memories + events + working set) |
| `T30` | `complete` | `backend` | `advanced` | T29 | Phase 4 — Scoring model with diagnostic mode (all required signals) |
| `T31` | `complete` | `backend` | `balanced` | T29, T30 | Phase 4 — Compact response shaper + inclusion reasons + expansion handles |
| `T32` | `complete` | `tests` | `balanced` | T27, T28, T29, T30, T31 | Phase 4 — Retrieval benchmark suite + golden anchors + ranking tests |
| `R33` | `complete` | `review` | `advanced` | T27, T28, T29, T30, T31, T32 | Backend review — Phase 4 Retrieval V1 |
| `T34` | `complete` | `backend` | `advanced` | R33 | Phase 5 — Working memory state model + working_memory_checkpoints table |
| `T35` | `complete` | `backend` | `balanced` | T34 | Phase 5 — Working memory operations (retrieve, summarize, filter, pin, evict, expand, compress, checkpoint) |
| `T36` | `complete` | `backend` | `balanced` | T34, T35 | Phase 5 — MCP surface for working memory inspection + event capture for include/exclude reasons |
| `T37` | `complete` | `tests` | `balanced` | T34, T35, T36 | Phase 5 — Working memory tests (budgets, pins, eviction, checkpoint restore) |
| `R38` | `complete` | `review` | `advanced` | T34, T35, T36, T37 | Backend review — Phase 5 working memory |
| `T39` | `complete` | `backend` | `advanced` | R38 | Phase 6 — Consolidation job runtime + proposal model (no silent writes) |
| `T40` | `complete` | `backend` | `balanced` | T39 | Phase 6 — Synchronous session consolidation (episode summaries) |
| `T41` | `complete` | `backend` | `balanced` | T39 | Phase 6 — Deterministic jobs: duplicate detection, supersession candidates, demote unused |
| `T42` | `complete` | `backend` | `advanced` | T39, T40, T41 | Phase 6 — LLM-driven jobs: episode summary, procedure extraction, contradiction detection, failure-pattern extraction |
| `T43` | `complete` | `backend` | `advanced` | T42 | Phase 6 — LLM provenance (model + prompt hash + response hash) + queue bounds + cost budgets |
| `T44` | `complete` | `backend` | `balanced` | T39, T42 | Phase 6 — Manual review queue for high-scope memory changes (repo + organization scope) |
| `T45` | `complete` | `backend` | `balanced` | T39, T40, T41, T42 | Phase 6 — Replay-safe execution + reversibility + provenance preservation |
| `T46` | `complete` | `tests` | `balanced` | T39, T40, T41, T42, T43, T44, T45 | Phase 6 — Consolidation tests (proposals, apply/reject, replay, queue bounds, failure events) |
| `R47` | `complete` | `review` | `advanced` | T39, T40, T41, T42, T43, T44, T45, T46 | Backend review — Phase 6 consolidation engine |
| `T48` | `complete` | `backend` | `balanced` | R47 | Phase 7 — Verification engine core (file/symbol/doc/test existence checks) |
| `T49` | `complete` | `backend` | `balanced` | T48 | Phase 7 — Exact-span evidence validation + content hashes |
| `T50` | `complete` | `backend` | `advanced` | T48 | Phase 7 — Branch/workspace scope enforcement (deny leakage) + negative tests |
| `T51` | `complete` | `backend` | `balanced` | T48, T49, T50 | Phase 7 — Time-bound expiry + incremental verification triggered by graph changes |
| `T52` | `complete` | `backend` | `advanced` | T48, T49, T50, T51 | Phase 7 — Stale surfacing in workflow bundles + label discipline (no trusted display of stale/contradicted) |
| `T53` | `complete` | `tests` | `balanced` | T48, T49, T50, T51, T52 | Phase 7 — Verification tests (stale never displayed as trusted, scope leakage blocked) |
| `R54` | `complete` | `review` | `advanced` | T48, T49, T50, T51, T52, T53 | Backend review — Phase 7 verification + freshness |
| `R55` | `complete` | `review` | `advanced` | R25, R33, R54 | Contract gate — Memory ↔ Verification ↔ Retrieval cross-layer |
| `T56` | `complete` | `backend` | `advanced` | R55 | Phase 8 — Redesigned prepare_change + plan_edit + trace_scenario + diagnose_failure |
| `T57` | `complete` | `backend` | `balanced` | R55 | Phase 8 — Redesigned get_context_capsule + get_docs_capsule + find_relevant_tests + impact_from_diff |
| `T58` | `complete` | `backend` | `balanced` | R55 | Phase 8 — New memory tools: get_task_memory + save_memory + propose_memory_evolution(action=apply|reject) |
| `T59` | `complete` | `backend` | `balanced` | R55 | Phase 8 — New memory tools: verify_memory + explain_memory (unified) + list_memory_conflicts |
| `T60` | `complete` | `backend` | `balanced` | R55 | Phase 8 — New tools: consolidate_session + get_memory_metrics + get_event_trace |
| `T61` | `complete` | `docs` | `advanced` | T56, T57, T58, T59, T60 | Phase 8 — Tool surface audit + final MCP reference doc (collapse overlapping tools per spec §MCP discipline) |
| `T62` | `complete` | `tests` | `balanced` | T56, T57, T58, T59, T60, T61 | Phase 8 — Workflow outcome recording integrated by default + composition tests |
| `R63` | `complete` | `review` | `advanced` | T56, T57, T58, T59, T60, T61, T62 | Backend review — Phase 8 Workflow Engine V2 |
| `R64` | `complete` | `review` | `advanced` | R63 | Contract gate — MCP tool surface schema regression |
| `T65` | `complete` | `backend` | `balanced` | R64 | Phase 9 — Metric collection module (all required signals from spec) |
| `T66` | `complete` | `tests` | `advanced` | R64, T65 | Phase 9 — Benchmark task fixture repository + golden anchors (cross-language) |
| `T67` | `complete` | `backend` | `balanced` | T65 | Phase 9 — get_memory_metrics + retrieval relevance surfaces wired through MCP |
| `T68` | `complete` | `backend` | `balanced` | T65, T66 | Phase 9 — Regression dashboard / CLI report |
| `T69` | `complete` | `tests` | `advanced` | T65, T66, T67, T68 | Phase 9 — Metrics regression coverage tests (success-criteria targets from spec) |
| `R70` | `complete` | `review` | `advanced` | T65, T66, T67, T68, T69 | Tests/Docs review — Phase 9 metrics + evaluation |
| `T71` | `complete` | `frontend` | `balanced` | R70 | Phase 10 — Extension review-panel scaffolding + JSON-RPC plumbing to new tools |
| `T72` | `complete` | `frontend` | `balanced` | T71 | Phase 10 — Memory inbox view + filters + status badges |
| `T73` | `complete` | `frontend` | `balanced` | T71 | Phase 10 — Promotion queue + contradiction queue with accept/reject (dialog modals) |
| `T74` | `complete` | `frontend` | `balanced` | T71 | Phase 10 — Stale memory view + evidence inspector + verification trigger |
| `T75` | `complete` | `frontend` | `balanced` | T71 | Phase 10 — Event trace view + retrieval explanation view |
| `T76` | `complete` | `frontend` | `balanced` | T71 | Phase 10 — Consolidation queue + indexing health + workspace graph health views |
| `T77` | `complete` | `tests` | `balanced` | T71, T72, T73, T74, T75, T76 | Phase 10 — Extension UI smoke tests (compile + lint + integration runner) |
| `R78` | `complete` | `review` | `advanced` | T71, T72, T73, T74, T75, T76, T77 | Frontend review — Phase 10 human review UI (UI-spec compliance) |
| `R79` | `complete` | `review` | `advanced` | R64, R78 | Contract gate — Extension ↔ Daemon review-UI MCP plumbing |
| `T80` | `complete` | `tests` | `advanced` | R79 | Phase 11 — Large-repo performance tests |
| `T81` | `complete` | `tests` | `advanced` | R79 | Phase 11 — Concurrency tests (parallel sessions, write contention, replay-safe) |
| `T82` | `complete` | `tests` | `advanced` | R79 | Phase 11 — Recovery + corrupted-event handling tests |
| `T83` | `complete` | `tests` | `balanced` | R79 | Phase 11 — Partial-index handling + workspace-boundary tests |
| `T84` | `complete` | `tests` | `advanced` | R79, T22, R64 | Phase 11 — Migration tests + MCP schema compatibility regression |
| `T85` | `complete` | `docs` | `advanced` | R47, R54, R63, R70, R78 | Phase 11 — Documentation suite (all 11 required docs from spec §Documentation Requirements) |
| `T86` | `complete` | `docs` | `balanced` | T82, T85 | Phase 11 — Operator runbook + recovery playbook |
| `R87` | `complete` | `review` | `advanced` | T80, T81, T82, T83, T84, T85, T86 | Tests/Docs review — Phase 11 hardening + documentation completeness |
| `R88` | `complete` | `review` | `advanced` | R87 | Definition-of-Done gate — shared checklist compliance for the whole fork |
| `R89` | `complete` | `review` | `advanced` | R05, R10, R18, R25, R26, R33, R38, R47, R54, R55, R63, R64, R70, R78, R79, R87, R88 | Final readiness review — end-to-end fork acceptance |

## Verification Contract

### T01: Phase 0 — Fork-or-extend decision doc with evidence

Expected files:
- `docs/architecture/2026-05-16-fork-or-extend-decision.md`

Verification commands:
- `test -f docs/architecture/2026-05-16-fork-or-extend-decision.md`
- `grep -q '## Decision' docs/architecture/2026-05-16-fork-or-extend-decision.md`
- `grep -q '## Evidence' docs/architecture/2026-05-16-fork-or-extend-decision.md`
- `grep -q '## Gate conditions' docs/architecture/2026-05-16-fork-or-extend-decision.md`
- `grep -q '## Branch or repo name' docs/architecture/2026-05-16-fork-or-extend-decision.md`
- `test -d docs`

### T02: Phase 0 — Successor architecture overview + compatibility policy

Expected files:
- `docs/architecture/2026-05-16-cognitive-workspace-architecture.md`
- `docs/architecture/2026-05-16-mcp-compatibility-policy.md`

Verification commands:
- `test -f docs/architecture/2026-05-16-cognitive-workspace-architecture.md`
- `test -f docs/architecture/2026-05-16-mcp-compatibility-policy.md`
- `grep -q '## Workspace graph' docs/architecture/2026-05-16-cognitive-workspace-architecture.md`
- `grep -q '## Event log' docs/architecture/2026-05-16-cognitive-workspace-architecture.md`
- `grep -q '## Memory graph' docs/architecture/2026-05-16-cognitive-workspace-architecture.md`
- `grep -q '## Backward compatibility' docs/architecture/2026-05-16-mcp-compatibility-policy.md`
- `test -d docs`

### T03: Phase 0 — Crate/module boundary plan + storage migration policy

Expected files:
- `docs/architecture/2026-05-16-crate-boundary-plan.md`
- `docs/architecture/2026-05-16-storage-migration-policy.md`

Verification commands:
- `test -f docs/architecture/2026-05-16-crate-boundary-plan.md`
- `test -f docs/architecture/2026-05-16-storage-migration-policy.md`
- `grep -q 'lattice-identity' docs/architecture/2026-05-16-crate-boundary-plan.md`
- `grep -q 'lattice-events' docs/architecture/2026-05-16-crate-boundary-plan.md`
- `grep -q 'lattice-memory' docs/architecture/2026-05-16-crate-boundary-plan.md`
- `grep -q '## Migration order' docs/architecture/2026-05-16-storage-migration-policy.md`
- `grep -q '## Rollback' docs/architecture/2026-05-16-storage-migration-policy.md`
- `test -d docs`

### T04: Phase 0 — Baseline benchmark capture (current Lattice workflows)

Expected files:
- `daemon/crates/lattice-core/benches/baseline_workflows.rs`
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json`
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/README.md`

Verification commands:
- `test -f daemon/crates/lattice-core/benches/baseline_workflows.rs`
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json`
- `cd daemon && cargo build --release --benches`
- `cd daemon && cargo bench --bench baseline_workflows -- --warm-up-time 1 --measurement-time 3`
- `test -d docs`

### R05: Foundation review — Phase 0 decisions and baselines

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R05-foundation.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R05-foundation.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R05-foundation.md`
- `grep -q '## Coding-standard alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R05-foundation.md`
- `grep -q '## Findings' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R05-foundation.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R05-foundation.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T06: Phase 1 — Identity type system (File, Symbol, Doc, Section, Event, Memory, Handle)

Expected files:
- `daemon/crates/lattice-core/src/identity/mod.rs`
- `daemon/crates/lattice-core/src/identity/kinds.rs`
- `daemon/crates/lattice-core/src/identity/encoding.rs`
- `daemon/crates/lattice-core/src/identity/tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/identity/mod.rs`
- `test -f daemon/crates/lattice-core/src/identity/kinds.rs`
- `cd daemon && cargo build -p lattice-core`
- `cd daemon && cargo test -p lattice-core --lib identity::tests`

### T07: Phase 1 — Identity resolver with ambiguity diagnostics

Expected files:
- `daemon/crates/lattice-core/src/identity/resolver.rs`
- `daemon/crates/lattice-core/src/identity/ambiguity.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/identity/resolver.rs`
- `cd daemon && cargo build -p lattice-core`
- `cd daemon && cargo test -p lattice-core --lib identity::resolver`
- `cd daemon && cargo test -p lattice-core --lib identity::ambiguity`

### T08: Phase 1 — Identity serialization in MCP payloads + legacy name compatibility shim

Expected files:
- `daemon/crates/lattice-core/src/identity/serialization.rs`
- `daemon/crates/lattice-daemon/src/rpc/identity_payload.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/identity/serialization.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/identity_payload.rs`
- `cd daemon && cargo build --release`
- `cd daemon && cargo test --workspace -- identity::serialization`
- `cd daemon && cargo test --workspace -- rpc::identity_payload`

### T09: Phase 1 — Identity resolution tests (renames, moves, duplicate names, branch changes, P99 budget)

Expected files:
- `daemon/crates/lattice-core/src/identity/resolver_tests.rs`
- `daemon/crates/lattice-core/src/identity/budget_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/identity/resolver_tests.rs`
- `test -f daemon/crates/lattice-core/src/identity/budget_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib identity::resolver_tests`
- `cd daemon && cargo test -p lattice-core --lib identity::budget_tests -- --include-ignored`

### R10: Backend review — Phase 1 identity substrate

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R10-identity.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R10-identity.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R10-identity.md`
- `grep -q '## Coding-standard alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R10-identity.md`
- `grep -q '## P99 budget evidence' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R10-identity.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R10-identity.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T11: Phase 2 — Event model types (all 20 event kinds + envelope)

Expected files:
- `daemon/crates/lattice-core/src/events/mod.rs`
- `daemon/crates/lattice-core/src/events/kinds.rs`
- `daemon/crates/lattice-core/src/events/envelope.rs`
- `daemon/crates/lattice-core/src/events/tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/events/mod.rs`
- `test -f daemon/crates/lattice-core/src/events/kinds.rs`
- `cd daemon && cargo build -p lattice-core`
- `cd daemon && cargo test -p lattice-core --lib events::tests`

### T12: Phase 2 — Append-only SQLite event tables + payload spillover + indexes

Expected files:
- `daemon/crates/lattice-core/src/events/store.rs`
- `daemon/crates/lattice-core/src/events/schema.sql`
- `daemon/crates/lattice-core/src/events/migrations.rs`
- `daemon/crates/lattice-core/src/events/store_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/events/store.rs`
- `test -f daemon/crates/lattice-core/src/events/schema.sql`
- `cd daemon && cargo build -p lattice-core`
- `cd daemon && cargo test -p lattice-core --lib events::store_tests`

### T13: Phase 2 — Event writer API + payload hashing + workspace scoping

Expected files:
- `daemon/crates/lattice-core/src/events/writer.rs`
- `daemon/crates/lattice-core/src/events/hashing.rs`
- `daemon/crates/lattice-core/src/events/writer_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/events/writer.rs`
- `cd daemon && cargo test -p lattice-core --lib events::writer_tests`

### T14: Phase 2 — Event reader/query API (filter by task/session/workspace/branch)

Expected files:
- `daemon/crates/lattice-core/src/events/reader.rs`
- `daemon/crates/lattice-core/src/events/query.rs`
- `daemon/crates/lattice-core/src/events/reader_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/events/reader.rs`
- `cd daemon && cargo test -p lattice-core --lib events::reader_tests`

### T15: Phase 2 — MCP/tool-call event capture wiring (all workflow tools)

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/event_capture.rs`
- `daemon/crates/lattice-daemon/src/rpc/event_capture_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/event_capture.rs`
- `cd daemon && cargo build --release`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::event_capture_tests`

### T16: Phase 2 — Event log compaction snapshot + bootstrap-from-snapshot

Expected files:
- `daemon/crates/lattice-core/src/events/snapshot.rs`
- `daemon/crates/lattice-core/src/events/compaction.rs`
- `daemon/crates/lattice-core/src/events/snapshot_tests.rs`
- `docs/architecture/2026-05-16-event-log-compaction.md`

Verification commands:
- `test -f daemon/crates/lattice-core/src/events/snapshot.rs`
- `test -f daemon/crates/lattice-core/src/events/compaction.rs`
- `test -f docs/architecture/2026-05-16-event-log-compaction.md`
- `cd daemon && cargo test -p lattice-core --lib events::snapshot_tests`
- `test -d docs`

### T17: Phase 2 — Event log tests (ordering, replay, corruption, scoping, P99 ≤5ms hot path)

Expected files:
- `daemon/crates/lattice-core/src/events/replay_tests.rs`
- `daemon/crates/lattice-core/src/events/corruption_tests.rs`
- `daemon/crates/lattice-core/src/events/budget_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/events/replay_tests.rs`
- `test -f daemon/crates/lattice-core/src/events/corruption_tests.rs`
- `test -f daemon/crates/lattice-core/src/events/budget_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib events::replay_tests`
- `cd daemon && cargo test -p lattice-core --lib events::corruption_tests`
- `cd daemon && cargo test -p lattice-core --lib events::budget_tests -- --include-ignored`

### R18: Backend review — Phase 2 event log substrate

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R18-event-log.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R18-event-log.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R18-event-log.md`
- `grep -q '## P99 budget evidence' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R18-event-log.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R18-event-log.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T19: Phase 3 — Memory schema redesign (classes incl CounterMemory + required fields)

Expected files:
- `daemon/crates/lattice-core/src/memory_graph/mod.rs`
- `daemon/crates/lattice-core/src/memory_graph/classes.rs`
- `daemon/crates/lattice-core/src/memory_graph/schema.sql`
- `docs/architecture/2026-05-16-memory-graph-schema.md`

Verification commands:
- `test -f daemon/crates/lattice-core/src/memory_graph/mod.rs`
- `test -f daemon/crates/lattice-core/src/memory_graph/classes.rs`
- `test -f daemon/crates/lattice-core/src/memory_graph/schema.sql`
- `test -f docs/architecture/2026-05-16-memory-graph-schema.md`
- `grep -q 'CounterMemory' daemon/crates/lattice-core/src/memory_graph/classes.rs`
- `cd daemon && cargo build -p lattice-core`
- `test -d docs`

### T20: Phase 3 — memory_links + memory_evidence + memory_accesses + memory_scores tables

Expected files:
- `daemon/crates/lattice-core/src/memory_graph/links.rs`
- `daemon/crates/lattice-core/src/memory_graph/evidence.rs`
- `daemon/crates/lattice-core/src/memory_graph/accesses.rs`
- `daemon/crates/lattice-core/src/memory_graph/scores.rs`
- `daemon/crates/lattice-core/src/memory_graph/links_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/memory_graph/links.rs`
- `test -f daemon/crates/lattice-core/src/memory_graph/evidence.rs`
- `cd daemon && cargo test -p lattice-core --lib memory_graph::links_tests`

### T21: Phase 3 — Memory stream taxonomy + scope enforcement helpers (deny-by-default)

Expected files:
- `daemon/crates/lattice-core/src/memory_graph/streams.rs`
- `daemon/crates/lattice-core/src/memory_graph/scope.rs`
- `daemon/crates/lattice-core/src/memory_graph/scope_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/memory_graph/streams.rs`
- `test -f daemon/crates/lattice-core/src/memory_graph/scope.rs`
- `cd daemon && cargo test -p lattice-core --lib memory_graph::scope_tests`

### T22: Phase 3 — Migration importer from existing Lattice memory rows

Expected files:
- `daemon/crates/lattice-core/src/memory_graph/migration.rs`
- `daemon/crates/lattice-core/src/memory_graph/migration_tests.rs`
- `docs/architecture/2026-05-16-memory-migration-guide.md`

Verification commands:
- `test -f daemon/crates/lattice-core/src/memory_graph/migration.rs`
- `test -f docs/architecture/2026-05-16-memory-migration-guide.md`
- `cd daemon && cargo test -p lattice-core --lib memory_graph::migration_tests`
- `test -d docs`

### T23: Phase 3 — Memory graph CRUD + verification status transitions + idempotent writes

Expected files:
- `daemon/crates/lattice-core/src/memory_graph/store.rs`
- `daemon/crates/lattice-core/src/memory_graph/transitions.rs`
- `daemon/crates/lattice-core/src/memory_graph/store_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/memory_graph/store.rs`
- `cd daemon && cargo test -p lattice-core --lib memory_graph::store_tests`

### T24: Phase 3 — Memory graph tests (links, contradiction, supersession, stale, replay)

Expected files:
- `daemon/crates/lattice-core/src/memory_graph/contradiction_tests.rs`
- `daemon/crates/lattice-core/src/memory_graph/supersession_tests.rs`
- `daemon/crates/lattice-core/src/memory_graph/replay_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/memory_graph/contradiction_tests.rs`
- `test -f daemon/crates/lattice-core/src/memory_graph/supersession_tests.rs`
- `test -f daemon/crates/lattice-core/src/memory_graph/replay_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib memory_graph::contradiction_tests`
- `cd daemon && cargo test -p lattice-core --lib memory_graph::supersession_tests`
- `cd daemon && cargo test -p lattice-core --lib memory_graph::replay_tests`

### R25: Backend review — Phase 3 memory graph storage

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R25-memory-graph.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R25-memory-graph.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R25-memory-graph.md`
- `grep -q '## CounterMemory coverage' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R25-memory-graph.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R25-memory-graph.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### R26: Contract gate — Identity ↔ Event Log ↔ Memory cross-layer contracts

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R26-contract-identity-events-memory.md`
- `daemon/crates/lattice-core/src/contract_tests/identity_event_memory.rs`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R26-contract-identity-events-memory.md`
- `test -f daemon/crates/lattice-core/src/contract_tests/identity_event_memory.rs`
- `grep -q '## Contract surfaces' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R26-contract-identity-events-memory.md`
- `grep -q '## Round-trip evidence' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R26-contract-identity-events-memory.md`
- `cd daemon && cargo test -p lattice-core --lib contract_tests::identity_event_memory`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T27: Phase 4 — Task intent classifier

Expected files:
- `daemon/crates/lattice-core/src/retrieval_v1/intent.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/intent_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/retrieval_v1/intent.rs`
- `cd daemon && cargo test -p lattice-core --lib retrieval_v1::intent_tests`

### T28: Phase 4 — Anchor extractor + resolver (paths, symbols, errors, commands, APIs, config keys)

Expected files:
- `daemon/crates/lattice-core/src/retrieval_v1/anchors.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/anchors_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/retrieval_v1/anchors.rs`
- `cd daemon && cargo test -p lattice-core --lib retrieval_v1::anchors_tests`

### T29: Phase 4 — Hybrid candidate retrieval (graph + docs + memories + events + working set)

Expected files:
- `daemon/crates/lattice-core/src/retrieval_v1/candidates.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/candidates_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/retrieval_v1/candidates.rs`
- `cd daemon && cargo test -p lattice-core --lib retrieval_v1::candidates_tests`

### T30: Phase 4 — Scoring model with diagnostic mode (all required signals)

Expected files:
- `daemon/crates/lattice-core/src/retrieval_v1/scoring.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/diagnostic.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/scoring_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/retrieval_v1/scoring.rs`
- `test -f daemon/crates/lattice-core/src/retrieval_v1/diagnostic.rs`
- `cd daemon && cargo test -p lattice-core --lib retrieval_v1::scoring_tests`

### T31: Phase 4 — Compact response shaper + inclusion reasons + expansion handles

Expected files:
- `daemon/crates/lattice-core/src/retrieval_v1/shaper.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/inclusion_reasons.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/shaper_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/retrieval_v1/shaper.rs`
- `cd daemon && cargo test -p lattice-core --lib retrieval_v1::shaper_tests`

### T32: Phase 4 — Retrieval benchmark suite + golden anchors + ranking tests

Expected files:
- `daemon/crates/lattice-core/src/retrieval_v1/benchmark.rs`
- `daemon/crates/lattice-core/src/retrieval_v1/golden_tests.rs`
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/retrieval_v1_metrics.json`

Verification commands:
- `test -f daemon/crates/lattice-core/src/retrieval_v1/benchmark.rs`
- `test -f daemon/crates/lattice-core/src/retrieval_v1/golden_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib retrieval_v1::golden_tests`
- `cd daemon && cargo test -p lattice-core --lib retrieval_v1::benchmark -- --include-ignored`

### R33: Backend review — Phase 4 Retrieval V1

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R33-retrieval-v1.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R33-retrieval-v1.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R33-retrieval-v1.md`
- `grep -q '## Inclusion-reason discipline' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R33-retrieval-v1.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R33-retrieval-v1.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T34: Phase 5 — Working memory state model + working_memory_checkpoints table

Expected files:
- `daemon/crates/lattice-core/src/working_memory/mod.rs`
- `daemon/crates/lattice-core/src/working_memory/state.rs`
- `daemon/crates/lattice-core/src/working_memory/schema.sql`
- `daemon/crates/lattice-core/src/working_memory/state_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/working_memory/mod.rs`
- `test -f daemon/crates/lattice-core/src/working_memory/state.rs`
- `cd daemon && cargo test -p lattice-core --lib working_memory::state_tests`

### T35: Phase 5 — Working memory operations (retrieve, summarize, filter, pin, evict, expand, compress, checkpoint)

Expected files:
- `daemon/crates/lattice-core/src/working_memory/operations.rs`
- `daemon/crates/lattice-core/src/working_memory/operations_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/working_memory/operations.rs`
- `cd daemon && cargo test -p lattice-core --lib working_memory::operations_tests`

### T36: Phase 5 — MCP surface for working memory inspection + event capture for include/exclude reasons

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/working_memory_tool.rs`
- `daemon/crates/lattice-core/src/working_memory/event_hooks.rs`
- `daemon/crates/lattice-daemon/src/rpc/working_memory_tool_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/working_memory_tool.rs`
- `cd daemon && cargo build --release`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::working_memory_tool_tests`

### T37: Phase 5 — Working memory tests (budgets, pins, eviction, checkpoint restore)

Expected files:
- `daemon/crates/lattice-core/src/working_memory/budget_tests.rs`
- `daemon/crates/lattice-core/src/working_memory/checkpoint_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/working_memory/budget_tests.rs`
- `test -f daemon/crates/lattice-core/src/working_memory/checkpoint_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib working_memory::budget_tests`
- `cd daemon && cargo test -p lattice-core --lib working_memory::checkpoint_tests`

### R38: Backend review — Phase 5 working memory

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R38-working-memory.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R38-working-memory.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R38-working-memory.md`
- `grep -q '## Excluded-context auditability' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R38-working-memory.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R38-working-memory.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T39: Phase 6 — Consolidation job runtime + proposal model (no silent writes)

Expected files:
- `daemon/crates/lattice-core/src/consolidation/mod.rs`
- `daemon/crates/lattice-core/src/consolidation/proposal.rs`
- `daemon/crates/lattice-core/src/consolidation/queue.rs`
- `daemon/crates/lattice-core/src/consolidation/schema.sql`
- `daemon/crates/lattice-core/src/consolidation/proposal_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/mod.rs`
- `test -f daemon/crates/lattice-core/src/consolidation/proposal.rs`
- `cd daemon && cargo test -p lattice-core --lib consolidation::proposal_tests`

### T40: Phase 6 — Synchronous session consolidation (episode summaries)

Expected files:
- `daemon/crates/lattice-core/src/consolidation/session.rs`
- `daemon/crates/lattice-core/src/consolidation/episode.rs`
- `daemon/crates/lattice-core/src/consolidation/session_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/session.rs`
- `cd daemon && cargo test -p lattice-core --lib consolidation::session_tests`

### T41: Phase 6 — Deterministic jobs: duplicate detection, supersession candidates, demote unused

Expected files:
- `daemon/crates/lattice-core/src/consolidation/duplicates.rs`
- `daemon/crates/lattice-core/src/consolidation/supersession.rs`
- `daemon/crates/lattice-core/src/consolidation/demotion.rs`
- `daemon/crates/lattice-core/src/consolidation/deterministic_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/duplicates.rs`
- `test -f daemon/crates/lattice-core/src/consolidation/supersession.rs`
- `cd daemon && cargo test -p lattice-core --lib consolidation::deterministic_tests`

### T42: Phase 6 — LLM-driven jobs: episode summary, procedure extraction, contradiction detection, failure-pattern extraction

Expected files:
- `daemon/crates/lattice-core/src/consolidation/llm/mod.rs`
- `daemon/crates/lattice-core/src/consolidation/llm/episode.rs`
- `daemon/crates/lattice-core/src/consolidation/llm/procedure.rs`
- `daemon/crates/lattice-core/src/consolidation/llm/contradiction.rs`
- `daemon/crates/lattice-core/src/consolidation/llm/failure_pattern.rs`
- `daemon/crates/lattice-core/src/consolidation/llm/llm_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/llm/mod.rs`
- `test -f daemon/crates/lattice-core/src/consolidation/llm/contradiction.rs`
- `cd daemon && cargo test -p lattice-core --lib consolidation::llm::llm_tests`

### T43: Phase 6 — LLM provenance (model + prompt hash + response hash) + queue bounds + cost budgets

Expected files:
- `daemon/crates/lattice-core/src/consolidation/llm/provenance.rs`
- `daemon/crates/lattice-core/src/consolidation/llm/budget.rs`
- `daemon/crates/lattice-core/src/consolidation/llm/provenance_tests.rs`
- `docs/architecture/2026-05-16-consolidation-llm-budgets.md`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/llm/provenance.rs`
- `test -f daemon/crates/lattice-core/src/consolidation/llm/budget.rs`
- `test -f docs/architecture/2026-05-16-consolidation-llm-budgets.md`
- `cd daemon && cargo test -p lattice-core --lib consolidation::llm::provenance_tests`
- `test -d docs`

### T44: Phase 6 — Manual review queue for high-scope memory changes (repo + organization scope)

Expected files:
- `daemon/crates/lattice-core/src/consolidation/review_queue.rs`
- `daemon/crates/lattice-core/src/consolidation/review_queue_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/review_queue.rs`
- `cd daemon && cargo test -p lattice-core --lib consolidation::review_queue_tests`

### T45: Phase 6 — Replay-safe execution + reversibility + provenance preservation

Expected files:
- `daemon/crates/lattice-core/src/consolidation/replay.rs`
- `daemon/crates/lattice-core/src/consolidation/replay_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/replay.rs`
- `cd daemon && cargo test -p lattice-core --lib consolidation::replay_tests`

### T46: Phase 6 — Consolidation tests (proposals, apply/reject, replay, queue bounds, failure events)

Expected files:
- `daemon/crates/lattice-core/src/consolidation/integration_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/consolidation/integration_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib consolidation::integration_tests`

### R47: Backend review — Phase 6 consolidation engine

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R47-consolidation.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R47-consolidation.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R47-consolidation.md`
- `grep -q '## Proposal discipline' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R47-consolidation.md`
- `grep -q '## LLM budget evidence' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R47-consolidation.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R47-consolidation.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T48: Phase 7 — Verification engine core (file/symbol/doc/test existence checks)

Expected files:
- `daemon/crates/lattice-core/src/verification/mod.rs`
- `daemon/crates/lattice-core/src/verification/existence.rs`
- `daemon/crates/lattice-core/src/verification/schema.sql`
- `daemon/crates/lattice-core/src/verification/existence_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/verification/mod.rs`
- `test -f daemon/crates/lattice-core/src/verification/existence.rs`
- `cd daemon && cargo test -p lattice-core --lib verification::existence_tests`

### T49: Phase 7 — Exact-span evidence validation + content hashes

Expected files:
- `daemon/crates/lattice-core/src/verification/spans.rs`
- `daemon/crates/lattice-core/src/verification/spans_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/verification/spans.rs`
- `cd daemon && cargo test -p lattice-core --lib verification::spans_tests`

### T50: Phase 7 — Branch/workspace scope enforcement (deny leakage) + negative tests

Expected files:
- `daemon/crates/lattice-core/src/verification/scope_enforcement.rs`
- `daemon/crates/lattice-core/src/verification/scope_leak_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/verification/scope_enforcement.rs`
- `test -f daemon/crates/lattice-core/src/verification/scope_leak_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib verification::scope_leak_tests`

### T51: Phase 7 — Time-bound expiry + incremental verification triggered by graph changes

Expected files:
- `daemon/crates/lattice-core/src/verification/expiry.rs`
- `daemon/crates/lattice-core/src/verification/incremental.rs`
- `daemon/crates/lattice-core/src/verification/incremental_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/verification/expiry.rs`
- `test -f daemon/crates/lattice-core/src/verification/incremental.rs`
- `cd daemon && cargo test -p lattice-core --lib verification::incremental_tests`

### T52: Phase 7 — Stale surfacing in workflow bundles + label discipline (no trusted display of stale/contradicted)

Expected files:
- `daemon/crates/lattice-core/src/verification/surfacing.rs`
- `daemon/crates/lattice-core/src/verification/surfacing_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/verification/surfacing.rs`
- `cd daemon && cargo test -p lattice-core --lib verification::surfacing_tests`

### T53: Phase 7 — Verification tests (stale never displayed as trusted, scope leakage blocked)

Expected files:
- `daemon/crates/lattice-core/src/verification/integration_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/verification/integration_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib verification::integration_tests`

### R54: Backend review — Phase 7 verification + freshness

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R54-verification.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R54-verification.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R54-verification.md`
- `grep -q '## Stale-label discipline' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R54-verification.md`
- `grep -q '## Scope-leak evidence' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R54-verification.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R54-verification.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### R55: Contract gate — Memory ↔ Verification ↔ Retrieval cross-layer

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R55-contract-memory-verification-retrieval.md`
- `daemon/crates/lattice-core/src/contract_tests/memory_verification_retrieval.rs`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R55-contract-memory-verification-retrieval.md`
- `test -f daemon/crates/lattice-core/src/contract_tests/memory_verification_retrieval.rs`
- `grep -q '## Contract surfaces' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R55-contract-memory-verification-retrieval.md`
- `cd daemon && cargo test -p lattice-core --lib contract_tests::memory_verification_retrieval`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T56: Phase 8 — Redesigned prepare_change + plan_edit + trace_scenario + diagnose_failure

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/prepare_change.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/plan_edit.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/trace_scenario.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/diagnose_failure.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/edit_workflows_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/workflow_v2/prepare_change.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/workflow_v2/plan_edit.rs`
- `cd daemon && cargo build --release`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::workflow_v2::edit_workflows_tests`

### T57: Phase 8 — Redesigned get_context_capsule + get_docs_capsule + find_relevant_tests + impact_from_diff

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/context_capsule.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/docs_capsule.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/relevant_tests.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/impact_from_diff.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/discovery_workflows_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/workflow_v2/context_capsule.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/workflow_v2/impact_from_diff.rs`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::workflow_v2::discovery_workflows_tests`

### T58: Phase 8 — New memory tools: get_task_memory + save_memory + propose_memory_evolution(action=apply|reject)

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_task_memory.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/save_memory.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/propose_memory_evolution.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/memory_tools_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/memory_v2/get_task_memory.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/memory_v2/save_memory.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/memory_v2/propose_memory_evolution.rs`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::memory_v2::memory_tools_tests`

### T59: Phase 8 — New memory tools: verify_memory + explain_memory (unified) + list_memory_conflicts

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/verify_explain_memory.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/list_memory_conflicts.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/verify_explain_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/memory_v2/verify_explain_memory.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/memory_v2/list_memory_conflicts.rs`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::memory_v2::verify_explain_tests`

### T60: Phase 8 — New tools: consolidate_session + get_memory_metrics + get_event_trace

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/consolidate_session.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_memory_metrics.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_event_trace.rs`
- `daemon/crates/lattice-daemon/src/rpc/memory_v2/admin_tools_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/memory_v2/consolidate_session.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/memory_v2/get_event_trace.rs`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::memory_v2::admin_tools_tests`

### T61: Phase 8 — Tool surface audit + final MCP reference doc (collapse overlapping tools per spec §MCP discipline)

Expected files:
- `docs/architecture/2026-05-16-mcp-tool-reference.md`
- `docs/architecture/2026-05-16-mcp-surface-audit.md`

Verification commands:
- `test -f docs/architecture/2026-05-16-mcp-tool-reference.md`
- `test -f docs/architecture/2026-05-16-mcp-surface-audit.md`
- `grep -q '## Final tool list' docs/architecture/2026-05-16-mcp-tool-reference.md`
- `grep -q '## Audit findings' docs/architecture/2026-05-16-mcp-surface-audit.md`
- `grep -q '## Collapsed tools' docs/architecture/2026-05-16-mcp-surface-audit.md`
- `test -d docs`

### T62: Phase 8 — Workflow outcome recording integrated by default + composition tests

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/outcome_capture.rs`
- `daemon/crates/lattice-daemon/src/rpc/workflow_v2/composition_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/workflow_v2/outcome_capture.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/workflow_v2/composition_tests.rs`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::workflow_v2::composition_tests`

### R63: Backend review — Phase 8 Workflow Engine V2

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R63-workflow-engine-v2.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R63-workflow-engine-v2.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R63-workflow-engine-v2.md`
- `grep -q '## Tool-surface size' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R63-workflow-engine-v2.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R63-workflow-engine-v2.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### R64: Contract gate — MCP tool surface schema regression

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R64-contract-mcp-schema.md`
- `daemon/crates/lattice-daemon/src/rpc/mcp_schema_tests.rs`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R64-contract-mcp-schema.md`
- `test -f daemon/crates/lattice-daemon/src/rpc/mcp_schema_tests.rs`
- `grep -q '## Schema surfaces' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R64-contract-mcp-schema.md`
- `grep -q '## Render-mode coverage' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R64-contract-mcp-schema.md`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::mcp_schema_tests`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T65: Phase 9 — Metric collection module (all required signals from spec)

Expected files:
- `daemon/crates/lattice-core/src/metrics/mod.rs`
- `daemon/crates/lattice-core/src/metrics/signals.rs`
- `daemon/crates/lattice-core/src/metrics/signals_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/metrics/mod.rs`
- `test -f daemon/crates/lattice-core/src/metrics/signals.rs`
- `cd daemon && cargo test -p lattice-core --lib metrics::signals_tests`

### T66: Phase 9 — Benchmark task fixture repository + golden anchors (cross-language)

Expected files:
- `daemon/crates/lattice-core/benches/fixtures/README.md`
- `daemon/crates/lattice-core/benches/fixtures/rust-repo/manifest.toml`
- `daemon/crates/lattice-core/benches/fixtures/typescript-repo/manifest.toml`
- `daemon/crates/lattice-core/benches/fixtures/python-repo/manifest.toml`
- `daemon/crates/lattice-core/benches/fixtures/golden_anchors.json`
- `daemon/crates/lattice-core/benches/cognitive_workspace_benchmark.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/benches/fixtures/golden_anchors.json`
- `test -f daemon/crates/lattice-core/benches/cognitive_workspace_benchmark.rs`
- `cd daemon && cargo build --release --benches`
- `cd daemon && cargo bench --bench cognitive_workspace_benchmark -- --warm-up-time 1 --measurement-time 3`

### T67: Phase 9 — get_memory_metrics + retrieval relevance surfaces wired through MCP

Expected files:
- `daemon/crates/lattice-daemon/src/rpc/metrics_surface.rs`
- `daemon/crates/lattice-daemon/src/rpc/metrics_surface_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/rpc/metrics_surface.rs`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::metrics_surface_tests`

### T68: Phase 9 — Regression dashboard / CLI report

Expected files:
- `daemon/crates/lattice-daemon/src/bin/lattice_report.rs`
- `daemon/crates/lattice-core/src/metrics/report.rs`
- `daemon/crates/lattice-core/src/metrics/report_tests.rs`
- `docs/architecture/2026-05-16-metrics-report.md`

Verification commands:
- `test -f daemon/crates/lattice-daemon/src/bin/lattice_report.rs`
- `test -f docs/architecture/2026-05-16-metrics-report.md`
- `cd daemon && cargo build --release --bin lattice_report`
- `cd daemon && cargo test -p lattice-core --lib metrics::report_tests`
- `test -d docs`

### T69: Phase 9 — Metrics regression coverage tests (success-criteria targets from spec)

Expected files:
- `daemon/crates/lattice-core/src/metrics/regression_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/metrics/regression_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib metrics::regression_tests -- --include-ignored`

### R70: Tests/Docs review — Phase 9 metrics + evaluation

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R70-metrics.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R70-metrics.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R70-metrics.md`
- `grep -q '## Success-criteria coverage' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R70-metrics.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R70-metrics.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T71: Phase 10 — Extension review-panel scaffolding + JSON-RPC plumbing to new tools

Expected files:
- `extension/src/review/reviewPanel.ts`
- `extension/src/review/rpcBridge.ts`
- `extension/src/review/i18n/en.json`
- `extension/package.json`

Verification commands:
- `test -f extension/src/review/reviewPanel.ts`
- `test -f extension/src/review/rpcBridge.ts`
- `test -f extension/src/review/i18n/en.json`
- `cd extension && npm install`
- `cd extension && npm run compile`
- `cd extension && npm run lint`

### T72: Phase 10 — Memory inbox view + filters + status badges

Expected files:
- `extension/src/review/memoryInbox.ts`
- `extension/src/review/components/MemoryRow.ts`
- `extension/src/review/components/StatusBadge.ts`

Verification commands:
- `test -f extension/src/review/memoryInbox.ts`
- `test -f extension/src/review/components/MemoryRow.ts`
- `cd extension && npm run compile`
- `cd extension && npm run lint`

### T73: Phase 10 — Promotion queue + contradiction queue with accept/reject (dialog modals)

Expected files:
- `extension/src/review/promotionQueue.ts`
- `extension/src/review/contradictionQueue.ts`
- `extension/src/review/components/ProposalDialog.ts`

Verification commands:
- `test -f extension/src/review/promotionQueue.ts`
- `test -f extension/src/review/contradictionQueue.ts`
- `test -f extension/src/review/components/ProposalDialog.ts`
- `cd extension && npm run compile`
- `cd extension && npm run lint`

### T74: Phase 10 — Stale memory view + evidence inspector + verification trigger

Expected files:
- `extension/src/review/staleView.ts`
- `extension/src/review/evidenceInspector.ts`

Verification commands:
- `test -f extension/src/review/staleView.ts`
- `test -f extension/src/review/evidenceInspector.ts`
- `cd extension && npm run compile`
- `cd extension && npm run lint`

### T75: Phase 10 — Event trace view + retrieval explanation view

Expected files:
- `extension/src/review/eventTraceView.ts`
- `extension/src/review/retrievalExplanationView.ts`

Verification commands:
- `test -f extension/src/review/eventTraceView.ts`
- `test -f extension/src/review/retrievalExplanationView.ts`
- `cd extension && npm run compile`
- `cd extension && npm run lint`

### T76: Phase 10 — Consolidation queue + indexing health + workspace graph health views

Expected files:
- `extension/src/review/consolidationQueueView.ts`
- `extension/src/review/indexingHealthView.ts`
- `extension/src/review/workspaceGraphHealthView.ts`

Verification commands:
- `test -f extension/src/review/consolidationQueueView.ts`
- `test -f extension/src/review/indexingHealthView.ts`
- `test -f extension/src/review/workspaceGraphHealthView.ts`
- `cd extension && npm run compile`
- `cd extension && npm run lint`

### T77: Phase 10 — Extension UI smoke tests (compile + lint + integration runner)

Expected files:
- `extension/src/test/review.test.ts`
- `extension/src/test/runTest.ts`

Verification commands:
- `test -f extension/src/test/review.test.ts`
- `test -f extension/src/test/runTest.ts`
- `cd extension && npm run compile`
- `cd extension && npm run lint`
- `cd extension && npm test`

### R78: Frontend review — Phase 10 human review UI (UI-spec compliance)

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R78-frontend.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R78-frontend.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R78-frontend.md`
- `grep -q '## UI-specification compliance' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R78-frontend.md`
- `grep -q '## i18n coverage' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R78-frontend.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R78-frontend.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### R79: Contract gate — Extension ↔ Daemon review-UI MCP plumbing

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R79-contract-extension-daemon.md`
- `extension/src/test/contract.test.ts`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R79-contract-extension-daemon.md`
- `test -f extension/src/test/contract.test.ts`
- `grep -q '## Contract surfaces' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R79-contract-extension-daemon.md`
- `grep -q '## Round-trip evidence' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R79-contract-extension-daemon.md`
- `cd extension && npm run compile`
- `cd extension && npm test`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### T80: Phase 11 — Large-repo performance tests

Expected files:
- `daemon/crates/lattice-core/src/hardening/large_repo_tests.rs`
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/large_repo_results.json`

Verification commands:
- `test -f daemon/crates/lattice-core/src/hardening/large_repo_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib hardening::large_repo_tests -- --include-ignored`

### T81: Phase 11 — Concurrency tests (parallel sessions, write contention, replay-safe)

Expected files:
- `daemon/crates/lattice-core/src/hardening/concurrency_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/hardening/concurrency_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib hardening::concurrency_tests -- --include-ignored`

### T82: Phase 11 — Recovery + corrupted-event handling tests

Expected files:
- `daemon/crates/lattice-core/src/hardening/recovery_tests.rs`
- `daemon/crates/lattice-core/src/hardening/corruption_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/hardening/recovery_tests.rs`
- `test -f daemon/crates/lattice-core/src/hardening/corruption_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib hardening::recovery_tests`
- `cd daemon && cargo test -p lattice-core --lib hardening::corruption_tests`

### T83: Phase 11 — Partial-index handling + workspace-boundary tests

Expected files:
- `daemon/crates/lattice-core/src/hardening/partial_index_tests.rs`
- `daemon/crates/lattice-core/src/hardening/workspace_boundary_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/hardening/partial_index_tests.rs`
- `test -f daemon/crates/lattice-core/src/hardening/workspace_boundary_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib hardening::partial_index_tests`
- `cd daemon && cargo test -p lattice-core --lib hardening::workspace_boundary_tests`

### T84: Phase 11 — Migration tests + MCP schema compatibility regression

Expected files:
- `daemon/crates/lattice-core/src/hardening/migration_tests.rs`
- `daemon/crates/lattice-daemon/src/rpc/mcp_compat_tests.rs`

Verification commands:
- `test -f daemon/crates/lattice-core/src/hardening/migration_tests.rs`
- `test -f daemon/crates/lattice-daemon/src/rpc/mcp_compat_tests.rs`
- `cd daemon && cargo test -p lattice-core --lib hardening::migration_tests`
- `cd daemon && cargo test -p lattice-daemon --lib rpc::mcp_compat_tests`

### T85: Phase 11 — Documentation suite (all 11 required docs from spec §Documentation Requirements)

Expected files:
- `docs/architecture/2026-05-16-successor-architecture-overview.md`
- `docs/architecture/2026-05-16-memory-model-reference.md`
- `docs/architecture/2026-05-16-event-log-design.md`
- `docs/architecture/2026-05-16-consolidation-design.md`
- `docs/architecture/2026-05-16-retrieval-ranking-design.md`
- `docs/architecture/2026-05-16-verification-freshness-design.md`
- `docs/operator-guide/2026-05-16-operator-guide.md`
- `docs/operator-guide/2026-05-16-migration-from-lattice.md`
- `docs/operator-guide/2026-05-16-benchmark-evaluation-guide.md`
- `docs/operator-guide/2026-05-16-extension-review-ui-guide.md`

Verification commands:
- `test -f docs/architecture/2026-05-16-successor-architecture-overview.md`
- `test -f docs/architecture/2026-05-16-memory-model-reference.md`
- `test -f docs/architecture/2026-05-16-event-log-design.md`
- `test -f docs/architecture/2026-05-16-consolidation-design.md`
- `test -f docs/architecture/2026-05-16-retrieval-ranking-design.md`
- `test -f docs/architecture/2026-05-16-verification-freshness-design.md`
- `test -f docs/operator-guide/2026-05-16-operator-guide.md`
- `test -f docs/operator-guide/2026-05-16-migration-from-lattice.md`
- `test -f docs/operator-guide/2026-05-16-benchmark-evaluation-guide.md`
- `test -f docs/operator-guide/2026-05-16-extension-review-ui-guide.md`
- `grep -q '## Overview' docs/architecture/2026-05-16-successor-architecture-overview.md`
- `grep -q '## Migration steps' docs/operator-guide/2026-05-16-migration-from-lattice.md`
- `test -d docs`

### T86: Phase 11 — Operator runbook + recovery playbook

Expected files:
- `docs/operator-guide/2026-05-16-runbook.md`
- `docs/operator-guide/2026-05-16-recovery-playbook.md`

Verification commands:
- `test -f docs/operator-guide/2026-05-16-runbook.md`
- `test -f docs/operator-guide/2026-05-16-recovery-playbook.md`
- `grep -q '## Daily operations' docs/operator-guide/2026-05-16-runbook.md`
- `grep -q '## Recovery procedures' docs/operator-guide/2026-05-16-recovery-playbook.md`
- `grep -q '## Replay from snapshot' docs/operator-guide/2026-05-16-recovery-playbook.md`
- `test -d docs`

### R87: Tests/Docs review — Phase 11 hardening + documentation completeness

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R87-hardening-docs.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R87-hardening-docs.md`
- `grep -q '## Spec alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R87-hardening-docs.md`
- `grep -q '## Doc coverage matrix' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R87-hardening-docs.md`
- `grep -q '## Coding-standard alignment' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R87-hardening-docs.md`
- `grep -q '## Verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R87-hardening-docs.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### R88: Definition-of-Done gate — shared checklist compliance for the whole fork

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Workflow Completion' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Failure Handling' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Security, Tenancy, and Audit' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Data Integrity and Workflow Controls' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Observability and Recovery' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Performance and Scale' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Documentation and Operator Readiness' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Verification Evidence' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `grep -q '## Explicit Deferrals' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R88-definition-of-done.md`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`

### R89: Final readiness review — end-to-end fork acceptance

Expected files:
- `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R89-final-readiness.md`

Verification commands:
- `test -f docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R89-final-readiness.md`
- `grep -q '## Spec coverage matrix' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R89-final-readiness.md`
- `grep -q '## Measurable success criteria' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R89-final-readiness.md`
- `grep -q '## Risk controls in place' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R89-final-readiness.md`
- `grep -q '## Ship verdict' docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R89-final-readiness.md`
- `cd daemon && cargo build --release`
- `cd daemon && cargo test --workspace`
- `cd extension && npm run compile`
- `cd extension && npm run lint`
- `cd extension && npm test`
- `test -d docs`
- `test -f /home/pete/cadres/shared/templates/definition-of-done-checklist.md`
