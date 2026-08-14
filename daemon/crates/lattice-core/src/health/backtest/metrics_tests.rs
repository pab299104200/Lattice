//! Tests for the pure evaluation metrics.
//!
//! Every expected number here is hand-computed from the definitions documented
//! in `metrics.rs` rather than captured from a run, so a regression in the
//! arithmetic fails loudly instead of re-baselining itself.

use super::*;

fn observation(key: &str, score_per_mille: u32, label: bool) -> ScoredObservation {
    ScoredObservation {
        key: key.to_owned(),
        score_per_mille,
        label,
    }
}

/// Four observations, alternating labels, all distinct scores.
///
/// Hand-computed: ranks ascending are 4,3,2,1 for the descending score order,
/// positives sit at ranks 4 and 2, so ROC-AUC = (6 - 3) / 4 = 0.75.
fn alternating() -> Vec<ScoredObservation> {
    vec![
        observation("a", 900, true),
        observation("b", 800, false),
        observation("c", 700, true),
        observation("d", 600, false),
    ]
}

#[test]
fn alternating_ranking_matches_hand_computed_metrics() {
    let evaluation = evaluate(alternating()).expect("evaluation");
    assert_eq!(evaluation.observations, 4);
    assert_eq!(evaluation.positives, 2);
    assert_eq!(evaluation.prevalence_per_mille, 500);
    assert_eq!(evaluation.roc_auc_per_mille, 750);
    // Trapezoid over the four tie groups: 1_000_000 + 0 + 583_500 + 0 in
    // per-mille^2, halved and reduced to per-mille.
    assert_eq!(evaluation.pr_auc_per_mille, 792);
    assert_eq!(evaluation.pr_auc_lift_per_mille, 1584);
}

#[test]
fn perfect_ranking_scores_both_areas_at_maximum() {
    let evaluation = evaluate(vec![
        observation("a", 900, true),
        observation("b", 800, true),
        observation("c", 700, false),
        observation("d", 600, false),
    ])
    .expect("evaluation");
    assert_eq!(evaluation.roc_auc_per_mille, 1000);
    assert_eq!(evaluation.pr_auc_per_mille, 1000);
}

#[test]
fn inverted_ranking_scores_roc_at_zero() {
    let evaluation = evaluate(vec![
        observation("a", 900, false),
        observation("b", 800, false),
        observation("c", 700, true),
        observation("d", 600, true),
    ])
    .expect("evaluation");
    assert_eq!(evaluation.roc_auc_per_mille, 0);
    // A PR curve never reaches zero even when the ranking is exactly wrong:
    // recall still climbs to 1000 as the threshold admits everything.
    assert_eq!(evaluation.pr_auc_per_mille, 292);
    assert!(evaluation.pr_auc_lift_per_mille < 1000);
}

#[test]
fn fully_tied_scores_are_indistinguishable_from_random() {
    let evaluation = evaluate(vec![
        observation("a", 500, true),
        observation("b", 500, false),
        observation("c", 500, true),
        observation("d", 500, false),
    ])
    .expect("evaluation");
    // A single tie group must not be able to manufacture a ranking: ROC lands
    // exactly on chance and PR-AUC lands exactly on prevalence.
    assert_eq!(evaluation.roc_auc_per_mille, 500);
    assert_eq!(evaluation.pr_auc_per_mille, 500);
    assert_eq!(evaluation.pr_auc_per_mille, evaluation.prevalence_per_mille);
    assert_eq!(evaluation.pr_auc_lift_per_mille, 1000);
}

#[test]
fn tie_groups_are_not_broken_by_key_order() {
    // Same scores, opposite label placement within the tie group. If the sort
    // broke ties by key and let ordering leak into the metric, these two would
    // differ.
    let ascending = evaluate(vec![
        observation("a", 700, true),
        observation("b", 700, false),
        observation("c", 300, true),
        observation("d", 300, false),
    ])
    .expect("evaluation");
    let descending = evaluate(vec![
        observation("a", 700, false),
        observation("b", 700, true),
        observation("c", 300, false),
        observation("d", 300, true),
    ])
    .expect("evaluation");
    assert_eq!(ascending.roc_auc_per_mille, descending.roc_auc_per_mille);
    assert_eq!(ascending.pr_auc_per_mille, descending.pr_auc_per_mille);
}

#[test]
fn input_order_never_changes_the_result() {
    let baseline = evaluate(alternating()).expect("evaluation");
    let mut shuffled = alternating();
    shuffled.reverse();
    shuffled.swap(0, 2);
    let reordered = evaluate(shuffled).expect("evaluation");
    assert_eq!(baseline, reordered);
}

#[test]
fn unusable_label_distributions_report_a_reason_instead_of_zero() {
    assert_eq!(
        evaluate(Vec::new()).unwrap_err(),
        EvaluationUnavailable::NoObservations
    );
    assert_eq!(
        evaluate(vec![observation("a", 900, false)]).unwrap_err(),
        EvaluationUnavailable::NoPositiveLabels
    );
    assert_eq!(
        evaluate(vec![observation("a", 900, true)]).unwrap_err(),
        EvaluationUnavailable::NoNegativeLabels
    );
}

#[test]
fn operating_points_report_precision_recall_and_lift_at_each_threshold() {
    let evaluation = evaluate(alternating()).expect("evaluation");
    let at_900 = evaluation
        .operating_points
        .iter()
        .find(|point| point.threshold_per_mille == 900)
        .expect("threshold 900");
    assert_eq!(at_900.selected, 1);
    assert_eq!(at_900.true_positives, 1);
    assert_eq!(at_900.precision_per_mille, 1000);
    assert_eq!(at_900.recall_per_mille, 500);
    // Harmonic mean of 1000 and 500.
    assert_eq!(at_900.f1_per_mille, 667);
    // Precision 1000 against a base rate of 500.
    assert_eq!(at_900.lift_per_mille, 2000);

    let at_600 = evaluation
        .operating_points
        .iter()
        .find(|point| point.threshold_per_mille == 600)
        .expect("threshold 600");
    assert_eq!(at_600.selected, 4);
    assert_eq!(at_600.precision_per_mille, 500);
    assert_eq!(at_600.recall_per_mille, 1000);
    assert_eq!(at_600.lift_per_mille, 1000);
}

#[test]
fn recall_never_decreases_as_the_threshold_falls() {
    let evaluation = evaluate(alternating()).expect("evaluation");
    let mut previous_recall = 0;
    // Operating points are emitted descending by threshold, so recall must be
    // non-decreasing across the list.
    for point in &evaluation.operating_points {
        assert!(point.recall_per_mille >= previous_recall);
        previous_recall = point.recall_per_mille;
    }
    assert_eq!(previous_recall, 1000);
}

#[test]
fn thresholds_selecting_nothing_report_zero_selected_rather_than_precision() {
    let evaluation = evaluate(vec![
        observation("a", 50, true),
        observation("b", 40, false),
        observation("c", 30, false),
    ])
    .expect("evaluation");
    let at_900 = &evaluation.operating_points[0];
    assert_eq!(at_900.threshold_per_mille, 900);
    assert_eq!(at_900.selected, 0);
    assert_eq!(at_900.true_positives, 0);
    assert_eq!(at_900.precision_per_mille, 0);
    assert_eq!(at_900.recall_per_mille, 0);
}

#[test]
fn calibration_buckets_cover_the_whole_score_range_including_both_extremes() {
    let evaluation = evaluate(vec![
        observation("floor", 0, false),
        observation("just_under_first_edge", 99, true),
        observation("first_edge", 100, false),
        observation("just_under_top", 999, true),
        observation("ceiling", 1000, true),
    ])
    .expect("evaluation");
    assert_eq!(evaluation.calibration.len(), 10);

    let bottom = &evaluation.calibration[0];
    assert_eq!(bottom.lower_per_mille, 0);
    assert_eq!(bottom.upper_per_mille, 100);
    assert_eq!(bottom.observations, 2);
    assert_eq!(bottom.positives, 1);
    assert_eq!(bottom.observed_rate_per_mille, 500);

    let second = &evaluation.calibration[1];
    assert_eq!(second.lower_per_mille, 100);
    assert_eq!(second.observations, 1);
    assert_eq!(second.positives, 0);
    assert_eq!(second.observed_rate_per_mille, 0);

    // A perfect 1000 must land in the top decile, not overflow past it.
    let top = &evaluation.calibration[9];
    assert_eq!(top.lower_per_mille, 900);
    assert_eq!(top.upper_per_mille, 1001);
    assert_eq!(top.observations, 2);
    assert_eq!(top.positives, 2);
    assert_eq!(top.observed_rate_per_mille, 1000);

    let total: u32 = evaluation
        .calibration
        .iter()
        .map(|bucket| bucket.observations)
        .sum();
    assert_eq!(total, evaluation.observations);
}

#[test]
fn empty_calibration_buckets_report_zero_observations_not_a_fabricated_rate() {
    let evaluation = evaluate(vec![
        observation("a", 950, true),
        observation("b", 940, false),
    ])
    .expect("evaluation");
    let empty = &evaluation.calibration[3];
    assert_eq!(empty.observations, 0);
    assert_eq!(empty.positives, 0);
    // The renderer prints `n/a` for a bucket with no observations; the stored
    // value is a placeholder that must never be read as a measured rate.
    assert_eq!(empty.observed_rate_per_mille, 0);
}

#[test]
fn rounding_is_half_up_and_platform_independent() {
    // 1/3 -> 333 (rounds down), 2/3 -> 667 (rounds up), 1/2 -> 500 exactly.
    assert_eq!(per_mille(1, 3), 333);
    assert_eq!(per_mille(2, 3), 667);
    assert_eq!(per_mille(1, 2), 500);
    // 1/8 = 125 exactly; 1/16 = 62.5 rounds half up to 63.
    assert_eq!(per_mille(1, 8), 125);
    assert_eq!(per_mille(1, 16), 63);
    assert_eq!(per_mille(0, 0), 0);
}

#[test]
fn large_populations_do_not_overflow_the_integer_accumulators() {
    // 40_000 observations at 10% prevalence exercises the u128 rank-sum path:
    // a u64 accumulator would still hold this, but the guard is the point.
    let observations: Vec<ScoredObservation> = (0..40_000u32)
        .map(|index| {
            let label = index % 10 == 0;
            observation(&format!("f{index:06}"), 1000 - index / 40, label)
        })
        .collect();
    let evaluation = evaluate(observations).expect("evaluation");
    assert_eq!(evaluation.observations, 40_000);
    assert_eq!(evaluation.positives, 4_000);
    assert_eq!(evaluation.prevalence_per_mille, 100);
    assert!(evaluation.roc_auc_per_mille <= 1000);
    assert!(evaluation.pr_auc_per_mille <= 1000);
}
