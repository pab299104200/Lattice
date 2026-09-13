//! Golden tests: exact per-mille output for fixture fact sets.
//!
//! Modelled on `retrieval_v1/golden_tests.rs`. Every number below was computed
//! by hand from the weight table and the fixtures' stated percentiles, so a
//! failure here means the engine's arithmetic changed, not that a fixture
//! drifted. Success criterion 4 of the spec — identical inputs produce
//! byte-identical bundles — is what these pin.

use super::bands::Band;
use super::engine::score_axis;
use super::facts::{FactAvailability, FactKind};
use super::test_support::{graph_only_file, hot_cyclic_file, quiet_leaf_file, weights};
use super::weights::{Axis, HEALTH_WEIGHTS_VERSION};

use crate::health::config::HEALTH_CONFIG_VERSION;

#[test]
fn a_central_churning_complex_file_scores_exactly_894_on_defect_risk() {
    // Σ(weight × percentile) = 4_220_360 over Σ(weight) = 4_720.
    let score = score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights());

    assert_eq!(score.score_per_mille, 894);
    assert_eq!(score.band, Band::Critical);
    assert_eq!(score.band_range.label(), "critical");
    assert_eq!(score.score_floor_per_mille, 894);
    assert_eq!(score.score_ceiling_per_mille, 894);
    assert!(score.inputs_missing.is_empty());
    assert_eq!(score.availability, FactAvailability::Available);
    assert_eq!(score.weights_version, HEALTH_WEIGHTS_VERSION);
    assert_eq!(score.config_version, HEALTH_CONFIG_VERSION);
}

#[test]
fn the_same_file_scores_exactly_885_on_maintainability() {
    let score = score_axis(&hot_cyclic_file(), Axis::Maintainability, weights());

    assert_eq!(score.score_per_mille, 885);
    assert_eq!(score.band, Band::Critical);
    assert!(score.inputs_missing.is_empty());
}

#[test]
fn a_quiet_leaf_file_scores_exactly_127_and_86() {
    let defect = score_axis(&quiet_leaf_file(), Axis::DefectRisk, weights());
    let maintainability = score_axis(&quiet_leaf_file(), Axis::Maintainability, weights());

    assert_eq!(defect.score_per_mille, 127);
    assert_eq!(defect.band, Band::Low);
    assert_eq!(maintainability.score_per_mille, 86);
    assert_eq!(maintainability.band, Band::Low);
}

#[test]
fn graph_facts_alone_still_score_and_say_what_was_missing() {
    // Σ(weight × percentile) = 973_960 over an available weight of 1_098, but
    // over a total axis weight of 4_720: the point estimate is 887 and the
    // honest interval around it is 206 to 974.
    let score = score_axis(&graph_only_file(), Axis::DefectRisk, weights());

    assert_eq!(score.score_per_mille, 887);
    assert_eq!(score.score_floor_per_mille, 206);
    assert_eq!(score.score_ceiling_per_mille, 974);
    assert_eq!(score.band_range.floor, Band::Low);
    assert_eq!(score.band_range.ceiling, Band::Critical);
    assert_eq!(score.band_range.label(), "low-critical");
    assert_eq!(score.inputs_missing.len(), 14);
    assert_eq!(score.availability, FactAvailability::Degraded);
}

#[test]
fn the_heaviest_evidence_is_reported_first() {
    let score = score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights());
    let order: Vec<FactKind> = score.facts.iter().map(|fact| fact.kind).collect();

    assert_eq!(
        &order[..5],
        &[
            FactKind::HotspotScore,
            FactKind::BugFixCommits,
            FactKind::BugFixDensity,
            FactKind::LineChurn,
            FactKind::FanOut,
        ]
    );
}

#[test]
fn every_contribution_reports_its_weight_rank_and_share() {
    let score = score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights());
    let hotspot = score
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::HotspotScore)
        .expect("hotspot contributes");

    assert_eq!(hotspot.value, 88);
    assert_eq!(hotspot.percentile_per_mille, 990);
    assert_eq!(hotspot.weight_per_mille, 600);
    // 600 × 990 / 4_720
    assert_eq!(hotspot.contribution_per_mille, 126);
    assert!(hotspot.backtested);
}

#[test]
fn contributions_sum_to_the_score_up_to_integer_rounding() {
    for axis in [Axis::DefectRisk, Axis::Maintainability] {
        for facts in [hot_cyclic_file(), quiet_leaf_file(), graph_only_file()] {
            let score = score_axis(&facts, axis, weights());
            let summed: u32 = score
                .facts
                .iter()
                .map(|fact| fact.contribution_per_mille)
                .sum();
            let delta = summed.abs_diff(u32::from(score.score_per_mille));
            assert!(
                delta <= score.facts.len() as u32,
                "{axis:?} contributions {summed} vs score {}",
                score.score_per_mille
            );
        }
    }
}

#[test]
fn an_unbacktested_fact_is_marked_as_such_in_the_bundle() {
    let score = score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights());
    let untested = score
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::UntestedChange)
        .expect("untested_change contributes to defect risk");

    assert!(!untested.backtested);
    assert_eq!(
        untested.weight_per_mille,
        super::PROVISIONAL_WEIGHT_PER_MILLE
    );
}

#[test]
fn evidence_carries_the_source_range_and_window_it_came_from() {
    let score = score_axis(&hot_cyclic_file(), Axis::Maintainability, weights());
    let longest = score
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::MaxFunctionLength)
        .expect("function length contributes to maintainability");
    let range = longest.source_range.as_ref().expect("range is carried");

    assert_eq!(range.path, "daemon/src/orchestrator.rs");
    assert_eq!(range.start_line, 118);
    assert_eq!(range.end_line, 332);

    let defect = score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights());
    let churn = defect
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::LineChurn)
        .expect("line churn contributes to defect risk");
    let window = churn.window.as_ref().expect("window is carried");

    assert_eq!(window.included_commits, 500);
    assert_eq!(window.head_commit.as_deref(), Some("2537234d3a"));
}

#[test]
fn a_summary_reads_as_evidence_rather_than_prediction() {
    let score = score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights());
    let summary = score.summary(4);

    assert_eq!(
        summary,
        "defect risk critical (894/1000): 88 commits in window (p990), \
         8 bug-fix commits in window (p960), bug-fix density 91/1000 (p880), \
         4200 lines churned in window (p970)"
    );
    for forbidden in ["will fail", "predict", "likely to break", "expected defect"] {
        assert!(!summary.contains(forbidden), "summary claims prediction");
    }
}

#[test]
fn a_graph_only_summary_names_the_inputs_it_lacked() {
    let score = score_axis(&graph_only_file(), Axis::DefectRisk, weights());
    let summary = score.summary(2);

    assert!(summary.starts_with("defect risk low-critical (887/1000): fan-out 31 (p900)"));
    assert!(summary.contains("scored without hotspot_score"));
    assert!(summary.contains("untested_change"));
}

#[test]
fn identical_facts_serialize_byte_identically() {
    let first = serde_json::to_string(&score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights()))
        .expect("serializes");
    let second =
        serde_json::to_string(&score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights()))
            .expect("serializes");

    assert_eq!(first, second);
    assert!(first.contains("\"axis\":\"defect_risk\""));
    assert!(first.contains("\"band\":\"critical\""));
}
