//! The fact vocabulary the score engine consumes, and the per-file bundle a
//! score is computed from.
//!
//! A [`FactKind`] here is always a fact some H2 producer already publishes.
//! The engine invents no signal of its own: if a number is not in this list, no
//! score can depend on it, and every entry can be traced back to the producer
//! that measured it.
//!
//! # Percentiles, not raw values
//!
//! A fan-in of 12 and a line churn of 4_000 are not on the same scale, so each
//! fact reaches the engine as both its raw value (for the evidence bundle a
//! reader sees) and a rank percentile in per-mille within the repository's
//! population of files (for the arithmetic). The percentile convention is
//! exactly the one the H1 backtest used to derive the weights
//! (`health::backtest::features`): **a file scores at the fraction of the
//! population it strictly exceeds**, ties stay tied, and a fact with no value
//! is absent rather than zero. Any other convention would mean the shipped
//! scores were not the scores the report measured.

use serde::{Deserialize, Serialize};

use crate::health::churn_facts;
use crate::health::complexity_facts;
use crate::health::dead_symbol_facts;
use crate::health::graph_facts;
use crate::health::test_proximity_facts;

/// Which fact producer a scored fact comes from.
///
/// The three families the backtest measured are joined here by the two it
/// never scored ([`FactFamily::TestProximity`] and [`FactFamily::DeadSymbol`]),
/// which is why [`FactKind::is_backtested`] exists: a consumer must be able to
/// tell measured evidence from provisional evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactFamily {
    /// `health::graph_facts`, from the published dependency graph.
    Graph,
    /// `git_intelligence` and `health::churn_facts`, from the commit window.
    Git,
    /// `health::complexity_facts`, from the parsed file.
    Complexity,
    /// `health::test_proximity_facts`, from edge-linked tests.
    TestProximity,
    /// `health::dead_symbol_facts`, from exported symbols with no dependents.
    DeadSymbol,
}

impl FactFamily {
    /// Stable identifier for payloads and report text.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Git => "git",
            Self::Complexity => "complexity",
            Self::TestProximity => "test_proximity",
            Self::DeadSymbol => "dead_symbol",
        }
    }
}

/// Which direction of a fact's raw value indicates elevated risk.
///
/// Recorded explicitly, as in the harness, so that normalization can put every
/// fact on one "higher means riskier" scale with no sign convention hidden in a
/// producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskDirection {
    /// Larger raw values indicate more risk.
    HigherIsRiskier,
    /// Smaller raw values indicate more risk.
    LowerIsRiskier,
}

/// One fact the score engine can weigh.
///
/// The first eighteen variants are exactly the features the H1 backtest
/// measured, in the report's own order
/// (`docs/reports/health-backtest/2026-08-14.md`, § "Per-fact discrimination").
/// The last three are facts H2 produces that the backtest never scored; see
/// [`FactKind::is_backtested`].
///
/// The variant order is canonical and load-bearing for deterministic output:
/// new facts append rather than insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
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
    /// Distinct commits touching the file in the mining window.
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
    /// Percentile cyclomatic complexity across the file's functions.
    P90CyclomaticComplexity,
    /// Longest function in the file, in lines.
    MaxFunctionLength,
    /// Deepest structural nesting of any function in the file.
    MaxNestingDepth,
    /// Share of the file's functions breaching any complexity threshold.
    OverThresholdShare,
    /// Number of functions in the file.
    FunctionCount,
    /// Dependency-direction violations originating in this file: edges to a
    /// file more unstable than itself, by more than the producer's threshold.
    UnstableDependencies,
    /// Whether the file has no edge-linked test (0 or 1).
    UntestedChange,
    /// Exported symbols in the file with no indexed dependents.
    DeadExportedSymbols,
}

/// Every fact, in canonical order.
pub const ALL_FACT_KINDS: [FactKind; 21] = [
    FactKind::FanIn,
    FactKind::FanOut,
    FactKind::SccSize,
    FactKind::CycleMember,
    FactKind::Instability,
    FactKind::HotspotScore,
    FactKind::BugFixCommits,
    FactKind::BugFixDensity,
    FactKind::LineChurn,
    FactKind::AuthorCount,
    FactKind::TopAuthorShare,
    FactKind::BusFactor,
    FactKind::MaxCyclomaticComplexity,
    FactKind::P90CyclomaticComplexity,
    FactKind::MaxFunctionLength,
    FactKind::MaxNestingDepth,
    FactKind::OverThresholdShare,
    FactKind::FunctionCount,
    FactKind::UnstableDependencies,
    FactKind::UntestedChange,
    FactKind::DeadExportedSymbols,
];

/// Number of distinct facts.
pub const FACT_COUNT: usize = ALL_FACT_KINDS.len();

impl FactKind {
    /// Position of this fact in [`ALL_FACT_KINDS`].
    pub fn index(&self) -> usize {
        *self as usize
    }

    /// Which producer publishes this fact.
    pub fn family(&self) -> FactFamily {
        match self {
            Self::FanIn
            | Self::FanOut
            | Self::SccSize
            | Self::CycleMember
            | Self::Instability
            | Self::UnstableDependencies => FactFamily::Graph,
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
            Self::UntestedChange => FactFamily::TestProximity,
            Self::DeadExportedSymbols => FactFamily::DeadSymbol,
        }
    }

    /// Which direction of the raw value indicates elevated risk.
    pub fn direction(&self) -> RiskDirection {
        match self {
            // Concentrated ownership is the risk signal, so *few* authors
            // covering the majority of a file's commits is the risky end. This
            // matches the harness, and the weight was derived under it.
            Self::BusFactor => RiskDirection::LowerIsRiskier,
            _ => RiskDirection::HigherIsRiskier,
        }
    }

    /// Whether the H1 backtest measured this fact's discrimination.
    ///
    /// `false` means no committed report has scored the fact against defect
    /// labels: its weight is a documented provisional choice, never a measured
    /// one, and nothing rendered from it may claim measured predictive value.
    pub fn is_backtested(&self) -> bool {
        !matches!(
            self,
            Self::UnstableDependencies | Self::UntestedChange | Self::DeadExportedSymbols
        )
    }

    /// Stable identifier for payloads and report text.
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
            Self::UnstableDependencies => "unstable_dependencies",
            Self::UntestedChange => "untested_change",
            Self::DeadExportedSymbols => "dead_exported_symbols",
        }
    }

    /// Read a fact back from its stable identifier.
    pub fn from_code(value: &str) -> Option<Self> {
        ALL_FACT_KINDS
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }

    /// Render one fact's raw value the way a reader should see it.
    ///
    /// Descriptive only: it states what was measured and never what will
    /// happen. The report's evidence is correlational
    /// (`docs/reports/health-backtest/2026-08-14.md`, § "What H3 may and may
    /// not conclude"), so no phrasing here may promise a future failure.
    pub fn describe_value(&self, value: u64) -> String {
        match self {
            Self::FanIn => format!("fan-in {value}"),
            Self::FanOut => format!("fan-out {value}"),
            Self::SccSize => format!("{value}-file dependency cycle"),
            Self::CycleMember => {
                if value > 0 {
                    "cycle member".to_string()
                } else {
                    "not in a dependency cycle".to_string()
                }
            }
            Self::Instability => format!("instability {value}/1000"),
            Self::HotspotScore => format!("{value} commits in window"),
            Self::BugFixCommits => format!("{value} bug-fix commits in window"),
            Self::BugFixDensity => format!("bug-fix density {value}/1000"),
            Self::LineChurn => format!("{value} lines churned in window"),
            Self::AuthorCount => format!("{value} authors in window"),
            Self::TopAuthorShare => format!("top author {value}/1000 of commits"),
            Self::BusFactor => format!("bus factor {value}"),
            Self::MaxCyclomaticComplexity => format!("max cyclomatic complexity {value}"),
            Self::P90CyclomaticComplexity => format!("p90 cyclomatic complexity {value}"),
            Self::MaxFunctionLength => format!("longest function {value} lines"),
            Self::MaxNestingDepth => format!("max nesting depth {value}"),
            Self::OverThresholdShare => {
                format!("{value}/1000 of functions over a complexity threshold")
            }
            Self::FunctionCount => format!("{value} functions"),
            Self::UnstableDependencies => {
                format!("{value} dependency-direction violations")
            }
            Self::UntestedChange => {
                if value > 0 {
                    "no edge-linked tests".to_string()
                } else {
                    "has edge-linked tests".to_string()
                }
            }
            Self::DeadExportedSymbols => {
                format!("{value} exported symbols with no indexed dependents")
            }
        }
    }
}

/// How much of a fact family the engine actually had.
///
/// Each H2 producer owns its own availability enum, which stays the source of
/// truth for that producer's persistence and tests. This is the single enum the
/// scoring boundary reasons in; the `From` implementations below are the only
/// place the four vocabularies meet, so unifying here costs the producers
/// nothing.
///
/// Ordered so that `max` composes: the availability of a score is the worst
/// availability of any input it used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactAvailability {
    /// Complete facts.
    Available,
    /// Facts exist but are known to be partial (incomplete index, truncated
    /// window, overflowed limits).
    Degraded,
    /// No facts at all.
    Unavailable,
}

impl FactAvailability {
    /// Stable identifier for payloads and report text.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
        }
    }

    /// The worse of two availabilities.
    pub fn worst(self, other: Self) -> Self {
        self.max(other)
    }
}

impl From<graph_facts::FactAvailability> for FactAvailability {
    fn from(value: graph_facts::FactAvailability) -> Self {
        match value {
            graph_facts::FactAvailability::Available => Self::Available,
            graph_facts::FactAvailability::Degraded => Self::Degraded,
            graph_facts::FactAvailability::Unavailable => Self::Unavailable,
        }
    }
}

impl From<complexity_facts::FactAvailability> for FactAvailability {
    fn from(value: complexity_facts::FactAvailability) -> Self {
        match value {
            complexity_facts::FactAvailability::Available => Self::Available,
            complexity_facts::FactAvailability::Degraded => Self::Degraded,
            complexity_facts::FactAvailability::Unavailable => Self::Unavailable,
        }
    }
}

impl From<test_proximity_facts::FactAvailability> for FactAvailability {
    fn from(value: test_proximity_facts::FactAvailability) -> Self {
        match value {
            test_proximity_facts::FactAvailability::Available => Self::Available,
            test_proximity_facts::FactAvailability::Degraded => Self::Degraded,
            test_proximity_facts::FactAvailability::Unavailable => Self::Unavailable,
        }
    }
}

impl From<dead_symbol_facts::FactAvailability> for FactAvailability {
    fn from(value: dead_symbol_facts::FactAvailability) -> Self {
        match value {
            dead_symbol_facts::FactAvailability::Available => Self::Available,
            dead_symbol_facts::FactAvailability::Degraded => Self::Degraded,
            dead_symbol_facts::FactAvailability::Unavailable => Self::Unavailable,
        }
    }
}

impl From<churn_facts::FactAvailability> for FactAvailability {
    fn from(value: churn_facts::FactAvailability) -> Self {
        // The churn producer has no `Unavailable`: a window always produces a
        // signal for a path it saw, so the only question is whether that window
        // was complete.
        match value {
            churn_facts::FactAvailability::Available => Self::Available,
            churn_facts::FactAvailability::Degraded => Self::Degraded,
        }
    }
}

/// Where in the tree a fact was observed.
///
/// Carried so that a rendered bundle can point at the code that produced the
/// evidence, per spec design decision 3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactSourceRange {
    /// Workspace-relative path the range belongs to.
    pub path: String,
    /// First line of the range, 1-based.
    pub start_line: u32,
    /// Last line of the range, 1-based and never before `start_line`.
    pub end_line: u32,
}

impl FactSourceRange {
    /// A range over one file.
    pub fn new(path: impl Into<String>, start_line: u32, end_line: u32) -> Self {
        Self {
            path: path.into(),
            start_line,
            end_line: end_line.max(start_line),
        }
    }
}

/// Which history window a git-derived fact was measured over.
///
/// A history fact without its window is unreadable: "8 bug-fix commits" means
/// nothing until a reader knows it is 8 out of a bounded, sampled window ending
/// at a named commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactWindow {
    /// Commits actually included in the mining window.
    pub included_commits: u32,
    /// Newest commit the window covers, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
}

/// One fact about one file, ready to be weighed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactValue {
    /// Which fact this is.
    pub kind: FactKind,
    /// The producer's raw value, as published.
    pub value: u64,
    /// Rank percentile of `value` within the repository population, in
    /// per-mille, oriented so that higher always means riskier.
    pub percentile_per_mille: u32,
    /// How complete the producing family's facts were.
    pub availability: FactAvailability,
    /// Where the fact was observed, when it has a location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_range: Option<FactSourceRange>,
    /// The history window, for git-derived facts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<FactWindow>,
}

impl FactValue {
    /// A fact with no location and no window.
    pub fn new(
        kind: FactKind,
        value: u64,
        percentile_per_mille: u32,
        availability: FactAvailability,
    ) -> Self {
        Self {
            kind,
            value,
            percentile_per_mille: percentile_per_mille.min(1000),
            availability,
            source_range: None,
            window: None,
        }
    }

    /// Attach the range the fact was observed at.
    pub fn with_source_range(mut self, range: FactSourceRange) -> Self {
        self.source_range = Some(range);
        self
    }

    /// Attach the history window the fact was measured over.
    pub fn with_window(mut self, window: FactWindow) -> Self {
        self.window = Some(window);
        self
    }
}

/// Every fact known about one file, in canonical fact order.
///
/// A fact the producers could not supply is simply absent: "unknown is never
/// zero" (spec design decision 4) is enforced by construction, because there is
/// no way to record a value without having one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileFacts {
    /// Workspace-relative path the facts describe.
    pub path: String,
    /// Facts that had a value, ordered by [`FactKind::index`].
    pub values: Vec<FactValue>,
}

impl FileFacts {
    /// An empty bundle for a path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            values: Vec::new(),
        }
    }

    /// Record a fact, replacing any previous value for the same kind and
    /// keeping the canonical ordering.
    pub fn insert(&mut self, value: FactValue) {
        match self
            .values
            .binary_search_by_key(&value.kind.index(), |existing| existing.kind.index())
        {
            Ok(position) => self.values[position] = value,
            Err(position) => self.values.insert(position, value),
        }
    }

    /// Record a fact, returning the bundle, for fixture construction.
    pub fn with(mut self, value: FactValue) -> Self {
        self.insert(value);
        self
    }

    /// Read one fact.
    pub fn get(&self, kind: FactKind) -> Option<&FactValue> {
        self.values.iter().find(|value| value.kind == kind)
    }

    /// Whether a fact has a value.
    pub fn has(&self, kind: FactKind) -> bool {
        self.get(kind).is_some()
    }

    /// Remove one fact, returning it when it was present.
    ///
    /// Used by callers modelling an input that went missing, and by the
    /// engine's own monotonicity tests.
    pub fn remove(&mut self, kind: FactKind) -> Option<FactValue> {
        let position = self.values.iter().position(|value| value.kind == kind)?;
        Some(self.values.remove(position))
    }

    /// The worst availability of any recorded fact, or
    /// [`FactAvailability::Unavailable`] when nothing was recorded.
    pub fn availability(&self) -> FactAvailability {
        self.values
            .iter()
            .map(|value| value.availability)
            .max()
            .unwrap_or(FactAvailability::Unavailable)
    }
}

#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;
