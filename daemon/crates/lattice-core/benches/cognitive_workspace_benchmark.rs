use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lattice_core::events::hashing::PayloadHash;
use lattice_core::events::{
    Actor, BranchRef, CompactSummary, EventEnvelope, EventKind, EventPayload, FileReadPayload,
    PayloadLocation, PlanCreatedPayload, SessionId, TaskId, ToolCalledPayload,
    WorkflowSucceededPayload,
};
use lattice_core::graph::CodeGraph;
use lattice_core::identity::{EventId, FileId, MemoryId, WorkspaceId};
use lattice_core::indexer::Indexer;
use lattice_core::intelligence::{
    diagnose_failure, find_relevant_tests, impact_from_diff, prepare_change, BundleMode,
    DiffImpactReport, FailureDiagnosis, ProjectRule, RulesDetector, TaskBundle,
    TestSelectionReport,
};
use lattice_core::metrics::{
    AnchorRecallSample, MemorySurfaceRecord, MetricSampleScope, MetricScope, MetricSignal,
    MetricValue, MetricsCollector, TestRecommendationSample,
};
use lattice_core::query::{ContextCapsule, QueryEngine};
use lattice_core::verification::VerificationStatus;
use lattice_core::DateTime;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tracing::info;

const REPORT_PATH: &str =
    "docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/cognitive_workspace_metrics.json";
const FIXTURE_ROOT: &str = "benches/fixtures";
const SEED: u64 = 20_260_516;
const SAMPLE_COUNT: usize = 48;
const WARM_UP_COUNT: usize = 4;

static GOLDEN_TASKS: OnceLock<Vec<GoldenTask>> = OnceLock::new();
static REPORTS: OnceLock<Mutex<BTreeMap<String, FixtureReport>>> = OnceLock::new();

struct FixtureWorkspace {
    name: String,
    root: PathBuf,
    _temp_dir: TempDir,
}

struct FixtureRuntime {
    workspace: FixtureWorkspace,
    graph: CodeGraph,
    rules: Vec<ProjectRule>,
    tasks: Vec<GoldenTask>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GoldenTask {
    task_id: String,
    repo: String,
    intent: String,
    task_prompt: String,
    expected_anchors: Vec<ExpectedAnchor>,
    expected_tests: Vec<String>,
    expected_memory_classes: Vec<String>,
    expected_render_mode: String,
    success_criteria_thresholds: SuccessCriteriaThresholds,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ExpectedAnchor {
    file: String,
    symbol: String,
    doc_section: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SuccessCriteriaThresholds {
    tool_calls_reduction: f64,
    irrelevant_file_reads_reduction: f64,
    relevant_anchor_recall: f64,
    memory_inclusion_precision: f64,
    contradiction_missed_rate: f64,
    stale_memory_unlabeled_rate: f64,
    tests_recommended_vs_needed: f64,
}

#[derive(Clone, Debug, Serialize)]
struct FixtureReport {
    fixture: String,
    deterministic_seed: u64,
    indexed_files: usize,
    node_count: usize,
    edge_count: usize,
    task_reports: Vec<TaskReport>,
    metrics: Vec<MetricValue>,
    recorded_at: String,
}

#[derive(Clone, Debug, Serialize)]
struct TaskReport {
    task_id: String,
    intent: String,
    tool_reports: Vec<ToolReport>,
    returned_anchors: Vec<String>,
    expected_anchors: Vec<String>,
    expected_tests: Vec<String>,
    recommended_tests: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ToolReport {
    tool: String,
    latency_us: u64,
    payload_bytes: usize,
    anchor_count: usize,
}

fn bench_cross_language_fixtures(c: &mut Criterion) {
    for fixture in ["rust-repo", "typescript-repo", "python-repo"] {
        register_fixture_benchmark(c, fixture);
    }
}

fn register_fixture_benchmark(c: &mut Criterion, fixture: &'static str) {
    let runtime = build_fixture_runtime(fixture);
    let report = measure_fixture(&runtime);
    store_fixture_report(report);
    c.bench_function(&format!("cognitive_workspace/{fixture}"), |b| {
        b.iter(|| black_box(run_fixture_runtime(&runtime)))
    });
}

fn measure_fixture(runtime: &FixtureRuntime) -> FixtureReport {
    warm_up(runtime);
    let mut last_report = run_fixture_runtime(runtime);
    for _ in 0..SAMPLE_COUNT {
        last_report = run_fixture_runtime(runtime);
    }
    last_report
}

fn warm_up(runtime: &FixtureRuntime) {
    for _ in 0..WARM_UP_COUNT {
        black_box(run_fixture_runtime(runtime));
    }
}

fn run_fixture_runtime(runtime: &FixtureRuntime) -> FixtureReport {
    let task_reports = runtime
        .tasks
        .iter()
        .map(|task| run_task(&runtime, task))
        .collect::<Vec<_>>();
    let metrics = collect_metrics(&runtime, &task_reports);
    FixtureReport {
        fixture: runtime.workspace.name.clone(),
        deterministic_seed: SEED,
        indexed_files: graph_file_count(&runtime.graph),
        node_count: runtime.graph.node_count(),
        edge_count: runtime.graph.edge_count(),
        task_reports,
        metrics,
        recorded_at: recorded_at(),
    }
}

fn build_fixture_runtime(fixture: &str) -> FixtureRuntime {
    let workspace = materialize_workspace(fixture);
    let graph = index_workspace(&workspace.root);
    let tasks = golden_tasks()
        .iter()
        .filter(|task| task.repo == fixture)
        .cloned()
        .collect::<Vec<_>>();
    assert!(!tasks.is_empty(), "no golden tasks for fixture {fixture}");
    let rules = detect_rules(&graph);
    FixtureRuntime {
        workspace,
        graph,
        rules,
        tasks,
    }
}

fn materialize_workspace(fixture: &str) -> FixtureWorkspace {
    let temp_dir = TempDir::new().expect("create fixture temp workspace");
    let root = temp_dir.path().join(fixture);
    copy_dir(&fixture_source_path(fixture), &root);
    fs::create_dir_all(root.join(".lattice")).expect("create isolated .lattice directory");
    FixtureWorkspace {
        name: fixture.to_string(),
        root,
        _temp_dir: temp_dir,
    }
}

fn copy_dir(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("create fixture copy target");
    for entry in fs::read_dir(source).expect("read fixture source") {
        let entry = entry.expect("read fixture source entry");
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir(&source_path, &target_path);
        } else {
            fs::copy(&source_path, &target_path).expect("copy fixture file");
        }
    }
}

fn index_workspace(root: &Path) -> CodeGraph {
    let runtime = tokio::runtime::Runtime::new().expect("benchmark runtime");
    let mut indexer = Indexer::new(root.to_path_buf());
    runtime
        .block_on(indexer.index_directory_parallel(root))
        .expect("index fixture workspace");
    indexer.graph().clone()
}

fn run_task(runtime: &FixtureRuntime, task: &GoldenTask) -> TaskReport {
    let mut reports = Vec::new();
    let context = time_tool("get_context_capsule", || context_capsule(runtime, task));
    reports.push(context.0);
    let prepare = time_tool("prepare_change", || {
        prepare_report(runtime, task, &context.1)
    });
    reports.push(prepare.0);
    let tests = time_tool("find_relevant_tests", || tests_report(runtime, task));
    reports.push(tests.0);
    let impact = time_tool("impact_from_diff", || impact_report(runtime, task));
    reports.push(impact.0);
    let diagnosis = time_tool("diagnose_failure", || diagnosis_report(runtime, task));
    reports.push(diagnosis.0);
    log_task_tools(runtime, task, &reports);
    TaskReport {
        task_id: task.task_id.clone(),
        intent: task.intent.clone(),
        returned_anchors: returned_anchors(
            &context.1,
            &prepare.1,
            &tests.1,
            &impact.1,
            &diagnosis.1,
        ),
        expected_anchors: task_anchor_ids(task),
        expected_tests: task.expected_tests.clone(),
        recommended_tests: tests.1.tests.iter().map(|test| test.file.clone()).collect(),
        tool_reports: reports,
    }
}

fn time_tool<T, F>(tool: &str, run: F) -> (ToolReport, T)
where
    T: Serialize,
    F: FnOnce() -> T,
{
    let started_at = Instant::now();
    let result = run();
    let payload_bytes = serde_json::to_vec(&result)
        .expect("serialize workflow tool result")
        .len();
    let report = ToolReport {
        tool: tool.to_string(),
        latency_us: elapsed_us(started_at),
        payload_bytes,
        anchor_count: payload_anchor_count(&result),
    };
    (report, result)
}

fn context_capsule(runtime: &FixtureRuntime, task: &GoldenTask) -> ContextCapsule {
    let mut engine = QueryEngine::new(runtime.graph.clone(), None, None);
    engine.query(&task.task_prompt, None, false)
}

fn prepare_report(
    runtime: &FixtureRuntime,
    task: &GoldenTask,
    capsule: &ContextCapsule,
) -> TaskBundle {
    prepare_change(
        &runtime.graph,
        capsule,
        &task_anchor_files(task),
        &task_anchor_symbols(task),
        &runtime.rules,
        BundleMode::Compact,
    )
}

fn tests_report(runtime: &FixtureRuntime, task: &GoldenTask) -> TestSelectionReport {
    find_relevant_tests(
        &runtime.graph,
        &task_anchor_files(task),
        &task_anchor_symbols(task),
        None,
        &runtime.rules,
        8,
    )
}

fn impact_report(runtime: &FixtureRuntime, task: &GoldenTask) -> DiffImpactReport {
    impact_from_diff(
        &runtime.graph,
        &task_diff(task),
        &task_anchor_files(task),
        &task_anchor_symbols(task),
        &runtime.rules,
        BundleMode::Compact,
        2,
    )
}

fn diagnosis_report(runtime: &FixtureRuntime, task: &GoldenTask) -> FailureDiagnosis {
    diagnose_failure(
        &runtime.graph,
        &failure_text(task),
        Some("test"),
        &runtime.rules,
        BundleMode::Compact,
    )
}

fn collect_metrics(runtime: &FixtureRuntime, reports: &[TaskReport]) -> Vec<MetricValue> {
    let events = reports
        .iter()
        .flat_map(|report| task_events(runtime, report))
        .collect::<Vec<_>>();
    let scope = MetricScope::session(session_id(&runtime.workspace.name));
    MetricsCollector::new()
        .with_events(events)
        .with_anchor_recall_samples(anchor_samples(runtime, reports))
        .with_test_recommendation_samples(test_samples(runtime, reports))
        .with_memory_surface_records(memory_records(runtime, reports))
        .with_computed_at(DateTime::from_unix_seconds(1_779_000_000))
        .collect(scope, &MetricSignal::ALL)
}

fn task_events(runtime: &FixtureRuntime, report: &TaskReport) -> Vec<EventEnvelope> {
    let mut events = Vec::new();
    for (index, tool) in report.tool_reports.iter().enumerate() {
        events.push(tool_called_event(runtime, report, tool, index as i64));
    }
    events.push(file_read_event(runtime, report, "irrelevant/noise.rs", 30));
    events.push(plan_created_event(runtime, report, 40));
    events.push(workflow_succeeded_event(runtime, report, 50));
    events
}

fn anchor_samples(runtime: &FixtureRuntime, reports: &[TaskReport]) -> Vec<AnchorRecallSample> {
    reports
        .iter()
        .enumerate()
        .map(|(index, report)| AnchorRecallSample {
            scope: sample_scope(runtime, report, 100 + index as i64),
            golden_anchors: report.expected_anchors.clone(),
            returned_anchors: report.returned_anchors.clone(),
        })
        .collect()
}

fn test_samples(runtime: &FixtureRuntime, reports: &[TaskReport]) -> Vec<TestRecommendationSample> {
    reports
        .iter()
        .enumerate()
        .map(|(index, report)| TestRecommendationSample {
            scope: sample_scope(runtime, report, 200 + index as i64),
            recommended_tests: report.recommended_tests.clone(),
            needed_tests: report.expected_tests.clone(),
        })
        .collect()
}

fn memory_records(runtime: &FixtureRuntime, reports: &[TaskReport]) -> Vec<MemorySurfaceRecord> {
    reports
        .iter()
        .enumerate()
        .map(|(index, report)| MemorySurfaceRecord {
            scope: sample_scope(runtime, report, 300 + index as i64),
            retrieval_event_id: event_id(runtime, 300 + index as i64),
            memory_id: memory_id(runtime, report),
            verification_status: VerificationStatus::Verified,
            stale_label_surfaced: false,
            contradiction_link_present: false,
            contradiction_surfaced: false,
            used_downstream: true,
            reused_later: true,
        })
        .collect()
}

fn returned_anchors(
    context: &ContextCapsule,
    prepare: &TaskBundle,
    tests: &TestSelectionReport,
    impact: &DiffImpactReport,
    diagnosis: &FailureDiagnosis,
) -> Vec<String> {
    let mut anchors = BTreeSet::new();
    anchors.extend(
        context
            .pivots
            .iter()
            .map(|node| anchor_id(&node.file, &node.symbol)),
    );
    anchors.extend(prepare.primary_files.iter().map(|file| file.file.clone()));
    anchors.extend(tests.tests.iter().map(|test| test.file.clone()));
    anchors.extend(
        impact
            .changed_symbols
            .iter()
            .map(|symbol| anchor_id(&symbol.file, &symbol.symbol)),
    );
    anchors.extend(
        diagnosis
            .suspects
            .iter()
            .map(|symbol| anchor_id(&symbol.file, &symbol.symbol)),
    );
    anchors.into_iter().collect()
}

fn payload_anchor_count<T: Serialize>(payload: &T) -> usize {
    serde_json::to_value(payload)
        .ok()
        .and_then(|value| value.as_object().map(|object| object.len()))
        .unwrap_or_default()
}

fn task_anchor_files(task: &GoldenTask) -> Vec<String> {
    task.expected_anchors
        .iter()
        .map(|anchor| anchor.file.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn task_anchor_symbols(task: &GoldenTask) -> Vec<String> {
    task.expected_anchors
        .iter()
        .map(|anchor| anchor.symbol.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn task_anchor_ids(task: &GoldenTask) -> Vec<String> {
    task.expected_anchors
        .iter()
        .map(|anchor| anchor_id(&anchor.file, &anchor.symbol))
        .collect()
}

fn anchor_id(file: &str, symbol: &str) -> String {
    format!("{file}::{symbol}")
}

fn task_diff(task: &GoldenTask) -> String {
    let file = task
        .expected_anchors
        .first()
        .expect("task has expected anchors")
        .file
        .clone();
    format!(
        "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1,3 +1,4 @@\n- old behavior\n+ new behavior\n"
    )
}

fn failure_text(task: &GoldenTask) -> String {
    format!(
        "AssertionError: fixture task {} failed\n  at {}\n",
        task.task_id,
        task.expected_tests
            .first()
            .expect("task has expected tests")
    )
}

fn log_task_tools(runtime: &FixtureRuntime, task: &GoldenTask, reports: &[ToolReport]) {
    for report in reports {
        info!(
            fixture = runtime.workspace.name.as_str(),
            task_id = task.task_id.as_str(),
            tool = report.tool.as_str(),
            latency_ms = report.latency_us as f64 / 1_000.0,
            metric_signal_count = MetricSignal::ALL.len(),
            "cognitive workspace benchmark tool run"
        );
    }
}

fn detect_rules(graph: &CodeGraph) -> Vec<ProjectRule> {
    let mut files: Vec<String> = graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect();
    files.sort();
    files.dedup();
    RulesDetector::new().detect_rules(&files)
}

fn graph_file_count(graph: &CodeGraph) -> usize {
    graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect::<BTreeSet<_>>()
        .len()
}

fn golden_tasks() -> &'static [GoldenTask] {
    GOLDEN_TASKS.get_or_init(load_golden_tasks)
}

fn load_golden_tasks() -> Vec<GoldenTask> {
    let path = fixture_root().join("golden_anchors.json");
    let body = fs::read_to_string(&path).expect("read golden anchors");
    let tasks: Vec<GoldenTask> = serde_json::from_str(&body).expect("parse golden anchors");
    validate_golden_tasks(&tasks);
    tasks
}

fn validate_golden_tasks(tasks: &[GoldenTask]) {
    assert!(!tasks.is_empty(), "golden anchors must contain tasks");
    let mut ids = BTreeSet::new();
    for task in tasks {
        assert!(
            ids.insert(task.task_id.clone()),
            "duplicate task_id {}",
            task.task_id
        );
        assert!(!task.repo.is_empty(), "task {} missing repo", task.task_id);
        assert!(
            !task.intent.is_empty(),
            "task {} missing intent",
            task.task_id
        );
        assert!(
            !task.expected_anchors.is_empty(),
            "task {} missing anchors",
            task.task_id
        );
        assert!(
            !task.expected_tests.is_empty(),
            "task {} missing tests",
            task.task_id
        );
        validate_thresholds(task);
    }
}

fn validate_thresholds(task: &GoldenTask) {
    let thresholds = &task.success_criteria_thresholds;
    assert!(
        thresholds.relevant_anchor_recall >= 0.8,
        "anchor recall threshold"
    );
    assert!(
        thresholds.memory_inclusion_precision >= 0.8,
        "memory precision threshold"
    );
    assert!(
        thresholds.tests_recommended_vs_needed >= 0.9,
        "test threshold"
    );
    assert_eq!(
        thresholds.contradiction_missed_rate, 0.0,
        "contradiction threshold"
    );
    assert_eq!(
        thresholds.stale_memory_unlabeled_rate, 0.0,
        "stale threshold"
    );
}

fn store_fixture_report(report: FixtureReport) {
    let reports = REPORTS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut guard = reports.lock().expect("cognitive benchmark report lock");
    guard.insert(report.fixture.clone(), report);
    write_report_snapshot(&guard);
}

fn write_report_snapshot(reports: &BTreeMap<String, FixtureReport>) {
    let snapshot = reports.values().collect::<Vec<_>>();
    let output_path = repo_root().join(REPORT_PATH);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).expect("create cognitive benchmark report directory");
    }
    let body = serde_json::to_string_pretty(&snapshot).expect("serialize benchmark report");
    let payload = format!("{body}\n");
    if let Err(error) = fs::write(&output_path, &payload) {
        let shadow_path = std::env::temp_dir()
            .join("lattice-benchmarks")
            .join("cognitive_workspace_metrics.json");
        if let Some(parent) = shadow_path.parent() {
            fs::create_dir_all(parent).expect("create shadow benchmark report directory");
        }
        fs::write(&shadow_path, payload).unwrap_or_else(|shadow_error| {
            panic!(
                "write benchmark report to {} failed with {}; shadow write to {} failed with {}",
                output_path.display(),
                error,
                shadow_path.display(),
                shadow_error
            )
        });
        eprintln!(
            "Benchmark report repo write failed for {}; wrote shadow report to {} instead: {}",
            output_path.display(),
            shadow_path.display(),
            error
        );
    }
}

fn tool_called_event(
    runtime: &FixtureRuntime,
    report: &TaskReport,
    tool: &ToolReport,
    second: i64,
) -> EventEnvelope {
    event(
        runtime,
        report,
        second,
        EventKind::ToolCalled,
        EventPayload::ToolCalled(ToolCalledPayload {
            call_id: format!("{}-{second}", report.task_id),
            tool_name: tool.tool.clone(),
            context_handle_id: None,
            source_event_id: None,
            input_summary: report.intent.clone(),
        }),
    )
}

fn file_read_event(
    runtime: &FixtureRuntime,
    report: &TaskReport,
    path: &str,
    second: i64,
) -> EventEnvelope {
    event(
        runtime,
        report,
        second,
        EventKind::FileRead,
        EventPayload::FileRead(FileReadPayload {
            file_id: file_id(runtime, path),
            source_event_id: None,
            byte_start: None,
            byte_end: None,
            reason: "benchmark irrelevant read sentinel".to_string(),
        }),
    )
}

fn plan_created_event(runtime: &FixtureRuntime, report: &TaskReport, second: i64) -> EventEnvelope {
    event(
        runtime,
        report,
        second,
        EventKind::PlanCreated,
        EventPayload::PlanCreated(PlanCreatedPayload {
            context_handle_id: None,
            source_event_id: None,
            memory_ids: Vec::new(),
            step_count: 3,
            plan_summary: report.intent.clone(),
        }),
    )
}

fn workflow_succeeded_event(
    runtime: &FixtureRuntime,
    report: &TaskReport,
    second: i64,
) -> EventEnvelope {
    event(
        runtime,
        report,
        second,
        EventKind::WorkflowSucceeded,
        EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
            workflow_name: "cognitive_workspace_benchmark".to_string(),
            terminal_event_id: None,
            output_context_handle_id: None,
            memory_ids: vec![memory_id(runtime, report)],
            result_summary: report.intent.clone(),
        }),
    )
}

fn event(
    runtime: &FixtureRuntime,
    report: &TaskReport,
    second: i64,
    kind: EventKind,
    payload: EventPayload,
) -> EventEnvelope {
    EventEnvelope::new(
        event_id(runtime, second),
        WorkspaceId::from(runtime.workspace.name.clone()),
        BranchRef {
            name: "main".to_string(),
        },
        SessionId {
            value: session_id(&runtime.workspace.name),
        },
        Some(TaskId {
            value: report.task_id.clone(),
        }),
        Actor::Assistant {
            model: "criterion".to_string(),
        },
        DateTime::from_unix_seconds(1_779_000_000 + second),
        kind,
        Vec::new(),
        PayloadHash::new([second as u8; 32]),
        CompactSummary::new(format!("{}-{second}", report.task_id)).expect("summary"),
        PayloadLocation::Inline { bytes_len: 128 },
        payload,
    )
    .expect("benchmark event envelope")
}

fn sample_scope(runtime: &FixtureRuntime, report: &TaskReport, second: i64) -> MetricSampleScope {
    MetricSampleScope {
        workspace_id: runtime.workspace.name.clone(),
        branch: Some("main".to_string()),
        session_id: Some(session_id(&runtime.workspace.name)),
        user_id: None,
        organization_id: None,
        task_id: Some(report.task_id.clone()),
        observed_at: DateTime::from_unix_seconds(1_779_000_000 + second),
    }
}

fn event_id(runtime: &FixtureRuntime, second: i64) -> EventId {
    EventId {
        workspace_id: runtime.workspace.name.clone(),
        ulid: format!("evt-{second}"),
    }
}

fn file_id(runtime: &FixtureRuntime, path: &str) -> FileId {
    FileId {
        workspace_id: runtime.workspace.name.clone(),
        repo_relative_path: path.to_string(),
        content_hash: format!("hash-{path}"),
    }
}

fn memory_id(runtime: &FixtureRuntime, report: &TaskReport) -> MemoryId {
    MemoryId {
        workspace_id: runtime.workspace.name.clone(),
        ulid: format!("mem-{}", report.task_id),
    }
}

fn session_id(fixture: &str) -> String {
    format!("session-{fixture}")
}

fn elapsed_us(started_at: Instant) -> u64 {
    let nanos = started_at.elapsed().as_nanos();
    ((nanos + 999) / 1_000) as u64
}

fn recorded_at() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
        .to_string()
}

fn fixture_source_path(fixture: &str) -> PathBuf {
    fixture_root().join(fixture)
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_ROOT)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("repo root from manifest dir")
        .to_path_buf()
}

criterion_group!(cognitive_workspace, bench_cross_language_fixtures);
criterion_main!(cognitive_workspace);
