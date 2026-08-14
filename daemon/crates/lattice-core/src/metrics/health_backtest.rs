//! Phase H5 of `docs/plans/2026-08-13-health-engine.md`: the health backtest as
//! a tracked regression signal.
//!
//! # Why this exists
//!
//! H1 proved that the health facts predict defects, and H3 turned that proof
//! into weights. Nothing so far stops a later edit to those weights, to
//! `looks_like_bug_fix`, or to a fact producer from quietly making the
//! prediction worse. This module closes that hole: it converts a
//! [`BacktestReport`] into [`MetricValue`]s that the existing regression report
//! evaluates against thresholds, so a degradation fails CI instead of shipping.
//!
//! # Where the baselines come from
//!
//! Every baseline here is transcribed from the committed report
//! `docs/reports/health-backtest/2026-08-14.md`, which
//! `docs/plans/2026-08-13-health-engine.md` § "Phase H1 — Historical backtest
//! harness (do first)" designates as the only authority for these numbers. No
//! threshold in this file was chosen to make a run pass.
//!
//! The three ranking figures are read from that report's
//! § "Pooled, derived weights, held out" table, which the report itself calls
//! "the number to quote" because each cut point there is scored with weights
//! derived from the *other* cut points. Uniform-weight figures are deliberately
//! not tracked: they measure an unfitted ranker, not the one that ships.
//!
//! # Integer discipline
//!
//! The harness reports per-mille integers so that a replay is byte-identical
//! across platforms (design decision 3 of the plan). The baselines and
//! tolerances below are therefore declared as per-mille integers and converted
//! to the report surface's `f64` exactly once, at the boundary. No threshold
//! comparison in this module is performed in floating point.
//!
//! # Corpus honesty
//!
//! A pooled figure is a property of the corpus that produced it: this repo
//! alone scores a PR-AUC of 0.031 where the five-repo pool scores 0.232, and
//! neither number is wrong. Comparing a rerun over a different corpus against
//! the committed pooled baseline would manufacture both false failures and
//! false passes, so [`signals_from_report`] refuses to do it: a rerun whose
//! repository set differs from the committed corpus yields values of `None`
//! carrying a reason that names the difference. That is design decision 4 of
//! the plan — "unknown is never zero" — applied to CI.

use std::collections::BTreeSet;

use crate::health::backtest::features::FamilySet;
use crate::health::backtest::report::{BacktestReport, FamilyResult, Weighting};
use crate::metrics::{MetricSignal, MetricSource, MetricValue};
use crate::{DateTime, Utc};

/// Pooled held-out PR-AUC of `graph+git+complexity`, per-mille.
///
/// `docs/reports/health-backtest/2026-08-14.md`
/// § "Pooled, derived weights, held out": 0.232.
pub const COMMITTED_PR_AUC_PER_MILLE: u32 = 232;

/// Pooled held-out ROC-AUC of `graph+git+complexity`, per-mille.
///
/// Same table: 0.901.
pub const COMMITTED_ROC_AUC_PER_MILLE: u32 = 901;

/// Pooled held-out ROC-AUC of `graph-only`, per-mille.
///
/// Same table: 0.785. Not tracked on its own; it is the subtrahend of
/// [`COMMITTED_FAMILY_UPLIFT_PER_MILLE`].
pub const COMMITTED_GRAPH_ONLY_ROC_AUC_PER_MILLE: u32 = 785;

/// How far `graph+git+complexity` outranks `graph-only`, per-mille: 0.116.
///
/// This is success criterion 1 of the plan stated as a number.
pub const COMMITTED_FAMILY_UPLIFT_PER_MILLE: u32 =
    COMMITTED_ROC_AUC_PER_MILLE - COMMITTED_GRAPH_ONLY_ROC_AUC_PER_MILLE;

/// Label-audit enrichment, per-mille.
///
/// `docs/reports/health-backtest/2026-08-14.md`
/// § "H1.2 label-quality audit": 1.336x.
pub const COMMITTED_LABEL_ENRICHMENT_PER_MILLE: u32 = 1336;

/// The repositories the committed report pooled, in report order.
///
/// `docs/reports/health-backtest/2026-08-14.md` § "Provenance".
pub const COMMITTED_CORPUS: [&str; 5] = ["lattice", "synapse", "rmm", "beacon", "keystone"];

/// How far a rerun may fall below the committed figure before it is a
/// regression, per-mille.
///
/// A tolerance is required rather than optional. History grows, so a rerun is
/// never scored over exactly the commits the committed report saw, and even a
/// code change of zero size moves these figures a little. The values below are
/// deliberately loose enough to absorb that drift and tight enough that losing
/// a fact family — which costs 0.116 of ROC-AUC, per the report's own
/// graph-only row — cannot pass.
pub const PR_AUC_TOLERANCE_PER_MILLE: u32 = 30;

/// Tolerance for ROC-AUC, per-mille. Tighter than PR-AUC's because ROC-AUC is
/// far less sensitive to prevalence, which is the quantity that drifts most as
/// history grows.
pub const ROC_AUC_TOLERANCE_PER_MILLE: u32 = 20;

/// Tolerance for the family uplift, per-mille.
pub const FAMILY_UPLIFT_TOLERANCE_PER_MILLE: u32 = 30;

/// Floor for label-audit enrichment, per-mille.
///
/// Not a tolerance below the committed 1.336x but an absolute floor at 1.000x,
/// which is the point the report defines as "indistinguishable from any other
/// commit by the independent signal". Below it the classifier has stopped
/// being corroborated at all, which is the only unambiguous failure this
/// measurement supports. The report is explicit that the raw agreement and
/// kappa figures are expected to be low even when the classifier is correct,
/// so neither is tracked.
pub const LABEL_ENRICHMENT_FLOOR_PER_MILLE: u32 = 1000;

/// Convert a per-mille integer to the ratio the report surface renders.
pub const fn per_mille_to_ratio(per_mille: u32) -> f64 {
    per_mille as f64 / 1000.0
}

/// The minimum acceptable value for each health signal, per-mille.
///
/// Exposed as integers so that a caller can assert on thresholds without
/// floating-point comparison.
pub fn floor_per_mille(signal: MetricSignal) -> Option<u32> {
    match signal {
        MetricSignal::HealthDefectPrAuc => {
            Some(COMMITTED_PR_AUC_PER_MILLE - PR_AUC_TOLERANCE_PER_MILLE)
        }
        MetricSignal::HealthDefectRocAuc => {
            Some(COMMITTED_ROC_AUC_PER_MILLE - ROC_AUC_TOLERANCE_PER_MILLE)
        }
        MetricSignal::HealthDefectFamilyUplift => {
            Some(COMMITTED_FAMILY_UPLIFT_PER_MILLE - FAMILY_UPLIFT_TOLERANCE_PER_MILLE)
        }
        MetricSignal::HealthLabelAuditEnrichment => Some(LABEL_ENRICHMENT_FLOOR_PER_MILLE),
        _ => None,
    }
}

/// The committed report's figure for each health signal, per-mille.
pub fn committed_per_mille(signal: MetricSignal) -> Option<u32> {
    match signal {
        MetricSignal::HealthDefectPrAuc => Some(COMMITTED_PR_AUC_PER_MILLE),
        MetricSignal::HealthDefectRocAuc => Some(COMMITTED_ROC_AUC_PER_MILLE),
        MetricSignal::HealthDefectFamilyUplift => Some(COMMITTED_FAMILY_UPLIFT_PER_MILLE),
        MetricSignal::HealthLabelAuditEnrichment => Some(COMMITTED_LABEL_ENRICHMENT_PER_MILLE),
        _ => None,
    }
}

/// The committed report rendered as the baseline column of a regression report.
///
/// Pass this as `ReportInput::baseline` alongside a rerun's
/// [`signals_from_report`] output to see the delta against the document of
/// record.
pub fn committed_baseline(computed_at: DateTime<Utc>) -> Vec<MetricValue> {
    MetricSignal::HEALTH
        .iter()
        .copied()
        .map(|signal| MetricValue {
            signal,
            value: committed_per_mille(signal).map(per_mille_to_ratio),
            denominator: None,
            sample_count: COMMITTED_POOLED_OBSERVATIONS,
            source: MetricSource::HealthBacktest,
            computed_at,
            incomplete: false,
            reason_if_null: None,
        })
        .collect()
}

/// Files scored across the committed report's whole corpus.
///
/// `docs/reports/health-backtest/2026-08-14.md` § "Provenance".
pub const COMMITTED_POOLED_OBSERVATIONS: u64 = 82_742;

/// Turn a freshly produced backtest report into health regression signals.
///
/// Returns one [`MetricValue`] per [`MetricSignal::HEALTH`] entry, in that
/// order. A figure the report could not measure — and every figure at all, when
/// the replayed corpus is not the committed one — comes back as `None` with a
/// truthful `reason_if_null` rather than as a number that would be compared
/// against a baseline it does not belong to.
pub fn signals_from_report(report: &BacktestReport, computed_at: DateTime<Utc>) -> Vec<MetricValue> {
    let corpus_mismatch = corpus_mismatch_reason(report);
    MetricSignal::HEALTH
        .iter()
        .copied()
        .map(|signal| {
            let (value, reason) = match &corpus_mismatch {
                Some(reason) => (None, Some(reason.clone())),
                None => measure(report, signal),
            };
            MetricValue {
                signal,
                value: value.map(per_mille_to_ratio),
                denominator: None,
                sample_count: u64::from(report.pooled_observations),
                source: MetricSource::HealthBacktest,
                computed_at,
                incomplete: corpus_mismatch.is_some(),
                reason_if_null: reason,
            }
        })
        .collect()
}

/// Read one signal's per-mille value out of a report.
fn measure(report: &BacktestReport, signal: MetricSignal) -> (Option<u32>, Option<String>) {
    match signal {
        MetricSignal::HealthDefectPrAuc => {
            held_out(report, FamilySet::All, |evaluation| evaluation.pr_auc_per_mille)
        }
        MetricSignal::HealthDefectRocAuc => {
            held_out(report, FamilySet::All, |evaluation| evaluation.roc_auc_per_mille)
        }
        MetricSignal::HealthDefectFamilyUplift => family_uplift(report),
        MetricSignal::HealthLabelAuditEnrichment => {
            (Some(report.audit.enrichment_per_mille), None)
        }
        _ => (
            None,
            Some("signal is not produced by the health backtest".to_string()),
        ),
    }
}

/// How much better `graph+git+complexity` ranks than `graph-only`, per-mille.
///
/// Saturates at zero rather than going negative: the signal answers "how much
/// uplift is there", and a negative uplift is reported as none, which fails the
/// floor exactly as it should.
fn family_uplift(report: &BacktestReport) -> (Option<u32>, Option<String>) {
    let (all, all_reason) = held_out(report, FamilySet::All, |evaluation| {
        evaluation.roc_auc_per_mille
    });
    let (graph_only, graph_reason) = held_out(report, FamilySet::GraphOnly, |evaluation| {
        evaluation.roc_auc_per_mille
    });
    match (all, graph_only) {
        (Some(all), Some(graph_only)) => (Some(all.saturating_sub(graph_only)), None),
        _ => (None, all_reason.or(graph_reason)),
    }
}

/// Pull a held-out family's evaluation out of the report.
fn held_out(
    report: &BacktestReport,
    family: FamilySet,
    read: impl Fn(&crate::health::backtest::metrics::Evaluation) -> u32,
) -> (Option<u32>, Option<String>) {
    let Some(result) = report
        .held_out_families
        .iter()
        .find(|candidate: &&FamilyResult| {
            candidate.family == family && candidate.weighting == Weighting::DerivedHeldOut
        })
    else {
        return (
            None,
            Some(format!(
                "the report contains no held-out evaluation for family `{}`",
                family.as_str()
            )),
        );
    };
    match &result.evaluation {
        Some(evaluation) => (Some(read(evaluation)), None),
        None => (
            None,
            Some(result.unavailable.clone().unwrap_or_else(|| {
                format!("family `{}` could not be evaluated", family.as_str())
            })),
        ),
    }
}

/// Why a report's corpus cannot be compared against the committed baseline.
///
/// `None` means the corpus matches and the figures are comparable.
pub fn corpus_mismatch_reason(report: &BacktestReport) -> Option<String> {
    let replayed = report
        .repositories
        .iter()
        .map(|repository| repository.name.as_str())
        .collect::<BTreeSet<_>>();
    let committed = COMMITTED_CORPUS.iter().copied().collect::<BTreeSet<_>>();
    if replayed == committed {
        return None;
    }
    let missing = committed
        .difference(&replayed)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let extra = replayed
        .difference(&committed)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("missing {missing}"));
    }
    if !extra.is_empty() {
        parts.push(format!("unexpected {extra}"));
    }
    Some(format!(
        "replayed corpus does not match the committed report's ({}); \
         pooled figures from a different corpus are not comparable to its baselines",
        parts.join("; ")
    ))
}
