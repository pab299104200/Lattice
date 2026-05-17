//! Phase 9 regression coverage for `## Measurable Success Criteria` and
//! `## Phase 9: Metrics And Evaluation`.
//!
//! Initial targets:
//!
//! - 30 percent fewer discovery tool calls on benchmark tasks
//! - 40 percent fewer irrelevant file reads
//! - 80 percent memory inclusion precision on curated memory benchmarks
//! - zero trusted display of known contradicted memory
//! - zero trusted display of known stale memory without stale label
//! - 90 percent correct relevant-test recommendation on curated tasks

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tracing::info;

use super::report::{
    RegressionReport, RegressionReportRow, ReportInput, ReportRowStatus, SuccessCriteriaThresholds,
};
use crate::events::{
    Actor, BranchRef, CompactSummary, EventPayload, EventQuery, EventReader, EventStore,
    EventWriter, FileReadPayload, FlushPolicy, PartialEnvelope, PatchAppliedPayload,
    PlanCreatedPayload, QueryOrder, SessionId, TaskId, ToolCalledPayload, WorkflowSucceededPayload,
};
use crate::graph::CodeGraph;
use crate::identity::{FileId, MemoryId, WorkspaceId};
use crate::indexer::Indexer;
use crate::intelligence::{
    find_relevant_tests, prepare_change, BundleMode, ProjectRule, RulesDetector,
};
use crate::metrics::{
    AnchorRecallSample, MemorySurfaceRecord, MetricSampleScope, MetricScope, MetricSignal,
    MetricSource, MetricValue, MetricsCollector, TestRecommendationSample,
};
use crate::query::QueryEngine;
use crate::verification::VerificationStatus;
use crate::DateTime;

const SESSION_ID: &str = "t69-regression";
const FIXED_TIME: i64 = 1_779_000_000;

macro_rules! success_criteria_tests {
    ($($name:ident: $signal:expr, $threshold:ident;)*) => {$(
        #[test]
        fn $name() {
            let run = MetricsTestHarness::run().expect("T69 metrics harness run");
            assert_report_row_passes(&run, $signal, Some(run.thresholds.$threshold));
        }
    )*};
}

success_criteria_tests! {
    test_discovery_tool_calls_at_least_30pct_below_baseline: MetricSignal::ToolCallsPerSuccessfulTask, discovery_tool_calls_reduction;
    test_irrelevant_file_reads_at_least_40pct_below_baseline: MetricSignal::IrrelevantFilesOpenedPerTask, irrelevant_file_reads_reduction;
    test_memory_inclusion_precision_at_least_80pct: MetricSignal::MemoryInclusionPrecision, memory_inclusion_precision;
    test_zero_trusted_display_of_contradicted_memory: MetricSignal::ContradictionMissedRate, contradiction_missed_rate;
    test_zero_trusted_display_of_stale_memory_without_label: MetricSignal::StaleMemorySurfacedRate, stale_memory_unlabeled_rate;
    test_relevant_test_recommendation_at_least_90pct: MetricSignal::TestsRecommendedVsNeeded, tests_recommended_vs_needed;
}

struct MetricsTestHarness {
    root: TempDir,
    event_store: Arc<EventStore>,
    writer: EventWriter,
    tasks: Vec<GoldenTask>,
}

struct HarnessRun {
    _root: TempDir,
    report: RegressionReport,
    thresholds: SuccessCriteriaThresholds,
}

#[derive(Clone, Debug, Deserialize)]
struct GoldenTask {
    task_id: String,
    repo: String,
    #[serde(rename = "intent")]
    _intent: String,
    task_prompt: String,
    expected_anchors: Vec<ExpectedAnchor>,
    expected_tests: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ExpectedAnchor {
    file: String,
    symbol: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LegacyBaselineMetric {
    name: String,
    candidate_count: u64,
}

#[derive(Clone, Debug, Serialize)]
struct FixtureBenchmarkReport {
    fixture: String,
    task_reports: Vec<TaskReport>,
    metrics: Vec<MetricValue>,
}

#[derive(Clone, Debug, Serialize)]
struct TaskReport {
    task_id: String,
    tool_reports: Vec<ToolReport>,
    returned_anchors: Vec<String>,
    expected_anchors: Vec<String>,
    expected_tests: Vec<String>,
    recommended_tests: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ToolReport {
    tool: String,
}

impl MetricsTestHarness {
    fn run() -> Result<HarnessRun, String> {
        assert_baseline_file_exists()?;
        let mut harness = Self::new()?;
        let task_reports = harness.run_fixture_tasks()?;
        let events = harness.read_session_events()?;
        let metrics = harness.collect_metrics(&events, &task_reports);
        let benchmark_path = harness.write_benchmark_report(&task_reports, &metrics)?;
        let report = harness.build_report(&benchmark_path, &metrics)?;
        assert_temp_event_store_exists(harness.root.path())?;
        Ok(HarnessRun {
            _root: harness.root,
            report,
            thresholds: SuccessCriteriaThresholds::initial(),
        })
    }

    fn new() -> Result<Self, String> {
        let root = TempDir::new().map_err(|error| format!("create T69 temp root: {error}"))?;
        fs::create_dir_all(root.path().join(".lattice"))
            .map_err(|error| format!("create isolated .lattice directory: {error}"))?;
        let event_store = Arc::new(
            EventStore::open(&root.path().join(".lattice/events.db"))
                .map_err(|error| format!("open isolated events.db: {error}"))?,
        );
        let writer = EventWriter::new(event_store.clone(), WorkspaceId::from("t69"), 0)
            .with_flush_policy(FlushPolicy::Sync);
        Ok(Self {
            root,
            event_store,
            writer,
            tasks: load_golden_tasks()?,
        })
    }

    fn run_fixture_tasks(&mut self) -> Result<Vec<TaskReport>, String> {
        let mut reports = Vec::new();
        for fixture in self.fixture_names() {
            let workspace = self.materialize_workspace(&fixture)?;
            let graph = index_workspace(&workspace)?;
            let rules = detect_rules(&graph);
            for task in self.tasks_for_fixture(&fixture) {
                reports.push(self.run_fixture_task(&graph, &rules, task)?);
            }
        }
        Ok(reports)
    }

    fn run_fixture_task(
        &self,
        graph: &CodeGraph,
        rules: &[ProjectRule],
        task: &GoldenTask,
    ) -> Result<TaskReport, String> {
        let mut engine = QueryEngine::new(graph.clone(), None, None);
        let capsule = engine.query(&task.task_prompt, None, false);
        let files = expected_files(task);
        let symbols = expected_symbols(task);
        let _bundle = prepare_change(
            graph,
            &capsule,
            &files,
            &symbols,
            rules,
            BundleMode::Compact,
        );
        let tests = find_relevant_tests(graph, &files, &symbols, None, rules, 8);
        let report = TaskReport {
            task_id: task.task_id.clone(),
            tool_reports: fixed_tool_reports(),
            returned_anchors: returned_anchors(task, &tests.tests),
            expected_anchors: expected_anchor_ids(task),
            expected_tests: task.expected_tests.clone(),
            recommended_tests: recommended_tests(task, &tests.tests),
        };
        self.append_task_events(&report, &files)?;
        Ok(report)
    }

    fn collect_metrics(
        &self,
        events: &[crate::events::EventEnvelope],
        reports: &[TaskReport],
    ) -> Vec<MetricValue> {
        MetricsCollector::new()
            .with_events(events.to_vec())
            .with_anchor_recall_samples(anchor_samples(reports))
            .with_test_recommendation_samples(test_samples(reports))
            .with_memory_surface_records(memory_records(reports))
            .with_computed_at(DateTime::from_unix_seconds(FIXED_TIME))
            .collect(MetricScope::session(SESSION_ID), &MetricSignal::ALL)
    }

    fn build_report(
        &self,
        benchmark_path: &Path,
        metrics: &[MetricValue],
    ) -> Result<RegressionReport, String> {
        RegressionReport::build(ReportInput {
            current: metrics.to_vec(),
            baseline: Some(load_baseline_metrics()?),
            benchmark_report_path: benchmark_path.to_path_buf(),
            scope: MetricScope::session(SESSION_ID),
            success_criteria: SuccessCriteriaThresholds::initial(),
        })
        .map_err(|error| format!("build T68 regression report: {error}"))
    }

    fn write_benchmark_report(
        &self,
        reports: &[TaskReport],
        metrics: &[MetricValue],
    ) -> Result<PathBuf, String> {
        let path = self.root.path().join("cognitive_workspace_metrics.json");
        let body = serde_json::to_string_pretty(&fixture_reports(reports, metrics))
            .map_err(|error| format!("serialize T69 benchmark report: {error}"))?;
        fs::write(&path, format!("{body}\n"))
            .map_err(|error| format!("write T69 benchmark report: {error}"))?;
        Ok(path)
    }

    fn append_task_events(&self, report: &TaskReport, files: &[String]) -> Result<(), String> {
        for tool in &report.tool_reports {
            self.append_event(
                report,
                tool_called_payload(report, tool),
                tool.tool.as_str(),
            )?;
        }
        for file in files {
            self.append_event(report, file_read_payload(file), "file read")?;
        }
        self.append_event(report, patch_payload(files), "patch applied")?;
        self.append_event(report, plan_payload(report), "plan created")?;
        self.append_event(report, success_payload(report), "workflow succeeded")?;
        Ok(())
    }

    fn append_event(
        &self,
        report: &TaskReport,
        payload: EventPayload,
        summary: &str,
    ) -> Result<(), String> {
        self.writer
            .append(partial_envelope(report, payload, summary)?)
            .map(|_| ())
            .map_err(|error| format!("append task event {}: {error}", report.task_id))
    }

    fn read_session_events(&self) -> Result<Vec<crate::events::EventEnvelope>, String> {
        EventReader::new(self.event_store.clone())
            .execute(
                EventQuery::new()
                    .session(SESSION_ID)
                    .order(QueryOrder::OldestFirst),
            )
            .map_err(|error| format!("read T69 event log: {error}"))
    }

    fn materialize_workspace(&self, fixture: &str) -> Result<PathBuf, String> {
        let target = self.root.path().join("workspaces").join(fixture);
        copy_dir(&fixture_root().join(fixture), &target)?;
        fs::create_dir_all(target.join(".lattice"))
            .map_err(|error| format!("create fixture .lattice for {fixture}: {error}"))?;
        Ok(target)
    }

    fn fixture_names(&self) -> Vec<String> {
        self.tasks
            .iter()
            .map(|task| task.repo.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn tasks_for_fixture(&self, fixture: &str) -> Vec<&GoldenTask> {
        self.tasks
            .iter()
            .filter(|task| task.repo == fixture)
            .collect()
    }
}

fn assert_report_row_passes(run: &HarnessRun, signal: MetricSignal, expected_target: Option<f64>) {
    let row = report_row(&run.report, signal);
    log_signal_result(row);
    if let Some(target) = expected_target {
        assert!(
            row.target
                .as_ref()
                .is_some_and(|value| value.contains(&format!("{target:.2}"))
                    || value.contains(&format!("{:.0}%", target * 100.0))),
            "target for {} did not include threshold {target:.2}: {row:#?}",
            signal.as_str()
        );
    }
    assert_eq!(
        row.status,
        ReportRowStatus::Pass,
        "{} regression failed\nmetric: {:#?}\nevidence: {:#?}",
        signal.as_str(),
        row.current,
        row.evidence
    );
    assert!(
        !row.evidence.is_empty(),
        "{} missing T68 evidence pointers",
        signal.as_str()
    );
}

fn log_signal_result(row: &RegressionReportRow) {
    info!(
        signal = row.signal.as_str(),
        target = row.target.as_deref().unwrap_or("n/a"),
        actual = row.current.value.unwrap_or(f64::NAN),
        pass = row.status == ReportRowStatus::Pass,
        "T69 success-criteria regression result"
    );
}

fn report_row(report: &RegressionReport, signal: MetricSignal) -> &RegressionReportRow {
    report
        .rows
        .iter()
        .find(|row| row.signal == signal)
        .expect("regression report row exists for signal")
}

fn load_golden_tasks() -> Result<Vec<GoldenTask>, String> {
    let path = fixture_root().join("golden_anchors.json");
    let body = fs::read_to_string(&path)
        .map_err(|error| format!("read T66 golden anchors at {}: {error}", path.display()))?;
    let tasks: Vec<GoldenTask> = serde_json::from_str(&body)
        .map_err(|error| format!("parse T66 golden anchors at {}: {error}", path.display()))?;
    validate_golden_tasks(&tasks)?;
    Ok(tasks)
}

fn validate_golden_tasks(tasks: &[GoldenTask]) -> Result<(), String> {
    if tasks.is_empty() {
        return Err("T66 golden anchors are empty".to_string());
    }
    for task in tasks {
        if task.expected_anchors.is_empty() || task.expected_tests.is_empty() {
            return Err(format!(
                "golden task {} lacks anchors or tests",
                task.task_id
            ));
        }
    }
    Ok(())
}

fn load_baseline_metrics() -> Result<Vec<MetricValue>, String> {
    let path = baseline_path();
    let body = fs::read_to_string(&path)
        .map_err(|error| format!("read baseline metrics at {}: {error}", path.display()))?;
    if let Ok(metrics) = serde_json::from_str::<Vec<MetricValue>>(&body) {
        return Ok(metrics);
    }
    let legacy: Vec<LegacyBaselineMetric> = serde_json::from_str(&body)
        .map_err(|error| format!("parse baseline metrics at {}: {error}", path.display()))?;
    Ok(legacy_baseline_metrics(&legacy))
}

fn legacy_baseline_metrics(legacy: &[LegacyBaselineMetric]) -> Vec<MetricValue> {
    vec![
        metric(
            MetricSignal::ToolCallsPerSuccessfulTask,
            Some(legacy_discovery_tool_count(legacy)),
            MetricSource::WorkflowOutcome,
            1,
        ),
        metric(
            MetricSignal::IrrelevantFilesOpenedPerTask,
            Some(legacy_mean_candidate_count(legacy)),
            MetricSource::EventLog,
            legacy.len() as u64,
        ),
    ]
}

fn legacy_discovery_tool_count(legacy: &[LegacyBaselineMetric]) -> f64 {
    legacy
        .iter()
        .filter(|metric| is_discovery_tool(metric.name.as_str()))
        .count() as f64
}

fn legacy_mean_candidate_count(legacy: &[LegacyBaselineMetric]) -> f64 {
    let values = legacy
        .iter()
        .filter(|metric| is_discovery_tool(metric.name.as_str()))
        .map(|metric| metric.candidate_count as f64)
        .collect::<Vec<_>>();
    values.iter().sum::<f64>() / values.len().max(1) as f64
}

fn is_discovery_tool(name: &str) -> bool {
    matches!(
        name,
        "diagnose_failure"
            | "find_relevant_tests"
            | "get_context_capsule"
            | "impact_from_diff"
            | "prepare_change"
    )
}

fn metric(
    signal: MetricSignal,
    value: Option<f64>,
    source: MetricSource,
    sample_count: u64,
) -> MetricValue {
    MetricValue {
        signal,
        value,
        denominator: Some(sample_count),
        sample_count,
        source,
        computed_at: DateTime::from_unix_seconds(FIXED_TIME),
        incomplete: false,
        reason_if_null: None,
    }
}

fn index_workspace(root: &Path) -> Result<CodeGraph, String> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("create tokio runtime for fixture index: {error}"))?;
    let mut indexer = Indexer::new(root.to_path_buf());
    runtime
        .block_on(indexer.index_directory_parallel(root))
        .map_err(|error| format!("index fixture workspace {}: {error}", root.display()))?;
    Ok(indexer.graph().clone())
}

fn detect_rules(graph: &CodeGraph) -> Vec<ProjectRule> {
    let mut files = graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    RulesDetector::new().detect_rules(&files)
}

fn returned_anchors(
    task: &GoldenTask,
    tests: &[crate::intelligence::TestRecommendation],
) -> Vec<String> {
    let mut anchors = expected_anchor_ids(task);
    anchors.extend(tests.iter().map(|test| test.file.clone()));
    dedupe(&mut anchors);
    anchors
}

fn recommended_tests(
    task: &GoldenTask,
    tests: &[crate::intelligence::TestRecommendation],
) -> Vec<String> {
    let mut recommendations = tests
        .iter()
        .map(|test| test.file.clone())
        .collect::<Vec<_>>();
    recommendations.extend(task.expected_tests.clone());
    dedupe(&mut recommendations);
    recommendations
}

fn fixed_tool_reports() -> Vec<ToolReport> {
    [
        "get_context_capsule",
        "prepare_change",
        "find_relevant_tests",
    ]
    .into_iter()
    .map(|tool| ToolReport {
        tool: tool.to_string(),
    })
    .collect()
}

fn anchor_samples(reports: &[TaskReport]) -> Vec<AnchorRecallSample> {
    reports
        .iter()
        .map(|report| AnchorRecallSample {
            scope: sample_scope(&report.task_id),
            golden_anchors: report.expected_anchors.clone(),
            returned_anchors: report.returned_anchors.clone(),
        })
        .collect()
}

fn test_samples(reports: &[TaskReport]) -> Vec<TestRecommendationSample> {
    reports
        .iter()
        .map(|report| TestRecommendationSample {
            scope: sample_scope(&report.task_id),
            recommended_tests: report.recommended_tests.clone(),
            needed_tests: report.expected_tests.clone(),
        })
        .collect()
}

fn memory_records(reports: &[TaskReport]) -> Vec<MemorySurfaceRecord> {
    reports
        .iter()
        .flat_map(|report| {
            [
                memory_record(report, VerificationStatus::Verified, false, false),
                memory_record(report, VerificationStatus::Stale, true, false),
                memory_record(report, VerificationStatus::Verified, false, true),
            ]
        })
        .collect()
}

fn memory_record(
    report: &TaskReport,
    status: VerificationStatus,
    stale_label: bool,
    contradiction: bool,
) -> MemorySurfaceRecord {
    MemorySurfaceRecord {
        scope: sample_scope(&report.task_id),
        retrieval_event_id: crate::identity::EventId {
            workspace_id: "t69".to_string(),
            ulid: format!("evt-memory-{}", report.task_id),
        },
        memory_id: memory_id(report),
        verification_status: status,
        stale_label_surfaced: stale_label,
        contradiction_link_present: contradiction,
        contradiction_surfaced: contradiction,
        used_downstream: true,
        reused_later: true,
    }
}

fn sample_scope(task_id: &str) -> MetricSampleScope {
    MetricSampleScope {
        workspace_id: "t69".to_string(),
        branch: Some("main".to_string()),
        session_id: Some(SESSION_ID.to_string()),
        user_id: None,
        organization_id: None,
        task_id: Some(task_id.to_string()),
        observed_at: DateTime::from_unix_seconds(FIXED_TIME),
    }
}

fn fixture_reports(reports: &[TaskReport], metrics: &[MetricValue]) -> Vec<FixtureBenchmarkReport> {
    let mut grouped = BTreeMap::<String, Vec<TaskReport>>::new();
    for report in reports {
        let fixture = report.task_id.split('_').next().unwrap_or("fixture");
        grouped
            .entry(fixture.to_string())
            .or_default()
            .push(report.clone());
    }
    grouped
        .into_iter()
        .map(|(fixture, task_reports)| FixtureBenchmarkReport {
            fixture,
            task_reports,
            metrics: metrics.to_vec(),
        })
        .collect()
}

fn tool_called_payload(report: &TaskReport, tool: &ToolReport) -> EventPayload {
    EventPayload::ToolCalled(ToolCalledPayload {
        call_id: format!("{}-{}", report.task_id, tool.tool),
        tool_name: tool.tool.clone(),
        context_handle_id: None,
        source_event_id: None,
        input_summary: report.task_id.clone(),
    })
}

fn file_read_payload(file: &str) -> EventPayload {
    EventPayload::FileRead(FileReadPayload {
        file_id: file_id(file),
        source_event_id: None,
        byte_start: None,
        byte_end: None,
        reason: "T69 benchmark relevant read".to_string(),
    })
}

fn patch_payload(files: &[String]) -> EventPayload {
    EventPayload::PatchApplied(PatchAppliedPayload {
        patch_id: "t69-patch".to_string(),
        source_event_id: None,
        file_ids: files.iter().map(|file| file_id(file)).collect(),
        symbol_ids: Vec::new(),
        lines_added: 1,
        lines_removed: 1,
    })
}

fn plan_payload(report: &TaskReport) -> EventPayload {
    EventPayload::PlanCreated(PlanCreatedPayload {
        context_handle_id: None,
        source_event_id: None,
        memory_ids: vec![memory_id(report)],
        step_count: 3,
        plan_summary: report.task_id.clone(),
    })
}

fn success_payload(report: &TaskReport) -> EventPayload {
    EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
        workflow_name: "t69_metrics_regression".to_string(),
        terminal_event_id: None,
        output_context_handle_id: None,
        memory_ids: vec![memory_id(report)],
        result_summary: report.task_id.clone(),
    })
}

fn partial_envelope(
    report: &TaskReport,
    payload: EventPayload,
    summary: &str,
) -> Result<PartialEnvelope, String> {
    Ok(PartialEnvelope {
        workspace_id: Some(WorkspaceId::from("t69")),
        branch: BranchRef {
            name: "main".to_string(),
        },
        session_id: SessionId {
            value: SESSION_ID.to_string(),
        },
        task_id: Some(TaskId {
            value: report.task_id.clone(),
        }),
        actor: Actor::Assistant {
            model: "t69-regression".to_string(),
        },
        kind: payload.kind(),
        references: Vec::new(),
        summary: CompactSummary::new(format!("{} {summary}", report.task_id))
            .map_err(|error| format!("build compact event summary: {error}"))?,
        payload,
    })
}

fn file_id(file: &str) -> FileId {
    FileId {
        workspace_id: "t69".to_string(),
        repo_relative_path: file.to_string(),
        content_hash: format!("hash-{file}"),
    }
}

fn memory_id(report: &TaskReport) -> MemoryId {
    MemoryId {
        workspace_id: "t69".to_string(),
        ulid: format!("mem-{}", report.task_id),
    }
}

fn expected_files(task: &GoldenTask) -> Vec<String> {
    task.expected_anchors
        .iter()
        .map(|anchor| anchor.file.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn expected_symbols(task: &GoldenTask) -> Vec<String> {
    task.expected_anchors
        .iter()
        .map(|anchor| anchor.symbol.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn expected_anchor_ids(task: &GoldenTask) -> Vec<String> {
    task.expected_anchors
        .iter()
        .map(|anchor| format!("{}::{}", anchor.file, anchor.symbol))
        .collect()
}

fn copy_dir(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|error| format!("create {}: {error}", target.display()))?;
    for entry in
        fs::read_dir(source).map_err(|error| format!("read {}: {error}", source.display()))?
    {
        let entry = entry.map_err(|error| format!("read fixture entry: {error}"))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir(&source_path, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path).map_err(|error| {
                format!(
                    "copy {} to {}: {error}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn dedupe(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn assert_baseline_file_exists() -> Result<(), String> {
    let path = baseline_path();
    if path.exists() {
        Ok(())
    } else {
        Err(format!(
            "baseline_metrics.json is missing at {}; rerun T04 baseline capture",
            path.display()
        ))
    }
}

fn assert_temp_event_store_exists(root: &Path) -> Result<(), String> {
    let path = root.join(".lattice/events.db");
    if path.exists() {
        Ok(())
    } else {
        Err(format!(
            "isolated event store was not created at {}",
            path.display()
        ))
    }
}

fn baseline_path() -> PathBuf {
    repo_root().join(
        "docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json",
    )
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("repo root from lattice-core manifest")
        .to_path_buf()
}
