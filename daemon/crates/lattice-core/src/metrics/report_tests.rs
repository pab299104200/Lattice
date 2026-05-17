use std::fs;
use std::path::PathBuf;

use super::report::{
    merge_missing_current_metrics, report_exit_code, BenchmarkEvidence, RegressionReport,
    ReportInput, ReportRowStatus, SuccessCriteriaThresholds,
};
use crate::metrics::{MetricScope, MetricSignal, MetricSource, MetricValue};
use crate::{DateTime, Utc};

#[test]
fn report_renders_deterministically_in_all_formats() {
    let report = RegressionReport::build(report_input()).expect("report builds");
    let text = report.render_text();
    let json = report.render_json().expect("json renders");
    let ci = report.render_ci_summary();

    assert!(text.contains("tool_calls_per_successful_task"));
    assert!(text.contains(">=30% reduction from baseline"));
    assert!(json.contains("\"rows\""));
    assert!(json.contains("\"tool_calls_per_successful_task\""));
    assert!(ci.contains("scope=session"));
}

#[test]
fn report_computes_baseline_delta_and_reduction() {
    let report = RegressionReport::build(report_input()).expect("report builds");
    let row = row(&report, MetricSignal::ToolCallsPerSuccessfulTask);
    assert_eq!(row.delta.absolute, Some(-2.0));
    assert_eq!(row.delta.reduction_ratio, Some(0.4));
    assert_eq!(row.status, ReportRowStatus::Pass);
}

#[test]
fn threshold_logic_covers_each_initial_target() {
    let report = RegressionReport::build(report_input()).expect("report builds");
    assert_eq!(
        row(&report, MetricSignal::ToolCallsPerSuccessfulTask).status,
        ReportRowStatus::Pass
    );
    assert_eq!(
        row(&report, MetricSignal::IrrelevantFilesOpenedPerTask).status,
        ReportRowStatus::Pass
    );
    assert_eq!(
        row(&report, MetricSignal::MemoryInclusionPrecision).status,
        ReportRowStatus::Pass
    );
    assert_eq!(
        row(&report, MetricSignal::ContradictionMissedRate).status,
        ReportRowStatus::Pass
    );
    assert_eq!(
        row(&report, MetricSignal::StaleMemorySurfacedRate).status,
        ReportRowStatus::Pass
    );
    assert_eq!(
        row(&report, MetricSignal::TestsRecommendedVsNeeded).status,
        ReportRowStatus::Pass
    );
}

#[test]
fn null_signals_render_as_na_with_reason_never_zero() {
    let report = RegressionReport::build(report_input()).expect("report builds");
    let row = row(&report, MetricSignal::RelevantAnchorRecall);
    assert_eq!(row.current.value, None);
    assert_eq!(row.status, ReportRowStatus::MissingData);
    let text = report.render_text();
    assert!(text.contains("n/a (no benchmark or live evidence)"));
}

#[test]
fn fail_on_regression_exit_code_is_non_zero_only_when_requested() {
    let mut report = RegressionReport::build(report_input()).expect("report builds");
    let tool_row = report
        .rows
        .iter_mut()
        .find(|entry| entry.signal == MetricSignal::ToolCallsPerSuccessfulTask)
        .expect("tool calls row");
    tool_row.status = ReportRowStatus::Fail;
    report.fail_count = 1;

    assert_eq!(report_exit_code(&report, false), 0);
    assert_eq!(report_exit_code(&report, true), 1);
}

#[test]
fn benchmark_evidence_round_trips_to_signal_rows() {
    let path = write_benchmark_fixture();
    let evidence = BenchmarkEvidence::from_path(&path)
        .expect("benchmark parses")
        .expect("benchmark exists");
    let merged = merge_missing_current_metrics(Vec::new(), Some(&evidence));
    let report = RegressionReport::build(ReportInput {
        current: merged,
        baseline: Some(baseline_metrics()),
        benchmark_report_path: path.clone(),
        scope: MetricScope::session("session-report"),
        success_criteria: SuccessCriteriaThresholds::initial(),
    })
    .expect("report builds");

    let row = row(&report, MetricSignal::TestsRecommendedVsNeeded);
    assert!(!row.evidence.is_empty());
    assert_eq!(row.evidence[0].task_ids, vec!["task-alpha".to_string()]);
    assert_eq!(row.evidence[0].benchmark_report_path.as_ref(), Some(&path));
}

fn report_input() -> ReportInput {
    ReportInput {
        current: vec![
            metric(
                MetricSignal::ToolCallsPerSuccessfulTask,
                Some(3.0),
                MetricSource::EventLog,
            ),
            metric(
                MetricSignal::IrrelevantFilesOpenedPerTask,
                Some(0.5),
                MetricSource::EventLog,
            ),
            metric_null(
                MetricSignal::RelevantAnchorRecall,
                MetricSource::EventLog,
                "no benchmark or live evidence",
            ),
            metric(
                MetricSignal::MemoryInclusionPrecision,
                Some(0.85),
                MetricSource::MemoryStore,
            ),
            metric(
                MetricSignal::MemoryLaterUsedRate,
                Some(0.60),
                MetricSource::MemoryStore,
            ),
            metric(
                MetricSignal::StaleMemorySurfacedRate,
                Some(0.0),
                MetricSource::Verifier,
            ),
            metric(
                MetricSignal::ContradictionMissedRate,
                Some(0.0),
                MetricSource::Verifier,
            ),
            metric(
                MetricSignal::TestsRecommendedVsNeeded,
                Some(0.92),
                MetricSource::WorkflowOutcome,
            ),
            metric(
                MetricSignal::WorkflowSuccessAfterFirstPlan,
                Some(0.95),
                MetricSource::WorkflowOutcome,
            ),
        ],
        baseline: Some(baseline_metrics()),
        benchmark_report_path: PathBuf::new(),
        scope: MetricScope::session("session-report"),
        success_criteria: SuccessCriteriaThresholds::initial(),
    }
}

fn baseline_metrics() -> Vec<MetricValue> {
    vec![
        metric(
            MetricSignal::ToolCallsPerSuccessfulTask,
            Some(5.0),
            MetricSource::EventLog,
        ),
        metric(
            MetricSignal::IrrelevantFilesOpenedPerTask,
            Some(1.0),
            MetricSource::EventLog,
        ),
    ]
}

fn metric(signal: MetricSignal, value: Option<f64>, source: MetricSource) -> MetricValue {
    MetricValue {
        signal,
        value,
        denominator: Some(1),
        sample_count: 1,
        source,
        computed_at: fixed_time(),
        incomplete: false,
        reason_if_null: None,
    }
}

fn metric_null(signal: MetricSignal, source: MetricSource, reason: &str) -> MetricValue {
    MetricValue {
        signal,
        value: None,
        denominator: None,
        sample_count: 0,
        source,
        computed_at: fixed_time(),
        incomplete: false,
        reason_if_null: Some(reason.to_string()),
    }
}

fn row(report: &RegressionReport, signal: MetricSignal) -> &super::report::RegressionReportRow {
    report
        .rows
        .iter()
        .find(|entry| entry.signal == signal)
        .expect("signal row")
}

fn fixed_time() -> DateTime<Utc> {
    DateTime::from_unix_seconds(1_779_100_000)
}

fn write_benchmark_fixture() -> PathBuf {
    let path = std::env::temp_dir().join("lattice-report-benchmark.json");
    let body = r#"
[
  {
    "fixture": "fixture-a",
    "task_reports": [
      {
        "task_id": "task-alpha",
        "tool_reports": [
          {"tool": "get_context_capsule"},
          {"tool": "prepare_change"},
          {"tool": "find_relevant_tests"}
        ],
        "returned_anchors": ["src/auth.rs::login", "tests/auth.rs"],
        "expected_anchors": ["src/auth.rs::login", "src/auth.rs::refresh"],
        "expected_tests": ["tests/auth.rs"],
        "recommended_tests": ["tests/auth.rs"]
      }
    ],
    "metrics": [
      {
        "signal": "tool_calls_per_successful_task",
        "value": 3.0,
        "denominator": 1,
        "sample_count": 1,
        "source": "event_log",
        "computed_at": "2026-05-17T06:40:00Z",
        "incomplete": false
      }
    ]
  }
]
"#;
    fs::write(&path, body).expect("benchmark fixture writes");
    path
}
