//! H3.2: a score over incomplete inputs says so, and widens rather than
//! pretending.

use super::bands::Band;
use super::engine::score_axis;
use super::facts::{FactAvailability, FactKind, FileFacts};
use super::test_support::{
    degraded_graph_only_file, graph_only_file, hot_cyclic_file, quiet_leaf_file, weights,
};
use super::weights::{Axis, ALL_AXES};

#[test]
fn complete_inputs_produce_an_exact_band() {
    let score = score_axis(&hot_cyclic_file(), Axis::DefectRisk, weights());

    assert!(score.is_exact());
    assert_eq!(score.band_range.floor, score.band_range.ceiling);
    assert_eq!(score.score_floor_per_mille, score.score_ceiling_per_mille);
    assert_eq!(score.availability, FactAvailability::Available);
    assert_eq!(score.band_label(), "critical");
}

#[test]
fn a_file_outside_the_git_window_is_still_scored_and_labelled() {
    let mut facts = hot_cyclic_file();
    for kind in [
        FactKind::HotspotScore,
        FactKind::BugFixCommits,
        FactKind::BugFixDensity,
        FactKind::LineChurn,
        FactKind::AuthorCount,
        FactKind::TopAuthorShare,
        FactKind::BusFactor,
    ] {
        facts.remove(kind);
    }

    let score = score_axis(&facts, Axis::DefectRisk, weights());

    assert_eq!(score.availability, FactAvailability::Degraded);
    assert!(!score.is_exact(), "a missing family must widen the band");
    assert!(score.inputs_missing.contains(&FactKind::HotspotScore));
    assert!(score.inputs_missing.contains(&FactKind::BusFactor));
    assert!(!score.facts.is_empty(), "what was known is still scored");
}

#[test]
fn an_unparsed_language_still_scores_on_the_facts_it_has() {
    let mut facts = hot_cyclic_file();
    for kind in [
        FactKind::MaxCyclomaticComplexity,
        FactKind::P90CyclomaticComplexity,
        FactKind::MaxFunctionLength,
        FactKind::MaxNestingDepth,
        FactKind::OverThresholdShare,
        FactKind::FunctionCount,
    ] {
        facts.remove(kind);
    }

    let score = score_axis(&facts, Axis::Maintainability, weights());

    assert_eq!(score.availability, FactAvailability::Degraded);
    assert_eq!(score.inputs_missing.len(), 6);
    assert!(score.score_floor_per_mille < score.score_ceiling_per_mille);
}

#[test]
fn a_graph_only_score_widens_across_bands_and_names_every_gap() {
    let score = score_axis(&graph_only_file(), Axis::DefectRisk, weights());

    assert_eq!(score.band_range.floor, Band::Low);
    assert_eq!(score.band_range.ceiling, Band::Critical);
    assert_eq!(score.band_label(), "low-critical");
    let missing: Vec<&str> = score
        .inputs_missing
        .iter()
        .map(|kind| kind.as_str())
        .collect();
    assert_eq!(
        missing,
        vec![
            "hotspot_score",
            "bug_fix_commits",
            "bug_fix_density",
            "line_churn",
            "author_count",
            "top_author_share",
            "bus_factor",
            "max_cyclomatic_complexity",
            "p90_cyclomatic_complexity",
            "max_function_length",
            "max_nesting_depth",
            "over_threshold_share",
            "function_count",
            "untested_change",
        ]
    );
}

#[test]
fn a_degraded_producer_degrades_the_score_it_feeds() {
    let score = score_axis(&degraded_graph_only_file(), Axis::DefectRisk, weights());

    assert_eq!(score.availability, FactAvailability::Degraded);
    for fact in &score.facts {
        assert_eq!(fact.availability, FactAvailability::Degraded);
    }
}

#[test]
fn degraded_facts_degrade_a_score_even_when_nothing_is_missing() {
    let mut facts = hot_cyclic_file();
    let existing = facts.get(FactKind::FanIn).expect("fan-in present").clone();
    facts.insert(super::facts::FactValue::new(
        FactKind::FanIn,
        existing.value,
        existing.percentile_per_mille,
        FactAvailability::Degraded,
    ));

    let score = score_axis(&facts, Axis::DefectRisk, weights());

    assert!(score.inputs_missing.is_empty());
    assert_eq!(score.availability, FactAvailability::Degraded);
    assert!(
        score.is_exact(),
        "partial facts still pin a band; only absent facts widen it"
    );
}

#[test]
fn no_inputs_at_all_is_unknown_rather_than_low() {
    for axis in ALL_AXES {
        let score = score_axis(&FileFacts::new("vendor/generated.min.js"), axis, weights());

        assert_eq!(score.availability, FactAvailability::Unavailable);
        assert_eq!(score.band_label(), "unknown");
        assert_eq!(score.score_floor_per_mille, 0);
        assert_eq!(score.score_ceiling_per_mille, 1000);
        assert_eq!(score.band_range.floor, Band::Low);
        assert_eq!(score.band_range.ceiling, Band::Critical);
        assert!(score.facts.is_empty());
        assert_eq!(score.inputs_missing, weights().inputs(axis));
        assert!(score.summary(3).contains("unknown"));
    }
}

#[test]
fn a_quiet_file_with_complete_facts_is_low_rather_than_unknown() {
    let score = score_axis(&quiet_leaf_file(), Axis::DefectRisk, weights());

    assert_eq!(score.availability, FactAvailability::Available);
    assert_eq!(score.band, Band::Low);
    assert!(score.is_exact());
}
