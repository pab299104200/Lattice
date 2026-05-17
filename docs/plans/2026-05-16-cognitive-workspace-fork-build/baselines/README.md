# Baseline Workflow Benchmarks

This directory stores the Phase 0 baseline required by `## Phase 0: Fork Foundation` in [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md](/home/pete/cadres/lattice/docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:566). The purpose is to measure the current Lattice workflow hot paths before the fork-build phases change identity resolution, event capture, retrieval, or metrics behavior.

The benchmark suite is implemented in [daemon/crates/lattice-core/benches/baseline_workflows.rs](/home/pete/cadres/lattice/daemon/crates/lattice-core/benches/baseline_workflows.rs:1). It uses a deterministic temp-workspace fixture, runs the current workflow logic with `cargo bench`, and rewrites `baseline_metrics.json` on every successful run.

## Re-run

Run the two required validation commands from the repo root:

```bash
cd daemon && cargo build --release --benches
cd daemon && cargo bench --bench baseline_workflows -- --warm-up-time 1 --measurement-time 3
```

The second command regenerates [baseline_metrics.json](/home/pete/cadres/lattice/docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json:1).

## Metrics File

`baseline_metrics.json` is the machine-readable snapshot consumed by later regression work. Each object includes:

- `name`: workflow benchmark name
- `p50_us`, `p95_us`, `p99_us`: latency percentiles in microseconds
- `bytes_returned`: serialized response size for the benchmarked workflow
- `candidate_count`: top-level result count captured with the payload size
- `commit_sha`: git commit recorded when the snapshot was captured
- `recorded_at`: unix timestamp captured at bench time

The baseline is intentionally compact. It is the reference point for the later success claims in `## Measurable Success Criteria` from [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md](/home/pete/cadres/lattice/docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:875), especially the discovery-call and irrelevant-read reductions.

## Budget Interpretation

Phase 1 and Phase 2 define hot-path budgets that later work must validate as deltas against this baseline:

- Identity resolution must add no more than 2 ms P99 to hot-path tool calls.
- Event write must add no more than 5 ms P99 to hot-path tool calls such as `prepare_change` and `get_context_capsule`.

Those targets come from the Phase 1 and Phase 2 definition-of-done text in [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md](/home/pete/cadres/lattice/docs/plans/2026-05-16-cognitive-workspace-fork-plan.md:609). This Phase 0 suite measures pre-Phase-1 behavior only. Later tasks should compare their new numbers against this file rather than replacing the meaning of the baseline.
