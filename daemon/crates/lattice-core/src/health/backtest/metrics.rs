//! Pure, integer-only evaluation metrics for the H1 backtest harness.
//!
//! Every quantity here is computed with `i64`/`u64` integer arithmetic and
//! reported in per-mille, so a given set of scored observations always yields
//! byte-identical numbers on every platform (spec "Design decisions" 3 and
//! "Success criteria" 4 in `docs/plans/2026-08-13-health-engine.md`).
//!
//! Two ranking metrics are reported because they answer different questions at
//! the low defect prevalence this harness measures:
//!
//! * **PR-AUC** — area under the precision/recall curve. Sensitive to
//!   prevalence, so it is only interpretable against the reported baseline
//!   (a random ranker scores exactly the prevalence).
//! * **ROC-AUC** — the Mann-Whitney statistic: the probability that a randomly
//!   chosen defect-labeled file outranks a randomly chosen clean one, with ties
//!   counted as half. Prevalence-independent, which makes it the more stable
//!   input for deriving H3 weights.
//!
//! Ties are handled explicitly rather than broken arbitrarily: files sharing a
//! score form one group, and a group contributes a single point to the PR curve
//! and averaged ranks to the ROC statistic. Breaking ties by key would let the
//! sort order manufacture precision the score did not earn.

use serde::{Deserialize, Serialize};

/// A single scored observation: one file at one cut point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoredObservation {
    /// Stable identity of the observation, used only for deterministic
    /// ordering within a tie group. Never influences a metric.
    pub key: String,
    /// The candidate signal's score, in per-mille (0..=1000).
    pub score_per_mille: u32,
    /// Ground truth: the file was touched by a fix-shaped commit inside the
    /// post-cut-point horizon.
    pub label: bool,
}

/// Why an evaluation could not be produced.
///
/// "Unknown is never zero" (spec design decision 4): a family with no usable
/// observations reports the reason rather than a zero score that would read as
/// a measured failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationUnavailable {
    /// No observations at all were produced for this family.
    NoObservations,
    /// Observations exist but none carry a positive label: precision and
    /// recall are undefined.
    NoPositiveLabels,
    /// Every observation carries a positive label: a ranker cannot be
    /// distinguished from a constant one.
    NoNegativeLabels,
}

impl EvaluationUnavailable {
    /// Human-readable reason for report text.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoObservations => "no observations",
            Self::NoPositiveLabels => "no defect-labeled observations in the horizon",
            Self::NoNegativeLabels => "every observation is defect-labeled",
        }
    }
}

/// Precision/recall at one score threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingPoint {
    /// Files scoring at or above this per-mille threshold are selected.
    pub threshold_per_mille: u32,
    /// Number of files selected at this threshold.
    pub selected: u32,
    /// Selected files that carry a positive label.
    pub true_positives: u32,
    /// `true_positives / selected`, per-mille. Zero selected files means the
    /// threshold is not an operating point at all; `selected == 0` marks it.
    pub precision_per_mille: u32,
    /// `true_positives / positives`, per-mille.
    pub recall_per_mille: u32,
    /// Harmonic mean of precision and recall, per-mille.
    pub f1_per_mille: u32,
    /// `precision / prevalence`, per-mille: 1000 means no better than picking
    /// files at random, 2000 means twice the base rate.
    pub lift_per_mille: u32,
}

/// One decile of the score range with its observed defect rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationBucket {
    /// Inclusive lower bound of the decile, per-mille.
    pub lower_per_mille: u32,
    /// Exclusive upper bound of the decile, per-mille. The top bucket's bound
    /// is 1001 so that a perfect score of 1000 falls inside it.
    pub upper_per_mille: u32,
    /// Observations whose score falls in this decile.
    pub observations: u32,
    /// Of those, how many carry a positive label.
    pub positives: u32,
    /// `positives / observations`, per-mille. Meaningless when
    /// `observations == 0`, which the renderer prints as `n/a`.
    pub observed_rate_per_mille: u32,
}

/// The full evaluation of one candidate ranking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    /// Total observations scored.
    pub observations: u32,
    /// Observations carrying a positive label.
    pub positives: u32,
    /// Base defect rate, per-mille. A random ranker's PR-AUC equals this.
    pub prevalence_per_mille: u32,
    /// Area under the precision/recall curve, per-mille.
    pub pr_auc_per_mille: u32,
    /// `pr_auc / prevalence`, per-mille: 1000 means indistinguishable from
    /// random.
    pub pr_auc_lift_per_mille: u32,
    /// Mann-Whitney ROC-AUC, per-mille. 500 means indistinguishable from
    /// random; below 500 means the score is anti-correlated with defects.
    pub roc_auc_per_mille: u32,
    /// Precision/recall at fixed score thresholds, descending.
    pub operating_points: Vec<OperatingPoint>,
    /// Observed defect rate per score decile.
    pub calibration: Vec<CalibrationBucket>,
}

/// The thresholds every evaluation reports operating points for, descending.
const OPERATING_THRESHOLDS: [u32; 9] = [900, 800, 700, 600, 500, 400, 300, 200, 100];

/// Round `numerator / denominator` to the nearest integer, halves up.
///
/// Deterministic on every platform: no floating point is involved anywhere in
/// this module.
fn round_div(numerator: u128, denominator: u128) -> u128 {
    if denominator == 0 {
        return 0;
    }
    (numerator * 2 + denominator) / (denominator * 2)
}

/// `numerator / denominator` expressed in per-mille, rounded half up.
fn per_mille(numerator: u64, denominator: u64) -> u32 {
    if denominator == 0 {
        return 0;
    }
    round_div(u128::from(numerator) * 1000, u128::from(denominator)) as u32
}

/// Evaluate a ranking against its ground-truth labels.
///
/// `observations` is taken by value and sorted internally; the caller's order
/// never affects the result. Within a tie group the observations are ordered by
/// `key` purely so that any derived listing is stable — the metrics themselves
/// treat the group as one unit.
pub fn evaluate(mut observations: Vec<ScoredObservation>) -> Result<Evaluation, EvaluationUnavailable> {
    if observations.is_empty() {
        return Err(EvaluationUnavailable::NoObservations);
    }
    let total = observations.len() as u64;
    let positives = observations.iter().filter(|observation| observation.label).count() as u64;
    if positives == 0 {
        return Err(EvaluationUnavailable::NoPositiveLabels);
    }
    let negatives = total - positives;
    if negatives == 0 {
        return Err(EvaluationUnavailable::NoNegativeLabels);
    }

    // Descending score, then ascending key: deterministic and independent of
    // the caller's insertion order.
    observations.sort_by(|left, right| {
        right
            .score_per_mille
            .cmp(&left.score_per_mille)
            .then_with(|| left.key.cmp(&right.key))
    });

    let prevalence_per_mille = per_mille(positives, total);
    let pr_auc_per_mille = pr_auc(&observations, positives);
    let roc_auc_per_mille = roc_auc(&observations, positives, negatives);

    let operating_points = OPERATING_THRESHOLDS
        .iter()
        .map(|threshold| operating_point(&observations, positives, prevalence_per_mille, *threshold))
        .collect();

    Ok(Evaluation {
        observations: total as u32,
        positives: positives as u32,
        prevalence_per_mille,
        pr_auc_per_mille,
        pr_auc_lift_per_mille: per_mille(
            u64::from(pr_auc_per_mille),
            u64::from(prevalence_per_mille),
        ),
        roc_auc_per_mille,
        operating_points,
        calibration: calibration(&observations),
    })
}

/// Area under the precision/recall curve, trapezoid rule over tie groups.
///
/// The curve starts at recall 0 with the precision of the top-scoring group
/// rather than at an undefined point, which is the conventional treatment and
/// is stated in the report so the number is reproducible.
fn pr_auc(sorted: &[ScoredObservation], positives: u64) -> u32 {
    let mut true_positives: u64 = 0;
    let mut selected: u64 = 0;
    // Curve accumulated in per-mille * per-mille to keep the trapezoid exact.
    let mut area: u128 = 0;
    let mut previous_recall: u64 = 0;
    let mut previous_precision: Option<u64> = None;

    let mut index = 0usize;
    while index < sorted.len() {
        let score = sorted[index].score_per_mille;
        // Consume the whole tie group before emitting a point.
        while index < sorted.len() && sorted[index].score_per_mille == score {
            if sorted[index].label {
                true_positives += 1;
            }
            selected += 1;
            index += 1;
        }
        let recall = u64::from(per_mille(true_positives, positives));
        let precision = u64::from(per_mille(true_positives, selected));
        let previous = previous_precision.unwrap_or(precision);
        area += u128::from(recall - previous_recall) * u128::from(precision + previous);
        previous_recall = recall;
        previous_precision = Some(precision);
    }

    // area currently holds 2 * (integral in per-mille^2); halve it and reduce
    // one per-mille factor to land back in per-mille.
    round_div(area, 2000) as u32
}

/// Mann-Whitney ROC-AUC with averaged ranks for ties.
///
/// Ranks are ascending (worst score gets rank 1). Averaged ranks can be
/// half-integers, so the sum is accumulated doubled and the halving is folded
/// into the final division — no fractional arithmetic anywhere.
fn roc_auc(sorted: &[ScoredObservation], positives: u64, negatives: u64) -> u32 {
    let total = sorted.len() as u64;
    let mut doubled_positive_rank_sum: u128 = 0;

    let mut index = 0usize;
    while index < sorted.len() {
        let score = sorted[index].score_per_mille;
        let group_start = index;
        while index < sorted.len() && sorted[index].score_per_mille == score {
            index += 1;
        }
        let group_len = (index - group_start) as u64;
        // `sorted` is descending, so the element at position `p` has ascending
        // rank `total - p`. The group spans ascending ranks
        // [total - index + 1, total - group_start], whose mean, doubled, is:
        let doubled_mean_rank =
            (total - index as u64 + 1) + (total - group_start as u64);
        let group_positives = sorted[group_start..index]
            .iter()
            .filter(|observation| observation.label)
            .count() as u64;
        debug_assert!(group_len > 0);
        doubled_positive_rank_sum += u128::from(doubled_mean_rank) * u128::from(group_positives);
    }

    // AUC = (S - npos*(npos+1)/2) / (npos*nneg) where S is the averaged-rank
    // sum of the positives. Substituting S = doubled/2 and multiplying through
    // by 1000 for per-mille:
    let numerator = doubled_positive_rank_sum - u128::from(positives) * u128::from(positives + 1);
    let denominator = 2u128 * u128::from(positives) * u128::from(negatives);
    round_div(numerator * 1000, denominator) as u32
}

/// Precision/recall for "select every file scoring at or above `threshold`".
fn operating_point(
    sorted: &[ScoredObservation],
    positives: u64,
    prevalence_per_mille: u32,
    threshold_per_mille: u32,
) -> OperatingPoint {
    let mut selected: u64 = 0;
    let mut true_positives: u64 = 0;
    for observation in sorted {
        if observation.score_per_mille < threshold_per_mille {
            break;
        }
        selected += 1;
        if observation.label {
            true_positives += 1;
        }
    }
    let precision_per_mille = per_mille(true_positives, selected);
    let recall_per_mille = per_mille(true_positives, positives);
    // Both operands are already per-mille, so their harmonic mean is too: this
    // must not be run through `per_mille`, which would scale it a second time.
    let f1_per_mille = if precision_per_mille + recall_per_mille == 0 {
        0
    } else {
        round_div(
            2 * u128::from(precision_per_mille) * u128::from(recall_per_mille),
            u128::from(precision_per_mille) + u128::from(recall_per_mille),
        ) as u32
    };
    OperatingPoint {
        threshold_per_mille,
        selected: selected as u32,
        true_positives: true_positives as u32,
        precision_per_mille,
        recall_per_mille,
        f1_per_mille,
        lift_per_mille: per_mille(
            u64::from(precision_per_mille),
            u64::from(prevalence_per_mille),
        ),
    }
}

/// Observed defect rate per score decile, ascending.
fn calibration(sorted: &[ScoredObservation]) -> Vec<CalibrationBucket> {
    let mut buckets: Vec<CalibrationBucket> = (0..10)
        .map(|decile| CalibrationBucket {
            lower_per_mille: decile * 100,
            // The top decile absorbs a perfect 1000.
            upper_per_mille: if decile == 9 { 1001 } else { (decile + 1) * 100 },
            observations: 0,
            positives: 0,
            observed_rate_per_mille: 0,
        })
        .collect();
    for observation in sorted {
        let index = ((observation.score_per_mille / 100) as usize).min(9);
        buckets[index].observations += 1;
        if observation.label {
            buckets[index].positives += 1;
        }
    }
    for bucket in &mut buckets {
        bucket.observed_rate_per_mille =
            per_mille(u64::from(bucket.positives), u64::from(bucket.observations));
    }
    buckets
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
