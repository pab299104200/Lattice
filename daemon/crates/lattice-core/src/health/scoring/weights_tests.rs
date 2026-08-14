use super::*;

use crate::health::scoring::facts::{FactFamily, ALL_FACT_KINDS};

/// The report's `Derived weight` column, § "Per-fact discrimination — the
/// weight derivation for H3", transcribed to per-mille.
const REPORTED_DERIVED_WEIGHTS: [(FactKind, u32); 18] = [
    (FactKind::FanIn, 270),
    (FactKind::FanOut, 550),
    (FactKind::SccSize, 62),
    (FactKind::CycleMember, 62),
    (FactKind::Instability, 154),
    (FactKind::HotspotScore, 600),
    (FactKind::BugFixCommits, 532),
    (FactKind::BugFixDensity, 578),
    (FactKind::LineChurn, 524),
    (FactKind::AuthorCount, 50),
    (FactKind::TopAuthorShare, 96),
    (FactKind::BusFactor, 104),
    (FactKind::MaxCyclomaticComplexity, 206),
    (FactKind::P90CyclomaticComplexity, 144),
    (FactKind::MaxFunctionLength, 192),
    (FactKind::MaxNestingDepth, 152),
    (FactKind::OverThresholdShare, 110),
    (FactKind::FunctionCount, 284),
];

#[test]
fn the_derivation_formula_reproduces_the_reports_own_weight_column() {
    for ((roc_kind, roc), (weight_kind, weight)) in BACKTEST_ROC_AUC_PER_MILLE
        .iter()
        .zip(REPORTED_DERIVED_WEIGHTS.iter())
    {
        assert_eq!(roc_kind, weight_kind, "the two tables list facts in order");
        assert_eq!(
            derive_weight_per_mille(*roc),
            *weight,
            "{}: max(0, {roc} - 500) * 2 should be {weight}",
            roc_kind.as_str()
        );
    }
}

#[test]
fn the_shipped_defect_risk_weights_are_the_reports_derived_weights() {
    for (kind, roc) in BACKTEST_ROC_AUC_PER_MILLE {
        assert_eq!(
            WEIGHTS_V1.weight(Axis::DefectRisk, kind),
            derive_weight_per_mille(roc),
            "{} ships a weight its cited ROC-AUC does not derive",
            kind.as_str()
        );
    }
}

#[test]
fn the_harness_derives_the_same_weights_the_engine_ships() {
    // The calibration loop, closed in code: the shipped table is compared
    // against the harness's own derivation function
    // (`health::backtest::features::FeatureWeights::from_univariate_roc`) run
    // over the report's published ROC-AUC values. If either side's arithmetic
    // changes, the two stop agreeing here rather than silently in production.
    use crate::health::backtest::features::{FeatureWeights, ALL_FEATURES, FEATURE_COUNT};

    let mut roc: [Option<u32>; FEATURE_COUNT] = [None; FEATURE_COUNT];
    for (kind, value) in BACKTEST_ROC_AUC_PER_MILLE {
        let feature = ALL_FEATURES
            .iter()
            .find(|feature| feature.as_str() == kind.as_str())
            .unwrap_or_else(|| panic!("{} has no harness feature", kind.as_str()));
        roc[feature.index()] = Some(value);
    }
    let harness = FeatureWeights::from_univariate_roc(&roc);

    for (kind, _) in BACKTEST_ROC_AUC_PER_MILLE {
        let feature = ALL_FEATURES
            .iter()
            .find(|feature| feature.as_str() == kind.as_str())
            .expect("feature exists");
        assert_eq!(
            WEIGHTS_V1.weight(Axis::DefectRisk, kind),
            harness.get(*feature),
            "{} disagrees with the harness's own derivation",
            kind.as_str()
        );
    }
}

#[test]
fn every_backtested_fact_appears_in_the_cited_roc_table() {
    let cited: Vec<FactKind> = BACKTEST_ROC_AUC_PER_MILLE
        .iter()
        .map(|(kind, _)| *kind)
        .collect();
    for kind in ALL_FACT_KINDS {
        assert_eq!(
            kind.is_backtested(),
            cited.contains(&kind),
            "{} claims a backtest status the report does not support",
            kind.as_str()
        );
    }
}

#[test]
fn an_unmeasured_fact_never_outweighs_a_measured_one() {
    let smallest_measured = BACKTEST_ROC_AUC_PER_MILLE
        .iter()
        .map(|(_, roc)| derive_weight_per_mille(*roc))
        .filter(|weight| *weight > 0)
        .min()
        .expect("the report measured at least one fact above chance");

    assert_eq!(
        PROVISIONAL_WEIGHT_PER_MILLE, smallest_measured,
        "the provisional weight is the smallest non-zero measured weight"
    );

    for kind in ALL_FACT_KINDS {
        if kind.is_backtested() {
            continue;
        }
        assert!(
            WEIGHTS_V1.weight(Axis::DefectRisk, kind) <= PROVISIONAL_WEIGHT_PER_MILLE,
            "{} is unmeasured and must not exceed the provisional weight",
            kind.as_str()
        );
    }
}

#[test]
fn a_fact_measured_at_or_below_chance_carries_no_weight() {
    // The report's § "What H3 may and may not conclude" is binding: nothing at
    // or below chance may be weighted, and nothing may be inverted.
    assert_eq!(derive_weight_per_mille(500), 0);
    assert_eq!(derive_weight_per_mille(499), 0);
    assert_eq!(derive_weight_per_mille(300), 0);
    assert_eq!(derive_weight_per_mille(1000), 1000);
}

#[test]
fn defect_risk_draws_on_every_family_the_report_measured() {
    let families: Vec<FactFamily> = WEIGHTS_V1
        .inputs(Axis::DefectRisk)
        .iter()
        .map(|kind| kind.family())
        .collect();

    for family in [FactFamily::Graph, FactFamily::Git, FactFamily::Complexity] {
        assert!(families.contains(&family), "{family:?} is a measured family");
    }
}

#[test]
fn maintainability_draws_on_the_inputs_the_spec_names_and_no_history() {
    let inputs = WEIGHTS_V1.inputs(Axis::Maintainability);

    for expected in [
        FactKind::MaxCyclomaticComplexity,
        FactKind::MaxFunctionLength,
        FactKind::Instability,
        FactKind::UnstableDependencies,
        FactKind::SccSize,
        FactKind::FanOut,
        FactKind::DeadExportedSymbols,
    ] {
        assert!(
            inputs.contains(&expected),
            "{} is a maintainability input in the spec",
            expected.as_str()
        );
    }

    for kind in inputs {
        assert_ne!(
            kind.family(),
            FactFamily::Git,
            "{} is history, which is not a property of the code as it stands",
            kind.as_str()
        );
    }
}

#[test]
fn only_the_defect_risk_axis_claims_a_backtest() {
    assert!(Axis::DefectRisk.is_backtested());
    assert!(
        !Axis::Maintainability.is_backtested(),
        "the report measured defect prediction, never maintainability"
    );
}

#[test]
fn every_axis_has_a_non_zero_total_weight() {
    for axis in ALL_AXES {
        assert!(WEIGHTS_V1.total_weight(axis) > 0);
        assert!(!WEIGHTS_V1.inputs(axis).is_empty());
    }
}

#[test]
fn inputs_are_reported_in_canonical_fact_order() {
    for axis in ALL_AXES {
        let inputs = WEIGHTS_V1.inputs(axis);
        let mut sorted = inputs.clone();
        sorted.sort_by_key(FactKind::index);
        assert_eq!(inputs, sorted);
    }
}

#[test]
fn a_fact_may_feed_both_axes_with_different_weights() {
    assert_eq!(WEIGHTS_V1.weight(Axis::DefectRisk, FactKind::FanOut), 550);
    assert_eq!(
        WEIGHTS_V1.weight(Axis::Maintainability, FactKind::FanOut),
        700
    );
}

#[test]
fn axis_codes_round_trip() {
    for axis in ALL_AXES {
        assert_eq!(Axis::from_code(axis.as_str()), Some(axis));
    }
    assert_eq!(Axis::from_code("performance_risk"), None);
}
