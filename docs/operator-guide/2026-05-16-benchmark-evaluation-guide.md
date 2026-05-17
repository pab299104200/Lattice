# Benchmark Evaluation Guide

This guide explains how to run and interpret cognitive workspace benchmarks. It is driven by [## Phase 9: Metrics And Evaluation](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-9-metrics-and-evaluation), [## Measurable Success Criteria](../plans/2026-05-16-cognitive-workspace-fork-plan.md#measurable-success-criteria), and [## Phase 11: Hardening](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-11-hardening).

## Overview

Benchmarks measure whether retrieval, memory, workflow, and hardening behavior improves beyond current Lattice without relying on anecdotes. Metrics architecture is in [Metrics Report Architecture](../architecture/2026-05-16-metrics-report.md#architecture). The baseline files live under `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/`.

## Running baseline benchmarks

From the repository root:

```bash
cd daemon && cargo build --release --benches
cd daemon && cargo bench --bench baseline_workflows -- --warm-up-time 1 --measurement-time 3
```

Compare output with `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json` and `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/cognitive_workspace_metrics.json`.

## Running large-repo benchmarks

Large-repo hardening is specified by T80 in [T80 Large-repo performance tests](../plans/2026-05-16-cognitive-workspace-fork-build/tasks/T80.md#t80--phase-11--large-repo-performance-tests). The expected output is `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/large_repo_results.json` when the T80 fixture corpus exists.

The T80 suite covers hot-path workflow calls, indexing wall-clock, event-log compaction, payload spillover, and P50/P95/P99 latency by fixture. Run the task-declared command when fixtures are present:

```bash
cd daemon && cargo test -p lattice-core --lib hardening::large_repo_tests -- --include-ignored
```

## Interpreting the metrics report

Use the signal definitions in [## Signal Definitions](../architecture/2026-05-16-metrics-report.md#signal-definitions). The report should identify the metric value, provenance, scope, time range, and null reason when canonical data is unavailable.

Treat nulls as useful diagnostics. A null with `session_metrics` fallback provenance is different from a canonical metric computed from bounded event evidence.

## Regression detection

Regression checks compare current output against baseline and target thresholds. Investigate:

- more tool calls per successful task
- higher irrelevant file reads
- lower relevant anchor recall
- lower memory inclusion precision
- stale or contradicted memory shown as trusted guidance
- worse relevant-test recommendation
- lower workflow success after first plan
- hot-path P99 regressions from Phase 1 or Phase 2 budgets

CI integration and regression runbook details are in [## CI Integration](../architecture/2026-05-16-metrics-report.md#ci-integration) and [## Regression Runbook](../architecture/2026-05-16-metrics-report.md#regression-runbook).

## Initial targets

The initial targets from [## Measurable Success Criteria](../plans/2026-05-16-cognitive-workspace-fork-plan.md#measurable-success-criteria) are:

- 30 percent fewer discovery tool calls on benchmark tasks
- 40 percent fewer irrelevant file reads
- 80 percent memory inclusion precision on curated memory benchmarks
- zero trusted display of known contradicted memory
- zero trusted display of known stale memory without stale label
- 90 percent correct relevant-test recommendation on curated tasks
