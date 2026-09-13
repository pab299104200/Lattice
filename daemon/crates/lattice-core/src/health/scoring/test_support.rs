//! Fixture fact bundles shared by the scoring tests.
//!
//! Percentiles are stated directly rather than derived from a population, so
//! that a golden expectation is an arithmetic fact about the engine and not a
//! second copy of the ranking code. The population path is covered separately
//! by `index_tests`.

use super::facts::{FactKind, FactSourceRange, FactValue, FactWindow, FileFacts};
use super::weights::WeightTable;

use crate::health::scoring::facts::FactAvailability;

/// A fact with a stated raw value and rank.
pub fn fact(kind: FactKind, value: u64, percentile: u32) -> FactValue {
    FactValue::new(kind, value, percentile, FactAvailability::Available)
}

/// A fact whose family was known to be incomplete.
pub fn degraded_fact(kind: FactKind, value: u64, percentile: u32) -> FactValue {
    FactValue::new(kind, value, percentile, FactAvailability::Degraded)
}

/// The window the git fixtures were mined over.
pub fn window() -> FactWindow {
    FactWindow {
        included_commits: 500,
        head_commit: Some("2537234d3a".to_string()),
    }
}

/// A file that is central, cyclic, churning, and complex: every `defect_risk`
/// input present and most of them high.
pub fn hot_cyclic_file() -> FileFacts {
    FileFacts::new("daemon/src/orchestrator.rs")
        .with(fact(FactKind::FanIn, 42, 950))
        .with(fact(FactKind::FanOut, 31, 900))
        .with(fact(FactKind::SccSize, 12, 980))
        .with(fact(FactKind::CycleMember, 1, 870))
        .with(fact(FactKind::Instability, 640, 700))
        .with(fact(FactKind::HotspotScore, 88, 990).with_window(window()))
        .with(fact(FactKind::BugFixCommits, 8, 960).with_window(window()))
        .with(fact(FactKind::BugFixDensity, 91, 880).with_window(window()))
        .with(fact(FactKind::LineChurn, 4200, 970).with_window(window()))
        .with(fact(FactKind::AuthorCount, 6, 820).with_window(window()))
        .with(fact(FactKind::TopAuthorShare, 610, 400).with_window(window()))
        .with(fact(FactKind::BusFactor, 2, 300).with_window(window()))
        .with(
            fact(FactKind::MaxCyclomaticComplexity, 34, 930)
                .with_source_range(FactSourceRange::new("daemon/src/orchestrator.rs", 118, 332)),
        )
        .with(fact(FactKind::P90CyclomaticComplexity, 12, 810))
        .with(
            fact(FactKind::MaxFunctionLength, 214, 940).with_source_range(FactSourceRange::new(
                "daemon/src/orchestrator.rs",
                118,
                332,
            )),
        )
        .with(fact(FactKind::MaxNestingDepth, 7, 900))
        .with(fact(FactKind::OverThresholdShare, 320, 860))
        .with(fact(FactKind::FunctionCount, 41, 910))
        .with(
            fact(FactKind::UnstableDependencies, 3, 890).with_source_range(FactSourceRange::new(
                "daemon/src/orchestrator.rs",
                12,
                12,
            )),
        )
        .with(fact(FactKind::UntestedChange, 1, 700))
        .with(fact(FactKind::DeadExportedSymbols, 2, 880))
}

/// A leaf file nothing depends on, rarely touched and simple: every input
/// present and all of them low.
pub fn quiet_leaf_file() -> FileFacts {
    FileFacts::new("daemon/src/util/clock.rs")
        .with(fact(FactKind::FanIn, 1, 120))
        .with(fact(FactKind::FanOut, 0, 0))
        .with(fact(FactKind::SccSize, 1, 0))
        .with(fact(FactKind::CycleMember, 0, 0))
        .with(fact(FactKind::Instability, 0, 40))
        .with(fact(FactKind::HotspotScore, 1, 150).with_window(window()))
        .with(fact(FactKind::BugFixCommits, 0, 0).with_window(window()))
        .with(fact(FactKind::BugFixDensity, 0, 0).with_window(window()))
        .with(fact(FactKind::LineChurn, 34, 210).with_window(window()))
        .with(fact(FactKind::AuthorCount, 1, 100).with_window(window()))
        .with(fact(FactKind::TopAuthorShare, 1000, 990).with_window(window()))
        .with(fact(FactKind::BusFactor, 1, 600).with_window(window()))
        .with(fact(FactKind::MaxCyclomaticComplexity, 3, 190))
        .with(fact(FactKind::P90CyclomaticComplexity, 2, 150))
        .with(fact(FactKind::MaxFunctionLength, 14, 220))
        .with(fact(FactKind::MaxNestingDepth, 2, 180))
        .with(fact(FactKind::OverThresholdShare, 0, 0))
        .with(fact(FactKind::FunctionCount, 4, 240))
        .with(fact(FactKind::UnstableDependencies, 0, 0))
        .with(fact(FactKind::UntestedChange, 0, 0))
        .with(fact(FactKind::DeadExportedSymbols, 0, 0))
}

/// The same central file seen with graph facts alone: no git window, no parsed
/// complexity, no test linkage. Spec H3.2's "still gets a graph-facts-only
/// score, labelled as such".
pub fn graph_only_file() -> FileFacts {
    FileFacts::new("daemon/src/orchestrator.rs")
        .with(fact(FactKind::FanIn, 42, 950))
        .with(fact(FactKind::FanOut, 31, 900))
        .with(fact(FactKind::SccSize, 12, 980))
        .with(fact(FactKind::CycleMember, 1, 870))
        .with(fact(FactKind::Instability, 640, 700))
        .with(fact(FactKind::UnstableDependencies, 3, 890))
}

/// A file whose facts arrived from an index that knew it was incomplete.
pub fn degraded_graph_only_file() -> FileFacts {
    FileFacts::new("daemon/src/orchestrator.rs")
        .with(degraded_fact(FactKind::FanIn, 42, 950))
        .with(degraded_fact(FactKind::FanOut, 31, 900))
        .with(degraded_fact(FactKind::SccSize, 12, 980))
        .with(degraded_fact(FactKind::CycleMember, 1, 870))
        .with(degraded_fact(FactKind::Instability, 640, 700))
        .with(degraded_fact(FactKind::UnstableDependencies, 3, 890))
}

/// The weight table under test.
pub fn weights() -> &'static WeightTable {
    super::weights::active_weights()
}
