# Cognitive Workspace Benchmark Fixtures

This fixture corpus supports the Phase 9 benchmark suite described by `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.

Spec anchors:

- `## Phase 9: Metrics And Evaluation`
- `## Measurable Success Criteria`
- `## Risks`, specifically `### Overfitting To Current Repo`

The risk control is binding at the fixture source:

> Control: fixture repos across languages, repo sizes, and documentation styles.

## Layout

Each fixture repository lives in one directory and is indexed as its own workspace:

- `rust-repo/` is a small, moderately documented Rust service fixture.
- `typescript-repo/` is a medium, richly documented TypeScript fixture with React and service modules.
- `python-repo/` is a small, docstring-rich Python package fixture.

Each fixture includes a `manifest.toml` with:

- `name`: stable fixture repository name.
- `size_class`: one of `small`, `medium`, or `large`.
- `documentation_style`: one of `sparse`, `moderate`, or `rich`.
- `benchmark_tasks`: task IDs owned by the fixture.
- `expected_event_counts`: deterministic event-count expectations for fixture indexing and task execution.
- `golden_anchors_path`: relative path to `../golden_anchors.json`.

## Golden Anchor Schema

`golden_anchors.json` is a stable array of benchmark task entries. Every entry has the same field set:

- `task_id`: stable task ID referenced by fixture manifests.
- `repo`: fixture repository directory name.
- `intent`: high-level workflow intent being measured.
- `task_prompt`: prompt supplied to the workflow tools.
- `expected_anchors`: stable identities expected from retrieval, each with `file`, `symbol`, and `doc_section`.
- `expected_tests`: tests expected from `find_relevant_tests`.
- `expected_memory_classes`: memory classes expected to be relevant for the task.
- `expected_render_mode`: compact or full workflow render expectation.
- `success_criteria_thresholds`: thresholds tied to `## Measurable Success Criteria`.

The benchmark validates this schema before running. A missing field, empty task ID, duplicate task ID, or task without anchors fails fast.

## Determinism

The deterministic seed is `20260516`. Benchmark fixtures must be static and hermetic. Fixture repositories change only through a documented fixture bump that updates `golden_anchors.json` in the same commit. Benchmark report generation must not read outside the fixture repository and the per-run temp workspace.

## Report

`cognitive_workspace_benchmark` writes its JSON report to:

`docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/cognitive_workspace_metrics.json`

T68 consumes that report with the canonical T65 `MetricValue` shape.
