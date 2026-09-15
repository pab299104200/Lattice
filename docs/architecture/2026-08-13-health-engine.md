# Health Engine

## Decision

Lattice derives per-file health signal from facts already sitting in the index
and the git-intelligence snapshot, scores it on read, and never persists a
score. Two axes exist: `defect_risk` and `maintainability`. Every axis is
returned as an evidence bundle — a score, a band, and the facts that produced
it — and a bare number is never an acceptable response shape.

This is deliberately narrower than a general code-quality product. It answers
one question the evidence supports ("which files carry more defect-adjacent
history and structure than others, and why") and refuses to answer questions
it cannot back with a fact: there is no `performance_risk` axis, and no fact
family here observes runtime behavior. If Lattice ever gains profiling or
production-signal inputs, that is a new spec with its own evidence source, not
an extension of this one.

Facts and scores are governed by the same discipline as git intelligence
(`docs/architecture/2026-08-13-git-intelligence.md`): unknown is never scored
as zero, every consumer must be able to distinguish absence of signal from a
low score, and all arithmetic is integer per-mille so output is byte-identical
across platforms and generations.

## Facts versus scores

A **health fact** is one deterministic, persisted observation about a file or
symbol at a given index generation — `fan_in`, `hotspot_score`,
`max_cyclomatic_complexity`, `exported_no_dependents`, and so on. Facts are
produced once per fact family per generation, following the git-intelligence
generational pattern: atomic pointer swap, completeness flags, canonical-path
validation. A fact never encodes a judgment; it encodes a measurement plus,
where relevant, the source range that produced it.

A **score** is a read-time computation over persisted facts and a versioned
weight table: `(facts generation, weights version, config version) → AxisScore`.
Scores are never persisted. Recalibrating the engine — new weights, new
thresholds — changes a version number, not stored data, and any response can
be reproduced exactly from the three version identifiers it was computed
under. This is the load-bearing consequence of design decision 2 in
`docs/plans/2026-08-13-health-engine.md`: it is what makes "recalibrate
without a migration" possible, and no code path may cache or write an
`AxisScore` to disk.

## Fact families

Every fact family is a pure function over data an already-published index or
git-intelligence snapshot has — no process, filesystem, database, or clock
dependency of its own, matching the `lattice-core::git_intelligence`
boundary discipline. All five live under `lattice_core::health`:

| Family | Module | Generational store | Source |
| --- | --- | --- | --- |
| Graph | `health::graph_facts` | `health_graph_facts_store` | the published `DiGraph` at publish time |
| Complexity | `health::complexity_facts` | `health_complexity_facts_store` | per-language parser output (`line`/`end_line`/branch nodes/signature) |
| Dead symbol | `health::dead_symbol_facts` | `health_dead_symbol_facts_store` | `is_exported` ∧ empty `get_dependents`, minus a deterministic root-exclusion list |
| Test proximity | `health::test_proximity_facts` | `health_test_proximity_facts_store` | the graph-edge portion of `find_relevant_tests`, persisted; fuzzy token scoring stays request-time |
| Churn | `health::churn_facts` and `git_intelligence`'s `FileHistorySignal` | the existing git-intelligence store | the bounded commit window's per-commit tree diff |

Graph facts include `fan_in`, `fan_out`, `cycle_member`, `scc_size`,
`instability` (Ce/(Ca+Ce), per-mille), and `unstable_dependencies` — edges
where a more-stable file depends on a less-stable one, each flag carrying both
file keys and the source range of the import or call. Complexity facts include
per-function `max_cyclomatic_complexity`, `p90_cyclomatic_complexity`,
`max_function_length`, `max_nesting_depth`, and file-level rollups
(`over_threshold_share`, `function_count`) against thresholds that live in the
versioned `health::config::HealthConfig` table, never as constants scattered
per language. Churn facts extend `FileHistorySignal` with `lines_added`,
`lines_deleted`, and `line_churn` (H2.5 — see git-intelligence's own Decision
log for the component-boundary revision this required). Dead-symbol and
test-proximity facts always carry an index-completeness caveat in their fact
text ("no *indexed* dependents", "no edge-linked test found"), because a
missing edge and a genuinely dead symbol are indistinguishable from the graph
alone.

`health::config::HEALTH_CONFIG_VERSION` versions every threshold in this
table. Recalibrating a complexity cutoff or dead-symbol exclusion rule means
publishing a new config version, echoed on every produced fact — never editing
a constant in a producer.

## The two axes

`Axis::DefectRisk` and `Axis::Maintainability` are the only axes this engine
computes (design decision 1). `defect_risk` draws on bug-fix density, line
churn, hotspot, cycle membership, fan-in, ownership concentration (bus
factor / top-author share), and untested-change; `maintainability` draws on
complexity rollups, function length, instability and direction violations,
`scc_size`, fan-out, and dead exported symbols. A fact may feed both axes with
different weights.

`performance_risk` was considered and rejected: static graph, git, and
complexity facts do not predict runtime performance, and shipping a score
without honest evidence would violate the engine's own explainability
principle. It is not planned as future work under this spec — a performance
axis needs runtime or profiling inputs and its own evidence-gathering spec
before it can exist.

The two shipped axes are not evidenced equally, and the engine says so in its
own source rather than only in this document. `defect_risk` weights are
copied, per fact, from the `Derived weight` column of the committed backtest
report (`docs/reports/health-backtest/2026-08-14.md`, § "Per-fact
discrimination"); a test
(`weights_tests::the_shipped_defect_risk_weights_are_the_reports_derived_weights`)
re-derives the whole table from the report's published ROC-AUC values so the
shipped weights cannot drift from the evidence that set them. `maintainability`
weights are a documented editorial ordering — the backtest measured only
whether a fact ranks files that later receive a fix-shaped commit, which is
the `defect_risk` question, and the report contains no evidence about
maintainability at all. Nothing rendered from the `maintainability` axis may
cite the backtest as its justification.

## The evidence-bundle contract

Every score a consumer receives is an `AxisScore`:

```text
{ axis, score_per_mille, band, band_range, score_floor_per_mille,
  score_ceiling_per_mille, facts: [FactContribution...], inputs_missing,
  availability, weights_version, config_version }
```

`facts` lists every contributing `FactContribution` — kind, family, raw value,
rank percentile, weight, and share of the score — heaviest first, so a
consumer can render "high defect risk: cycle member (12-file SCC), fan-in 23,
8 bug-fix commits in a 500-commit window, no linked tests" and never a bare
number (design decision 3). `score_per_mille` is a weighted mean of available
facts' percentiles, matching the rule the backtest harness itself used to
score files (`health::backtest::features::score`), so the calibration table in
the backtest report describes exactly the scores this engine emits.

A bare integer is never an acceptable response shape for a health score, in
any verb, in any render mode.

### Unknown is never zero

Design decision 4 extends the git-intelligence rule ("unknown history is never
scored as zero risk") to every fact family, and this engine enforces it
structurally rather than by convention: a missing input widens the answer
instead of substituting a value for it.

```text
floor   = Σ(weight_f × percentile_f)                    / Σ(weight_all)
ceiling = (Σ(weight_f × percentile_f) + 1000 × missing)  / Σ(weight_all)
```

`missing` is the total weight of fact kinds with no value. When every input is
present, floor, `score_per_mille`, and ceiling coincide and the band is exact.
When inputs are missing, `band_range` widens to the `BandRange` implied by the
floor and ceiling, and every absent fact is named in `inputs_missing`.
Removing a fact can only widen this interval — the floor can only fall and the
ceiling can only rise, because the removed fact's weight moves from the
numerator into the unknown mass at both ends — which is the honest statement
of "less evidence never strengthens a claim" and is proved exhaustively by
`scoring::property_tests`.

Note the one asymmetry this implies: `score_per_mille` itself is a *mean*
over only the available facts, so it is not monotone under removal in
isolation — dropping a fact that scored below a file's other percentiles can
raise the mean of what remains. That is inherent to the harness's own scoring
rule (changing it would mean shipping scores the backtest report never
measured), which is exactly why the bundle carries the floor, the ceiling, and
`inputs_missing` as first-class fields rather than as decoration: a consumer
comparing two files with different available inputs compares their ranges,
not their point estimates alone.

A file outside the git window, in an unparsed language, or with an
incompletely indexed generation still receives a score — labeled by its
`availability` and `inputs_missing` — never a silently absent or
zero-substituted one.

## Weights and the backtest report

`docs/reports/health-backtest/2026-08-14.md` is the sole authority for
`defect_risk` weights and calibration cutoffs (design decision 6: backtest
before claim). No response text may use predictive language ("likely defect")
absent a committed backtest report backing it.

The report's headline, held out across cut points not used to derive the
weights: graph+git beats graph-only (ROC-AUC 0.890 vs 0.785), and
graph+git+complexity is best of the three families measured (ROC-AUC 0.901).
Per-fact discrimination varies widely and several facts carry a derived weight
near zero (`scc_size`, `cycle_member`, `author_count`) because their measured
ROC-AUC was near chance on this evidence; a fact measured at or below chance
is dropped, never inverted, because flipping its sign would invent a signal
the producer never claimed.

This doc does not restate the report's numbers as a durable claim; it points
at the report because the report, not this document, is what changes when the
weights are recalibrated. Anyone relying on a specific number should read the
report directly, current as of its own commit.

The report states its own limits and this document does not relax them:

- Ground truth is a subject-line heuristic (`looks_like_bug_fix`). An
  independent signal (commits touching both a test file and a production
  file) corroborates it — enrichment 1.336x — but the classifier has a
  measured 15.9% recall gap: fixes named by their body rather than a
  recognized subject prefix are missed and their files counted as clean. Every
  accuracy figure in the report inherits that classifier's error, in the
  conservative direction.
- The measurement is correlational, not causal. A fact that ranks
  defect-prone files well is not thereby a cause of defects, and a score is
  evidence, never a prediction of a specific future failure.
- Thirty cut points across five repositories is a small sample. Differences
  between fact families smaller than the spread across cut points are not
  evidence.

## Consumer contracts

Health rides inside the existing eight-verb surface exactly as git
intelligence does; no new MCP tool exists for it (design decision 5).

- **`impact`** may use the `defect_risk` bundle as the *primary* ordering
  signal within an existing graph-distance and severity tier, superseding the
  hotspot tie-break for `impact` specifically. Every ranked entry carries its
  fact bundle, so an ordering decision is always explainable from printed
  facts alone. See the corresponding revision recorded in
  `docs/architecture/2026-08-13-git-intelligence.md`'s Decision log; retrieval
  ranking is unaffected and keeps the tie-break-only rule described there.
- **`context` / `prepare_change`** render a bounded `health` section (default
  cap 10 files) for the files in the proposed change set or subsystem:
  per-file band with its top contributing facts. No health content is shown
  for files outside that set, and files with unavailable facts are listed with
  their availability metadata rather than omitted silently.
- **`diagnose`** treats `defect_risk` as a secondary ranking feature over
  candidate fault locations, applied only after stack-trace and graph
  proximity have selected candidates — health evidence never admits a file the
  trace evidence did not select.
- **`status{scope:"health"}`** reports fact-generation freshness and
  completeness, a band histogram, top-N files per axis, the active weights and
  config versions, and a reference to the last backtest report, so an agent
  can see exactly when to distrust a result.
- **PostToolUse hook** cites the `defect_risk` band and its heaviest
  contributing fact in place of a raw hotspot count, under the same bounds and
  silence rules as the existing git-intelligence warning (best-effort, exits
  `0` when the daemon is unavailable).

## Fact production and the read path

Facts are produced by a per-repository runtime and served to requests from an
in-memory handoff. They are never produced by a request.

`rpc::mcp` must not reopen `graph.db` mid-request, so a stored generation
cannot reach a request by loading it. `HealthFactsRuntime` — the health
counterpart of `GitIntelligenceRuntime`, sharing its latest-wins debounce and
the process-wide `IndexWorkCoordinator` — produces facts on watcher change
notifications, publishes them through the four generational stores, and fills
`HealthFactsSnapshotHandle`. A request clones an `Arc` from that handle.

The preference is **per family, never all-or-nothing**. A family with no active
generation falls back to deriving itself from the live graph, exactly as every
request did before the handoff existed. A cold start or a partial publication
therefore loses speed and never loses availability, which keeps the "unknown is
never zero" guarantee intact: a family that is genuinely absent still reports
`Degraded`/`Unavailable` and is named in `inputs_missing`.

Complexity facts are the one family with no fallback and never will have one,
because producing them means re-parsing file contents. Before the handoff
existed they were permanently unavailable at runtime; a published generation is
the only way they become available.

### Incrementality per family

| Family | Refresh | Why |
| --- | --- | --- |
| Graph | Whole-graph, coalesced to the settling window | Cycle membership is an SCC property: one added edge can merge components sharing no file with the edited one, so no changed-file subset bounds the recomputation. |
| Dead symbol | Whole-graph, coalesced | "Dead" asserts that nothing *anywhere* depends on a symbol; one new call edge in an unrelated file can revive a candidate. |
| Test proximity | Whole-graph, coalesced | Test linkage is reachability; a new edge anywhere can link a previously untested file. |
| Complexity | True per-file incremental | The unit of computation is one file, and `write_file_facts` refreshes exactly one file's rows inside the published generation. |

The `file_delta` and `candidate_delta` methods on the three snapshot families
are *comparison* helpers — each takes an already-produced snapshot and reports
which paths moved — not incremental producers. Wiring them into the runtime
would not have avoided a pass. Coalescing is therefore the honest bound: the
whole-graph pass runs at most once per settling window however many files were
saved, instead of once per request.

Changed paths accumulate across a burst rather than being replaced. The
sequence number is latest-wins so the worker collapses a burst into one pass,
but the *set* of touched files must survive that collapse or an incremental
complexity refresh would skip files saved while a pass was running.

### Measured effect

Publication cost is unchanged by this design; what moved is the read path
(`daemon/crates/lattice-core/benches/incremental_graph_maintenance.rs`,
`health_index_read_path`):

| Corpus | Live recomputation | Published generation |
| --- | --- | --- |
| 609 files | 10.43 ms | 0.97 ms |
| 5,000 files | 94.35 ms | 11.29 ms |

The residual cost on the published path is the ranking join, which still walks
every fact to compute populations; it is not a graph traversal.

## Out of scope

- Cross-file duplication / clone detection (design decision 7): the
  highest-cost, lowest-leverage item surveyed for this engine. Deferred to its
  own spec if backtesting ever shows the other fact families saturate.
- `performance_risk` (see "The two axes" above).
- Coverage-data ingestion, issue-tracker linkage, and blame/mailmap ownership
  — health's ownership-adjacent facts remain the attribution-signal-only model
  the git-intelligence contract already establishes; this engine adds no new
  ownership claim.
- Dashboards, trend UIs, and LLM-generated health prose. The engine's output
  is deterministic facts and scores inside existing verbs.

## Decision log

- **2026-08-13 — history-derived facts may set `impact`'s primary ordering
  via the score bundle.** Superseded the tie-break-only rule for `impact`
  specifically (design decision moved to `impact` ordering by `defect_risk`
  within each existing tier, per `docs/plans/2026-08-13-health-engine.md`
  H4.1/H4.2). Retrieval ranking keeps the tie-break-only rule unchanged,
  because retrieval's relevance model has no equivalent evidence-bundle
  contract to fall back on when history is unavailable. Recorded here and
  mirrored in `docs/architecture/2026-08-13-git-intelligence.md`'s own
  Decision log.
- **2026-08-13 — the git adapter now records per-file line stats.** H2.5
  extended the `git2` adapter's first-parent tree diff to record per-file
  added/deleted line counts (diff stats only, via `git2::Patch::line_stats()`)
  feeding the `churn` fact family's `line_churn` fact. This is a deliberate,
  narrow revision of git intelligence's "paths only" component boundary — no
  blob content, blame, or rename similarity was added. See the corresponding
  entry in `docs/architecture/2026-08-13-git-intelligence.md`'s Decision log.

## Startup production and history completeness

Graph publication schedules an initial full health refresh even when a persisted graph needs no changes. Refresh work runs under index admission; graph localization and analysis execute on blocking workers. Failures retain prior persisted generations for diagnosis, omit failed families from the live handoff, and retry with exponential backoff capped at 60 seconds. Incremental source read errors retry; confirmed watcher deletions remove facts. Full generation replacement removes files absent from the published graph.

Complexity generations live in a repository-specific `health-complexity-<repository_id>.db` beside the graph database, avoiding a shared active-generation pointer across roots. These are derived caches: startup regenerates them from the graph and source, so old graph-database complexity rows are not authoritative and no live source or memory data is migrated or deleted.

Git freshness gates every history family. Complete per-file history remains usable when broad commits exceed co-change limits. Symbol overflow suppresses symbol evidence; missing or invalid file observations suppress file-dependent evidence; incomplete co-change observations suppress partner advisories. Status preserves aggregate Git metadata alongside these family-specific availability fields. Rendering uses the family supplying the delivered evidence.

Health coverage denominators count indexed graph files. Test proximity describes production files and excludes tests/support files; it does not assert tests were executed. Complexity has no applicable measurement for documentation without executable control flow. Other missing measurements degrade the family rather than manufacturing zero complexity.

The runtime publishes complexity replacements only after all scoped source reads succeed; partial database writes remain unpublished and are discarded on failure. Old generations are pruned after successful publication and on startup, preserving the active pointer. No retry renews source truth from a partial cache.

Run the real CLI/daemon startup regression with `python3 daemon/tests/health_startup_smoke.py daemon/target/debug/lattice` from the repository root after building. It uses a private Git repository, loopback listener and transport directory, checks cold and warm startup without edits, and terminates only its own daemon. It needs local socket permission; it does not restart the shared daemon.
