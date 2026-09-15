//! Joining published fact snapshots into scoreable per-file bundles.
//!
//! The engine ([`super::engine::score_axis`]) is pure and knows nothing about
//! producers. This module is the adapter: it reads whatever H2 snapshots a
//! caller has, ranks each fact against the repository's own population, and
//! hands the engine a [`FileFacts`] bundle per file.
//!
//! # Why the population lives here
//!
//! A percentile is meaningless without the population it ranks against, and the
//! population that makes the H1 weights valid is *the repository's files at one
//! point in time* — exactly what a published snapshot is. Building the index
//! once and scoring many files from it therefore reproduces the harness's
//! arithmetic, while scoring a file against a population of one never could.
//!
//! # What "absent" means here
//!
//! A fact is recorded only when a producer published a value for it. Two
//! deliberate exceptions mirror the harness (`health::backtest::replay`)
//! exactly, because the weights were derived under them:
//!
//! * A file the git window never touched has `hotspot_score`,
//!   `bug_fix_commits` and `line_churn` of **0** — genuinely zero *for this
//!   window* — while the ratios (`bug_fix_density`, `top_author_share`,
//!   `bus_factor`, `author_count`) stay unknown, having no denominator.
//! * A file the dead-symbol producer examined and found nothing in has
//!   `dead_exported_symbols` of 0, and likewise `unstable_dependencies`.
//!
//! Every other gap — no git snapshot at all, an unparsed language, a file the
//! test-proximity producer did not classify — leaves the fact absent, which the
//! engine reports in `inputs_missing` and pays for by widening the band.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::git_intelligence::GitIntelligenceSnapshot;
use crate::graph::CodeGraph;
use crate::health::complexity_facts::FileComplexityFacts;
use crate::health::dead_symbol_facts::DeadSymbolFactsSnapshot;
use crate::health::graph_facts::{GraphFactProducer, GraphFactsSnapshot};
use crate::health::test_proximity_facts::TestProximitySnapshot;

use super::engine::{score_axis, AxisScore};
use super::facts::{
    FactAvailability, FactKind, FactSourceRange, FactValue, FactWindow, FileFacts, ALL_FACT_KINDS,
    FACT_COUNT,
};
use super::weights::{active_weights, Axis, WeightTable};

/// A raw fact value before it is ranked.
#[derive(Debug, Clone)]
struct RawFact {
    kind: FactKind,
    value: u64,
    availability: FactAvailability,
    source_range: Option<FactSourceRange>,
    window: Option<FactWindow>,
}

/// The sorted populations each fact is ranked against.
///
/// One population per fact, holding the value of every file that had one.
/// Files without a value are excluded from the population *and* from its
/// denominator, so a sparse fact does not rank its own absence.
#[derive(Debug, Clone)]
pub struct FactPopulations {
    sorted: Vec<Vec<u64>>,
}

impl Default for FactPopulations {
    fn default() -> Self {
        Self::new()
    }
}

impl FactPopulations {
    /// An empty population set.
    pub fn new() -> Self {
        Self {
            sorted: vec![Vec::new(); FACT_COUNT],
        }
    }

    fn record(&mut self, kind: FactKind, value: u64) {
        self.sorted[kind.index()].push(value);
    }

    fn finish(&mut self) {
        for population in &mut self.sorted {
            population.sort_unstable();
        }
    }

    /// How many files contributed to a fact's population.
    pub fn size(&self, kind: FactKind) -> usize {
        self.sorted[kind.index()].len()
    }

    /// Rank percentile of a value, in per-mille, oriented so higher means
    /// riskier.
    ///
    /// The convention is the harness's: the fraction of the population the
    /// value *strictly exceeds*, so tied values stay tied and the whole zero
    /// mass of a sparse fact shares percentile 0. For a fact whose risky end is
    /// the low end (`bus_factor`), the complement is used, which keeps ties
    /// tied rather than inverting the rank.
    pub fn percentile_per_mille(&self, kind: FactKind, value: u64) -> u32 {
        let population = &self.sorted[kind.index()];
        let total = population.len() as u128;
        if total == 0 {
            return 0;
        }
        match kind.direction() {
            super::facts::RiskDirection::HigherIsRiskier => {
                let strictly_below = population.partition_point(|other| *other < value) as u128;
                crate::health::arithmetic::round_div(strictly_below * 1000, total) as u32
            }
            super::facts::RiskDirection::LowerIsRiskier => {
                let strictly_above =
                    total - population.partition_point(|other| *other <= value) as u128;
                crate::health::arithmetic::round_div(strictly_above * 1000, total) as u32
            }
        }
    }
}

/// Every fact known about a repository, ready to score any of its files.
#[derive(Debug, Clone)]
pub struct HealthFactIndex {
    files: BTreeMap<String, FileFacts>,
    populations: FactPopulations,
    weights: &'static WeightTable,
    availability: FactAvailability,
}

impl HealthFactIndex {
    /// A builder to join snapshots into an index.
    pub fn builder() -> HealthFactIndexBuilder {
        HealthFactIndexBuilder::default()
    }

    /// An index with no facts at all: every score from it is `unavailable`.
    pub fn empty() -> Self {
        Self {
            files: BTreeMap::new(),
            populations: FactPopulations::new(),
            weights: active_weights(),
            availability: FactAvailability::Unavailable,
        }
    }

    /// An index built from a dependency graph alone.
    ///
    /// This is the graph-facts-only case spec H3.2 requires to remain
    /// scoreable: no git window, no parsed complexity, no test linkage. Scores
    /// from it name every family they lacked in `inputs_missing` and carry a
    /// widened band, so a reader can see that a graph-only ranking is what they
    /// are looking at.
    ///
    /// `index_complete` is the indexer's own completeness state; passing
    /// `false` marks every fact degraded rather than pretending an incomplete
    /// index produced whole facts.
    pub fn from_graph(graph: &CodeGraph, index_complete: bool) -> Self {
        Self::builder()
            .with_graph_facts(GraphFactProducer::default().produce(graph, index_complete))
            .build()
    }

    /// The facts known about one file.
    pub fn facts(&self, path: &str) -> Option<&FileFacts> {
        self.files.get(path)
    }

    /// Score one file on one axis.
    ///
    /// A path the index never saw scores `unavailable` rather than zero: an
    /// unindexed file is unknown, not healthy.
    pub fn score(&self, path: &str, axis: Axis) -> AxisScore {
        match self.files.get(path) {
            Some(facts) => score_axis(facts, axis, self.weights),
            None => AxisScore::unavailable(axis, self.weights),
        }
    }

    /// Files the index holds facts for.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Paths the index holds facts for, in canonical order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The populations facts were ranked against.
    pub fn populations(&self) -> &FactPopulations {
        &self.populations
    }

    /// The weight table scores are computed under.
    pub fn weights(&self) -> &'static WeightTable {
        self.weights
    }

    /// Worst availability of any family the index holds.
    pub fn availability(&self) -> FactAvailability {
        self.availability
    }
}

/// Joins published snapshots into a [`HealthFactIndex`].
///
/// Every input is optional: an index over fewer families is not an error, it is
/// a score with more missing inputs and a wider band.
/// Every input is held behind an [`Arc`] so that a runtime which has already
/// published a generation can hand the same snapshot to many requests without
/// deep-copying it. `build` only ever reads its inputs, so sharing them changes
/// no result; on a repository the size of the committed backtest corpus
/// (`docs/reports/health-backtest/2026-08-14.md`) a per-request deep clone of
/// the file vectors would dominate the read path it is meant to make cheap.
#[derive(Debug, Default)]
pub struct HealthFactIndexBuilder {
    graph: Option<Arc<GraphFactsSnapshot>>,
    git: Option<Arc<GitIntelligenceSnapshot>>,
    complexity: Arc<BTreeMap<String, FileComplexityFacts>>,
    complexity_supplied: bool,
    test_proximity: Option<Arc<TestProximitySnapshot>>,
    dead_symbols: Option<Arc<DeadSymbolFactsSnapshot>>,
}

impl HealthFactIndexBuilder {
    /// Add published graph facts (H2.1).
    pub fn with_graph_facts(self, snapshot: GraphFactsSnapshot) -> Self {
        self.with_shared_graph_facts(Arc::new(snapshot))
    }

    /// Add already-shared published graph facts (H2.1).
    pub fn with_shared_graph_facts(mut self, snapshot: Arc<GraphFactsSnapshot>) -> Self {
        self.graph = Some(snapshot);
        self
    }

    /// Add a published git-intelligence snapshot, including line churn (H2.5).
    pub fn with_git_intelligence(self, snapshot: GitIntelligenceSnapshot) -> Self {
        self.with_shared_git_intelligence(Arc::new(snapshot))
    }

    /// Add an already-shared published git-intelligence snapshot (H2.5).
    pub fn with_shared_git_intelligence(mut self, snapshot: Arc<GitIntelligenceSnapshot>) -> Self {
        self.git = Some(snapshot);
        self
    }

    /// Add published complexity facts (H2.2), keyed by path.
    pub fn with_complexity_facts(self, facts: BTreeMap<String, FileComplexityFacts>) -> Self {
        self.with_shared_complexity_facts(Arc::new(facts))
    }

    /// Add already-shared published complexity facts (H2.2), keyed by path.
    pub fn with_shared_complexity_facts(
        mut self,
        facts: Arc<BTreeMap<String, FileComplexityFacts>>,
    ) -> Self {
        self.complexity = facts;
        self.complexity_supplied = true;
        self
    }

    /// Add published test-proximity facts (H2.4).
    pub fn with_test_proximity_facts(self, snapshot: TestProximitySnapshot) -> Self {
        self.with_shared_test_proximity_facts(Arc::new(snapshot))
    }

    /// Add already-shared published test-proximity facts (H2.4).
    pub fn with_shared_test_proximity_facts(
        mut self,
        snapshot: Arc<TestProximitySnapshot>,
    ) -> Self {
        self.test_proximity = Some(snapshot);
        self
    }

    /// Add published dead-symbol facts (H2.3).
    pub fn with_dead_symbol_facts(self, snapshot: DeadSymbolFactsSnapshot) -> Self {
        self.with_shared_dead_symbol_facts(Arc::new(snapshot))
    }

    /// Add already-shared published dead-symbol facts (H2.3).
    pub fn with_shared_dead_symbol_facts(mut self, snapshot: Arc<DeadSymbolFactsSnapshot>) -> Self {
        self.dead_symbols = Some(snapshot);
        self
    }

    /// Join the snapshots, rank every fact, and build the index.
    pub fn build(self) -> HealthFactIndex {
        let mut raw: BTreeMap<String, Vec<RawFact>> = BTreeMap::new();
        let mut availability = FactAvailability::Available;
        let mut any_family = false;

        if let Some(graph) = &self.graph {
            any_family = true;
            availability = availability.worst(graph.availability().into());
            self.collect_graph_facts(graph, &mut raw);
        }
        let git_availability = self.git.as_ref().map(|git| {
            if git.report.file_history_complete() {
                FactAvailability::Available
            } else {
                FactAvailability::Degraded
            }
        });
        if let (Some(git), Some(git_availability)) = (&self.git, git_availability) {
            any_family = true;
            availability = availability.worst(git_availability);
            self.collect_git_facts(git, git_availability, &mut raw);
        }
        if self.complexity_supplied {
            any_family = true;
            self.collect_complexity_facts(&mut raw, &mut availability);
        }
        if let Some(tests) = &self.test_proximity {
            any_family = true;
            availability = availability.worst(tests.availability().into());
            self.collect_test_proximity_facts(tests, &mut raw);
        }
        if let Some(dead) = &self.dead_symbols {
            any_family = true;
            availability = availability.worst(dead.availability().into());
            self.collect_dead_symbol_facts(dead, &mut raw);
        }

        if !any_family {
            return HealthFactIndex::empty();
        }

        // Both zero-fills run once every family has contributed its paths, so
        // that "the window never touched this file" and "the producer found
        // nothing in this file" are recorded for every file the index knows
        // about, whichever family first revealed it.
        if let (Some(git), Some(git_availability)) = (&self.git, git_availability) {
            self.fill_untouched_git_files(git, git_availability, &mut raw);
        }
        if let Some(dead) = &self.dead_symbols {
            self.fill_files_without_dead_symbols(dead, &mut raw);
        }

        let mut populations = FactPopulations::new();
        for facts in raw.values() {
            for fact in facts {
                populations.record(fact.kind, fact.value);
            }
        }
        populations.finish();

        let mut files: BTreeMap<String, FileFacts> = BTreeMap::new();
        for (path, facts) in raw {
            let mut bundle = FileFacts::new(path.clone());
            for fact in facts {
                let percentile = populations.percentile_per_mille(fact.kind, fact.value);
                let mut value =
                    FactValue::new(fact.kind, fact.value, percentile, fact.availability);
                value.source_range = fact.source_range;
                value.window = fact.window;
                bundle.insert(value);
            }
            files.insert(path, bundle);
        }

        HealthFactIndex {
            files,
            populations,
            weights: active_weights(),
            availability,
        }
    }

    fn collect_graph_facts(
        &self,
        snapshot: &GraphFactsSnapshot,
        raw: &mut BTreeMap<String, Vec<RawFact>>,
    ) {
        let availability: FactAvailability = snapshot.availability().into();

        // Direction violations, counted per originating file, keeping the first
        // violating edge's range as the evidence a reader can open.
        let mut violations: BTreeMap<&str, (u64, FactSourceRange)> = BTreeMap::new();
        for signal in &snapshot.unstable_dependencies {
            let entry = violations
                .entry(signal.from_path.as_str())
                .or_insert_with(|| {
                    (
                        0,
                        FactSourceRange::new(
                            signal.from_path.clone(),
                            signal.source_line,
                            signal.source_end_line,
                        ),
                    )
                });
            entry.0 += 1;
        }

        for file in &snapshot.files {
            let entry = raw.entry(file.path.clone()).or_default();
            entry.push(RawFact {
                kind: FactKind::FanIn,
                value: u64::from(file.fan_in),
                availability,
                source_range: None,
                window: None,
            });
            entry.push(RawFact {
                kind: FactKind::FanOut,
                value: u64::from(file.fan_out),
                availability,
                source_range: None,
                window: None,
            });
            entry.push(RawFact {
                kind: FactKind::SccSize,
                value: u64::from(file.scc_size),
                availability,
                source_range: None,
                window: None,
            });
            entry.push(RawFact {
                kind: FactKind::CycleMember,
                value: u64::from(file.cycle_member),
                availability,
                source_range: None,
                window: None,
            });
            if let Some(instability) = file.instability_per_mille {
                entry.push(RawFact {
                    kind: FactKind::Instability,
                    value: u64::from(instability),
                    availability,
                    source_range: None,
                    window: None,
                });
            }
            let (count, range) = match violations.get(file.path.as_str()) {
                Some((count, range)) => (*count, Some(range.clone())),
                None => (0, None),
            };
            entry.push(RawFact {
                kind: FactKind::UnstableDependencies,
                value: count,
                availability,
                source_range: range,
                window: None,
            });
        }
    }

    fn collect_git_facts(
        &self,
        snapshot: &GitIntelligenceSnapshot,
        availability: FactAvailability,
        raw: &mut BTreeMap<String, Vec<RawFact>>,
    ) {
        let window = FactWindow {
            included_commits: snapshot.report.included_commits,
            head_commit: snapshot.head_commit_id().map(str::to_string),
        };

        for file in &snapshot.files {
            let entry = raw.entry(file.path.clone()).or_default();
            let mut push = |kind: FactKind, value: u64| {
                entry.push(RawFact {
                    kind,
                    value,
                    availability,
                    source_range: None,
                    window: Some(window.clone()),
                });
            };
            push(FactKind::HotspotScore, u64::from(file.hotspot_score));
            push(FactKind::BugFixCommits, u64::from(file.bug_fix_commits));
            push(
                FactKind::BugFixDensity,
                u64::from(file.bug_fix_density_per_mille),
            );
            push(FactKind::LineChurn, file.line_churn);
            push(FactKind::AuthorCount, u64::from(file.author_count));
            if let Some(share) = file.top_author_share_per_mille {
                push(FactKind::TopAuthorShare, u64::from(share));
            }
            if let Some(bus_factor) = file.bus_factor {
                push(FactKind::BusFactor, u64::from(bus_factor));
            }
        }
    }

    /// A file the git window never touched: counts are zero *for this window*,
    /// and the ratios, having no denominator, stay unknown.
    ///
    /// This is the harness's rule (`health::backtest::replay`), under which the
    /// git weights were derived.
    fn fill_untouched_git_files(
        &self,
        snapshot: &GitIntelligenceSnapshot,
        availability: FactAvailability,
        raw: &mut BTreeMap<String, Vec<RawFact>>,
    ) {
        let window = FactWindow {
            included_commits: snapshot.report.included_commits,
            head_commit: snapshot.head_commit_id().map(str::to_string),
        };
        for facts in raw.values_mut() {
            if facts.iter().any(|fact| fact.kind == FactKind::HotspotScore) {
                continue;
            }
            for kind in [
                FactKind::HotspotScore,
                FactKind::BugFixCommits,
                FactKind::LineChurn,
            ] {
                facts.push(RawFact {
                    kind,
                    value: 0,
                    availability,
                    source_range: None,
                    window: Some(window.clone()),
                });
            }
        }
    }

    fn collect_complexity_facts(
        &self,
        raw: &mut BTreeMap<String, Vec<RawFact>>,
        availability: &mut FactAvailability,
    ) {
        for (path, facts) in self.complexity.iter() {
            if facts.unavailable_reason == Some(crate::health::complexity_facts::ComplexityUnavailableReason::NoExecutableControlFlow) {
                continue;
            }
            let file_availability: FactAvailability = facts.availability.into();
            // An unmeasurable file is missing evidence, not a loss of all
            // independently available facts in the workspace.
            *availability = availability.worst(match file_availability {
                FactAvailability::Unavailable => FactAvailability::Degraded,
                other => other,
            });
            let Some(rollup) = &facts.rollup else {
                continue;
            };
            let entry = raw.entry(path.clone()).or_default();

            let mut push = |kind: FactKind, value: u64, range: Option<FactSourceRange>| {
                entry.push(RawFact {
                    kind,
                    value,
                    availability: file_availability,
                    source_range: range,
                    window: None,
                });
            };

            if let Some(value) = rollup.max_cyclomatic_complexity {
                let range = extremum_range(path, facts, |symbol| symbol.cyclomatic_complexity);
                push(FactKind::MaxCyclomaticComplexity, u64::from(value), range);
            }
            if let Some(value) = rollup.p90_cyclomatic_complexity {
                push(FactKind::P90CyclomaticComplexity, u64::from(value), None);
            }
            if let Some(value) = rollup.max_function_length {
                let range = extremum_range(path, facts, |symbol| symbol.function_length);
                push(FactKind::MaxFunctionLength, u64::from(value), range);
            }
            if let Some(value) = rollup.max_nesting_depth {
                let range = extremum_range(path, facts, |symbol| symbol.max_nesting_depth);
                push(FactKind::MaxNestingDepth, u64::from(value), range);
            }
            push(
                FactKind::OverThresholdShare,
                u64::from(rollup.over_threshold_share_per_mille),
                None,
            );
            push(
                FactKind::FunctionCount,
                u64::from(rollup.function_count),
                None,
            );
        }
    }

    fn collect_test_proximity_facts(
        &self,
        snapshot: &TestProximitySnapshot,
        raw: &mut BTreeMap<String, Vec<RawFact>>,
    ) {
        let availability: FactAvailability = snapshot.availability().into();
        for file in &snapshot.files {
            raw.entry(file.path.clone()).or_default().push(RawFact {
                kind: FactKind::UntestedChange,
                value: u64::from(file.untested_change),
                availability,
                source_range: None,
                window: None,
            });
        }
    }

    fn collect_dead_symbol_facts(
        &self,
        snapshot: &DeadSymbolFactsSnapshot,
        raw: &mut BTreeMap<String, Vec<RawFact>>,
    ) {
        let availability: FactAvailability = snapshot.availability().into();
        let mut counts: BTreeMap<&str, (u64, FactSourceRange)> = BTreeMap::new();
        for candidate in snapshot.flagged() {
            let entry = counts.entry(candidate.path.as_str()).or_insert_with(|| {
                (
                    0,
                    FactSourceRange::new(
                        candidate.path.clone(),
                        candidate.source_line,
                        candidate.source_end_line,
                    ),
                )
            });
            entry.0 += 1;
        }

        for (path, (count, range)) in counts {
            raw.entry(path.to_string()).or_default().push(RawFact {
                kind: FactKind::DeadExportedSymbols,
                value: count,
                availability,
                source_range: Some(range),
                window: None,
            });
        }
    }

    /// The dead-symbol producer examined the whole graph, so a file it flagged
    /// nothing in genuinely has no dead exports: a measured zero, not a gap.
    fn fill_files_without_dead_symbols(
        &self,
        snapshot: &DeadSymbolFactsSnapshot,
        raw: &mut BTreeMap<String, Vec<RawFact>>,
    ) {
        let availability: FactAvailability = snapshot.availability().into();
        for facts in raw.values_mut() {
            if facts
                .iter()
                .any(|fact| fact.kind == FactKind::DeadExportedSymbols)
            {
                continue;
            }
            facts.push(RawFact {
                kind: FactKind::DeadExportedSymbols,
                value: 0,
                availability,
                source_range: None,
                window: None,
            });
        }
    }
}

/// The line range of the function achieving a rollup extremum.
///
/// Evidence a reader can open: "longest function 214 lines" is far more useful
/// pointing at the function than at the file.
fn extremum_range(
    path: &str,
    facts: &FileComplexityFacts,
    metric: impl Fn(&crate::health::complexity_facts::SymbolComplexityFacts) -> u32,
) -> Option<FactSourceRange> {
    let best = facts.symbols.iter().max_by(|left, right| {
        metric(left)
            .cmp(&metric(right))
            // Ties resolve to the earliest symbol, which is stable across
            // runs because the producer orders symbols by byte offset.
            .then(right.byte_offset.cmp(&left.byte_offset))
    })?;
    Some(FactSourceRange::new(
        path.to_string(),
        best.line as u32,
        best.end_line as u32,
    ))
}

/// Every fact an axis could draw on, for callers reporting coverage.
pub fn axis_inputs(axis: Axis) -> Vec<FactKind> {
    ALL_FACT_KINDS
        .iter()
        .copied()
        .filter(|kind| active_weights().weight(axis, *kind) > 0)
        .collect()
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;
