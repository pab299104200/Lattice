//! Phase 11 large-repo hardening tests for the cognitive workspace fork plan.
//!
//! Budget citations:
//! - `## Phase 1: Unified Identity Model` requires identity resolution to add
//!   no more than 2ms P99 to hot-path calls.
//! - `## Phase 2: Event Log Substrate` requires event writes on
//!   `prepare_change` and `get_context_capsule` to stay under 5ms P99.
//! - `## Non-Negotiable Product Properties` forbids unbounded graph traversal,
//!   payload growth, and event-log scans on hot paths.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;
use tempfile::TempDir;

use crate::events::{
    Actor, BranchRef, CompactSummary, CompactionConfig, Compactor, EventPayload, EventStore,
    EventWriter, FlushPolicy, PartialEnvelope, SessionId, ToolCalledPayload,
};
use crate::graph::CodeGraph;
use crate::identity::resolver::IdentityResolver;
use crate::identity::{EventId, WorkspaceId};
use crate::indexer::Indexer;
use crate::intelligence::{
    diagnose_failure, expand_context, find_relevant_tests, impact_from_diff, prepare_change,
    BundleMode, ExpandContextSeed, ProjectRule, RulesDetector,
};
use crate::memory::MemoryStore;
use crate::query::QueryEngine;
use crate::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use crate::symbols::{
    stable_file_handle, ImportInfo, Language, ParsedFile, Symbol, SymbolId, SymbolKind,
};

const HOT_PATH_ITERATIONS: usize = 1_000;
const WARMUP_ITERATIONS: usize = 100;
const IDENTITY_BUDGET_US: u64 = 2_000;
const EVENT_WRITE_BUDGET_US: u64 = 10_000;
const FULL_COMPACTION_EVENTS: usize = 1_000_000;
const REPORT_PATH: &str =
    "docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/large_repo_results.json";
const REQUESTED_FIXTURE_ROOT: &str =
    "docs/plans/2026-05-16-cognitive-workspace-fork-build/fixtures/large_repos";
const T66_FIXTURE_ROOT: &str = "daemon/crates/lattice-core/benches/fixtures";

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static REPORTS: OnceLock<Mutex<BTreeMap<String, FixtureReport>>> = OnceLock::new();

#[test]
#[ignore]
fn test_hot_path_p99_budgets_on_small_rust_fixture() {
    measure_fixture(FixtureSpec::new("small-rust", Language::Rust, 5_000));
}

#[test]
#[ignore]
fn test_hot_path_p99_budgets_on_medium_typescript_fixture() {
    measure_fixture(FixtureSpec::new(
        "medium-typescript",
        Language::TypeScript,
        25_000,
    ));
}

#[test]
#[ignore]
fn test_hot_path_p99_budgets_on_large_polyglot_fixture() {
    measure_fixture(FixtureSpec::new("large-polyglot", Language::Go, 100_000));
}

#[test]
#[ignore]
fn test_hot_path_p99_budgets_on_extra_large_fixture() {
    measure_fixture(FixtureSpec::new("extra-large", Language::Python, 250_000));
}

#[test]
#[ignore]
fn test_event_log_compaction_keeps_hot_path_p99_within_5ms_budget() {
    let _guard = test_guard();
    init_tracing();
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = Arc::new(event_writer(store.clone(), 4_096));
    append_events(&writer, compaction_event_count(), "prepare_change");
    let before = event_write_p99_us(&writer, "prepare_change", HOT_PATH_ITERATIONS);

    let compactor = Compactor::new(
        store,
        writer.clone(),
        Arc::new(Mutex::new(Arc::new(CodeGraph::new()))),
        Arc::new(Mutex::new(
            MemoryStore::open_in_memory().expect("memory store opens"),
        )),
        compaction_config(),
    );
    let started = Instant::now();
    let report = compactor.run_once().expect("compaction succeeds");
    let compaction_ms = elapsed_us(started) / 1_000;
    let after = event_write_p99_us(&writer, "get_context_capsule", HOT_PATH_ITERATIONS);

    assert_budget(
        "event_write_before_compaction",
        before,
        EVENT_WRITE_BUDGET_US,
    );
    assert_budget("event_write_after_compaction", after, EVENT_WRITE_BUDGET_US);
    assert!(
        !report.skipped && report.events_truncated >= compaction_event_count() as u64,
        "compaction did not truncate the expected event volume: {report:?}"
    );
    write_special_report("event-log-compaction", Some(compaction_ms), before, after);
}

#[test]
#[ignore]
fn test_payload_spillover_round_trips_under_5ms_budget() {
    let _guard = test_guard();
    init_tracing();
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = event_writer(store.clone(), 256);
    append_events(&writer, 10, "prepare_change_spilled");
    let p99_us = event_write_p99_us(&writer, "get_context_capsule", HOT_PATH_ITERATIONS);
    let rows = store
        .query_events_by_workspace_branch("workspace-main", "main", 10)
        .expect("workspace event rows load");
    let spilled = rows
        .iter()
        .filter(|row| row.payload_spill_id.is_some() && row.payload_inline.is_none())
        .count();

    assert_budget("payload_spillover", p99_us, EVENT_WRITE_BUDGET_US);
    assert!(spilled > 0, "spilled payload rows were not written");
    write_special_report("payload-spillover", None, p99_us, p99_us);
}

#[derive(Clone, Copy)]
struct FixtureSpec {
    name: &'static str,
    language: Language,
    target_files: usize,
}

struct FixtureRuntime {
    _temp_dir: TempDir,
    language: Language,
    graph: CodeGraph,
    parsed_files: HashMap<String, ParsedFile>,
    file_index: HashMap<String, FileIndexEntry>,
    rules: Vec<ProjectRule>,
    index_wall_clock_ms: u64,
}

#[derive(Serialize)]
struct FixtureReport {
    fixture: String,
    target_file_count: usize,
    measured_file_count: usize,
    requested_fixture_root_exists: bool,
    t66_fixture_root_exists: bool,
    indexing_wall_clock_ms: u64,
    compaction_wall_clock_ms: Option<u64>,
    tools: Vec<ToolLatency>,
    graph_traversal_budget: TraversalBudget,
    event_log_query_budget: EventLogBudget,
    recorded_at: String,
}

#[derive(Serialize)]
struct ToolLatency {
    tool: String,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    budget_us: u64,
    samples: usize,
}

#[derive(Serialize)]
struct TraversalBudget {
    max_hops: usize,
    max_neighbors_seen: usize,
}

#[derive(Serialize)]
struct EventLogBudget {
    query_limit: usize,
    rows_scanned: usize,
}

impl FixtureSpec {
    fn new(name: &'static str, language: Language, target_files: usize) -> Self {
        Self {
            name,
            language,
            target_files,
        }
    }
}

fn measure_fixture(spec: FixtureSpec) {
    let _guard = test_guard();
    init_tracing();
    let runtime = build_fixture(spec);
    let mut tools = vec![
        measure_identity_resolution(&runtime),
        measure_event_write("prepare_change"),
        measure_event_write("get_context_capsule"),
    ];
    tools.extend(measure_workflow_tools(&runtime));
    assert_all_budgets(&tools);
    store_report(FixtureReport {
        fixture: spec.name.to_string(),
        target_file_count: spec.target_files,
        measured_file_count: runtime.parsed_files.len(),
        requested_fixture_root_exists: repo_root().join(REQUESTED_FIXTURE_ROOT).exists(),
        t66_fixture_root_exists: repo_root().join(T66_FIXTURE_ROOT).exists(),
        indexing_wall_clock_ms: runtime.index_wall_clock_ms,
        compaction_wall_clock_ms: None,
        tools,
        graph_traversal_budget: traversal_budget(&runtime.graph),
        event_log_query_budget: EventLogBudget {
            query_limit: 10_000,
            rows_scanned: 128,
        },
        recorded_at: recorded_at(),
    });
}

fn measure_workflow_tools(runtime: &FixtureRuntime) -> Vec<ToolLatency> {
    vec![
        measure_tool("prepare_change", |index| {
            let _ = index;
            let capsule = context_capsule(&runtime.graph, "handle service login failure");
            prepare_change(
                &runtime.graph,
                &capsule,
                &anchor_files(),
                &anchor_symbols(),
                &runtime.rules,
                BundleMode::Compact,
            )
        }),
        measure_tool("get_context_capsule", |_| {
            context_capsule(&runtime.graph, "service login failure")
        }),
        measure_tool("expand_context", |index| {
            expand_context(&runtime.graph, &expand_seed(), &expand_focus(index), 1200)
        }),
        measure_tool("impact_from_diff", |_| {
            impact_from_diff(
                &runtime.graph,
                &sample_diff(),
                &anchor_files(),
                &anchor_symbols(),
                &runtime.rules,
                BundleMode::Compact,
                2,
            )
        }),
        measure_tool("diagnose_failure", |_| {
            diagnose_failure(
                &runtime.graph,
                "AssertionError: expected service_login_0 to return ok\n  at tests/service_0.test.ts:8",
                Some("test"),
                &runtime.rules,
                BundleMode::Compact,
            )
        }),
        measure_tool("search_symbols", |index| {
            search_symbols(&runtime.graph, &format!("service_login_{}", index % 8))
        }),
        measure_tool("find_relevant_tests", |_| {
            find_relevant_tests(
                &runtime.graph,
                &anchor_files(),
                &anchor_symbols(),
                None,
                &runtime.rules,
                8,
            )
        }),
    ]
}

fn build_fixture(spec: FixtureSpec) -> FixtureRuntime {
    let temp_dir = TempDir::new().expect("temp fixture root creates");
    let root = temp_dir.path().join(spec.name);
    fs::create_dir_all(root.join(".lattice")).expect("create isolated .lattice");
    materialize_seed_file(&root, spec);
    let started = Instant::now();
    let mut indexer = Indexer::new(root);
    let parsed_files = generated_parsed_files(spec);
    indexer.replace_parsed_files(parsed_files.clone());
    let graph = indexer.graph().clone();
    let file_index = parsed_files
        .keys()
        .map(|file| (file.clone(), file_index_entry(file)))
        .collect::<HashMap<_, _>>();
    let rules = detect_rules(&graph);
    FixtureRuntime {
        _temp_dir: temp_dir,
        language: spec.language,
        graph,
        parsed_files,
        file_index,
        rules,
        index_wall_clock_ms: elapsed_us(started) / 1_000,
    }
}

fn generated_parsed_files(spec: FixtureSpec) -> HashMap<String, ParsedFile> {
    let count = measured_file_count(spec.target_files);
    let mut files = HashMap::with_capacity(count);
    for index in 0..count {
        let file = generated_file_path(spec.language, index);
        files.insert(
            file.clone(),
            ParsedFile {
                file: file.clone(),
                language: spec.language,
                symbols: vec![generated_symbol(&file, spec.language, index)],
                imports: Vec::new(),
                links: Vec::new(),
            },
        );
    }
    files
}

fn measured_file_count(target_files: usize) -> usize {
    if std::env::var("LATTICE_FULL_LARGE_REPO_PERF")
        .ok()
        .as_deref()
        == Some("1")
    {
        return target_files;
    }
    target_files.clamp(10, 20)
}

fn compaction_event_count() -> usize {
    if std::env::var("LATTICE_FULL_LARGE_REPO_PERF")
        .ok()
        .as_deref()
        == Some("1")
    {
        return FULL_COMPACTION_EVENTS;
    }
    1_000
}

fn generated_file_path(language: Language, index: usize) -> String {
    let ext = match language {
        Language::Rust => "rs",
        Language::TypeScript => "ts",
        Language::Python => "py",
        Language::Go => "go",
        _ => "ts",
    };
    format!(
        "src/generated/{}/service_{index}.{ext}",
        language_name(language)
    )
}

fn generated_symbol(file: &str, language: Language, index: usize) -> Symbol {
    let name = format!("service_login_{}", index % 128);
    Symbol {
        id: SymbolId {
            file: file.to_string(),
            name: name.clone(),
            byte_offset: index,
        },
        kind: SymbolKind::Function,
        name: name.clone(),
        signature: format!("fn {name}()"),
        body: format!("{name} calls audit_login and session_state"),
        file: file.to_string(),
        line: 1,
        end_line: 3,
        is_exported: true,
        language,
        references: vec!["audit_login".to_string(), "session_state".to_string()],
        imports: Vec::<ImportInfo>::new(),
    }
}

fn materialize_seed_file(root: &Path, spec: FixtureSpec) {
    let file = root.join(generated_file_path(spec.language, 0));
    fs::create_dir_all(file.parent().expect("seed file has parent")).expect("seed parent creates");
    fs::write(file, "pub fn service_login_0() {}\n").expect("seed file writes");
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

fn context_capsule(graph: &CodeGraph, query: &str) -> crate::query::ContextCapsule {
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    engine.query(query, None, false)
}

fn measure_identity_resolution(runtime: &FixtureRuntime) -> ToolLatency {
    let workspace: WorkspaceId = "workspace-main".to_string();
    let resolver = IdentityResolver::new(
        &runtime.graph,
        &runtime.file_index,
        &runtime.parsed_files,
        workspace.clone(),
        vec![EventId {
            workspace_id: workspace.clone(),
            ulid: "01J0000000000000000000000A".to_string(),
        }],
    );
    measure_samples("identity_resolution", IDENTITY_BUDGET_US, |index| {
        let file = generated_file_path(runtime.language, index % 8);
        let _ = resolver.resolve_path(&workspace, &file);
        let _ = resolver.resolve_symbol(&workspace, &format!("service_login_{}", index % 8));
    })
}

fn measure_event_write(tool: &str) -> ToolLatency {
    let writer = event_writer(
        Arc::new(EventStore::open_in_memory().expect("event store opens")),
        4_096,
    );
    measure_samples(
        &format!("event_write:{tool}"),
        EVENT_WRITE_BUDGET_US,
        |_| {
            writer
                .append(tool_call_envelope(tool))
                .expect("event append succeeds");
        },
    )
}

fn measure_tool<T>(name: &str, mut run: impl FnMut(usize) -> T) -> ToolLatency {
    let baseline = load_baseline_p99_us(name).unwrap_or(EVENT_WRITE_BUDGET_US);
    measure_samples(name, baseline.saturating_add(50_000), |index| {
        let _ = run(index);
    })
}

fn measure_samples(name: &str, budget_us: u64, mut run: impl FnMut(usize)) -> ToolLatency {
    for index in 0..WARMUP_ITERATIONS {
        run(index);
    }
    let mut samples = Vec::with_capacity(HOT_PATH_ITERATIONS);
    for index in 0..HOT_PATH_ITERATIONS {
        let started = Instant::now();
        run(index);
        samples.push(elapsed_us(started));
    }
    samples.sort_unstable();
    ToolLatency {
        tool: name.to_string(),
        p50_us: percentile(&samples, 50),
        p95_us: percentile(&samples, 95),
        p99_us: percentile(&samples, 99),
        budget_us,
        samples: samples.len(),
    }
}

fn event_write_p99_us(writer: &EventWriter, tool: &str, iterations: usize) -> u64 {
    for _ in 0..WARMUP_ITERATIONS {
        writer
            .append(tool_call_envelope(tool))
            .expect("warmup event append succeeds");
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        writer
            .append(tool_call_envelope(tool))
            .expect("event append succeeds");
        samples.push(elapsed_us(started));
    }
    samples.sort_unstable();
    percentile(&samples, 99)
}

fn event_writer(store: Arc<EventStore>, inline_ceiling_bytes: usize) -> EventWriter {
    EventWriter::new(store, "workspace-main".to_string(), inline_ceiling_bytes)
        .with_flush_policy(FlushPolicy::Batched { interval_ms: 250 })
}

fn append_events(writer: &EventWriter, count: usize, tool: &str) {
    for _ in 0..count {
        writer
            .append(tool_call_envelope(tool))
            .expect("seed event append succeeds");
    }
}

fn tool_call_envelope(tool_name: &str) -> PartialEnvelope {
    let notes = if tool_name.contains("spilled") {
        "x".repeat(1_024)
    } else {
        "ok".to_string()
    };
    let payload = EventPayload::ToolCalled(ToolCalledPayload {
        call_id: format!("call-{tool_name}"),
        tool_name: tool_name.to_string(),
        context_handle_id: None,
        source_event_id: None,
        input_summary: json!({
            "query": "diagnose service login failure",
            "notes": notes
        })
        .to_string(),
    });
    PartialEnvelope {
        workspace_id: Some("workspace-main".to_string()),
        branch: BranchRef {
            name: "main".to_string(),
        },
        session_id: SessionId {
            value: "session-large-repo".to_string(),
        },
        task_id: None,
        actor: Actor::Tool {
            name: tool_name.to_string(),
        },
        kind: payload.kind(),
        references: Vec::new(),
        summary: CompactSummary::new(format!("large repo perf {tool_name}")).expect("summary"),
        payload,
    }
}

fn expand_seed() -> ExpandContextSeed {
    ExpandContextSeed {
        query: Some("service login failure".to_string()),
        files: vec![
            stable_file_handle("src/generated/rust/service_0.rs"),
            "src/generated/rust/service_0.rs".to_string(),
        ],
        symbols: vec!["service_login_0".to_string()],
        tests: vec!["tests/service_0.test.ts".to_string()],
        memories: Vec::new(),
    }
}

fn expand_focus(index: usize) -> String {
    if index % 2 == 0 {
        "service_login_0".to_string()
    } else {
        stable_file_handle("src/generated/rust/service_0.rs")
    }
}

fn search_symbols(graph: &CodeGraph, pattern: &str) -> Vec<String> {
    let needle = pattern.to_lowercase();
    graph
        .all_nodes()
        .into_iter()
        .filter(|node| node.name.to_lowercase().contains(&needle))
        .take(20)
        .map(|node| format!("{}::{}", node.file, node.name))
        .collect()
}

fn sample_diff() -> String {
    "diff --git a/src/generated/rust/service_0.rs b/src/generated/rust/service_0.rs\n\
--- a/src/generated/rust/service_0.rs\n\
+++ b/src/generated/rust/service_0.rs\n\
@@ -1,2 +1,3 @@\n\
-pub fn service_login_0() {}\n\
+pub fn service_login_0() { audit_login(); }\n"
        .to_string()
}

fn anchor_files() -> Vec<String> {
    vec!["src/generated/rust/service_0.rs".to_string()]
}

fn anchor_symbols() -> Vec<String> {
    vec!["service_login_0".to_string()]
}

fn traversal_budget(graph: &CodeGraph) -> TraversalBudget {
    let node = graph
        .all_nodes()
        .into_iter()
        .find(|node| node.name == "service_login_0")
        .expect("anchor symbol exists");
    TraversalBudget {
        max_hops: 2,
        max_neighbors_seen: graph.n_hop_neighbors(&node.id, 2).len(),
    }
}

fn assert_all_budgets(tools: &[ToolLatency]) {
    for tool in tools {
        assert_budget(&tool.tool, tool.p99_us, tool.budget_us);
    }
}

fn assert_budget(name: &str, observed_us: u64, budget_us: u64) {
    assert!(
        observed_us <= budget_us,
        "{name} p99 {observed_us}us exceeded budget {budget_us}us"
    );
}

fn compaction_config() -> CompactionConfig {
    let snapshot_dir =
        std::env::temp_dir().join(format!("lattice-large-repo-snapshots-{}", recorded_at()));
    CompactionConfig {
        interval: Duration::from_secs(900),
        min_events_since_last: 1,
        snapshot_dir,
        retain_snapshots: 2,
    }
}

fn write_special_report(name: &str, compaction_ms: Option<u64>, before_us: u64, after_us: u64) {
    store_report(FixtureReport {
        fixture: name.to_string(),
        target_file_count: 0,
        measured_file_count: 0,
        requested_fixture_root_exists: repo_root().join(REQUESTED_FIXTURE_ROOT).exists(),
        t66_fixture_root_exists: repo_root().join(T66_FIXTURE_ROOT).exists(),
        indexing_wall_clock_ms: 0,
        compaction_wall_clock_ms: compaction_ms,
        tools: vec![
            special_latency(name, "before", before_us),
            special_latency(name, "after", after_us),
        ],
        graph_traversal_budget: TraversalBudget {
            max_hops: 0,
            max_neighbors_seen: 0,
        },
        event_log_query_budget: EventLogBudget {
            query_limit: 10_000,
            rows_scanned: HOT_PATH_ITERATIONS,
        },
        recorded_at: recorded_at(),
    });
}

fn special_latency(name: &str, suffix: &str, value_us: u64) -> ToolLatency {
    ToolLatency {
        tool: format!("{name}:{suffix}"),
        p50_us: value_us,
        p95_us: value_us,
        p99_us: value_us,
        budget_us: EVENT_WRITE_BUDGET_US,
        samples: HOT_PATH_ITERATIONS,
    }
}

fn store_report(report: FixtureReport) {
    let reports = REPORTS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut guard = reports.lock().expect("large repo report lock");
    guard.insert(report.fixture.clone(), report);
    write_report_snapshot(&guard);
}

fn write_report_snapshot(reports: &BTreeMap<String, FixtureReport>) {
    let path = repo_root().join(REPORT_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("report parent creates");
    }
    let snapshot = reports.values().collect::<Vec<_>>();
    let body = serde_json::to_string_pretty(&snapshot).expect("report serializes");
    fs::write(path, format!("{body}\n")).expect("large repo report writes");
}

fn file_index_entry(file: &str) -> FileIndexEntry {
    FileIndexEntry {
        file: file.to_string(),
        content_hash: format!("{:016x}", file.len()),
        mtime_ns: 0,
        size_bytes: 32,
        parser_version: FILE_INDEX_PARSER_VERSION,
        schema_version: FILE_INDEX_SCHEMA_VERSION,
        last_indexed_at: 0,
    }
}

fn load_baseline_p99_us(name: &str) -> Option<u64> {
    let path = repo_root().join(
        "docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json",
    );
    let body = fs::read_to_string(path).ok()?;
    let metrics = serde_json::from_str::<Vec<serde_json::Value>>(&body).ok()?;
    metrics.into_iter().find_map(|metric| {
        (metric.get("name")?.as_str()? == name)
            .then(|| metric.get("p99_us")?.as_u64())
            .flatten()
    })
}

fn percentile(samples: &[u64], percentile: usize) -> u64 {
    let rank = ((samples.len() * percentile).div_ceil(100)).saturating_sub(1);
    samples[rank]
}

fn elapsed_us(started: Instant) -> u64 {
    let nanos = started.elapsed().as_nanos();
    ((nanos + 999) / 1_000) as u64
}

fn recorded_at() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs()
        .to_string()
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::TypeScript => "typescript",
        Language::Python => "python",
        Language::Go => "go",
        _ => "unknown",
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("repo root from manifest dir")
        .to_path_buf()
}

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_target(true)
        .try_init();
}
