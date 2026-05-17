# T-followup-R05-A — `find_relevant_tests` baseline must record ≥ 1 candidate

**Phase:** 0 (follow-up from R05)
**Type:** benchmark hardening
**Model class:** balanced
**Depends on:** R05
**Opened by:** R05 (foundation review)
**Spec anchor:** [§Phase 0: Fork Foundation](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation), [`baselines/README.md#metrics-file`](../baselines/README.md#metrics-file)

## Finding context

R05 finding F1. The `find_relevant_tests` benchmark in `daemon/crates/lattice-core/benches/baseline_workflows.rs:309-318` invokes the workflow against `src/auth.ts` + `src/session.ts` and symbols `loginUser` + `createSession`. The fixture also materializes `tests/auth.test.ts` and `tests/session.test.ts` (`fixture_files()` at lines 180–195). Despite that, the regenerated `baselines/baseline_metrics.json` records `"candidate_count": 0` for `find_relevant_tests`. The latency baseline is still valid as an empty-result baseline, but the workflow does not exercise the tested-by graph traversal path.

## Goal

Make the `find_relevant_tests` baseline non-empty so later phase reviews can detect retrieval regressions in the test-discovery code path.

## Acceptable approaches

Either of the following:

1. Register the fixture test files through the same indexer path that production uses so `find_relevant_tests` sees them as test candidates. Investigate why the existing fixture tests are not picked up — likely a missing tested-by edge or a missing test-discovery rule for the fixture workspace layout.
2. Pass explicit test paths into the `find_relevant_tests` invocation so the recommender has a non-empty surface.

Whichever path is taken, the bench fixture must still be deterministic (no network, no external repo) and the regenerated `baseline_metrics.json` must show `"candidate_count": >= 1` for `find_relevant_tests`.

## Verification

- `cd daemon && cargo bench --bench baseline_workflows -- --warm-up-time 1 --measurement-time 3`
- `python3 -c "import json; d=json.load(open('docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json')); m=[x for x in d if x['name']=='find_relevant_tests'][0]; assert m['candidate_count'] >= 1, m"`

## Definition of done

- [ ] `find_relevant_tests` baseline metric has `candidate_count >= 1`.
- [ ] Bench fixture remains deterministic and self-contained.
- [ ] `baselines/README.md` updated if the bench inputs change.
- [ ] No regression in the other six workflow baselines (file still contains all 7).
