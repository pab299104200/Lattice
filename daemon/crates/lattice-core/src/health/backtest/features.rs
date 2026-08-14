//! Per-file feature vectors, rank normalization, and the fact families the
//! backtest compares.
//!
//! # What a feature is
//!
//! A feature is one number the H2 producers already publish about a file, read
//! verbatim. The harness invents no signals of its own: if it measured
//! something the fact producers do not persist, its conclusions could not be
//! carried into H3.
//!
//! # Normalization
//!
//! Raw fact values are incomparable — a fan-in of 12 and a line churn of 4_000
//! are not on the same scale, and neither is comparable across repositories of
//! different size and age. Every feature is therefore converted, within a
//! single cut point, to a rank percentile in per-mille: **a file scores at the
//! fraction of files it strictly exceeds.** Consequences worth stating because
//! they are deliberate:
//!
//! * The entire zero mass of a sparse feature shares percentile 0. Most files
//!   have no bug-fix commits, and "no evidence" must not rank above "no
//!   evidence"; a mid-rank convention would hand every untouched file a
//!   middling score it did not earn.
//! * Ties are preserved exactly, so ordering within a tie can never
//!   manufacture a ranking (the same discipline [`super::metrics`] applies).
//!
//! # Unknown is never zero
//!
//! A feature with no value for a file (outside the git window, unparsed
//! language, no dependency edges at all) is *excluded from that file's score
//! and from its denominator*, not substituted with zero. A file with only
//! graph facts is scored on graph facts, and the report says how many inputs
//! each family actually had. This is spec design decision 4 applied to the
//! harness itself.

use serde::{Deserialize, Serialize};

/// Which fact producer a feature comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactFamily {
    /// `health::graph_facts`, from the dependency graph at the cut point.
    Graph,
    /// `git_intelligence`, from the commit window before the cut point.
    Git,
    /// `health::complexity_facts`, from the file contents at the cut point.
    Complexity,
}

impl FactFamily {
    /// Stable identifier for report text and JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Git => "git",
            Self::Complexity => "complexity",
        }
    }
}

/// Which direction of a feature indicates elevated risk.
///
/// Recorded explicitly rather than assumed so that normalization can put every
/// feature on a single "higher means riskier" scale without any silent sign
/// convention hidden in the producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskDirection {
    /// Larger raw values indicate more risk.
    HigherIsRiskier,
    /// Smaller raw values indicate more risk (bus factor, for instance).
    LowerIsRiskier,
}

/// One measured per-file feature.
///
/// The variant order is the canonical report order and is load-bearing for
/// deterministic output; new features append rather than insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureKind {
    /// Files depending on this one.
    FanIn,
    /// Files this one depends on.
    FanOut,
    /// Size of the strongly connected component this file belongs to.
    SccSize,
    /// Whether the file participates in a dependency cycle (0 or 1).
    CycleMember,
    /// Ce/(Ca+Ce) in per-mille.
    Instability,
    /// Distinct commits touching the file in the pre-cut-point window.
    HotspotScore,
    /// Fix-shaped commits touching the file in that window.
    BugFixCommits,
    /// Fix-shaped share of the file's commits, per-mille.
    BugFixDensity,
    /// Lines added plus deleted in that window.
    LineChurn,
    /// Distinct known authors touching the file.
    AuthorCount,
    /// Share of the file's commits from its single largest author, per-mille.
    TopAuthorShare,
    /// Minimum authors covering a majority of the file's commits.
    BusFactor,
    /// Highest cyclomatic complexity of any function in the file.
    MaxCyclomaticComplexity,
    /// p90 cyclomatic complexity across the file's functions.
    P90CyclomaticComplexity,
    /// Longest function in the file, in lines.
    MaxFunctionLength,
    /// Deepest structural nesting of any function in the file.
    MaxNestingDepth,
    /// Share of the file's functions breaching any complexity threshold,
    /// per-mille.
    OverThresholdShare,
    /// Number of functions in the file.
    FunctionCount,
}

/// Every feature, in canonical report order.
pub const ALL_FEATURES: [FeatureKind; 18] = [
    FeatureKind::FanIn,
    FeatureKind::FanOut,
    FeatureKind::SccSize,
    FeatureKind::CycleMember,
    FeatureKind::Instability,
    FeatureKind::HotspotScore,
    FeatureKind::BugFixCommits,
    FeatureKind::BugFixDensity,
    FeatureKind::LineChurn,
    FeatureKind::AuthorCount,
    FeatureKind::TopAuthorShare,
    FeatureKind::BusFactor,
    FeatureKind::MaxCyclomaticComplexity,
    FeatureKind::P90CyclomaticComplexity,
    FeatureKind::MaxFunctionLength,
    FeatureKind::MaxNestingDepth,
    FeatureKind::OverThresholdShare,
    FeatureKind::FunctionCount,
];

/// Number of features in a vector.
pub const FEATURE_COUNT: usize = ALL_FEATURES.len();

impl FeatureKind {
    /// Position of this feature in [`ALL_FEATURES`].
    pub fn index(&self) -> usize {
        *self as usize
    }

    /// Which producer publishes this feature.
    pub fn family(&self) -> FactFamily {
        match self {
            Self::FanIn
            | Self::FanOut
            | Self::SccSize
            | Self::CycleMember
            | Self::Instability => FactFamily::Graph,
            Self::HotspotScore
            | Self::BugFixCommits
            | Self::BugFixDensity
            | Self::LineChurn
            | Self::AuthorCount
            | Self::TopAuthorShare
            | Self::BusFactor => FactFamily::Git,
            Self::MaxCyclomaticComplexity
            | Self::P90CyclomaticComplexity
            | Self::MaxFunctionLength
            | Self::MaxNestingDepth
            | Self::OverThresholdShare
            | Self::FunctionCount => FactFamily::Complexity,
        }
    }

    /// Which direction of the raw value indicates elevated risk.
    pub fn direction(&self) -> RiskDirection {
        match self {
            // Concentrated ownership is the risk signal, so *few* authors
            // covering the majority of a file's commits is the risky end.
            Self::BusFactor => RiskDirection::LowerIsRiskier,
            _ => RiskDirection::HigherIsRiskier,
        }
    }

    /// Stable identifier for report text and JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FanIn => "fan_in",
            Self::FanOut => "fan_out",
            Self::SccSize => "scc_size",
            Self::CycleMember => "cycle_member",
            Self::Instability => "instability",
            Self::HotspotScore => "hotspot_score",
            Self::BugFixCommits => "bug_fix_commits",
            Self::BugFixDensity => "bug_fix_density",
            Self::LineChurn => "line_churn",
            Self::AuthorCount => "author_count",
            Self::TopAuthorShare => "top_author_share",
            Self::BusFactor => "bus_factor",
            Self::MaxCyclomaticComplexity => "max_cyclomatic_complexity",
            Self::P90CyclomaticComplexity => "p90_cyclomatic_complexity",
            Self::MaxFunctionLength => "max_function_length",
            Self::MaxNestingDepth => "max_nesting_depth",
            Self::OverThresholdShare => "over_threshold_share",
            Self::FunctionCount => "function_count",
        }
    }
}

/// The fact families a scoring configuration draws on.
///
/// These are the three comparisons the spec requires: whether git history adds
/// anything to the graph, and whether complexity adds anything to both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilySet {
    /// Graph facts alone.
    GraphOnly,
    /// Graph plus git history.
    GraphGit,
    /// Graph plus git history plus complexity.
    All,
}

/// Every family set, in canonical report order.
pub const ALL_FAMILY_SETS: [FamilySet; 3] =
    [FamilySet::GraphOnly, FamilySet::GraphGit, FamilySet::All];

impl FamilySet {
    /// Stable identifier for report text and JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GraphOnly => "graph_only",
            Self::GraphGit => "graph_git",
            Self::All => "all",
        }
    }

    /// Human-readable name for report headings.
    pub fn label(&self) -> &'static str {
        match self {
            Self::GraphOnly => "graph-only",
            Self::GraphGit => "graph+git",
            Self::All => "graph+git+complexity",
        }
    }

    /// Whether this set draws on a family.
    pub fn includes(&self, family: FactFamily) -> bool {
        match self {
            Self::GraphOnly => family == FactFamily::Graph,
            Self::GraphGit => matches!(family, FactFamily::Graph | FactFamily::Git),
            Self::All => true,
        }
    }

    /// The features this set draws on, in canonical order.
    pub fn features(&self) -> Vec<FeatureKind> {
        ALL_FEATURES
            .iter()
            .copied()
            .filter(|feature| self.includes(feature.family()))
            .collect()
    }
}

/// Raw feature values for one file at one cut point.
///
/// `None` means the producing fact was unavailable for this file, which is
/// carried through scoring as an absent input rather than a zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFeatureVector {
    /// Canonical repository-relative path.
    pub path: String,
    /// Raw values, indexed by [`FeatureKind::index`].
    pub values: [Option<u64>; FEATURE_COUNT],
}

impl FileFeatureVector {
    /// An all-unknown vector for a path.
    pub fn new(path: String) -> Self {
        Self {
            path,
            values: [None; FEATURE_COUNT],
        }
    }

    /// Record a raw value.
    pub fn set(&mut self, feature: FeatureKind, value: u64) {
        self.values[feature.index()] = Some(value);
    }

    /// Record an optional raw value, leaving it unknown when absent.
    pub fn set_optional(&mut self, feature: FeatureKind, value: Option<u64>) {
        if let Some(value) = value {
            self.set(feature, value);
        }
    }

    /// Read a raw value.
    pub fn get(&self, feature: FeatureKind) -> Option<u64> {
        self.values[feature.index()]
    }
}

/// Normalized feature values for one file, in per-mille, oriented so that
/// higher always means riskier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedFeatureVector {
    /// Canonical repository-relative path.
    pub path: String,
    /// Percentiles in per-mille, indexed by [`FeatureKind::index`].
    pub percentiles: [Option<u32>; FEATURE_COUNT],
}

impl NormalizedFeatureVector {
    /// Read a normalized value.
    pub fn get(&self, feature: FeatureKind) -> Option<u32> {
        self.percentiles[feature.index()]
    }
}

/// Round `numerator / denominator` to the nearest integer, halves up.
fn round_div(numerator: u128, denominator: u128) -> u128 {
    if denominator == 0 {
        return 0;
    }
    (numerator * 2 + denominator) / (denominator * 2)
}

/// Convert raw feature values to rank percentiles within one cut point.
///
/// The population for a feature is the files that have a value for it; files
/// without one keep `None`. See the module docs for why the convention is
/// "fraction of the population strictly exceeded" rather than a mid-rank.
pub fn normalize(vectors: &[FileFeatureVector]) -> Vec<NormalizedFeatureVector> {
    let mut normalized: Vec<NormalizedFeatureVector> = vectors
        .iter()
        .map(|vector| NormalizedFeatureVector {
            path: vector.path.clone(),
            percentiles: [None; FEATURE_COUNT],
        })
        .collect();

    for feature in ALL_FEATURES {
        let index = feature.index();
        let mut population: Vec<u64> = vectors
            .iter()
            .filter_map(|vector| vector.values[index])
            .collect();
        if population.is_empty() {
            continue;
        }
        population.sort_unstable();
        let total = population.len() as u128;

        for (slot, vector) in normalized.iter_mut().zip(vectors.iter()) {
            let Some(value) = vector.values[index] else {
                continue;
            };
            // Count of population values strictly less than this one.
            let strictly_below = population.partition_point(|other| *other < value) as u128;
            let percentile = round_div(strictly_below * 1000, total) as u32;
            slot.percentiles[index] = Some(match feature.direction() {
                RiskDirection::HigherIsRiskier => percentile,
                // Invert so that the risky end is always the high end. Using
                // the complement of the strictly-below share keeps ties tied.
                RiskDirection::LowerIsRiskier => {
                    let strictly_above =
                        total - population.partition_point(|other| *other <= value) as u128;
                    round_div(strictly_above * 1000, total) as u32
                }
            });
        }
    }

    normalized
}

/// A weight per feature, in per-mille, for combining normalized values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureWeights {
    /// Weight per feature, indexed by [`FeatureKind::index`].
    pub weights: [u32; FEATURE_COUNT],
}

impl FeatureWeights {
    /// Every feature weighted identically.
    ///
    /// This is the harness's *unfitted* baseline: it asks whether a family's
    /// facts carry signal at all, without any parameter chosen by looking at
    /// the labels. Every comparison in the report is available in this form so
    /// that a reader can see the result that could not have been overfitted.
    pub fn uniform() -> Self {
        Self {
            weights: [1000; FEATURE_COUNT],
        }
    }

    /// Weight per feature from measured univariate discrimination.
    ///
    /// A feature's weight is how far its ROC-AUC rises above chance:
    /// `max(0, roc_auc_per_mille - 500) * 2`, so a perfectly discriminating
    /// feature weighs 1000 and a feature at or below chance weighs nothing.
    /// Doubling makes the scale match [`FeatureWeights::uniform`].
    ///
    /// A feature *below* chance is dropped rather than inverted: the harness
    /// reports the anti-correlation so H3 can decide deliberately, but silently
    /// flipping a sign would invent a signal the fact producer never claimed.
    pub fn from_univariate_roc(roc_per_mille: &[Option<u32>; FEATURE_COUNT]) -> Self {
        let mut weights = [0u32; FEATURE_COUNT];
        for (slot, roc) in weights.iter_mut().zip(roc_per_mille.iter()) {
            *slot = match roc {
                Some(value) if *value > 500 => (value - 500) * 2,
                _ => 0,
            };
        }
        Self { weights }
    }

    /// Read one feature's weight.
    pub fn get(&self, feature: FeatureKind) -> u32 {
        self.weights[feature.index()]
    }

    /// Whether any feature in a set carries a non-zero weight.
    pub fn has_signal(&self, set: FamilySet) -> bool {
        set.features()
            .iter()
            .any(|feature| self.get(*feature) > 0)
    }
}

/// Why a file could not be scored under a family set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreUnavailable {
    /// No feature in the set had a value for this file.
    NoAvailableFeatures,
    /// Every feature with a value carried zero weight.
    NoWeightedFeatures,
}

/// Combine normalized features into one per-mille score for a family set.
///
/// The result is the weighted mean of the *available* features only, so a file
/// missing an input is scored on what is known about it rather than penalized
/// or credited for the gap.
pub fn score(
    vector: &NormalizedFeatureVector,
    set: FamilySet,
    weights: &FeatureWeights,
) -> Result<u32, ScoreUnavailable> {
    let mut weighted_sum: u128 = 0;
    let mut weight_total: u128 = 0;
    let mut any_available = false;

    for feature in set.features() {
        let Some(percentile) = vector.get(feature) else {
            continue;
        };
        any_available = true;
        let weight = u128::from(weights.get(feature));
        if weight == 0 {
            continue;
        }
        weighted_sum += weight * u128::from(percentile);
        weight_total += weight;
    }

    if !any_available {
        return Err(ScoreUnavailable::NoAvailableFeatures);
    }
    if weight_total == 0 {
        return Err(ScoreUnavailable::NoWeightedFeatures);
    }
    Ok(round_div(weighted_sum, weight_total) as u32)
}

#[cfg(test)]
#[path = "features_tests.rs"]
mod tests;
