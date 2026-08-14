use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::intelligence::{
    diagnose_failure, expand_context, find_relevant_tests, impact_from_diff, prepare_change,
    BundleMode, ExpandContextSeed, ExpandedContext, FailureDiagnosis, ProjectRule, RulesDetector,
    TaskBundle, TestSelectionReport,
};
use lattice_core::query::{ContextCapsule, QueryEngine};
use lattice_core::symbols::stable_file_handle;
use serde::Serialize;

const PREPARE_QUERY: &str = "fix login session creation and audit trail";
const CONTEXT_QUERY: &str = "understand the login session creation flow";
const SEARCH_PATTERN: &str = "login";
const DIFF_TEXT: &str = "\
diff --git a/src/auth.ts b/src/auth.ts
--- a/src/auth.ts
+++ b/src/auth.ts
@@ -6,8 +6,10 @@ export async function loginUser(
-  const session = createSession(email);
-  recordLogin(email);
+  const session = createSession(email, \"password\");
+  recordLogin(email);
+  incrementLoginMetric(email);
   return session;
 }
";
const FAILURE_INPUT: &str = "\
AssertionError: expected session strategy to be password
  at tests/auth.test.ts:10
  at src/auth.ts:8
";
const SAMPLE_COUNT: usize = 96;
const WARM_UP_COUNT: usize = 8;

static FIXTURE: OnceLock<BenchmarkFixture> = OnceLock::new();
static METRICS: OnceLock<Mutex<BTreeMap<String, BaselineMetric>>> = OnceLock::new();

#[derive(Clone)]
struct BenchmarkFixture {
    graph: CodeGraph,
    rules: Vec<ProjectRule>,
    expand_seed: ExpandContextSeed,
}

#[derive(Clone, Serialize)]
struct SearchSymbolMatch {
    symbol: String,
    file: String,
    line: usize,
}

#[derive(Clone, Serialize)]
struct SearchSymbolsReport {
    pattern: String,
    results: Vec<SearchSymbolMatch>,
    count: usize,
}

#[derive(Clone, Serialize)]
struct BaselineMetric {
    name: String,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    bytes_returned: usize,
    candidate_count: usize,
    commit_sha: String,
    recorded_at: String,
}

fn fixture() -> &'static BenchmarkFixture {
    FIXTURE.get_or_init(build_fixture)
}

fn build_fixture() -> BenchmarkFixture {
    let workspace_root = fixture_workspace_root();
    materialize_fixture_workspace(&workspace_root);

    let runtime = tokio::runtime::Runtime::new().expect("benchmark runtime");
    let mut indexer = Indexer::new(workspace_root.clone());
    runtime
        .block_on(indexer.index_directory_parallel(&workspace_root))
        .expect("fixture indexing");

    let graph = indexer.graph().clone();
    let rules = detect_rules(&graph);
    let expand_seed = build_expand_seed(&graph);
    BenchmarkFixture {
        graph,
        rules,
        expand_seed,
    }
}

fn fixture_workspace_root() -> PathBuf {
    let root = std::env::temp_dir().join("lattice-baseline-workflows-fixture");
    if root.exists() {
        fs::remove_dir_all(&root).expect("remove old fixture workspace");
    }
    fs::create_dir_all(&root).expect("create fixture workspace");
    root
}

fn materialize_fixture_workspace(root: &Path) {
    for (relative_path, content) in fixture_files() {
        let path = root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent directory");
        }
        fs::write(path, content).expect("write fixture file");
    }
}

fn fixture_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "src/auth.ts",
            "import { verifyPassword } from './security/password';\n\
import { createSession } from './session';\n\
import { recordLogin } from './audit';\n\
import { incrementLoginMetric } from './metrics';\n\
\n\
export async function loginUser(email: string, password: string) {\n\
  verifyPassword(password);\n\
  const session = createSession(email, 'password');\n\
  recordLogin(email);\n\
  incrementLoginMetric(email);\n\
  return session;\n\
}\n",
        ),
        (
            "src/session.ts",
            "export interface Session {\n\
  user: string;\n\
  strategy: string;\n\
}\n\
\n\
export function createSession(user: string, strategy: string): Session {\n\
  return { user, strategy };\n\
}\n",
        ),
        (
            "src/routes/auth.ts",
            "import { loginUser } from '../auth';\n\
\n\
export async function loginRoute(request: { email: string; password: string }) {\n\
  return loginUser(request.email, request.password);\n\
}\n",
        ),
        (
            "src/security/password.ts",
            "export function verifyPassword(password: string): void {\n\
  if (password.length < 8) {\n\
    throw new Error('Password too short');\n\
  }\n\
}\n",
        ),
        (
            "src/audit.ts",
            "export function recordLogin(email: string): string {\n\
  return `login:${email}`;\n\
}\n",
        ),
        (
            "src/metrics.ts",
            "export function incrementLoginMetric(metricKey: string): string {\n\
  return `metric:${metricKey}`;\n\
}\n",
        ),
        (
            "tests/auth.test.ts",
            "import { loginUser } from '../src/auth';\n\
\n\
test('loginUser returns a password session', async () => {\n\
  const session = await loginUser('alice@example.com', 'supersecret');\n\
  expect(session.strategy).toBe('password');\n\
});\n",
        ),
        (
            "tests/session.test.ts",
            "import { createSession } from '../src/session';\n\
\n\
test('createSession captures the strategy', () => {\n\
  expect(createSession('alice@example.com', 'password').strategy).toBe('password');\n\
});\n",
        ),
        (
            "docs/auth.md",
            "# Auth Flow\n\n\
The login route calls `loginUser`, which validates the password,\n\
creates a session, records an audit event, and increments login metrics.\n",
        ),
        (
            "docs/session.md",
            "# Session Lifecycle\n\n\
`createSession` stores the user identifier and the selected strategy.\n",
        ),
    ]
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

fn build_expand_seed(graph: &CodeGraph) -> ExpandContextSeed {
    let capsule = context_capsule(graph, CONTEXT_QUERY);
    let primary = capsule
        .pivots
        .first()
        .map(|item| (item.file.clone(), item.symbol.clone(), item.line))
        .or_else(|| {
            capsule
                .context
                .first()
                .map(|item| (item.file.clone(), item.symbol.clone(), item.line))
        })
        .expect("expected a symbol for expand_context fixture");
    let stable_symbol = graph
        .all_nodes()
        .into_iter()
        .find(|node| node.file == primary.0 && node.name == primary.1 && node.line == primary.2)
        .map(|node| node.id.stable_handle())
        .expect("expected stable symbol handle");
    ExpandContextSeed {
        query: Some(CONTEXT_QUERY.to_string()),
        files: vec![
            stable_file_handle(&primary.0),
            primary.0.clone(),
            "src/session.ts".to_string(),
            stable_file_handle("src/session.ts"),
        ],
        symbols: vec![
            stable_symbol,
            primary.1.clone(),
            "createSession".to_string(),
        ],
        tests: vec!["tests/auth.test.ts".to_string()],
        memories: Vec::new(),
    }
}

fn context_capsule(graph: &CodeGraph, query: &str) -> ContextCapsule {
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    engine.query(query, None, false)
}

fn prepare_change_report(fixture: &BenchmarkFixture) -> TaskBundle {
    let capsule = context_capsule(&fixture.graph, PREPARE_QUERY);
    prepare_change(
        &fixture.graph,
        &capsule,
        &[],
        &[],
        &fixture.rules,
        BundleMode::Compact,
        None,
    )
}

fn expand_context_report(fixture: &BenchmarkFixture) -> ExpandedContext {
    let focus = fixture
        .expand_seed
        .symbols
        .first()
        .expect("expand seed symbol")
        .clone();
    expand_context(&fixture.graph, &fixture.expand_seed, &focus, 1200)
}

fn impact_from_diff_report(
    fixture: &BenchmarkFixture,
) -> lattice_core::intelligence::DiffImpactReport {
    impact_from_diff(
        &fixture.graph,
        DIFF_TEXT,
        &[],
        &[],
        &fixture.rules,
        BundleMode::Compact,
        2,
        None,
    )
}

fn diagnose_failure_report(fixture: &BenchmarkFixture) -> FailureDiagnosis {
    diagnose_failure(
        &fixture.graph,
        FAILURE_INPUT,
        Some("test"),
        &fixture.rules,
        BundleMode::Compact,
        None,
    )
}

fn find_relevant_tests_report(fixture: &BenchmarkFixture) -> TestSelectionReport {
    find_relevant_tests(
        &fixture.graph,
        &["src/auth.ts".to_string(), "src/session.ts".to_string()],
        &["loginUser".to_string(), "createSession".to_string()],
        None,
        &fixture.rules,
        8,
    )
}

fn search_symbols_report(fixture: &BenchmarkFixture) -> SearchSymbolsReport {
    let pattern = SEARCH_PATTERN.to_lowercase();
    let mut results: Vec<SearchSymbolMatch> = fixture
        .graph
        .all_nodes()
        .into_iter()
        .filter(|node| node.name.to_lowercase().contains(&pattern))
        .map(|node| SearchSymbolMatch {
            symbol: node.name.clone(),
            file: node.file.clone(),
            line: node.line,
        })
        .collect();
    results.truncate(20);
    SearchSymbolsReport {
        pattern: SEARCH_PATTERN.to_string(),
        count: results.len(),
        results,
    }
}

fn register_report_benchmark<T, F, C>(c: &mut Criterion, name: &'static str, run: F, count: C)
where
    T: Serialize,
    F: Fn() -> T + Copy,
    C: Fn(&T) -> usize + Copy,
{
    store_metric(measure_report(name, run, count));
    c.bench_function(name, |b| b.iter(|| black_box(run())));
}

fn measure_report<T, F, C>(name: &str, run: F, count: C) -> BaselineMetric
where
    T: Serialize,
    F: Fn() -> T,
    C: Fn(&T) -> usize,
{
    warm_up(&run);
    let samples = collect_samples(&run);
    let report = run();
    let bytes_returned = serde_json::to_vec(&report)
        .expect("serialize benchmark report")
        .len();
    BaselineMetric {
        name: name.to_string(),
        p50_us: percentile_us(&samples, 0.50),
        p95_us: percentile_us(&samples, 0.95),
        p99_us: percentile_us(&samples, 0.99),
        bytes_returned,
        candidate_count: count(&report),
        commit_sha: git_commit_sha(),
        recorded_at: recorded_at(),
    }
}

fn warm_up<T, F>(run: &F)
where
    F: Fn() -> T,
{
    for _ in 0..WARM_UP_COUNT {
        black_box(run());
    }
}

fn collect_samples<T, F>(run: &F) -> Vec<u64>
where
    F: Fn() -> T,
{
    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let started_at = Instant::now();
        black_box(run());
        samples.push(elapsed_us(started_at));
    }
    samples.sort_unstable();
    samples
}

fn elapsed_us(started_at: Instant) -> u64 {
    let nanos = started_at.elapsed().as_nanos();
    ((nanos + 999) / 1_000) as u64
}

fn percentile_us(samples: &[u64], percentile: f64) -> u64 {
    let max_index = samples.len().saturating_sub(1);
    let index = ((max_index as f64) * percentile).round() as usize;
    samples.get(index).copied().unwrap_or(0)
}

fn store_metric(metric: BaselineMetric) {
    let metrics = METRICS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut guard = metrics.lock().expect("baseline metrics lock");
    guard.insert(metric.name.clone(), metric);
    write_metrics_snapshot(&guard);
}

fn write_metrics_snapshot(metrics: &BTreeMap<String, BaselineMetric>) {
    let snapshot: Vec<&BaselineMetric> = metrics.values().collect();
    let output_path = benchmark_root().join(
        "docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json",
    );
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).expect("create baseline metrics directory");
    }
    let body = serde_json::to_string_pretty(&snapshot).expect("serialize metrics snapshot");
    let payload = format!("{body}\n");
    if let Err(error) = fs::write(&output_path, &payload) {
        let shadow_path = shadow_metrics_path();
        if let Some(parent) = shadow_path.parent() {
            fs::create_dir_all(parent).expect("create shadow metrics directory");
        }
        fs::write(&shadow_path, payload).unwrap_or_else(|shadow_error| {
            panic!(
                "write baseline metrics snapshot to {} failed with {}; shadow write to {} also failed with {}",
                output_path.display(),
                error,
                shadow_path.display(),
                shadow_error
            )
        });
        eprintln!(
            "Baseline metrics repo write failed for {}; wrote shadow snapshot to {} instead: {}",
            output_path.display(),
            shadow_path.display(),
            error
        );
    }
}

fn benchmark_root() -> PathBuf {
    if let Ok(pwd) = std::env::var("PWD") {
        let candidate = PathBuf::from(pwd);
        if let Some(parent) = candidate.parent() {
            let repo_root = parent.to_path_buf();
            if repo_root.join("docs").is_dir() && repo_root.join("daemon").is_dir() {
                return repo_root;
            }
        }
    }

    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(parent) = current_dir.parent() {
            let candidate = parent.to_path_buf();
            if candidate.join("docs").is_dir() && candidate.join("daemon").is_dir() {
                return candidate;
            }
        }
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("repo root from manifest dir")
        .to_path_buf()
}

fn git_commit_sha() -> String {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(benchmark_root())
        .output()
        .expect("run git rev-parse HEAD");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn recorded_at() -> String {
    let unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs();
    unix_seconds.to_string()
}

fn shadow_metrics_path() -> PathBuf {
    std::env::temp_dir()
        .join("lattice-benchmarks")
        .join("baseline_metrics.json")
}

fn bench_prepare_change(c: &mut Criterion) {
    let fixture = fixture();
    register_report_benchmark(
        c,
        "prepare_change",
        || prepare_change_report(fixture),
        |report| {
            report.primary_files.len()
                + report.secondary_files.len()
                + report.symbols.len()
                + report.tests.len()
        },
    );
}

fn bench_get_context_capsule(c: &mut Criterion) {
    let fixture = fixture();
    register_report_benchmark(
        c,
        "get_context_capsule",
        || context_capsule(&fixture.graph, CONTEXT_QUERY),
        |report| report.pivots.len() + report.context.len(),
    );
}

fn bench_expand_context(c: &mut Criterion) {
    let fixture = fixture();
    register_report_benchmark(
        c,
        "expand_context",
        || expand_context_report(fixture),
        |report| {
            report.files.len() + report.symbols.len() + report.tests.len() + report.memories.len()
        },
    );
}

fn bench_impact_from_diff(c: &mut Criterion) {
    let fixture = fixture();
    register_report_benchmark(
        c,
        "impact_from_diff",
        || impact_from_diff_report(fixture),
        |report| {
            report.changed_files.len()
                + report.changed_symbols.len()
                + report.affected_symbols.len()
                + report.tests.len()
        },
    );
}

fn bench_diagnose_failure(c: &mut Criterion) {
    let fixture = fixture();
    register_report_benchmark(
        c,
        "diagnose_failure",
        || diagnose_failure_report(fixture),
        |report| report.suspects.len() + report.related_symbols.len() + report.tests.len(),
    );
}

fn bench_search_symbols(c: &mut Criterion) {
    let fixture = fixture();
    register_report_benchmark(
        c,
        "search_symbols",
        || search_symbols_report(fixture),
        |report| report.results.len(),
    );
}

fn bench_find_relevant_tests(c: &mut Criterion) {
    let fixture = fixture();
    register_report_benchmark(
        c,
        "find_relevant_tests",
        || find_relevant_tests_report(fixture),
        |report| report.tests.len(),
    );
}

criterion_group!(
    baseline_workflows,
    bench_prepare_change,
    bench_get_context_capsule,
    bench_expand_context,
    bench_impact_from_diff,
    bench_diagnose_failure,
    bench_search_symbols,
    bench_find_relevant_tests
);
criterion_main!(baseline_workflows);
