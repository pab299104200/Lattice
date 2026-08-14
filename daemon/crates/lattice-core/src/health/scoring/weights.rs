//! The versioned weight table, and where every number in it comes from.
//!
//! # `defect_risk`: measured, and only measured
//!
//! Every `defect_risk` weight below is copied from the `Derived weight` column
//! of `docs/reports/health-backtest/2026-08-14.md`, § "Per-fact discrimination
//! — the weight derivation for H3", scaled from the report's `0.000–1.000` to
//! per-mille. That column is itself
//! `max(0, roc_auc - 0.500) * 2`: how far a fact's univariate ROC-AUC rises
//! above chance, doubled so a perfect discriminator would weigh 1000.
//! [`derive_weight_per_mille`] is that formula, and
//! `weights_tests::the_shipped_defect_risk_weights_are_the_reports_derived_weights`
//! re-derives the whole table from the report's published ROC-AUC values, so
//! the shipped weights cannot drift away from the evidence that set them.
//!
//! The report's § "What H3 may and may not conclude" is binding: a fact
//! measured at or below chance carries no weight, and is not inverted — the
//! harness reports anti-correlation so that a human can decide deliberately,
//! and flipping a sign here would invent a signal no producer claimed.
//!
//! # `maintainability`: judgment, and labelled as judgment
//!
//! The backtest measured one thing: whether a fact ranks files that later
//! receive a fix-shaped commit. That is the `defect_risk` question. It is *not*
//! the maintainability question, and the report contains no evidence about
//! maintainability at all. The `maintainability` weights below are therefore a
//! documented editorial ordering, not a measured one, and nothing rendered from
//! this axis may cite the backtest as its justification. Their reasoning is
//! recorded per fact at [`MAINTAINABILITY_WEIGHTS`].
//!
//! # Scale
//!
//! Weights are per-mille and their absolute magnitude is deliberately not
//! normalized to any total. A score is the weight-weighted *mean* of the
//! percentiles of the facts that were available (see
//! [`super::engine::score_axis`]), so only the ratios between weights affect a
//! score, and the result is in `[0, 1000]` whatever the weights sum to. This is
//! exactly how the harness combined features
//! (`health::backtest::features::score`), which is why the report's calibration
//! table describes the scores this engine produces.

use serde::{Deserialize, Serialize};

use super::facts::{FactKind, ALL_FACT_KINDS, FACT_COUNT};

/// Version of the weight table. Bump on any change to a weight, to a band
/// cutoff, or to the scoring rule; every score echoes it.
pub const HEALTH_WEIGHTS_VERSION: u32 = 1;

/// The weight given to a fact the backtest never scored.
///
/// Set to the smallest non-zero weight the report derived for any measured fact
/// (`author_count`, 0.050), so that a provisional fact can never outrank
/// measured evidence. It is not zero because these facts are genuine evidence a
/// producer publishes and a reader can check; it is not larger because no
/// committed report has measured them.
pub const PROVISIONAL_WEIGHT_PER_MILLE: u32 = 50;

/// A scored axis.
///
/// Two axes, not three: `performance_risk` is rejected by spec design decision
/// 1 because static graph, git, and complexity facts do not measure runtime
/// behaviour.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// How this file ranks on evidence associated with later fix-shaped
    /// commits. Backtested.
    DefectRisk,
    /// How costly this file is to change safely, from its structure and
    /// complexity. Not backtested.
    Maintainability,
}

/// Every axis, in canonical order.
pub const ALL_AXES: [Axis; 2] = [Axis::DefectRisk, Axis::Maintainability];

impl Axis {
    /// Stable identifier for payloads and report text.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DefectRisk => "defect_risk",
            Self::Maintainability => "maintainability",
        }
    }

    /// Read an axis back from its stable identifier.
    pub fn from_code(value: &str) -> Option<Self> {
        ALL_AXES.iter().copied().find(|axis| axis.as_str() == value)
    }

    /// Whether a committed backtest report measured this axis's weights.
    ///
    /// Consumers use this to decide how strongly they may speak: the measured
    /// axis may cite the report, the unmeasured one may not.
    pub fn is_backtested(&self) -> bool {
        matches!(self, Self::DefectRisk)
    }
}

/// The univariate ROC-AUC the report published for each measured fact, in
/// per-mille, from § "Per-fact discrimination — the weight derivation for H3".
///
/// Held here so that the shipped weights can be re-derived from their cited
/// evidence in a test rather than trusted by eye.
pub const BACKTEST_ROC_AUC_PER_MILLE: [(FactKind, u32); 18] = [
    (FactKind::FanIn, 635),
    (FactKind::FanOut, 775),
    (FactKind::SccSize, 531),
    (FactKind::CycleMember, 531),
    (FactKind::Instability, 577),
    (FactKind::HotspotScore, 800),
    (FactKind::BugFixCommits, 766),
    (FactKind::BugFixDensity, 789),
    (FactKind::LineChurn, 762),
    (FactKind::AuthorCount, 525),
    (FactKind::TopAuthorShare, 548),
    (FactKind::BusFactor, 552),
    (FactKind::MaxCyclomaticComplexity, 603),
    (FactKind::P90CyclomaticComplexity, 572),
    (FactKind::MaxFunctionLength, 596),
    (FactKind::MaxNestingDepth, 576),
    (FactKind::OverThresholdShare, 555),
    (FactKind::FunctionCount, 642),
];

/// The report's weight derivation: `max(0, roc_auc - 0.500) * 2` in per-mille.
///
/// A fact at or below chance weighs nothing. Doubling puts a perfect
/// discriminator at 1000, matching the uniform-weight scale the harness
/// compared against.
pub fn derive_weight_per_mille(roc_auc_per_mille: u32) -> u32 {
    roc_auc_per_mille.saturating_sub(500) * 2
}

/// `defect_risk` weights, per-mille, indexed by [`FactKind::index`].
///
/// The eighteen measured entries are the report's `Derived weight` column.
/// `untested_change` carries [`PROVISIONAL_WEIGHT_PER_MILLE`]: the spec lists
/// it as a `defect_risk` input, but the backtest never scored it, so it enters
/// at the floor. `unstable_dependencies` and `dead_exported_symbols` are
/// maintainability evidence and carry no weight here at all.
pub const DEFECT_RISK_WEIGHTS: [u32; FACT_COUNT] = [
    270, // fan_in                     roc 0.635
    550, // fan_out                    roc 0.775
    62,  // scc_size                   roc 0.531
    62,  // cycle_member               roc 0.531
    154, // instability                roc 0.577
    600, // hotspot_score              roc 0.800
    532, // bug_fix_commits            roc 0.766
    578, // bug_fix_density            roc 0.789
    524, // line_churn                 roc 0.762
    50,  // author_count               roc 0.525
    96,  // top_author_share           roc 0.548
    104, // bus_factor                 roc 0.552
    206, // max_cyclomatic_complexity  roc 0.603
    144, // p90_cyclomatic_complexity  roc 0.572
    192, // max_function_length        roc 0.596
    152, // max_nesting_depth          roc 0.576
    110, // over_threshold_share       roc 0.555
    284, // function_count             roc 0.642
    0,   // unstable_dependencies      not a defect-risk input
    PROVISIONAL_WEIGHT_PER_MILLE, // untested_change  provisional, unmeasured
    0,   // dead_exported_symbols      not a defect-risk input
];

/// `maintainability` weights, per-mille, indexed by [`FactKind::index`].
///
/// Not derived from the backtest — see this module's header. The ordering
/// expresses one consistent judgment about what makes a file expensive to
/// change safely, in three tiers:
///
/// 1. **How hard the code is to read and reason about** (heaviest): the
///    complexity extrema, led by `max_cyclomatic_complexity`, because the
///    single worst function in a file is what a change is most likely to have
///    to understand. `over_threshold_share` sits just below it: it says the
///    problem is the whole file rather than one outlier.
/// 2. **How hard the code is to change without breaking something else**:
///    `scc_size` and `fan_out` (a change here reaches, or is reached by, many
///    files), `cycle_member`, `instability`, and `unstable_dependencies` (the
///    classic direction violation — a stable file depending on a volatile one).
/// 3. **Accumulated cruft** (lightest): `dead_exported_symbols`, which raises
///    the cost of reading a file without making any single change harder, and
///    `function_count`, which is size rather than difficulty.
///
/// History facts carry no maintainability weight: how often a file was edited
/// last quarter is a fact about the team's attention, not about how hard the
/// code in front of you is to change. `fan_in` likewise carries none — being
/// depended upon raises the *blast radius* of a mistake, which is the
/// `defect_risk` axis's business, not the cost of making the change.
pub const MAINTAINABILITY_WEIGHTS: [u32; FACT_COUNT] = [
    0,    // fan_in                    blast radius, not change cost
    700,  // fan_out
    800,  // scc_size
    500,  // cycle_member
    600,  // instability
    0,    // hotspot_score             history is not a property of the code
    0,    // bug_fix_commits
    0,    // bug_fix_density
    0,    // line_churn
    0,    // author_count
    0,    // top_author_share
    0,    // bus_factor
    1000, // max_cyclomatic_complexity
    700,  // p90_cyclomatic_complexity
    700,  // max_function_length
    700,  // max_nesting_depth
    800,  // over_threshold_share
    300,  // function_count
    600,  // unstable_dependencies
    0,    // untested_change           a change fact, not a file property
    400,  // dead_exported_symbols
];

/// One version of the weight table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeightTable {
    /// Value of [`HEALTH_WEIGHTS_VERSION`] this table represents.
    pub version: u32,
    /// `defect_risk` weights, indexed by [`FactKind::index`].
    pub defect_risk: [u32; FACT_COUNT],
    /// `maintainability` weights, indexed by [`FactKind::index`].
    pub maintainability: [u32; FACT_COUNT],
}

impl WeightTable {
    /// The weight one fact carries on one axis; zero means "not an input".
    pub fn weight(&self, axis: Axis, kind: FactKind) -> u32 {
        match axis {
            Axis::DefectRisk => self.defect_risk[kind.index()],
            Axis::Maintainability => self.maintainability[kind.index()],
        }
    }

    /// The facts an axis draws on, in canonical order.
    pub fn inputs(&self, axis: Axis) -> Vec<FactKind> {
        ALL_FACT_KINDS
            .iter()
            .copied()
            .filter(|kind| self.weight(axis, *kind) > 0)
            .collect()
    }

    /// Total weight of every input to an axis, available or not.
    ///
    /// The denominator of the score's uncertainty interval: it is what the
    /// score would be divided by if every input were known.
    pub fn total_weight(&self, axis: Axis) -> u64 {
        ALL_FACT_KINDS
            .iter()
            .map(|kind| u64::from(self.weight(axis, *kind)))
            .sum()
    }
}

/// Version 1 of the weight table.
pub static WEIGHTS_V1: WeightTable = WeightTable {
    version: HEALTH_WEIGHTS_VERSION,
    defect_risk: DEFECT_RISK_WEIGHTS,
    maintainability: MAINTAINABILITY_WEIGHTS,
};

/// The weight table scoring must use.
pub fn active_weights() -> &'static WeightTable {
    &WEIGHTS_V1
}

#[cfg(test)]
#[path = "weights_tests.rs"]
mod tests;
