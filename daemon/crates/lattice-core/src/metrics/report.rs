//! Phase 9 regression reporting for operators and CI.
//!
//! This module implements the report surface required by
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 9: Metrics And Evaluation` and
//! `## Measurable Success Criteria`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::identity::{EventId, MemoryId};
use crate::metrics::{MetricScope, MetricSignal, MetricSource, MetricValue};
use crate::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportInput {
    pub current: Vec<MetricValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Vec<MetricValue>>,
    pub benchmark_report_path: PathBuf,
    pub scope: MetricScope,
    pub success_criteria: SuccessCriteriaThresholds,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SuccessCriteriaThresholds {
    pub discovery_tool_calls_reduction: f64,
    pub irrelevant_file_reads_reduction: f64,
    pub memory_inclusion_precision: f64,
    pub contradiction_missed_rate: f64,
    pub stale_memory_unlabeled_rate: f64,
    pub tests_recommended_vs_needed: f64,
}

impl SuccessCriteriaThresholds {
    pub fn initial() -> Self {
        Self {
            discovery_tool_calls_reduction: 0.30,
            irrelevant_file_reads_reduction: 0.40,
            memory_inclusion_precision: 0.80,
            contradiction_missed_rate: 0.0,
            stale_memory_unlabeled_rate: 0.0,
            tests_recommended_vs_needed: 0.90,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportRowStatus {
    Pass,
    Fail,
    MissingData,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduction_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalEvidencePointer {
    pub source: MetricSource,
    #[serde(default)]
    pub task_ids: Vec<String>,
    #[serde(default)]
    pub event_ids: Vec<EventId>,
    #[serde(default)]
    pub memory_ids: Vec<MemoryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_report_path: Option<PathBuf>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionReportRow {
    pub signal: MetricSignal,
    pub current: MetricValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<MetricValue>,
    pub delta: SignalDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub status: ReportRowStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_reason: Option<String>,
    #[serde(default)]
    pub evidence: Vec<SignalEvidencePointer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionReport {
    pub scope: MetricScope,
    pub success_criteria: SuccessCriteriaThresholds,
    pub rows: Vec<RegressionReportRow>,
    pub benchmark_report_path: PathBuf,
    pub generated_at: DateTime<Utc>,
    pub pass_count: usize,
    pub fail_count: usize,
    pub missing_count: usize,
    pub not_applicable_count: usize,
}

#[derive(Debug, Clone)]
pub struct BenchmarkEvidence {
    pub(crate) metrics: Vec<MetricValue>,
    pub(crate) evidence: HashMap<MetricSignal, Vec<SignalEvidencePointer>>,
}

#[derive(Debug, Error)]
pub enum ReportError {
    #[error("failed to read benchmark report at {path}: {source}")]
    ReadBenchmark {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse benchmark report at {path}: {source}")]
    ParseBenchmark {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl BenchmarkEvidence {
    pub fn from_path(path: &Path) -> Result<Option<Self>, ReportError> {
        super::report_benchmark::load_benchmark_evidence(path)
    }

    pub fn metrics(&self) -> &[MetricValue] {
        &self.metrics
    }

    pub fn evidence_for(&self, signal: MetricSignal) -> Vec<SignalEvidencePointer> {
        self.evidence.get(&signal).cloned().unwrap_or_default()
    }
}

impl RegressionReport {
    pub fn build(input: ReportInput) -> Result<Self, ReportError> {
        let benchmark = BenchmarkEvidence::from_path(&input.benchmark_report_path)?;
        let current = merge_missing_current_metrics(input.current, benchmark.as_ref());
        let baseline_map = baseline_map(input.baseline.as_deref());
        let current_map = current_map(&current);
        let generated_at = latest_metric_time(&current);
        let rows = MetricSignal::ALL
            .iter()
            .copied()
            .map(|signal| {
                build_row(
                    signal,
                    current_map.get(&signal).cloned().unwrap_or_else(|| {
                        missing_metric(signal, benchmark_source(signal), generated_at)
                    }),
                    baseline_map.get(&signal).cloned(),
                    &input.success_criteria,
                    benchmark.as_ref(),
                )
            })
            .collect::<Vec<_>>();
        Ok(finalize_report(
            input.scope,
            input.success_criteria,
            rows,
            input.benchmark_report_path,
            generated_at,
        ))
    }

    pub fn render_text(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(
            output,
            "Phase 9 regression report ({})",
            self.scope.kind_label()
        );
        let _ = writeln!(
            output,
            "pass={} fail={} missing={} n/a={}",
            self.pass_count, self.fail_count, self.missing_count, self.not_applicable_count
        );
        let _ = writeln!(
            output,
            "signal | current | baseline | delta | target | status"
        );
        for row in &self.rows {
            let _ = writeln!(
                output,
                "{} | {} | {} | {} | {} | {}",
                row.signal.as_str(),
                format_metric_value(&row.current),
                row.baseline
                    .as_ref()
                    .map(format_metric_value)
                    .unwrap_or_else(|| "n/a".to_string()),
                format_delta(&row.delta),
                row.target.clone().unwrap_or_else(|| "n/a".to_string()),
                status_label(&row.status),
            );
            if let Some(reason) = &row.evaluation_reason {
                let _ = writeln!(output, "  reason: {reason}");
            }
            if !row.evidence.is_empty() {
                let _ = writeln!(
                    output,
                    "  evidence: {}",
                    render_evidence_digest(&row.evidence)
                );
            }
        }
        output
    }

    pub fn render_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn render_ci_summary(&self) -> String {
        let failed = self
            .rows
            .iter()
            .filter(|row| row.status == ReportRowStatus::Fail)
            .map(|row| row.signal.as_str())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "scope={} pass={} fail={} missing={} not_applicable={} failed_signals={}",
            self.scope.kind_label(),
            self.pass_count,
            self.fail_count,
            self.missing_count,
            self.not_applicable_count,
            if failed.is_empty() { "none" } else { &failed }
        )
    }
}

pub fn merge_missing_current_metrics(
    current: Vec<MetricValue>,
    benchmark: Option<&BenchmarkEvidence>,
) -> Vec<MetricValue> {
    let Some(benchmark) = benchmark else {
        return current;
    };
    let benchmark_map = current_map(benchmark.metrics());
    MetricSignal::ALL
        .iter()
        .copied()
        .map(|signal| {
            let current_value = current
                .iter()
                .find(|value| value.signal == signal)
                .cloned()
                .or_else(|| benchmark_map.get(&signal).cloned());
            match current_value {
                Some(value) if value.value.is_some() => value,
                Some(value) => benchmark_map
                    .get(&signal)
                    .cloned()
                    .filter(|candidate| candidate.value.is_some())
                    .unwrap_or(value),
                None => benchmark_map.get(&signal).cloned().unwrap_or_else(|| {
                    missing_metric(signal, benchmark_source(signal), Utc::now())
                }),
            }
        })
        .collect()
}

pub fn report_exit_code(report: &RegressionReport, fail_on_regression: bool) -> i32 {
    if fail_on_regression && report.fail_count > 0 {
        1
    } else {
        0
    }
}

fn build_row(
    signal: MetricSignal,
    current: MetricValue,
    baseline: Option<MetricValue>,
    thresholds: &SuccessCriteriaThresholds,
    benchmark: Option<&BenchmarkEvidence>,
) -> RegressionReportRow {
    let delta = compute_delta(
        current.value,
        baseline.as_ref().and_then(|value| value.value),
    );
    let (target, status, evaluation_reason) =
        evaluate_signal(signal, &current, baseline.as_ref(), &delta, thresholds);
    let evidence = benchmark
        .map(|value| value.evidence_for(signal))
        .unwrap_or_default();
    RegressionReportRow {
        signal,
        current,
        baseline,
        delta,
        target,
        status,
        evaluation_reason,
        evidence,
    }
}

fn evaluate_signal(
    signal: MetricSignal,
    current: &MetricValue,
    baseline: Option<&MetricValue>,
    delta: &SignalDelta,
    thresholds: &SuccessCriteriaThresholds,
) -> (Option<String>, ReportRowStatus, Option<String>) {
    match signal {
        MetricSignal::ToolCallsPerSuccessfulTask => evaluate_reduction(
            current,
            baseline,
            delta,
            thresholds.discovery_tool_calls_reduction,
            ">=30% reduction from baseline",
        ),
        MetricSignal::IrrelevantFilesOpenedPerTask => evaluate_reduction(
            current,
            baseline,
            delta,
            thresholds.irrelevant_file_reads_reduction,
            ">=40% reduction from baseline",
        ),
        MetricSignal::MemoryInclusionPrecision => evaluate_minimum(
            current,
            thresholds.memory_inclusion_precision,
            ">=0.80 precision",
        ),
        MetricSignal::ContradictionMissedRate => evaluate_maximum(
            current,
            thresholds.contradiction_missed_rate,
            "=0 contradicted memories surfaced as trusted",
        ),
        MetricSignal::StaleMemorySurfacedRate => evaluate_maximum(
            current,
            thresholds.stale_memory_unlabeled_rate,
            "=0 stale memories surfaced without stale label",
        ),
        MetricSignal::TestsRecommendedVsNeeded => evaluate_minimum(
            current,
            thresholds.tests_recommended_vs_needed,
            ">=0.90 test recommendation recall",
        ),
        MetricSignal::RelevantAnchorRecall
        | MetricSignal::MemoryLaterUsedRate
        | MetricSignal::WorkflowSuccessAfterFirstPlan => (
            None,
            if current.value.is_some() {
                ReportRowStatus::NotApplicable
            } else {
                ReportRowStatus::MissingData
            },
            current.reason_if_null.clone().or_else(|| {
                Some("spec does not define an initial threshold for this signal".to_string())
            }),
        ),
    }
}

fn evaluate_reduction(
    current: &MetricValue,
    baseline: Option<&MetricValue>,
    delta: &SignalDelta,
    threshold: f64,
    label: &str,
) -> (Option<String>, ReportRowStatus, Option<String>) {
    let target = Some(format!("{label} ({:.0}%)", threshold * 100.0));
    match (
        current.value,
        baseline.and_then(|value| value.value),
        delta.reduction_ratio,
    ) {
        (None, _, _) => (
            target,
            ReportRowStatus::MissingData,
            current.reason_if_null.clone(),
        ),
        (Some(_), None, _) => (
            target,
            ReportRowStatus::MissingData,
            Some("baseline value is unavailable".to_string()),
        ),
        (Some(_), Some(_), Some(reduction)) => (
            target,
            if reduction >= threshold {
                ReportRowStatus::Pass
            } else {
                ReportRowStatus::Fail
            },
            Some(format!("observed reduction {:.2}%", reduction * 100.0)),
        ),
        _ => (
            target,
            ReportRowStatus::MissingData,
            Some("reduction could not be computed".to_string()),
        ),
    }
}

fn evaluate_minimum(
    current: &MetricValue,
    threshold: f64,
    label: &str,
) -> (Option<String>, ReportRowStatus, Option<String>) {
    let target = Some(format!("{label} ({threshold:.2})"));
    match current.value {
        Some(value) => (
            target,
            if value >= threshold {
                ReportRowStatus::Pass
            } else {
                ReportRowStatus::Fail
            },
            Some(format!("observed value {value:.4}")),
        ),
        None => (
            target,
            ReportRowStatus::MissingData,
            current.reason_if_null.clone(),
        ),
    }
}

fn evaluate_maximum(
    current: &MetricValue,
    threshold: f64,
    label: &str,
) -> (Option<String>, ReportRowStatus, Option<String>) {
    let target = Some(format!("{label} ({threshold:.2})"));
    match current.value {
        Some(value) => (
            target,
            if value <= threshold {
                ReportRowStatus::Pass
            } else {
                ReportRowStatus::Fail
            },
            Some(format!("observed value {value:.4}")),
        ),
        None => (
            target,
            ReportRowStatus::MissingData,
            current.reason_if_null.clone(),
        ),
    }
}

fn compute_delta(current: Option<f64>, baseline: Option<f64>) -> SignalDelta {
    match (current, baseline) {
        (Some(current), Some(baseline)) => SignalDelta {
            absolute: Some(current - baseline),
            reduction_ratio: if baseline.abs() > f64::EPSILON {
                Some((baseline - current) / baseline)
            } else {
                None
            },
            description: Some(format!("current={current:.4}, baseline={baseline:.4}")),
        },
        (Some(current), None) => SignalDelta {
            absolute: None,
            reduction_ratio: None,
            description: Some(format!("current only ({current:.4})")),
        },
        (None, Some(baseline)) => SignalDelta {
            absolute: None,
            reduction_ratio: None,
            description: Some(format!("baseline only ({baseline:.4})")),
        },
        (None, None) => SignalDelta {
            absolute: None,
            reduction_ratio: None,
            description: Some("neither current nor baseline is available".to_string()),
        },
    }
}

fn baseline_map(values: Option<&[MetricValue]>) -> HashMap<MetricSignal, MetricValue> {
    values
        .unwrap_or(&[])
        .iter()
        .map(|value| (value.signal, value.clone()))
        .collect()
}

fn current_map(values: &[MetricValue]) -> HashMap<MetricSignal, MetricValue> {
    values
        .iter()
        .map(|value| (value.signal, value.clone()))
        .collect()
}

fn latest_metric_time(values: &[MetricValue]) -> DateTime<Utc> {
    values
        .iter()
        .map(|value| value.computed_at)
        .max_by_key(|value| value.unix_seconds())
        .unwrap_or_else(Utc::now)
}

fn finalize_report(
    scope: MetricScope,
    success_criteria: SuccessCriteriaThresholds,
    rows: Vec<RegressionReportRow>,
    benchmark_report_path: PathBuf,
    generated_at: DateTime<Utc>,
) -> RegressionReport {
    let pass_count = rows
        .iter()
        .filter(|row| row.status == ReportRowStatus::Pass)
        .count();
    let fail_count = rows
        .iter()
        .filter(|row| row.status == ReportRowStatus::Fail)
        .count();
    let missing_count = rows
        .iter()
        .filter(|row| row.status == ReportRowStatus::MissingData)
        .count();
    let not_applicable_count = rows
        .iter()
        .filter(|row| row.status == ReportRowStatus::NotApplicable)
        .count();
    RegressionReport {
        scope,
        success_criteria,
        rows,
        benchmark_report_path,
        generated_at,
        pass_count,
        fail_count,
        missing_count,
        not_applicable_count,
    }
}

fn format_metric_value(value: &MetricValue) -> String {
    match value.value {
        Some(number) => format!("{number:.4}"),
        None => {
            let reason = value
                .reason_if_null
                .clone()
                .unwrap_or_else(|| "missing".to_string());
            format!("n/a ({reason})")
        }
    }
}

fn format_delta(delta: &SignalDelta) -> String {
    match (delta.absolute, delta.reduction_ratio) {
        (Some(absolute), Some(reduction_ratio)) => {
            format!("{absolute:.4} ({:.2}% reduction)", reduction_ratio * 100.0)
        }
        (Some(absolute), None) => format!("{absolute:.4}"),
        _ => delta
            .description
            .clone()
            .unwrap_or_else(|| "n/a".to_string()),
    }
}

fn render_evidence_digest(evidence: &[SignalEvidencePointer]) -> String {
    evidence
        .iter()
        .map(|pointer| {
            let mut parts = Vec::new();
            if !pointer.task_ids.is_empty() {
                parts.push(format!("tasks={}", pointer.task_ids.join(",")));
            }
            if !pointer.event_ids.is_empty() {
                parts.push(format!(
                    "events={}",
                    pointer
                        .event_ids
                        .iter()
                        .map(|id| id.ulid.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            if !pointer.memory_ids.is_empty() {
                parts.push(format!(
                    "memories={}",
                    pointer
                        .memory_ids
                        .iter()
                        .map(|id| id.ulid.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            parts.push(pointer.note.clone());
            parts.join(" ")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn status_label(status: &ReportRowStatus) -> &'static str {
    match status {
        ReportRowStatus::Pass => "pass",
        ReportRowStatus::Fail => "fail",
        ReportRowStatus::MissingData => "missing",
        ReportRowStatus::NotApplicable => "n/a",
    }
}

fn missing_metric(
    signal: MetricSignal,
    source: MetricSource,
    computed_at: DateTime<Utc>,
) -> MetricValue {
    MetricValue {
        signal,
        value: None,
        denominator: None,
        sample_count: 0,
        source,
        computed_at,
        incomplete: false,
        reason_if_null: Some("signal was not collected".to_string()),
    }
}

pub(crate) fn benchmark_source(signal: MetricSignal) -> MetricSource {
    match signal {
        MetricSignal::ToolCallsPerSuccessfulTask
        | MetricSignal::IrrelevantFilesOpenedPerTask
        | MetricSignal::RelevantAnchorRecall => MetricSource::EventLog,
        MetricSignal::MemoryInclusionPrecision | MetricSignal::MemoryLaterUsedRate => {
            MetricSource::MemoryStore
        }
        MetricSignal::StaleMemorySurfacedRate | MetricSignal::ContradictionMissedRate => {
            MetricSource::Verifier
        }
        MetricSignal::TestsRecommendedVsNeeded | MetricSignal::WorkflowSuccessAfterFirstPlan => {
            MetricSource::WorkflowOutcome
        }
    }
}

trait ScopeKindLabel {
    fn kind_label(&self) -> &'static str;
}

impl ScopeKindLabel for MetricScope {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            crate::metrics::MetricScopeKind::Session => "session",
            crate::metrics::MetricScopeKind::Branch => "branch",
            crate::metrics::MetricScopeKind::Repo => "repo",
            crate::metrics::MetricScopeKind::User => "user",
            crate::metrics::MetricScopeKind::Organization => "organization",
        }
    }
}
