//! Canonical Phase 9 metrics collection for retrieval, memory, and workflow evaluation.
//!
//! This module implements the plan contract from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 9: Metrics And Evaluation`,
//! `## Event Log`,
//! `## Memory Graph`, and
//! `## Verification Engine`.

pub mod health_backtest;
pub mod report;
mod report_benchmark;
pub mod signals;

#[cfg(test)]
mod health_backtest_tests;
#[cfg(test)]
mod regression_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod signals_tests;

pub use report::{
    merge_missing_current_metrics, report_exit_code, BenchmarkEvidence, RegressionReport,
    RegressionReportRow, ReportError, ReportInput, ReportRowStatus, SignalDelta,
    SignalEvidencePointer, SuccessCriteriaThresholds,
};
pub use signals::{
    AnchorRecallSample, MemorySurfaceRecord, MetricSampleScope, MetricScope, MetricScopeKind,
    MetricSignal, MetricSource, MetricTimeRange, MetricValue, MetricsCollector,
    TestRecommendationSample,
};
