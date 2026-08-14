use super::*;

use crate::health::scoring::facts::{FactAvailability, FactKind, FactValue, FileFacts};
use crate::health::scoring::test_support::weights;
use crate::health::scoring::weights::Axis;

/// A bundle holding exactly the facts named, all at the same rank.
fn facts_at(percentile: u32, kinds: &[FactKind]) -> FileFacts {
    let mut facts = FileFacts::new("src/lib.rs");
    for kind in kinds {
        facts.insert(FactValue::new(
            *kind,
            1,
            percentile,
            FactAvailability::Available,
        ));
    }
    facts
}

#[test]
fn every_fact_at_the_same_rank_scores_at_that_rank() {
    let facts = facts_at(640, &weights().inputs(Axis::DefectRisk));

    let score = score_axis(&facts, Axis::DefectRisk, weights());

    assert_eq!(score.score_per_mille, 640);
    assert_eq!(score.score_floor_per_mille, 640);
    assert_eq!(score.score_ceiling_per_mille, 640);
    assert_eq!(score.band, Band::High);
}

#[test]
fn the_floor_and_ceiling_close_as_inputs_arrive() {
    let all = weights().inputs(Axis::DefectRisk);
    let full = score_axis(&facts_at(500, &all), Axis::DefectRisk, weights());
    let half = score_axis(
        &facts_at(500, &all[..all.len() / 2]),
        Axis::DefectRisk,
        weights(),
    );

    let full_width = full.score_ceiling_per_mille - full.score_floor_per_mille;
    let half_width = half.score_ceiling_per_mille - half.score_floor_per_mille;

    assert_eq!(full_width, 0);
    assert!(half_width > 0);
}

#[test]
fn a_fact_with_no_weight_on_an_axis_is_neither_scored_nor_reported_missing() {
    let mut facts = facts_at(900, &weights().inputs(Axis::DefectRisk));
    // Weightless on defect risk: an input to maintainability only.
    facts.insert(FactValue::new(
        FactKind::UnstableDependencies,
        4,
        1000,
        FactAvailability::Available,
    ));

    let score = score_axis(&facts, Axis::DefectRisk, weights());

    assert!(score
        .facts
        .iter()
        .all(|fact| fact.kind != FactKind::UnstableDependencies));
    assert!(!score
        .inputs_missing
        .contains(&FactKind::UnstableDependencies));
    assert_eq!(score.score_per_mille, 900);
}

#[test]
fn a_bundle_holding_only_weightless_facts_scores_unavailable() {
    let facts = facts_at(900, &[FactKind::UnstableDependencies]);

    let score = score_axis(&facts, Axis::DefectRisk, weights());

    assert_eq!(score.availability, FactAvailability::Unavailable);
    assert_eq!(score.band_label(), "unknown");
}

#[test]
fn contributions_are_ordered_by_weight_when_ranks_are_equal() {
    let facts = facts_at(500, &weights().inputs(Axis::DefectRisk));

    let score = score_axis(&facts, Axis::DefectRisk, weights());
    let weights_in_order: Vec<u32> = score
        .facts
        .iter()
        .map(|fact| fact.weight_per_mille)
        .collect();
    let mut descending = weights_in_order.clone();
    descending.sort_unstable_by(|left, right| right.cmp(left));

    assert_eq!(weights_in_order, descending);
}

#[test]
fn top_facts_never_reads_past_the_end() {
    let score = score_axis(
        &facts_at(500, &[FactKind::FanIn, FactKind::FanOut]),
        Axis::DefectRisk,
        weights(),
    );

    assert_eq!(score.top_facts(50).len(), 2);
    assert_eq!(score.top_facts(1).len(), 1);
    assert_eq!(score.top_facts(0).len(), 0);
}

#[test]
fn an_axis_score_round_trips_through_json() {
    let score = score_axis(
        &facts_at(700, &weights().inputs(Axis::Maintainability)),
        Axis::Maintainability,
        weights(),
    );

    let encoded = serde_json::to_string(&score).expect("serializes");
    let decoded: AxisScore = serde_json::from_str(&encoded).expect("deserializes");

    assert_eq!(decoded, score);
}
