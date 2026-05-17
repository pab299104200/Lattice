# R05 — Foundation Review (Phase 0)

**Date:** 2026-05-16
**Reviewer:** R05 (advanced model class)
**Scope:** Phase 0 deliverables from T01, T02, T03, T04
**Plan anchor:** [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation)

## Spec alignment

| Phase 0 deliverable | Spec heading | Evidence artifact and heading | Verdict |
|---|---|---|---|
| Fork/extend decision with evidence | [`## Phase 0: Fork Foundation`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation) and the fork-gate at lines 579–586 of the plan | [`docs/architecture/2026-05-16-fork-or-extend-decision.md#decision`](../../../architecture/2026-05-16-fork-or-extend-decision.md#decision), [`#evidence`](../../../architecture/2026-05-16-fork-or-extend-decision.md#evidence), [`#gate-conditions`](../../../architecture/2026-05-16-fork-or-extend-decision.md#gate-conditions). All four spec gate conditions are evaluated `Unmet` with file-line citations into `daemon/crates/lattice-core/src/memory/store.rs`, `src/symbols.rs`, `src/graph/model.rs`, and `src/storage/graph_store.rs`. Branch name `feat/cognitive-workspace` is committed under [`## Branch or repo name`](../../../architecture/2026-05-16-fork-or-extend-decision.md#branch-or-repo-name). | PASS |
| Architecture overview covering workspace, event, and memory substrates | [`## Design Thesis`](../../2026-05-16-cognitive-workspace-fork-plan.md#design-thesis) and [`## System Architecture`](../../2026-05-16-cognitive-workspace-fork-plan.md#system-architecture) | [`docs/architecture/2026-05-16-cognitive-workspace-architecture.md`](../../../architecture/2026-05-16-cognitive-workspace-architecture.md) with required sections [`## Workspace graph`](../../../architecture/2026-05-16-cognitive-workspace-architecture.md#workspace-graph), [`## Event log`](../../../architecture/2026-05-16-cognitive-workspace-architecture.md#event-log), [`## Memory graph`](../../../architecture/2026-05-16-cognitive-workspace-architecture.md#memory-graph). Each enumerates the spec-mandated node/edge/event/memory families literally (15 node families, 14 edge families, 20 event types, 13 memory classes, 11 link types) rather than paraphrasing. | PASS |
| MCP compatibility policy classifying every existing tool | [`## MCP Surface`](../../2026-05-16-cognitive-workspace-fork-plan.md#mcp-surface) and [`## MCP Tool Contract Principles`](../../2026-05-16-cognitive-workspace-fork-plan.md#mcp-tool-contract-principles) | [`docs/architecture/2026-05-16-mcp-compatibility-policy.md`](../../../architecture/2026-05-16-mcp-compatibility-policy.md). Cross-checked against `daemon/crates/lattice-daemon/src/rpc/mcp.rs` `tools/list` (lines 267–1328, 37 canonical tools) and 5 legacy aliases (lines 1356, 1375, 1377, 1378, 1380). Every tool is classified `stable`, `additive`, `redesigned-with-shim`, or `deprecated-with-deadline`. The binding rule is stated in [`## Backward compatibility`](../../../architecture/2026-05-16-mcp-compatibility-policy.md#backward-compatibility) ("Every existing assistant client must continue to function for one full phase cycle after a redesigned tool lands"). | PASS |
| Crate/module boundary plan covering all seven new substrates | [`### Phase 1: Unified Identity Model`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model) through [`### Phase 7: Verification And Freshness`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-7-verification-and-freshness) | [`docs/architecture/2026-05-16-crate-boundary-plan.md`](../../../architecture/2026-05-16-crate-boundary-plan.md). All seven reserved names appear under headed sections with layout decisions and rationale: `lattice-identity` (line 20), `lattice-events` (line 46), `lattice-memory` (line 72), `lattice-retrieval` (line 99), `lattice-working-memory` (line 128), `lattice-consolidation` (line 154), `lattice-verification` (line 182). Dependency DAG is rendered in [`## Dependency graph`](../../../architecture/2026-05-16-crate-boundary-plan.md#dependency-graph) and is acyclic. | PASS |
| Storage migration policy with chronological phase migrations and rollback | [`## Storage Design`](../../2026-05-16-cognitive-workspace-fork-plan.md#storage-design) and the per-phase definition-of-done blocks | [`docs/architecture/2026-05-16-storage-migration-policy.md#migration-order`](../../../architecture/2026-05-16-storage-migration-policy.md#migration-order) enumerates 17 migrations covering Phase 1→Phase 7 in chronological phase prefix order (`p1_001`…`p7_001`). [`## Rollback`](../../../architecture/2026-05-16-storage-migration-policy.md#rollback) lists an inverse operation and `archived` data-loss class for every entry. [`## Compaction snapshot policy`](../../../architecture/2026-05-16-storage-migration-policy.md#compaction-snapshot-policy) implements the event-log compaction rule. | PASS |
| Baseline benchmark suite for current workflows | [`## Phase 0: Fork Foundation`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation) deliverable "baseline benchmark suite from current Lattice workflows" | [`daemon/crates/lattice-core/benches/baseline_workflows.rs`](../../../../daemon/crates/lattice-core/benches/baseline_workflows.rs) registers all 7 required workflows (`bench_prepare_change`, `bench_get_context_capsule`, `bench_expand_context`, `bench_impact_from_diff`, `bench_diagnose_failure`, `bench_search_symbols`, `bench_find_relevant_tests` — `criterion_group!` lines 584–593). Running `cargo bench --bench baseline_workflows -- --warm-up-time 1 --measurement-time 3` regenerates [`baselines/baseline_metrics.json`](../baselines/baseline_metrics.json) with p50/p95/p99 microseconds, payload size, candidate count, commit SHA, and timestamp per workflow. Re-run during this review at `recorded_at` 1778988568–1778988619 against commit `b85dee7`. | PASS |

## Coding-standard alignment

### Hard limits — file length (`wc -l`)

| File | Lines | Limit | Verdict |
|---|---|---|---|
| `docs/architecture/2026-05-16-fork-or-extend-decision.md` | 143 | 800 | PASS |
| `docs/architecture/2026-05-16-cognitive-workspace-architecture.md` | 232 | 800 | PASS |
| `docs/architecture/2026-05-16-mcp-compatibility-policy.md` | 126 | 800 | PASS |
| `docs/architecture/2026-05-16-crate-boundary-plan.md` | 279 | 800 | PASS |
| `docs/architecture/2026-05-16-storage-migration-policy.md` | 109 | 800 | PASS |
| `daemon/crates/lattice-core/benches/baseline_workflows.rs` | 594 | 800 | PASS |
| `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/README.md` | 38 | 800 | PASS |

### Forbidden tokens

`grep -nE 'TODO|FIXME|XXX'` against all seven Phase 0 artifacts: zero matches. No `TODO`, `FIXME`, or `XXX` markers in committed Phase 0 content.

### Commented-out code

`grep -nE '^\s*//'` against `baseline_workflows.rs`: zero matches. No commented-out blocks in the bench file. The Markdown artifacts do not contain commented HTML blocks either.

### Suppression audit

No `#[allow(...)]`, `#[cfg(...)]` escape hatches, `eslint-disable`, `@ts-ignore`, `# noqa`, or `# type: ignore` suppressions appear in any Phase 0 artifact. The bench file uses standard `criterion` macros without suppression.

### Naming audit — bench function names

| Function | Naming verdict |
|---|---|
| `bench_prepare_change` | Mirrors canonical MCP workflow name with `bench_` prefix. Clear, no abbreviation. PASS |
| `bench_get_context_capsule` | Mirrors canonical MCP workflow name. PASS |
| `bench_expand_context` | Mirrors canonical MCP workflow name. PASS |
| `bench_impact_from_diff` | Mirrors canonical MCP workflow name. PASS |
| `bench_diagnose_failure` | Mirrors canonical MCP workflow name. PASS |
| `bench_search_symbols` | Mirrors canonical MCP workflow name. PASS |
| `bench_find_relevant_tests` | Mirrors canonical MCP workflow name. PASS |

Support functions in the bench file (`fixture`, `build_fixture`, `materialize_fixture_workspace`, `detect_rules`, `build_expand_seed`, `context_capsule`, `register_report_benchmark`, `measure_report`, `warm_up`, `collect_samples`, `percentile_us`, `store_metric`, `write_metrics_snapshot`, `benchmark_root`, `git_commit_sha`, `recorded_at`, `shadow_metrics_path`) are intent-bearing, full-word names with no generic containers and no `_unused` placeholders. PASS.

### Doc citation discipline

Per `lattice/CLAUDE.md` § Markdown Heading References, doc citations should use exact `## Heading` references. Spot-checked:

- `fork-or-extend-decision.md` cites exact line ranges in `daemon/crates/lattice-core/src/memory/store.rs`, `src/symbols.rs`, `src/graph/model.rs`, `src/storage/graph_store.rs`, and exact `## Heading` anchors in companion docs (`#data-model`, `#persistence-and-migration`, `#durable-identity-basis`, `#contract-notes`). PASS.
- `cognitive-workspace-architecture.md` cites `#design-thesis`, `#system-architecture`, `#storage-design`, `#phase-2-event-log-substrate`, `#4-memory-graph`, `#summary`, `#data-model`, `#persistence-and-migration`, `#explainability-boundary`, `#durable-identity-basis`. PASS.
- `mcp-compatibility-policy.md` cites `#phase-0-fork-foundation`, `#mcp-surface`, `#mcp-tool-contract-principles`, `#backward-compatibility`, plus shared substrate anchors. PASS.
- `crate-boundary-plan.md` and `storage-migration-policy.md` cite phase-specific spec sections (`#phase-1-unified-identity-model` etc.) and the storage design heading. PASS.

No "see the spec" prose anywhere in Phase 0 artifacts.

## Findings

### F1 — `find_relevant_tests` baseline records zero candidates (minor)

**Observation.** The `find_relevant_tests` benchmark in `daemon/crates/lattice-core/benches/baseline_workflows.rs:309-318` invokes the workflow against `src/auth.ts` + `src/session.ts` and symbols `loginUser` + `createSession`. The fixture also materializes `tests/auth.test.ts` and `tests/session.test.ts` (`fixture_files()` at lines 180–195). However, the regenerated [`baseline_metrics.json`](../baselines/baseline_metrics.json) records `"candidate_count": 0` for `find_relevant_tests`.

**Impact.** The latency baseline (p50 22µs, p99 30µs) is still a valid empty-result baseline, and the workflow exits cleanly. But it does not exercise the full tested-by graph traversal path, so post-Phase-4 retrieval regressions that touch test discovery could go unnoticed in this specific workflow.

**Severity.** Minor. The baseline file is dominated by the higher-cost workflows (`prepare_change`, `get_context_capsule`, `diagnose_failure`) and the spec budgets in `## Phase 1: Unified Identity Model` and `## Phase 2: Event Log Substrate` reference hot-path tool calls without naming `find_relevant_tests`.

**Suggested follow-up.** Open T-followup-R05-A in this build to extend the fixture or the bench invocation so `find_relevant_tests` produces ≥ 1 candidate before Phase 4 retrieval reviews begin. Acceptable fixes: register the fixture tests through the indexer's test-discovery path, or pass explicit test paths into `find_relevant_tests` so the recommender has a non-empty search surface.

### F2 — Benchmark P99 variance crosses prior recorded baseline by 2× on small workflows (minor)

**Observation.** The pre-existing `baseline_metrics.json` recorded `get_context_capsule` at p99 194µs; the in-review re-run records 413µs. `prepare_change` moved from p99 522µs to 462µs. `search_symbols` moved from p99 4µs to 3µs. The criterion `change` reports flag "Performance has regressed" on four of seven workflows.

**Impact.** All workflows remain at least an order of magnitude below the Phase 1 (2ms P99) and Phase 2 (5ms P99) budgets stated in [`## Phase 1: Unified Identity Model`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model) and [`## Phase 2: Event Log Substrate`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate). The deltas are run-to-run system noise, not a regression. However, future reviewers comparing single point-in-time numbers may misread the variance.

**Severity.** Minor. The `baselines/README.md#budget-interpretation` already documents that the file is a reference point for relative comparisons, but the variance band itself is not captured.

**Suggested follow-up.** Open T-followup-R05-B in this build to add a variance note (e.g., min/max across 3 runs, or a coefficient of variation) to either `baselines/README.md` or the per-workflow metric so later phase reviews can distinguish noise from real regressions. No code change is required for Phase 0 to pass; this is documentation hardening.

### Items confirmed clean (no finding)

- Definition-of-done item "current Lattice daemon tests pass in the fork before major changes" is the responsibility of later phase reviews; Phase 0 only ships docs + benches and the bench suite builds and runs cleanly.
- All five Phase 0 docs explicitly cite the [`## Phase 0: Fork Foundation`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation) anchor and downstream architecture cross-references.
- The fork decision binds R05, T06–T09, R10, and all transitive later tasks through `tasks.json`; this is explicit in [`## Downstream binding implications`](../../../architecture/2026-05-16-fork-or-extend-decision.md#downstream-binding-implications).
- Compatibility policy explicitly preserves all 5 legacy aliases observed in `daemon/crates/lattice-daemon/src/rpc/mcp.rs:1356-1380` with named deadlines tied to "one full phase cycle".
- Storage policy reserves a forward-compatible operator command shape (`lattice storage rollback --workspace <root> --migration <id> --archive-newer`) before any phase claims it.
- Bench file writes to a shadow path when the repo-root write fails, so the metrics regeneration is robust under read-only filesystem conditions (lines 416–447).

## Verdict

**PASS-WITH-FOLLOWUP-TASK-T-followup-R05-A and PASS-WITH-FOLLOWUP-TASK-T-followup-R05-B.**

Phase 0 satisfies every required deliverable: the fork/extend decision is evidence-backed and committed to a named branch (`feat/cognitive-workspace`), the architecture overview enumerates all three substrates with literal node/edge/event/memory taxonomies, the MCP compatibility policy classifies every one of the 37 canonical tools and 5 legacy aliases with a binding backward-compatibility rule, the crate boundary plan reserves all seven Phase 1–7 substrate names with an acyclic dependency graph, the storage migration policy enumerates 17 migrations in chronological phase order with per-migration `archived` rollback, and the baseline benchmark suite covers all 7 required workflows and regenerates `baseline_metrics.json` on demand. No `TODO`/`FIXME`/`XXX`, no suppressions, no over-length files, no doc-citation drift. Phase 1+ tasks may proceed against this substrate.

The two findings above are documentation/fixture improvements that should be opened in this build (per the task's "follow-up tasks must be opened in this build") and resolved before Phase 4 retrieval reviews. Neither blocks Phase 1 identity work or Phase 2 event log work.
