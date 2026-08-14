//! Tests for feature normalization, family sets, and weighted scoring.

use super::*;

fn vector(path: &str, pairs: &[(FeatureKind, u64)]) -> FileFeatureVector {
    let mut vector = FileFeatureVector::new(path.to_owned());
    for (feature, value) in pairs {
        vector.set(*feature, *value);
    }
    vector
}

#[test]
fn feature_indices_match_their_position_in_the_canonical_order() {
    for (position, feature) in ALL_FEATURES.iter().enumerate() {
        assert_eq!(feature.index(), position, "{}", feature.as_str());
    }
    assert_eq!(FEATURE_COUNT, 18);
}

#[test]
fn every_feature_has_a_distinct_identifier() {
    let mut identifiers: Vec<&str> = ALL_FEATURES.iter().map(FeatureKind::as_str).collect();
    let before = identifiers.len();
    identifiers.sort_unstable();
    identifiers.dedup();
    assert_eq!(identifiers.len(), before);
}

#[test]
fn family_sets_are_strictly_nested() {
    let graph_only = FamilySet::GraphOnly.features();
    let graph_git = FamilySet::GraphGit.features();
    let all = FamilySet::All.features();
    assert_eq!(all.len(), FEATURE_COUNT);
    assert!(graph_only.iter().all(|feature| graph_git.contains(feature)));
    assert!(graph_git.iter().all(|feature| all.contains(feature)));
    assert!(graph_only.len() < graph_git.len());
    assert!(graph_git.len() < all.len());
    // Nesting is what makes the comparison meaningful: any improvement from
    // graph-only to graph+git is attributable to the added family alone.
    assert!(graph_only
        .iter()
        .all(|feature| feature.family() == FactFamily::Graph));
}

#[test]
fn a_file_scores_at_the_fraction_of_files_it_strictly_exceeds() {
    let normalized = normalize(&[
        vector("a", &[(FeatureKind::FanIn, 0)]),
        vector("b", &[(FeatureKind::FanIn, 1)]),
        vector("c", &[(FeatureKind::FanIn, 2)]),
        vector("d", &[(FeatureKind::FanIn, 3)]),
    ]);
    assert_eq!(normalized[0].get(FeatureKind::FanIn), Some(0));
    assert_eq!(normalized[1].get(FeatureKind::FanIn), Some(250));
    assert_eq!(normalized[2].get(FeatureKind::FanIn), Some(500));
    assert_eq!(normalized[3].get(FeatureKind::FanIn), Some(750));
}

#[test]
fn the_whole_zero_mass_of_a_sparse_feature_shares_percentile_zero() {
    // The realistic shape of bug-fix counts: most files have none.
    let mut vectors: Vec<FileFeatureVector> = (0..9)
        .map(|index| vector(&format!("clean{index}"), &[(FeatureKind::BugFixCommits, 0)]))
        .collect();
    vectors.push(vector("fixed", &[(FeatureKind::BugFixCommits, 5)]));

    let normalized = normalize(&vectors);
    for slot in &normalized[..9] {
        // "No evidence" must not outrank "no evidence".
        assert_eq!(slot.get(FeatureKind::BugFixCommits), Some(0));
    }
    assert_eq!(normalized[9].get(FeatureKind::BugFixCommits), Some(900));
}

#[test]
fn ties_normalize_identically_regardless_of_input_position() {
    let normalized = normalize(&[
        vector("a", &[(FeatureKind::FanIn, 7)]),
        vector("b", &[(FeatureKind::FanIn, 1)]),
        vector("c", &[(FeatureKind::FanIn, 7)]),
        vector("d", &[(FeatureKind::FanIn, 7)]),
    ]);
    assert_eq!(normalized[0].get(FeatureKind::FanIn), Some(250));
    assert_eq!(normalized[2].get(FeatureKind::FanIn), Some(250));
    assert_eq!(normalized[3].get(FeatureKind::FanIn), Some(250));
    assert_eq!(normalized[1].get(FeatureKind::FanIn), Some(0));
}

#[test]
fn a_lower_is_riskier_feature_is_inverted_so_high_always_means_risk() {
    // Bus factor: one author covering the majority is the risky end.
    let normalized = normalize(&[
        vector("sole_owner", &[(FeatureKind::BusFactor, 1)]),
        vector("shared", &[(FeatureKind::BusFactor, 2)]),
        vector("well_spread", &[(FeatureKind::BusFactor, 3)]),
        vector("very_well_spread", &[(FeatureKind::BusFactor, 4)]),
    ]);
    assert_eq!(FeatureKind::BusFactor.direction(), RiskDirection::LowerIsRiskier);
    assert_eq!(normalized[0].get(FeatureKind::BusFactor), Some(750));
    assert_eq!(normalized[1].get(FeatureKind::BusFactor), Some(500));
    assert_eq!(normalized[2].get(FeatureKind::BusFactor), Some(250));
    assert_eq!(normalized[3].get(FeatureKind::BusFactor), Some(0));
}

#[test]
fn an_inverted_feature_keeps_its_ties_tied() {
    let normalized = normalize(&[
        vector("a", &[(FeatureKind::BusFactor, 1)]),
        vector("b", &[(FeatureKind::BusFactor, 1)]),
        vector("c", &[(FeatureKind::BusFactor, 9)]),
        vector("d", &[(FeatureKind::BusFactor, 9)]),
    ]);
    assert_eq!(normalized[0].get(FeatureKind::BusFactor), Some(500));
    assert_eq!(normalized[1].get(FeatureKind::BusFactor), Some(500));
    assert_eq!(normalized[2].get(FeatureKind::BusFactor), Some(0));
    assert_eq!(normalized[3].get(FeatureKind::BusFactor), Some(0));
}

#[test]
fn an_unknown_value_stays_unknown_and_leaves_the_population_to_the_others() {
    let normalized = normalize(&[
        vector("a", &[(FeatureKind::FanIn, 1)]),
        FileFeatureVector::new("no_facts".to_owned()),
        vector("c", &[(FeatureKind::FanIn, 9)]),
    ]);
    assert_eq!(normalized[1].get(FeatureKind::FanIn), None);
    // The population is the two files that have a value, so the lower of them
    // scores 0 and the higher 500 — the unknown file does not pad the ranks.
    assert_eq!(normalized[0].get(FeatureKind::FanIn), Some(0));
    assert_eq!(normalized[2].get(FeatureKind::FanIn), Some(500));
}

#[test]
fn a_feature_nobody_has_normalizes_to_nothing() {
    let normalized = normalize(&[vector("a", &[(FeatureKind::FanIn, 1)])]);
    assert_eq!(normalized[0].get(FeatureKind::LineChurn), None);
}

#[test]
fn scoring_averages_the_available_features_of_the_set() {
    let normalized = NormalizedFeatureVector {
        path: "a".to_owned(),
        percentiles: {
            let mut percentiles = [None; FEATURE_COUNT];
            percentiles[FeatureKind::FanIn.index()] = Some(800);
            percentiles[FeatureKind::FanOut.index()] = Some(400);
            // A git feature that graph-only must ignore.
            percentiles[FeatureKind::LineChurn.index()] = Some(1000);
            percentiles
        },
    };
    let weights = FeatureWeights::uniform();
    assert_eq!(
        score(&normalized, FamilySet::GraphOnly, &weights),
        Ok(600),
        "graph-only must average 800 and 400 without seeing line churn"
    );
    assert_eq!(
        score(&normalized, FamilySet::GraphGit, &weights),
        Ok(733),
        "graph+git averages 800, 400 and 1000"
    );
}

#[test]
fn a_missing_input_is_omitted_from_the_average_rather_than_counted_as_zero() {
    let mut percentiles = [None; FEATURE_COUNT];
    percentiles[FeatureKind::FanIn.index()] = Some(900);
    let vector = NormalizedFeatureVector {
        path: "a".to_owned(),
        percentiles,
    };
    // Four of the five graph features are unknown. Counting them as zero would
    // give 180; omitting them gives the one thing actually known.
    assert_eq!(
        score(&vector, FamilySet::GraphOnly, &FeatureWeights::uniform()),
        Ok(900)
    );
}

#[test]
fn a_file_with_no_features_in_the_set_reports_unavailable() {
    let vector = NormalizedFeatureVector {
        path: "a".to_owned(),
        percentiles: [None; FEATURE_COUNT],
    };
    assert_eq!(
        score(&vector, FamilySet::All, &FeatureWeights::uniform()),
        Err(ScoreUnavailable::NoAvailableFeatures)
    );
}

#[test]
fn a_file_whose_only_features_carry_zero_weight_reports_unavailable() {
    let mut percentiles = [None; FEATURE_COUNT];
    percentiles[FeatureKind::FanIn.index()] = Some(900);
    let vector = NormalizedFeatureVector {
        path: "a".to_owned(),
        percentiles,
    };
    let weights = FeatureWeights {
        weights: [0; FEATURE_COUNT],
    };
    assert_eq!(
        score(&vector, FamilySet::GraphOnly, &weights),
        Err(ScoreUnavailable::NoWeightedFeatures)
    );
}

#[test]
fn weights_derive_from_measured_discrimination_above_chance() {
    let mut roc = [None; FEATURE_COUNT];
    roc[FeatureKind::FanIn.index()] = Some(1000);
    roc[FeatureKind::FanOut.index()] = Some(750);
    roc[FeatureKind::SccSize.index()] = Some(500);
    // Anti-correlated: measured worse than chance.
    roc[FeatureKind::CycleMember.index()] = Some(200);
    let weights = FeatureWeights::from_univariate_roc(&roc);

    assert_eq!(weights.get(FeatureKind::FanIn), 1000);
    assert_eq!(weights.get(FeatureKind::FanOut), 500);
    // Exactly chance carries no information and so carries no weight.
    assert_eq!(weights.get(FeatureKind::SccSize), 0);
    // Below chance is dropped, never silently inverted into a signal the fact
    // producer did not claim.
    assert_eq!(weights.get(FeatureKind::CycleMember), 0);
    // An unmeasured feature weighs nothing rather than defaulting to a value.
    assert_eq!(weights.get(FeatureKind::LineChurn), 0);
}

#[test]
fn weighted_scoring_favours_the_features_that_measured_well() {
    let mut percentiles = [None; FEATURE_COUNT];
    percentiles[FeatureKind::FanIn.index()] = Some(1000);
    percentiles[FeatureKind::FanOut.index()] = Some(0);
    let vector = NormalizedFeatureVector {
        path: "a".to_owned(),
        percentiles,
    };

    assert_eq!(
        score(&vector, FamilySet::GraphOnly, &FeatureWeights::uniform()),
        Ok(500)
    );

    let mut roc = [None; FEATURE_COUNT];
    roc[FeatureKind::FanIn.index()] = Some(900);
    roc[FeatureKind::FanOut.index()] = Some(550);
    let weighted = FeatureWeights::from_univariate_roc(&roc);
    // Weights 800 and 100: (800*1000 + 100*0) / 900.
    assert_eq!(score(&vector, FamilySet::GraphOnly, &weighted), Ok(889));
}

#[test]
fn a_weight_set_reports_whether_a_family_has_any_signal_left() {
    let mut roc = [None; FEATURE_COUNT];
    roc[FeatureKind::LineChurn.index()] = Some(900);
    let weights = FeatureWeights::from_univariate_roc(&roc);
    assert!(!weights.has_signal(FamilySet::GraphOnly));
    assert!(weights.has_signal(FamilySet::GraphGit));
    assert!(weights.has_signal(FamilySet::All));
}

#[test]
fn normalization_is_independent_of_input_order() {
    let forward = vec![
        vector("a", &[(FeatureKind::FanIn, 3), (FeatureKind::LineChurn, 10)]),
        vector("b", &[(FeatureKind::FanIn, 1), (FeatureKind::LineChurn, 90)]),
        vector("c", &[(FeatureKind::FanIn, 2), (FeatureKind::LineChurn, 50)]),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();

    let forward_normalized = normalize(&forward);
    let reversed_normalized = normalize(&reversed);
    for slot in &forward_normalized {
        let counterpart = reversed_normalized
            .iter()
            .find(|other| other.path == slot.path)
            .expect("same paths");
        assert_eq!(slot.percentiles, counterpart.percentiles);
    }
}
