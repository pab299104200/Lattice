//! Tests for the Phase H5 health regression signals.
//!
//! These prove three things the CI wiring depends on: that the thresholds are
//! the committed report's figures rather than invented ones, that a degraded
//! rerun fails loudly with a non-zero exit code, and that a rerun over the
//! wrong corpus reports missing data instead of a comparison it is not entitled
//! to make.

use std::path::PathBuf;

use super::health_backtest::{
    committed_baseline, committed_per_mille, corpus_mismatch_reason, floor_per_mille,
    per_mille_to_ratio, signals_from_report, COMMITTED_CORPUS, COMMITTED_FAMILY_UPLIFT_PER_MILLE,
    COMMITTED_GRAPH_ONLY_ROC_AUC_PER_MILLE, COMMITTED_LABEL_ENRICHMENT_PER_MILLE,
    COMMITTED_PR_AUC_PER_MILLE, COMMITTED_ROC_AUC_PER_MILLE,
};
use super::report::{
    report_exit_code, RegressionReport, ReportInput, ReportRowStatus, SuccessCriteriaThresholds,
};
use super::{MetricScope, MetricScopeKind, MetricSignal, MetricSource, MetricValue};
use crate::health::backtest::audit::{AuditVerdict, LabelAudit};
use crate::health::backtest::features::{FamilySet, FeatureWeights};
use crate::health::backtest::metrics::Evaluation;
use crate::health::backtest::replay::{ReplayLimits, ReplayReport};
use crate::health::backtest::report::{BacktestReport, FamilyResult, RepositoryReport, Weighting};
use crate::{DateTime, Utc};

/// A fixed instant, so no assertion depends on wall-clock time.
fn at() -> DateTime<Utc> {
    DateTime::from_unix_seconds(1_770_000_000)
}

fn scope() -> MetricScope {
    let scope = MetricScope::repo("health-backtest-tests");
    assert_eq!(scope.kind, MetricScopeKind::Repo);
    scope
}

/// An evaluation carrying the two figures the health signals read.
fn evaluation(pr_auc_per_mille: u32, roc_auc_per_mille: u32) -> Evaluation {
    Evaluation {
        observations: 82_742,
        positives: 2_100,
        prevalence_per_mille: 25,
        pr_auc_per_mille,
        pr_auc_lift_per_mille: 0,
        roc_auc_per_mille,
        operating_points: Vec::new(),
        calibration: Vec::new(),
    }
}

fn family(family: FamilySet, pr_auc_per_mille: u32, roc_auc_per_mille: u32) -> FamilyResult {
    FamilyResult {
        family,
        weighting: Weighting::DerivedHeldOut,
        evaluation: Some(evaluation(pr_auc_per_mille, roc_auc_per_mille)),
        unavailable: None,
    }
}

fn audit(enrichment_per_mille: u32) -> LabelAudit {
    LabelAudit {
        commits_considered: 2_360,
        classified_fix: 571,
        touches_test_and_production: 1_251,
        both: 404,
        fix_only: 167,
        signal_only: 847,
        neither: 942,
        percent_agreement_per_mille: 570,
        cohen_kappa_per_mille: 167,
        fix_rate_per_mille: 242,
        co_modification_rate_per_mille: 530,
        co_modification_given_fix_per_mille: 708,
        co_modification_given_non_fix_per_mille: 473,
        enrichment_per_mille,
        unclassified_with_fix_vocabulary: 108,
        recall_gap_per_mille: 159,
        verdict: AuditVerdict::Corroborated,
        sample: Vec::new(),
    }
}

fn repository(name: &str) -> RepositoryReport {
    RepositoryReport {
        name: name.to_string(),
        head_commit: "0000000000".to_string(),
        replay: ReplayReport {
            limits: ReplayLimits::default(),
            spine_length: 0,
            spine_truncated: false,
            cut_points_placed: 0,
            reserves_reduced: false,
            horizon_reserve: 0,
            mining_reserve: 0,
        },
        cut_points: Vec::new(),
        observations: 0,
        positives: 0,
        families: Vec::new(),
        features: Vec::new(),
    }
}

/// A synthetic report over the committed corpus.
fn report(
    all_pr_auc: u32,
    all_roc_auc: u32,
    graph_only_roc_auc: u32,
    enrichment: u32,
) -> BacktestReport {
    BacktestReport {
        harness_version: 1,
        health_config_version: 1,
        repositories: COMMITTED_CORPUS
            .iter()
            .map(|name| repository(name))
            .collect(),
        pooled_observations: 82_742,
        pooled_positives: 2_100,
        pooled_cut_points: 30,
        pooled_families: Vec::new(),
        held_out_families: vec![
            family(FamilySet::GraphOnly, 116, graph_only_roc_auc),
            family(FamilySet::GraphGit, 218, 890),
            family(FamilySet::All, all_pr_auc, all_roc_auc),
        ],
        features: Vec::new(),
        derived_weights: FeatureWeights::uniform(),
        audit: audit(enrichment),
    }
}

/// A report reproducing the committed figures exactly.
fn committed_report() -> BacktestReport {
    report(
        COMMITTED_PR_AUC_PER_MILLE,
        COMMITTED_ROC_AUC_PER_MILLE,
        COMMITTED_GRAPH_ONLY_ROC_AUC_PER_MILLE,
        COMMITTED_LABEL_ENRICHMENT_PER_MILLE,
    )
}

fn build(current: Vec<MetricValue>) -> RegressionReport {
    RegressionReport::build(ReportInput {
        current,
        baseline: Some(committed_baseline(at())),
        benchmark_report_path: PathBuf::from("does-not-exist.json"),
        scope: scope(),
        success_criteria: SuccessCriteriaThresholds::initial(),
    })
    .expect("a missing benchmark file is not an error")
}

fn row_status(report: &RegressionReport, signal: MetricSignal) -> ReportRowStatus {
    report
        .rows
        .iter()
        .find(|row| row.signal == signal)
        .unwrap_or_else(|| panic!("no row for {}", signal.as_str()))
        .status
        .clone()
}

#[test]
fn every_health_signal_declares_a_floor_below_its_committed_figure() {
    for signal in MetricSignal::HEALTH {
        let floor = floor_per_mille(signal).expect("health signals declare floors");
        let committed = committed_per_mille(signal).expect("health signals declare a baseline");
        assert!(
            floor <= committed,
            "{} floor {floor} must not exceed its committed figure {committed}",
            signal.as_str()
        );
    }
}

#[test]
fn the_uplift_baseline_is_the_difference_the_report_records() {
    assert_eq!(
        COMMITTED_FAMILY_UPLIFT_PER_MILLE,
        COMMITTED_ROC_AUC_PER_MILLE - COMMITTED_GRAPH_ONLY_ROC_AUC_PER_MILLE
    );
    // Success criterion 1 of the plan: the extra families beat graph alone.
    assert!(COMMITTED_FAMILY_UPLIFT_PER_MILLE > 0);
}

#[test]
fn no_health_signal_is_scored_by_the_metrics_collector() {
    for signal in MetricSignal::HEALTH {
        assert!(signal.is_health());
        assert_eq!(
            super::report::benchmark_source(signal),
            MetricSource::HealthBacktest
        );
    }
}

#[test]
fn a_report_reproducing_the_committed_figures_passes_every_health_signal() {
    let current = signals_from_report(&committed_report(), at());
    assert_eq!(current.len(), MetricSignal::HEALTH.len());
    let report = build(current);
    for signal in MetricSignal::HEALTH {
        assert_eq!(
            row_status(&report, signal),
            ReportRowStatus::Pass,
            "{} should pass when the rerun reproduces the committed report",
            signal.as_str()
        );
    }
    assert_eq!(report_exit_code(&report, true), 0);
}

#[test]
fn drift_inside_the_tolerance_still_passes() {
    // One per-mille above each floor: the tolerance exists precisely so that
    // history growing between reruns does not fail the build.
    let pr_auc = floor_per_mille(MetricSignal::HealthDefectPrAuc).unwrap() + 1;
    let roc_auc = floor_per_mille(MetricSignal::HealthDefectRocAuc).unwrap() + 1;
    let uplift_floor = floor_per_mille(MetricSignal::HealthDefectFamilyUplift).unwrap();
    let current = signals_from_report(
        &report(pr_auc, roc_auc, roc_auc - uplift_floor - 1, 1_001),
        at(),
    );
    let report = build(current);
    for signal in MetricSignal::HEALTH {
        assert_eq!(
            row_status(&report, signal),
            ReportRowStatus::Pass,
            "{} should tolerate drift that stays above its floor",
            signal.as_str()
        );
    }
    assert_eq!(report_exit_code(&report, true), 0);
}

#[test]
fn a_degraded_pr_auc_fails_and_sets_a_non_zero_exit_code() {
    let below = floor_per_mille(MetricSignal::HealthDefectPrAuc).unwrap() - 1;
    let current = signals_from_report(
        &report(
            below,
            COMMITTED_ROC_AUC_PER_MILLE,
            COMMITTED_GRAPH_ONLY_ROC_AUC_PER_MILLE,
            COMMITTED_LABEL_ENRICHMENT_PER_MILLE,
        ),
        at(),
    );
    let report = build(current);
    assert_eq!(
        row_status(&report, MetricSignal::HealthDefectPrAuc),
        ReportRowStatus::Fail
    );
    assert_eq!(
        row_status(&report, MetricSignal::HealthDefectRocAuc),
        ReportRowStatus::Pass
    );
    assert_eq!(report_exit_code(&report, true), 1);
    assert!(report.render_ci_summary().contains("health_defect_pr_auc"));
}

#[test]
fn losing_the_git_and_complexity_families_fails_the_uplift_signal() {
    // The whole point of the uplift signal: if the extra fact families stop
    // helping, the engine's headline claim is false even when the absolute
    // ROC-AUC still looks respectable.
    let current = signals_from_report(
        &report(
            COMMITTED_PR_AUC_PER_MILLE,
            COMMITTED_ROC_AUC_PER_MILLE,
            COMMITTED_ROC_AUC_PER_MILLE,
            COMMITTED_LABEL_ENRICHMENT_PER_MILLE,
        ),
        at(),
    );
    let report = build(current);
    assert_eq!(
        row_status(&report, MetricSignal::HealthDefectFamilyUplift),
        ReportRowStatus::Fail
    );
    assert_eq!(report_exit_code(&report, true), 1);
}

#[test]
fn an_uncorroborated_fix_classifier_fails_the_label_audit_signal() {
    let current = signals_from_report(
        &report(
            COMMITTED_PR_AUC_PER_MILLE,
            COMMITTED_ROC_AUC_PER_MILLE,
            COMMITTED_GRAPH_ONLY_ROC_AUC_PER_MILLE,
            // Below 1.000x: fix-shaped commits are no longer distinguishable
            // from any other commit by the independent signal.
            940,
        ),
        at(),
    );
    let report = build(current);
    assert_eq!(
        row_status(&report, MetricSignal::HealthLabelAuditEnrichment),
        ReportRowStatus::Fail
    );
    assert_eq!(report_exit_code(&report, true), 1);
}

#[test]
fn a_rerun_over_a_different_corpus_reports_missing_data_rather_than_a_verdict() {
    let mut narrowed = committed_report();
    narrowed
        .repositories
        .retain(|repository| repository.name == "lattice");
    let reason =
        corpus_mismatch_reason(&narrowed).expect("a one-repo corpus is not the committed one");
    assert!(
        reason.contains("keystone"),
        "reason should name what is missing: {reason}"
    );

    let current = signals_from_report(&narrowed, at());
    for value in &current {
        assert!(
            value.value.is_none(),
            "{} must not report a pooled figure from the wrong corpus",
            value.signal.as_str()
        );
        assert!(value.incomplete);
        assert!(value.reason_if_null.is_some());
    }

    let report = build(current);
    for signal in MetricSignal::HEALTH {
        assert_eq!(
            row_status(&report, signal),
            ReportRowStatus::MissingData,
            "{} should be missing, not failing, over the wrong corpus",
            signal.as_str()
        );
    }
    // A corpus the run could not assemble is not a regression.
    assert_eq!(report_exit_code(&report, true), 0);
}

#[test]
fn an_unmeasurable_family_reports_its_own_reason() {
    let mut degraded = committed_report();
    for result in &mut degraded.held_out_families {
        if result.family == FamilySet::All {
            result.evaluation = None;
            result.unavailable = Some("no positive labels".to_string());
        }
    }
    let current = signals_from_report(&degraded, at());
    let pr_auc = current
        .iter()
        .find(|value| value.signal == MetricSignal::HealthDefectPrAuc)
        .expect("pr-auc is produced");
    assert!(pr_auc.value.is_none());
    assert_eq!(pr_auc.reason_if_null.as_deref(), Some("no positive labels"));

    let report = build(current);
    assert_eq!(
        row_status(&report, MetricSignal::HealthDefectPrAuc),
        ReportRowStatus::MissingData
    );
    // The enrichment signal is independent of the family evaluations and still
    // reports normally.
    assert_eq!(
        row_status(&report, MetricSignal::HealthLabelAuditEnrichment),
        ReportRowStatus::Pass
    );
}

#[test]
fn the_baseline_column_reproduces_the_committed_report() {
    let baseline = committed_baseline(at());
    assert_eq!(baseline.len(), MetricSignal::HEALTH.len());
    for value in baseline {
        assert_eq!(value.source, MetricSource::HealthBacktest);
        assert_eq!(
            value.value,
            committed_per_mille(value.signal).map(per_mille_to_ratio)
        );
    }
}

#[test]
fn health_rows_render_in_the_text_report_with_their_targets() {
    let report = build(signals_from_report(&committed_report(), at()));
    let text = report.render_text();
    for signal in MetricSignal::HEALTH {
        assert!(
            text.contains(signal.as_str()),
            "{} should appear in the rendered report",
            signal.as_str()
        );
    }
    assert!(text.contains("uplift over graph-only"));
}
