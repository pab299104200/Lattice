# T-followup-R05-B — Document baseline benchmark variance band

**Phase:** 0 (follow-up from R05)
**Type:** docs
**Model class:** balanced
**Depends on:** R05
**Opened by:** R05 (foundation review)
**Spec anchor:** [§Phase 0: Fork Foundation](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation), [`baselines/README.md#budget-interpretation`](../baselines/README.md#budget-interpretation)

## Finding context

R05 finding F2. When R05 re-ran `cargo bench --bench baseline_workflows`, criterion reported "Performance has regressed" on `get_context_capsule`, `expand_context`, `impact_from_diff`, `search_symbols`, and `find_relevant_tests`. Investigation showed the deltas are run-to-run system noise — all workflows still sit well below the Phase 1 (2 ms P99) and Phase 2 (5 ms P99) hot-path budgets stated in `## Phase 1: Unified Identity Model` and `## Phase 2: Event Log Substrate` of the plan. However, the current `baselines/baseline_metrics.json` records a single point sample per workflow, with no variance information, so a future reviewer comparing a single point-in-time number to the baseline could misread normal jitter as a regression.

## Goal

Make the baseline self-documenting about its noise floor so later phase reviews can distinguish real regressions from system noise.

## Acceptable approaches

Either of the following:

1. Extend `baselines/README.md` with a "Variance band" section that documents the expected coefficient of variation for each workflow (computed from at least 3 independent runs on the reference machine) and the rule for when a later run constitutes a real regression vs. noise (e.g., "p99 outside ±25% of the recorded baseline, repeated across 2 consecutive runs").
2. Extend the benchmark to capture min/max (or stddev) across a small number of repeated measurements per workflow and write those into the metric struct, while keeping backward compatibility with the existing JSON consumers.

Approach 1 is cheaper and sufficient for the Phase 0 baseline. Approach 2 is preferred if Phase 9 metrics work will consume the file programmatically.

## Verification

- For approach 1: `grep -q '## Variance' docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/README.md` (or whichever heading is chosen) and a written rule for distinguishing noise from regression.
- For approach 2: `python3 -c "import json; d=json.load(open('docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json')); assert all('p99_us_stddev' in m or 'p99_us_max' in m for m in d)"` (or equivalent shape check).

## Definition of done

- [ ] Baseline documentation distinguishes run-to-run noise from real performance regressions.
- [ ] The rule is explicit enough that a future R-task can mechanically apply it.
- [ ] No regression in the other Phase 0 baseline guarantees (7 workflows, p50/p95/p99 µs, payload size, candidate count, commit SHA, timestamp).
