# Deterministic Health Engine — Change Spec

**Date:** 2026-08-13
**Status:** Proposed implementation plan
**Audience:** Implementing agent. Self-contained; do not assume access to the conversation that produced it.
**Prior art:** `docs/plans/2026-08-12-agent-adoption-recovery.md` (item C1 built the git-intelligence layer this spec extends); `docs/architecture/2026-08-13-git-intelligence.md` (the governing contract this spec deliberately revises in one place — see H4).

> Findings below are a code-survey snapshot from 2026-08-13, verified by direct
> reading and grep. They describe what existed at that time; establish current
> behavior from the implementation and tests before acting on a dated claim.

## Problem statement

Lattice has the raw inputs of a code-health engine but no engine. What exists
today (verified 2026-08-13):

- **Git intelligence (strong):** `daemon/crates/lattice-core/src/git_intelligence.rs`
  computes per-file hotspot, bug-fix commits and per-mille density, co-change,
  author count, top-author share, and bus factor over a bounded 500-commit
  window; persisted generationally in `graph.db`; surfaced in `context`/`impact`
  payloads (`rpc/mcp.rs:7671-7789`) — but only as tie-breaks and advisories.
- **Graph (partial):** `graph/model.rs` exposes raw dependency/dependent lists
  and one normalized metric (degree centrality, ~line 423). No SCC/cycle
  detection anywhere (zero grep hits for `tarjan|kosaraju|is_cyclic|strongly_connected`),
  no instability metric, no fan-in/fan-out persisted as facts.
- **Complexity: absent.** Parsers record `line`/`end_line`/`signature`/`body`
  per node but compute no complexity, nesting, or size metric.
- **Dead symbols: absent** — despite `is_exported` + `get_dependents` making
  "exported, zero dependents" a trivial graph query.
- **Test linkage (exists):** `find_relevant_tests` (`intelligence/agent.rs:2620`)
  scores tests by graph edges plus path/name heuristics and reports `gaps`; not
  persisted as a per-file fact.
- **Risk (crude):** `risk_for_node` (`agent.rs:7474`) buckets purely on
  downstream count thresholds (≥6 high, ≥3 medium) plus export promotion.
- **No composite health signal, no health content in `status`, and no
  backtesting** — `looks_like_bug_fix` (subject-prefix heuristic,
  `git_intelligence.rs:682`) has zero measured precision, and nothing replays
  history to test whether any signal predicts later defects.

The gap versus Repowise-class systems is a deterministic, *explainable* health
layer: per-file facts, separately scored risk axes that cite their evidence,
integrated into the existing eight verbs — with predictive claims proven by
historical backtest before they are made.

## Design decisions (fixed for this spec)

1. **Two scores, not three:** `defect_risk` and `maintainability`.
   `performance_risk` is explicitly rejected: static graph/git/complexity facts
   do not predict performance, and shipping a score without honest evidence
   violates the engine's own explainability principle. If a performance signal
   is ever added it needs runtime/profile inputs and its own spec.
2. **Facts first, scores derived.** Health *facts* are persisted per generation
   (same generational model as git intelligence). *Scores* are computed at read
   time from persisted facts plus a versioned weight table — never persisted —
   so recalibration changes weights, not stored data, and every response can be
   reproduced from `(facts generation, weights version)`.
3. **Every score ships its evidence.** A score is an ordered bundle:
   `{axis, score_per_mille, band, facts: [{fact_kind, value, source_range, window/head where applicable}]}`.
   A consumer must be able to render "high defect risk: cycle member (12-file
   SCC), fan-in 23, 8 bug-fix commits in 500-commit window, no linked tests" —
   never a bare number. Integer per-mille arithmetic throughout, matching the
   git-intelligence determinism discipline.
4. **Unknown is never zero.** Missing history, an incomplete index, or a parse
   failure yields `availability: unavailable|degraded` on the affected facts and
   the score reports which inputs were absent. This extends the existing
   git-intelligence rule ("unknown history is never scored as zero risk") to
   every fact family.
5. **No new MCP tools.** Health rides inside `impact`, `context`,
   `prepare_change`, `diagnose`, and `status`, exactly as git intelligence does
   today. The eight-verb surface is unchanged.
6. **Backtest before claim.** The backtest harness (H1) lands before the scores
   (H3) and its output *sets* the initial weights. Until the backtest report
   exists in-repo, no response text may use predictive language ("likely
   defect"); facts are stated descriptively.
7. **Duplication detection is out of scope.** Cross-file clone detection across
   six languages is the highest-cost, lowest-leverage item surveyed; it is
   deferred to its own spec if backtesting ever shows the other facts saturate.

## The plan

Ordering principle: **H1 backtest harness → H2 fact producers → H3 scores
calibrated by H1 → H4 verb integration + contract revision → H5 measurement.**
The harness comes first because it is the only honest source of weights and it
immediately audits the already-shipped `looks_like_bug_fix` heuristic.

### Phase H1 — Historical backtest harness (do first)

**H1.1 Replay engine.** New `lattice-core::health::backtest` module (pure, like
the git miner) plus a `git2` replay adapter: split the repository's first-parent
history at cut-point commit `T`; mine facts from the window *before* `T`
(reusing the existing miner and the H2 producers as they land); label each file
by whether it is touched by a fix-shaped commit in the horizon *after* `T`
(default horizon: the next 90 days or 200 commits, whichever first). Multiple
cut points per repo (default 6, evenly spaced) to avoid single-split luck.
Bounded exactly like the git adapter: paths only, no blob parsing, clamped
windows, exclusion counters.

**H1.2 Label quality audit.** Before trusting fix-shaped subjects as ground
truth, emit a labeled sample (default 50 subjects: 25 classified fix, 25 not)
into the report for human spot-review, and compute classifier agreement against
a second signal (commits whose diff touches an existing test file *and* a
production file). Record both in the report; if agreement is poor, tighten the
vocabulary in `looks_like_bug_fix` in this same change (contract permits it —
the classifier is declared "an explainable heuristic").

**H1.3 Report.** `lattice health-backtest` CLI subcommand (operational report,
like `metrics` — not an MCP verb) producing a deterministic markdown + JSON
report: precision/recall/PR-AUC per fact family (graph-only vs graph+git vs
all), calibration table (predicted band vs observed defect rate per decile),
per-repo and pooled across the Cadres repos, with window sizes, cut points, and
exclusion counters. The report is committed under
`docs/reports/health-backtest/<date>.md` and is the *only* authority for H3
weights.

**Acceptance:** harness runs on this repo and at least two sibling Cadres
repos; fixture tests prove identical output for replayed input, correct
cut-point isolation (no post-`T` leakage into facts — test with a synthetic
repo where leakage would flip a label), and horizon boundary handling; the
first committed report exists with graph-only vs graph+git comparison.

### Phase H2 — Per-file health facts at index/refresh time

All fact producers are pure `lattice-core` functions over data the index
already has; persistence follows the git-intelligence generational pattern
(new tables keyed by generation + stable file/symbol key; atomic pointer swap;
completeness flags; canonical-path validation).

**H2.1 Graph facts** (new `health::graph_facts`, computed from the built
`DiGraph` at publish time):
- `fan_in`, `fan_out` per file and per exported symbol (persisted counts, not
  request-time list lengths);
- `cycle_member` + `scc_size` + `scc_id` via `petgraph::algo::tarjan_scc` over
  the file-level dependency condensation;
- `instability` = Ce/(Ca+Ce) in per-mille, and `unstable_dependency` flags:
  edges where a more-stable file depends on a less-stable one by more than a
  threshold (the classic direction violation), each flag carrying both file
  keys and the underlying import/call source range.

**H2.2 Complexity facts** (extend the parser layer, per language):
- `function_length` from existing `line`/`end_line` (free);
- `branch_count`-based cyclomatic approximation: each language parser
  contributes its branch node-kind set (if/match/loop/catch/ternary/boolean-op
  guards) — six contained additions following the existing `parser/*.rs`
  structure; plus `max_nesting_depth` and `param_count` from the signature.
- Per-file rollups: max and p90 function complexity/length, count of functions
  over thresholds. Thresholds live in the versioned weight/config table, not in
  code constants scattered per language.

**H2.3 Dead-symbol facts:** `exported_no_dependents` per symbol via the
existing graph (`is_exported` ∧ `get_dependents` empty), with a deterministic
exclusion list for legitimate roots (binary entry points, test symbols via
`is_test_file`, `pub` items re-exported at crate roots, trait impl methods).
Presented always as "no *indexed* dependents" — the fact text must carry the
index-completeness caveat, wired to the parse-failure state from `status`.

**H2.4 Test-proximity facts:** persist, per production file, `linked_test_count`
and `strongest_link_kind` (graph-edge vs name/path heuristic) by running the
graph-edge portion of `find_relevant_tests` (`agent.rs:2620`) at publish time —
only the deterministic edge-based signals are persisted as facts; the fuzzy
token scoring remains request-time ranking. A changed production file with zero
edge-linked tests is the `untested_change` fact `impact` consumes.

**H2.5 Line-level churn (git adapter extension):** extend the `git2` adapter's
first-parent tree diff to record per-file added/deleted line counts (diff
stats only — still no blob content, no blame; this is a deliberate revision of
the "paths only" boundary in the git-intelligence contract, documented in H4).
Aggregate as `line_churn` per file per window alongside the existing
commit-count hotspot. Bounds: reuse the existing per-commit path cap; an
over-wide commit is excluded from churn exactly as it is from hotspots.

**Acceptance:** pure-producer fixture tests per fact family (ordering,
determinism, per-mille arithmetic, threshold edges, SCC correctness on a known
cyclic fixture, dead-symbol exclusion list, leakage-free churn); store tests
prove generational isolation and atomic swap for the new tables; incremental
index of one file updates only affected facts; `cargo test --workspace` green.

### Phase H3 — Scores as explainable fact bundles

**H3.1 Score engine** (`health::scoring`): pure function
`(facts, weights) → AxisScore` for `defect_risk` and `maintainability`.
Weighted per-mille sum with per-fact contribution recorded in the output, plus
a `band` (low/moderate/high/critical) from calibrated cutoffs. Weights and
cutoffs come from the H1 report, stored in a versioned in-crate table
(`weights_version` echoed in every response); the golden-test pattern from
`retrieval_v1/golden_tests.rs` pins score stability across refactors.
- `defect_risk` inputs: bug-fix density, line churn, hotspot, cycle membership,
  fan-in, ownership concentration (bus factor/top-author share), untested-change.
- `maintainability` inputs: complexity rollups, function length, instability +
  direction violations, scc_size, fan-out, dead exported symbols.
  A fact may feed both axes with different weights.

**H3.2 Availability composition:** a score over incomplete inputs reports
`inputs_missing: [fact_kind…]` and widens its band to a range rather than
faking precision (extends design decision 4). A file outside the git window or
in an unparsed language still gets a graph-facts-only score, labeled as such.

**H3.3 Replace `risk_for_node`'s thresholds** (`agent.rs:7474`) with the
`defect_risk` bundle wherever `RiskRecommendation` is produced — same struct
shape extended with the evidence bundle; the old ≥6/≥3 magic numbers are
superseded (no-legacy-debt: delete, don't alias).

**Acceptance:** golden tests pin exact per-mille outputs for fixture fact sets;
property test: removing a contributing fact never *raises* an axis score;
availability tests prove degraded inputs produce labeled, band-widened output;
the H1 harness re-run with H3 weights reproduces the report's stated
precision/recall (the calibration loop closes).

### Phase H4 — Verb integration and contract revision

**H4.1 Contract first.** New `docs/architecture/2026-08-13-health-engine.md`
capturing the design decisions above, and a revision to
`docs/architecture/2026-08-13-git-intelligence.md` in the same change:
- § "Component boundary": the adapter now records per-file line stats (H2.5) —
  still no blob content, blame, or rename similarity;
- § "Consumer contracts → impact": history-derived facts may now contribute to
  the *primary* ordering **only via the health score bundle** (which always
  carries its evidence); the tie-break-only rule is superseded for `impact` and
  retained for retrieval ranking. This is an explicit product decision, not a
  drift — record it in both docs' Decision sections.

**H4.2 `impact`:** order affected files within graph-distance tier by
`defect_risk` (replacing the hotspot tie-break), each entry carrying its fact
bundle; keep `missing_cochange_partners` unchanged; add `untested_change`
entries for diff files with zero edge-linked tests.

**H4.3 `context` / `prepare_change`:** a bounded `health` section for the files
in the proposed change set / subsystem: per-file bands with top-3 contributing
facts, capped (default 10 files), omitted with availability metadata when
facts are unavailable. No health content on unrelated files.

**H4.4 `diagnose`:** when candidate fault locations are ranked, `defect_risk`
becomes a secondary ranking feature (after stack-trace/graph proximity — never
admitting a file the trace evidence didn't select), with the contributing facts
in the explanation.

**H4.5 `status`:** new `scope: "health"` — repository fact-generation
freshness/completeness, band histogram, top-N files per axis, weights version,
last backtest report reference, and explicit incomplete-analysis truth (parse
failures, git-window degradation) so an agent can distrust results correctly.
`lattice metrics` gains a health-adoption row (whether injected health evidence
was followed), reusing the adoption-ledger pipeline.

**H4.6 PostToolUse hook:** the existing hotspot warning (top-decile cutoff,
max 5 files, best-effort, exit 0) upgrades to cite the `defect_risk` band and
its top fact instead of raw hotspot count — same bounds, same silence rules.

**Acceptance:** `impact` on a diff in this repo shows risk-ordered files with
readable evidence; MCP schema tests updated for every payload change
(`mcp_schema_tests` asserts schemas ↔ handlers); render-response contract tests
(`docs/architecture/2026-08-13-render-response-contract.md`) cover the health
sections in markdown and JSON renders; hook fixture tests prove the new warning
text and unchanged silence behavior; both architecture docs updated in the same
change; grep proves no response text uses predictive language absent a
committed backtest report.

### Phase H5 — Prove it stays honest

- Wire the backtest into the regression-report machinery
  (`lattice-core/src/metrics/report.rs`): precision/recall/calibration become
  tracked `MetricSignal`s with thresholds and CI exit codes, so a weights or
  classifier change that degrades prediction fails loudly.
- Add a health task to the worth-it benchmark
  (`docs/architecture/2026-08-13-worth-it-benchmark-contract.md`): "which files
  in this diff are riskiest and why" — scored on citing the evidence files.
- Criterion bench proving fact production stays within the incremental-index
  budget (extend `benches/incremental_graph_maintenance.rs`).

**Acceptance:** regression report includes the three health signals with
baselines; benchmark run recorded; full-repo fact publish measured and within
the index deadline bounds on this repo.

## Out of scope

- `performance_risk` (design decision 1) and duplication/clone detection
  (design decision 7) — each needs its own spec with its own evidence source.
- Coverage-data ingestion, issue-tracker linkage, blame/mailmap ownership —
  the ownership facts remain the attribution-signal-only model of the git
  contract.
- Dashboards, trend UIs, LLM-generated health prose. The engine's output is
  deterministic facts inside existing verbs.
- Retrieval-ranking changes beyond what the git contract already allows.

## Success criteria

1. A committed backtest report exists showing graph+git+complexity beats
   graph-only on precision/recall for this repo, with calibration by decile —
   before any response claims predictive value.
2. `impact` on a real diff orders files by defect risk and every ranked entry
   is explainable from its printed facts alone ("cycle member, fan-in 23,
   8 bug-fix commits, no linked tests"), never a bare number.
3. `status{scope:"health"}` reports fact freshness, completeness, and weights
   version; degraded inputs are visible, never silently zero.
4. All facts and scores are deterministic: identical input generations produce
   byte-identical bundles on every platform (golden tests enforce).
5. The git-intelligence contract revision (primary ranking via score bundles,
   line-stat mining) is recorded in both architecture docs; no superseded
   tie-break code path or `risk_for_node` threshold survives.
6. `cargo test --workspace` green; health regression signals wired into the
   metrics report with CI thresholds.
