use std::fs;
use std::path::{Path, PathBuf};

use lattice_core::events::{EventQuery, EventReader, EventStore, QueryOrder};
use lattice_core::metrics::{
    merge_missing_current_metrics, report_exit_code, BenchmarkEvidence, MetricScope,
    MetricScopeKind, MetricSignal, MetricSource, MetricTimeRange, MetricValue, MetricsCollector,
    RegressionReport, ReportInput, SuccessCriteriaThresholds,
};
use lattice_core::{DateTime, Utc};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

const DEFAULT_BENCHMARK_PATH: &str =
    "docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/cognitive_workspace_metrics.json";

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), CliError> {
    init_tracing();
    let args = CliArgs::parse(std::env::args().skip(1).collect())?;
    let workspace_root = std::env::current_dir().map_err(CliError::CurrentDir)?;
    let workspace_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.clone());
    let scope = args.metric_scope(&workspace_root)?;
    let signals = args
        .signals
        .clone()
        .unwrap_or_else(|| MetricSignal::ALL.to_vec());
    let current = collect_current_metrics(&workspace_root, &scope, &signals)?;
    let benchmark = BenchmarkEvidence::from_path(&args.benchmark_path())?;
    let current = merge_missing_current_metrics(current, benchmark.as_ref());
    let baseline = match &args.baseline {
        Some(path) => Some(load_baseline_metrics(path)?),
        None => None,
    };
    let report = RegressionReport::build(ReportInput {
        current,
        baseline,
        benchmark_report_path: args.benchmark_path(),
        scope: scope.clone(),
        success_criteria: SuccessCriteriaThresholds::initial(),
    })?;
    tracing::info!(
        scope = scope_label(&scope),
        signal_count = report.rows.len(),
        pass_count = report.pass_count,
        fail_count = report.fail_count,
        format = args.format.as_str(),
        "lattice_report evaluated phase 9 thresholds"
    );
    let rendered = args.render(&report)?;
    write_output(args.output.as_deref(), &rendered)?;
    let exit_code = report_exit_code(&report, args.fail_on_regression);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct CliArgs {
    scope: MetricScopeKind,
    time_range: Option<MetricTimeRange>,
    signals: Option<Vec<MetricSignal>>,
    baseline: Option<PathBuf>,
    benchmark: Option<PathBuf>,
    format: OutputFormat,
    output: Option<PathBuf>,
    fail_on_regression: bool,
}

#[derive(Debug, Clone, Copy)]
enum OutputFormat {
    Text,
    Json,
    CiSummary,
}

impl OutputFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::CiSummary => "ci-summary",
        }
    }
}

#[derive(Debug, Error)]
enum CliError {
    #[error("unknown argument `{0}`")]
    UnknownArgument(String),
    #[error("missing value for `{0}`")]
    MissingValue(&'static str),
    #[error("invalid scope `{0}`")]
    InvalidScope(String),
    #[error("invalid output format `{0}`")]
    InvalidFormat(String),
    #[error("invalid signal `{0}`")]
    InvalidSignal(String),
    #[error("invalid time range `{0}`; expected RFC3339,RFC3339")]
    InvalidTimeRange(String),
    #[error("session scope requires at least one captured event in the active workspace")]
    MissingSessionScope,
    #[error("failed to resolve current directory: {0}")]
    CurrentDir(std::io::Error),
    #[error("failed to open event store at {path}: {source}")]
    EventStoreOpen {
        path: PathBuf,
        source: lattice_core::events::EventStoreError,
    },
    #[error("failed to read events for metrics collection: {0}")]
    EventRead(lattice_core::events::EventQueryError),
    #[error("failed to read output path {path}: {source}")]
    OutputWrite {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse baseline metrics at {path}: {message}")]
    BaselineParse { path: PathBuf, message: String },
    #[error(transparent)]
    Report(#[from] lattice_core::metrics::ReportError),
    #[error("failed to serialize report JSON: {0}")]
    JsonRender(serde_json::Error),
}

impl CliArgs {
    fn parse(args: Vec<String>) -> Result<Self, CliError> {
        let mut scope = MetricScopeKind::Repo;
        let mut time_range = None;
        let mut signals = None;
        let mut baseline = None;
        let mut benchmark = None;
        let mut format = OutputFormat::Text;
        let mut output = None;
        let mut fail_on_regression = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--scope" => {
                    scope = parse_scope(next_arg(&args, index, "--scope")?)?;
                    index += 2;
                }
                "--time-range" => {
                    time_range = Some(parse_time_range(next_arg(&args, index, "--time-range")?)?);
                    index += 2;
                }
                "--signals" => {
                    signals = Some(parse_signals(next_arg(&args, index, "--signals")?)?);
                    index += 2;
                }
                "--baseline" => {
                    baseline = Some(PathBuf::from(next_arg(&args, index, "--baseline")?));
                    index += 2;
                }
                "--benchmark" => {
                    benchmark = Some(PathBuf::from(next_arg(&args, index, "--benchmark")?));
                    index += 2;
                }
                "--format" => {
                    format = parse_format(next_arg(&args, index, "--format")?)?;
                    index += 2;
                }
                "--output" => {
                    output = Some(PathBuf::from(next_arg(&args, index, "--output")?));
                    index += 2;
                }
                "--fail-on-regression" => {
                    fail_on_regression = true;
                    index += 1;
                }
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }
        Ok(Self {
            scope,
            time_range,
            signals,
            baseline,
            benchmark,
            format,
            output,
            fail_on_regression,
        })
    }

    fn benchmark_path(&self) -> PathBuf {
        self.benchmark
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_BENCHMARK_PATH))
    }

    fn metric_scope(&self, workspace_root: &Path) -> Result<MetricScope, CliError> {
        let scope = match self.scope {
            MetricScopeKind::Session => {
                MetricScope::session(resolve_latest_session(workspace_root)?)
            }
            MetricScopeKind::Branch => MetricScope::branch(
                workspace_root.to_string_lossy(),
                current_git_branch(workspace_root).unwrap_or_else(|| "main".to_string()),
            ),
            MetricScopeKind::Repo => MetricScope::repo(workspace_root.to_string_lossy()),
            MetricScopeKind::User => MetricScope {
                kind: MetricScopeKind::User,
                workspace_id: None,
                branch: None,
                session_id: None,
                user_id: None,
                organization_id: None,
                time_range: None,
            },
            MetricScopeKind::Organization => MetricScope {
                kind: MetricScopeKind::Organization,
                workspace_id: None,
                branch: None,
                session_id: None,
                user_id: None,
                organization_id: None,
                time_range: None,
            },
        };
        Ok(match &self.time_range {
            Some(range) => scope.with_time_range(range.since, range.until),
            None => scope,
        })
    }

    fn render(&self, report: &RegressionReport) -> Result<String, CliError> {
        match self.format {
            OutputFormat::Text => Ok(report.render_text()),
            OutputFormat::Json => report.render_json().map_err(CliError::JsonRender),
            OutputFormat::CiSummary => Ok(report.render_ci_summary()),
        }
    }
}

fn collect_current_metrics(
    workspace_root: &Path,
    scope: &MetricScope,
    signals: &[MetricSignal],
) -> Result<Vec<MetricValue>, CliError> {
    let events = load_scope_events(workspace_root, scope)?;
    for signal in signals {
        tracing::info!(
            signal = signal.as_str(),
            source = "event_log",
            event_count = events.len(),
            "lattice_report collected live signal input"
        );
    }
    Ok(MetricsCollector::new()
        .with_events(events)
        .collect(scope.clone(), signals))
}

fn load_scope_events(
    workspace_root: &Path,
    scope: &MetricScope,
) -> Result<Vec<lattice_core::events::EventEnvelope>, CliError> {
    let event_store_path = workspace_root.join(".lattice").join("events.db");
    let store = EventStore::open(&event_store_path).map_err(|source| CliError::EventStoreOpen {
        path: event_store_path,
        source,
    })?;
    let reader = EventReader::new(std::sync::Arc::new(store));
    let query = event_query_for_scope(workspace_root, scope)?;
    reader.execute(query).map_err(CliError::EventRead)
}

fn event_query_for_scope(
    workspace_root: &Path,
    scope: &MetricScope,
) -> Result<EventQuery, CliError> {
    let mut query = match scope.kind {
        MetricScopeKind::Session => EventQuery::new()
            .session(
                scope
                    .session_id
                    .clone()
                    .ok_or(CliError::MissingSessionScope)?,
            )
            .workspace(workspace_root.to_string_lossy()),
        MetricScopeKind::Branch | MetricScopeKind::Repo => EventQuery::new()
            .workspace(workspace_root.to_string_lossy())
            .branch(current_git_branch(workspace_root).unwrap_or_else(|| "main".to_string())),
        MetricScopeKind::User | MetricScopeKind::Organization => {
            return Ok(EventQuery::new()
                .workspace(workspace_root.to_string_lossy())
                .branch(current_git_branch(workspace_root).unwrap_or_else(|| "main".to_string()))
                .limit(1));
        }
    }
    .order(QueryOrder::OldestFirst)
    .limit(1_000);
    if let Some(range) = &scope.time_range {
        if let Some(since) = range.since {
            query = query.after(since);
        }
        if let Some(until) = range.until {
            query = query.before(until);
        }
    }
    Ok(query)
}

fn resolve_latest_session(workspace_root: &Path) -> Result<String, CliError> {
    let event_store_path = workspace_root.join(".lattice").join("events.db");
    let store = EventStore::open(&event_store_path).map_err(|source| CliError::EventStoreOpen {
        path: event_store_path,
        source,
    })?;
    let reader = EventReader::new(std::sync::Arc::new(store));
    let query = EventQuery::new()
        .workspace(workspace_root.to_string_lossy())
        .branch(current_git_branch(workspace_root).unwrap_or_else(|| "main".to_string()))
        .order(QueryOrder::NewestFirst)
        .limit(1);
    let latest = reader.execute(query).map_err(CliError::EventRead)?;
    latest
        .first()
        .map(|event| event.session_id.value.clone())
        .ok_or(CliError::MissingSessionScope)
}

fn load_baseline_metrics(path: &Path) -> Result<Vec<MetricValue>, CliError> {
    let body = fs::read_to_string(path).map_err(|error| CliError::BaselineParse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if let Ok(metrics) = serde_json::from_str::<Vec<MetricValue>>(&body) {
        return Ok(metrics);
    }
    if let Ok(fixtures) = serde_json::from_str::<Vec<FixtureBaselineReport>>(&body) {
        return Ok(flatten_fixture_metrics(&fixtures));
    }
    if let Ok(legacy) = serde_json::from_str::<Vec<LegacyBaselineMetric>>(&body) {
        return Ok(legacy_baseline_metrics(&legacy));
    }
    Err(CliError::BaselineParse {
        path: path.to_path_buf(),
        message: "unsupported baseline schema".to_string(),
    })
}

fn flatten_fixture_metrics(fixtures: &[FixtureBaselineReport]) -> Vec<MetricValue> {
    let mut merged = Vec::new();
    for signal in MetricSignal::ALL {
        let values = fixtures
            .iter()
            .flat_map(|fixture| fixture.metrics.iter())
            .filter(|metric| metric.signal == signal)
            .filter_map(|metric| metric.value)
            .collect::<Vec<_>>();
        if values.is_empty() {
            continue;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        merged.push(MetricValue {
            signal,
            value: Some(mean),
            denominator: Some(values.len() as u64),
            sample_count: values.len() as u64,
            source: fixtures
                .iter()
                .flat_map(|fixture| fixture.metrics.iter())
                .find(|metric| metric.signal == signal)
                .map(|metric| metric.source)
                .unwrap_or(MetricSource::EventLog),
            computed_at: fixtures
                .iter()
                .flat_map(|fixture| fixture.metrics.iter().map(|metric| metric.computed_at))
                .max_by_key(|value| value.unix_seconds())
                .unwrap_or_else(Utc::now),
            incomplete: false,
            reason_if_null: None,
        });
    }
    merged
}

fn legacy_baseline_metrics(legacy: &[LegacyBaselineMetric]) -> Vec<MetricValue> {
    let count = legacy
        .iter()
        .filter(|metric| {
            matches!(
                metric.name.as_str(),
                "prepare_change"
                    | "get_context_capsule"
                    | "find_relevant_tests"
                    | "impact_from_diff"
                    | "diagnose_failure"
            )
        })
        .count() as f64;
    let mut metrics = Vec::new();
    if count > 0.0 {
        metrics.push(MetricValue {
            signal: MetricSignal::ToolCallsPerSuccessfulTask,
            value: Some(count),
            denominator: Some(1),
            sample_count: 1,
            source: MetricSource::WorkflowOutcome,
            computed_at: Utc::now(),
            incomplete: false,
            reason_if_null: None,
        });
        metrics.push(MetricValue {
            signal: MetricSignal::IrrelevantFilesOpenedPerTask,
            value: None,
            denominator: None,
            sample_count: 0,
            source: MetricSource::EventLog,
            computed_at: Utc::now(),
            incomplete: false,
            reason_if_null: Some(
                "legacy baseline bench did not capture irrelevant file read counts".to_string(),
            ),
        });
    }
    metrics
}

fn write_output(path: Option<&Path>, rendered: &str) -> Result<(), CliError> {
    if let Some(path) = path {
        fs::write(path, rendered).map_err(|source| CliError::OutputWrite {
            path: path.to_path_buf(),
            source,
        })?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
}

fn parse_scope(value: &str) -> Result<MetricScopeKind, CliError> {
    match value {
        "session" => Ok(MetricScopeKind::Session),
        "branch" => Ok(MetricScopeKind::Branch),
        "repo" => Ok(MetricScopeKind::Repo),
        "user" => Ok(MetricScopeKind::User),
        "organization" => Ok(MetricScopeKind::Organization),
        other => Err(CliError::InvalidScope(other.to_string())),
    }
}

fn parse_format(value: &str) -> Result<OutputFormat, CliError> {
    match value {
        "text" => Ok(OutputFormat::Text),
        "json" => Ok(OutputFormat::Json),
        "ci-summary" => Ok(OutputFormat::CiSummary),
        other => Err(CliError::InvalidFormat(other.to_string())),
    }
}

fn parse_signals(value: &str) -> Result<Vec<MetricSignal>, CliError> {
    value
        .split(',')
        .filter(|item| !item.trim().is_empty())
        .map(|item| match item.trim() {
            "tool_calls_per_successful_task" => Ok(MetricSignal::ToolCallsPerSuccessfulTask),
            "irrelevant_files_opened_per_task" => Ok(MetricSignal::IrrelevantFilesOpenedPerTask),
            "relevant_anchor_recall" => Ok(MetricSignal::RelevantAnchorRecall),
            "memory_inclusion_precision" => Ok(MetricSignal::MemoryInclusionPrecision),
            "memory_later_used_rate" => Ok(MetricSignal::MemoryLaterUsedRate),
            "stale_memory_surfaced_rate" => Ok(MetricSignal::StaleMemorySurfacedRate),
            "contradiction_missed_rate" => Ok(MetricSignal::ContradictionMissedRate),
            "tests_recommended_vs_needed" => Ok(MetricSignal::TestsRecommendedVsNeeded),
            "workflow_success_after_first_plan" => Ok(MetricSignal::WorkflowSuccessAfterFirstPlan),
            other => Err(CliError::InvalidSignal(other.to_string())),
        })
        .collect()
}

fn parse_time_range(value: &str) -> Result<MetricTimeRange, CliError> {
    let mut parts = value.split(',');
    let since = parts
        .next()
        .ok_or_else(|| CliError::InvalidTimeRange(value.to_string()))?;
    let until = parts
        .next()
        .ok_or_else(|| CliError::InvalidTimeRange(value.to_string()))?;
    if parts.next().is_some() {
        return Err(CliError::InvalidTimeRange(value.to_string()));
    }
    Ok(MetricTimeRange {
        since: Some(
            DateTime::parse_rfc3339(since)
                .map_err(|_| CliError::InvalidTimeRange(value.to_string()))?,
        ),
        until: Some(
            DateTime::parse_rfc3339(until)
                .map_err(|_| CliError::InvalidTimeRange(value.to_string()))?,
        ),
    })
}

fn next_arg<'a>(args: &'a [String], index: usize, flag: &'static str) -> Result<&'a str, CliError> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or(CliError::MissingValue(flag))
}

fn current_git_branch(workspace_root: &Path) -> Option<String> {
    let git_path = workspace_root.join(".git");
    let head_path = if git_path.is_dir() {
        git_path.join("HEAD")
    } else if git_path.is_file() {
        let gitdir = std::fs::read_to_string(&git_path).ok()?;
        let relative = gitdir.trim().strip_prefix("gitdir:")?.trim();
        workspace_root.join(relative).join("HEAD")
    } else {
        return None;
    };
    let head = std::fs::read_to_string(head_path).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

fn scope_label(scope: &MetricScope) -> &'static str {
    match scope.kind {
        MetricScopeKind::Session => "session",
        MetricScopeKind::Branch => "branch",
        MetricScopeKind::Repo => "repo",
        MetricScopeKind::User => "user",
        MetricScopeKind::Organization => "organization",
    }
}

#[derive(Debug, serde::Deserialize)]
struct FixtureBaselineReport {
    metrics: Vec<MetricValue>,
}

#[derive(Debug, serde::Deserialize)]
struct LegacyBaselineMetric {
    name: String,
}
