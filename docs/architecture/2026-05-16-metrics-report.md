# Metrics Report Architecture

This document describes the Phase 9 regression report shipped by [daemon/crates/lattice-daemon/src/bin/lattice_report.rs](/home/pete/cadres/lattice/daemon/crates/lattice-daemon/src/bin/lattice_report.rs:1) and [daemon/crates/lattice-core/src/metrics/report.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/metrics/report.rs:1).

Spec anchors:

- `## Phase 9: Metrics And Evaluation`
- `## Measurable Success Criteria`

The goal is operational, not aspirational: changes to retrieval, memory, and workflow behavior must be evaluable from a repeatable report rather than anecdote.

## Architecture

The implementation is split at the existing crate boundary:

- `lattice-core::metrics::signals` remains the canonical collector for `MetricValue`.
- `lattice-core::metrics::report` turns collected values plus optional baseline and benchmark inputs into a `RegressionReport`.
- `lattice_report` is the operator and CI entry point. It gathers live workspace events from `.lattice/events.db`, optionally supplements missing signals from the T66 benchmark report, optionally loads a baseline snapshot, renders the report, and enforces `--fail-on-regression`.

This keeps the report logic reusable from tests and later review tooling while leaving workspace IO and CLI parsing in `lattice-daemon`.

## Signal Definitions

The report does not define new metrics. It consumes the T65 canonical signals:

- `tool_calls_per_successful_task`
- `irrelevant_files_opened_per_task`
- `relevant_anchor_recall`
- `memory_inclusion_precision`
- `memory_later_used_rate`
- `stale_memory_surfaced_rate`
- `contradiction_missed_rate`
- `tests_recommended_vs_needed`
- `workflow_success_after_first_plan`

Definitions, collection boundaries, honest-null behavior, and provenance sources live in [daemon/crates/lattice-core/src/metrics/signals.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/src/metrics/signals.rs:1). The report layer preserves that contract; it does not reinterpret missing data as zero.

## Thresholds

`SuccessCriteriaThresholds::initial()` encodes the initial targets from `## Measurable Success Criteria` explicitly:

- 30% reduction in discovery tool calls
- 40% reduction in irrelevant file reads
- 80% memory inclusion precision
- 0 contradiction missed rate
- 0 stale-memory-without-label rate
- 90% relevant-test recommendation recall

Signals without an initial target in the spec currently render as `not_applicable` rather than silently passing.

## Inputs

`lattice_report` consumes up to three evidence sources:

1. Live workspace events from `./.lattice/events.db`
2. Optional baseline JSON from `--baseline`
3. Optional benchmark JSON from `--benchmark`

Supported baseline shapes:

- `Vec<MetricValue>` produced by the report stack itself
- T66 fixture benchmark snapshots containing `metrics`
- T04 legacy baseline workflow snapshots

The T04 legacy benchmark is only partially mappable to Phase 9 signals. When that file lacks a truthful value, the report records `n/a` with the real reason instead of fabricating a baseline.

Supported benchmark shape:

- The T66 JSON fixture report at `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/cognitive_workspace_metrics.json`

The benchmark report is also the evidence source for per-signal task, event, and memory pointers used during regression investigation.

## Output Formats

The binary supports three formats:

- `text`: human-readable row table with current, baseline, delta, target, status, and evidence digest
- `json`: serialized `RegressionReport`
- `ci-summary`: one line with pass/fail counts and failed signal names

Core JSON schema:

- `scope`
- `success_criteria`
- `rows`
- `benchmark_report_path`
- `generated_at`
- `pass_count`
- `fail_count`
- `missing_count`
- `not_applicable_count`

Each row contains:

- `signal`
- `current`
- `baseline`
- `delta`
- `target`
- `status`
- `evaluation_reason`
- `evidence`

## Workspace Boundary Rules

The CLI is workspace-bounded by construction:

- Live collection always reads `./.lattice/events.db` from the current working directory.
- Workspace id is the canonicalized current directory path.
- Branch and repo scopes never cross into sibling workspaces.
- Session scope resolves the latest captured session inside the active workspace only.

The benchmark file can live elsewhere, but it is read as inert input data; it does not cause workspace traversal.

## Observability

`lattice_report` emits structured `tracing` events to stderr for CI and operator logs. Each invocation records:

- `scope`
- `signal_count`
- `pass_count`
- `fail_count`
- `format`

It also logs per-signal live collection with `signal` and `event_count`, so a missing or unexpectedly empty report is diagnosable from stderr alone.

## CI Integration

Example commands:

```bash
cd daemon && cargo build --release --bin lattice_report
cd daemon && ./target/release/lattice_report \
  --scope repo \
  --benchmark ../docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/cognitive_workspace_metrics.json \
  --format ci-summary \
  --fail-on-regression
```

Use `--output <path>` when the job needs a persisted artifact in addition to stderr logs.

`--fail-on-regression` exits non-zero when any thresholded signal fails. Missing data remains visible in the report; it is not coerced to pass.

## Regression Runbook

When a row fails:

1. Read the row’s `evaluation_reason`.
2. Inspect the row’s evidence pointers:
   - `task_ids` point to the benchmark task or workflow slice.
   - `event_ids` point to the event sequence that contributed to the signal.
   - `memory_ids` point to the retrieved memory records involved in the rate.
3. Re-run the relevant benchmark task or workspace workflow and compare the new `MetricValue` against the failing row.
4. Check whether the regression is live-workspace only, benchmark-only, or both:
   - live-workspace regressions usually indicate event-capture, workflow, or scoping drift
   - benchmark-only regressions usually indicate retrieval, test recommendation, or memory-surfacing drift
5. Fix the behavior at the canonical layer, then re-run the report and the T69 regression tests.

## Build And Deploy

Build:

```bash
cd daemon && cargo build --release --bin lattice_report
```

The binary is emitted at:

- `daemon/target/release/lattice_report`

If you are installing updated report binaries for operators, copy or package `lattice_report` explicitly; it is separate from `lattice`.
