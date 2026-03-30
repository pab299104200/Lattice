// Agent-workflow benchmark tests — index the current Lattice repo and score the
// workflow tools against realistic maintenance tasks.
//
// Run with:
// cargo test workflow_bench -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::graph::model::{CodeGraph, EdgeKind};
use crate::intelligence::{
    diagnose_failure, find_relevant_tests, get_working_set_context, impact_from_diff,
    prepare_change, BundleMode, RulesDetector,
};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use crate::query::{CapsuleStats, ContextCapsule, QueryEngine, QueryIntent};
use crate::symbols::{Language, SymbolId, SymbolKind};
use serde_json::json;

const DEFAULT_WORKFLOW_CALLS: usize = 1;
const CHARS_PER_TOKEN_ESTIMATE: f64 = 4.0;

static BENCHMARK_GRAPH: OnceLock<CodeGraph> = OnceLock::new();

#[derive(Clone, Copy)]
enum WorkflowTool {
    PrepareChange,
    ImpactFromDiff,
    WorkingSet,
    DiagnoseFailure,
    MemoryRecall,
}

impl WorkflowTool {
    fn as_str(self) -> &'static str {
        match self {
            Self::PrepareChange => "prepare_change",
            Self::ImpactFromDiff => "impact_from_diff",
            Self::WorkingSet => "get_working_set_context",
            Self::DiagnoseFailure => "diagnose_failure",
            Self::MemoryRecall => "memory_recall",
        }
    }
}

struct WorkflowResult {
    tool: WorkflowTool,
    payload_bytes: usize,
    estimated_tokens: usize,
    calls_saved: usize,
    top3_hit: bool,
    target_hit: bool,
    stale_precision: Option<f64>,
}

fn benchmark_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn get_benchmark_graph() -> Option<&'static CodeGraph> {
    let root = benchmark_root();
    if !root.exists() {
        return None;
    }

    Some(BENCHMARK_GRAPH.get_or_init(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut indexer = crate::indexer::Indexer::new(root.clone());
        let count = rt
            .block_on(indexer.index_directory_parallel(&root))
            .unwrap();
        eprintln!(
            "Workflow benchmark: indexed {} files, {} nodes, {} edges",
            count,
            indexer.graph().node_count(),
            indexer.graph().edge_count(),
        );
        indexer.graph().clone()
    }))
}

fn graph_files(graph: &CodeGraph) -> Vec<String> {
    let mut files: Vec<String> = graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect();
    files.sort();
    files.dedup();
    files
}

fn project_rules(graph: &CodeGraph) -> Vec<crate::intelligence::ProjectRule> {
    RulesDetector::new().detect_rules(&graph_files(graph))
}

fn estimate_tokens(payload_bytes: usize) -> usize {
    (payload_bytes as f64 / CHARS_PER_TOKEN_ESTIMATE).ceil() as usize
}

fn benchmark_symbol_line(graph: &CodeGraph, file: &str, symbol: &str) -> usize {
    graph
        .all_nodes()
        .into_iter()
        .find(|node| node.file == file && (node.name == symbol || node.name.ends_with(symbol)))
        .map(|node| node.line)
        .unwrap_or_else(|| panic!("Benchmark symbol {} not found in {}", symbol, file))
}

fn make_id(file: &str, name: &str, offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset: offset,
    }
}

fn build_certificate_guardrail_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let cert_id = make_id("routers/certificates.py", "_verify_org_access", 0);
    let shared_id = make_id("routers/compliance_mgmt/_shared.py", "_verify_org_access", 0);
    let policy_id = make_id("routers/certificates.py", "upsert_renewal_policy", 1);
    let cert_model_id = make_id("models/certificates.py", "CertificateRenewalPolicy", 0);
    let account_id = make_id("models/account.py", "Organization", 0);
    let isolation_test_id = make_id(
        "tests/test_certificate_tenant_isolation.py",
        "TestCrossAccountRenewalPolicy.test_put_policy_foreign_org_returns_404",
        0,
    );
    let renewal_test_id = make_id("tests/test_cert_renewal.py", "test_put_renewal_policy", 0);
    let noisy_test_id = make_id(
        "tests/test_security_hardening.py",
        "test_verify_access_policy",
        0,
    );

    graph.add_node(
        cert_id.clone(),
        SymbolKind::Function,
        "_verify_org_access".to_string(),
        "def _verify_org_access(perms, current_user, org_id, db)".to_string(),
        "def _verify_org_access(...): raise HTTPException(status_code=403)".to_string(),
        "routers/certificates.py".to_string(),
        66,
        84,
        false,
        Language::Python,
    );
    graph.add_node(
        shared_id.clone(),
        SymbolKind::Function,
        "_verify_org_access".to_string(),
        "def _verify_org_access(perms, org_id)".to_string(),
        "def _verify_org_access(...): raise HTTPException(status_code=404)".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        14,
        20,
        false,
        Language::Python,
    );
    graph.add_node(
        policy_id.clone(),
        SymbolKind::Function,
        "upsert_renewal_policy".to_string(),
        "async def upsert_renewal_policy(body)".to_string(),
        "async def upsert_renewal_policy(...): _verify_org_access(...)".to_string(),
        "routers/certificates.py".to_string(),
        365,
        430,
        true,
        Language::Python,
    );
    graph.add_node(
        cert_model_id.clone(),
        SymbolKind::Class,
        "CertificateRenewalPolicy".to_string(),
        "class CertificateRenewalPolicy".to_string(),
        "class CertificateRenewalPolicy: pass".to_string(),
        "models/certificates.py".to_string(),
        20,
        80,
        true,
        Language::Python,
    );
    graph.add_node(
        account_id.clone(),
        SymbolKind::Class,
        "Organization".to_string(),
        "class Organization".to_string(),
        "class Organization: pass".to_string(),
        "models/account.py".to_string(),
        98,
        180,
        true,
        Language::Python,
    );
    graph.add_node(
        isolation_test_id.clone(),
        SymbolKind::Method,
        "TestCrossAccountRenewalPolicy.test_put_policy_foreign_org_returns_404".to_string(),
        "def test_put_policy_foreign_org_returns_404(self)".to_string(),
        "def test_put_policy_foreign_org_returns_404(self): pass".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        357,
        373,
        false,
        Language::Python,
    );
    graph.add_node(
        renewal_test_id.clone(),
        SymbolKind::Function,
        "test_put_renewal_policy".to_string(),
        "def test_put_renewal_policy()".to_string(),
        "def test_put_renewal_policy(): pass".to_string(),
        "tests/test_cert_renewal.py".to_string(),
        545,
        560,
        false,
        Language::Python,
    );
    graph.add_node(
        noisy_test_id.clone(),
        SymbolKind::Function,
        "test_verify_access_policy".to_string(),
        "def test_verify_access_policy()".to_string(),
        "def test_verify_access_policy(): pass".to_string(),
        "tests/test_security_hardening.py".to_string(),
        12,
        24,
        false,
        Language::Python,
    );

    graph.add_edge(&policy_id, &cert_id, EdgeKind::Calls);
    graph.add_edge(&policy_id, &cert_model_id, EdgeKind::Calls);
    graph.add_edge(&cert_id, &account_id, EdgeKind::Calls);
    graph.add_edge(&policy_id, &account_id, EdgeKind::Calls);
    graph.add_edge(&isolation_test_id, &policy_id, EdgeKind::Calls);
    graph.add_edge(&renewal_test_id, &policy_id, EdgeKind::Calls);

    graph
}

fn benchmark_memory(
    session_id: &str,
    content: &str,
    memory_type: MemoryType,
    linked_symbols: &[&str],
    linked_files: &[&str],
) -> Memory {
    Memory {
        id: String::new(),
        session_id: session_id.to_string(),
        content: content.to_string(),
        memory_type,
        scope: MemoryScope::Repo,
        confidence: 1.0,
        linked_symbols: linked_symbols.iter().map(|value| (*value).to_string()).collect(),
        linked_files: linked_files.iter().map(|value| (*value).to_string()).collect(),
        workspace_id: Some("lattice-benchmark".to_string()),
        branch: Some("main".to_string()),
        refresh_key: None,
        source_query: None,
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
    }
}

fn bench_prepare_change(graph: &CodeGraph) -> WorkflowResult {
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query("fix memory recall for new session", None, false);
    let rules = project_rules(graph);
    let report = prepare_change(
        graph,
        &capsule,
        &["daemon/crates/lattice-core/src/memory/store.rs".to_string()],
        &["search_across_sessions".to_string()],
        &rules,
        BundleMode::Compact,
    );

    WorkflowResult {
        tool: WorkflowTool::PrepareChange,
        payload_bytes: serde_json::to_vec(&report).unwrap().len(),
        estimated_tokens: estimate_tokens(serde_json::to_vec(&report).unwrap().len()),
        calls_saved: 4usize.saturating_sub(DEFAULT_WORKFLOW_CALLS),
        top3_hit: report
            .primary_files
            .iter()
            .take(3)
            .any(|item| item.file == "daemon/crates/lattice-core/src/memory/store.rs"),
        target_hit: report.symbols.iter().any(|item| {
            item.symbol == "search_across_sessions"
                || item.symbol.ends_with(".search_across_sessions")
        }),
        stale_precision: None,
    }
}

fn bench_impact_from_diff(graph: &CodeGraph) -> WorkflowResult {
    let rules = project_rules(graph);
    let file = "daemon/crates/lattice-core/src/memory/store.rs";
    let line = benchmark_symbol_line(graph, file, "search_across_sessions");
    let diff = format!(
        "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -{line},1 +{line},1 @@\n-    pub fn search_across_sessions(\n+    pub fn search_across_sessions(\n",
    );
    let report = impact_from_diff(
        graph,
        &diff,
        &[],
        &["search_across_sessions".to_string()],
        &rules,
        BundleMode::Compact,
        2,
    );

    WorkflowResult {
        tool: WorkflowTool::ImpactFromDiff,
        payload_bytes: serde_json::to_vec(&report).unwrap().len(),
        estimated_tokens: estimate_tokens(serde_json::to_vec(&report).unwrap().len()),
        calls_saved: 5usize.saturating_sub(DEFAULT_WORKFLOW_CALLS),
        top3_hit: report
            .changed_files
            .iter()
            .take(3)
            .any(|item| item.file == "daemon/crates/lattice-core/src/memory/store.rs"),
        target_hit: report.changed_symbols.iter().any(|item| {
            item.symbol == "search_across_sessions"
                || item.symbol.ends_with(".search_across_sessions")
        }),
        stale_precision: None,
    }
}

fn bench_working_set(graph: &CodeGraph) -> WorkflowResult {
    let rules = project_rules(graph);
    let report = get_working_set_context(
        graph,
        &["daemon/crates/lattice-core/src/memory/store.rs".to_string()],
        &["search_across_sessions".to_string()],
        Some("memory recall regression"),
        &[],
        &rules,
        BundleMode::Compact,
    );

    WorkflowResult {
        tool: WorkflowTool::WorkingSet,
        payload_bytes: serde_json::to_vec(&report).unwrap().len(),
        estimated_tokens: estimate_tokens(serde_json::to_vec(&report).unwrap().len()),
        calls_saved: 3usize.saturating_sub(DEFAULT_WORKFLOW_CALLS),
        top3_hit: report
            .files
            .iter()
            .take(3)
            .any(|item| item.file == "daemon/crates/lattice-core/src/memory/store.rs"),
        target_hit: report
            .active_symbols
            .iter()
            .chain(report.nearby_symbols.iter())
            .any(|item| {
                item.symbol == "search_across_sessions"
                    || item.symbol.ends_with(".search_across_sessions")
            }),
        stale_precision: None,
    }
}

fn bench_diagnose_failure(graph: &CodeGraph) -> WorkflowResult {
    let rules = project_rules(graph);
    let file = "daemon/crates/lattice-core/src/memory/store.rs";
    let line = benchmark_symbol_line(graph, file, "search_across_sessions");
    let failure = format!(
        "{file}:{line}:9 error: search_across_sessions failed during session recall"
    );
    let report = diagnose_failure(
        graph,
        &failure,
        Some("runtime"),
        &rules,
        BundleMode::Compact,
    );

    WorkflowResult {
        tool: WorkflowTool::DiagnoseFailure,
        payload_bytes: serde_json::to_vec(&report).unwrap().len(),
        estimated_tokens: estimate_tokens(serde_json::to_vec(&report).unwrap().len()),
        calls_saved: 4usize.saturating_sub(DEFAULT_WORKFLOW_CALLS),
        top3_hit: report
            .suspects
            .iter()
            .take(3)
            .any(|item| item.file == "daemon/crates/lattice-core/src/memory/store.rs"),
        target_hit: report.suspects.iter().any(|item| {
            item.symbol == "search_across_sessions"
                || item.symbol.ends_with(".search_across_sessions")
        }),
        stale_precision: None,
    }
}

fn bench_memory_recall() -> WorkflowResult {
    let store = MemoryStore::open_in_memory().expect("Failed to open benchmark memory store");
    let changed_file = "daemon/crates/lattice-core/src/memory/store.rs";

    let recall_id = store
        .store(benchmark_memory(
            "session-prev",
            "search_across_sessions is the main recall path for previous-session memories in new sessions",
            MemoryType::Pattern,
            &["search_across_sessions"],
            &[],
        ))
        .expect("Failed to store relevant recall memory");

    store
        .store(benchmark_memory(
            "session-prev",
            "Old recall behavior note for store.rs before the busy timeout and WAL fixes",
            MemoryType::Observation,
            &[],
            &[changed_file],
        ))
        .expect("Failed to store stale benchmark memory");

    store
        .store(benchmark_memory(
            "session-prev",
            "Sidebar action layout note unrelated to memory recall",
            MemoryType::Observation,
            &[],
            &["extension/src/sidebar.ts"],
        ))
        .expect("Failed to store unrelated benchmark memory");

    store
        .mark_stale_by_file(changed_file, "store.rs changed")
        .expect("Failed to mark stale benchmark memories");

    let recalled = store
        .search_across_sessions("previous session recall new session", Some("session-current"), 5)
        .expect("Failed to run recall benchmark");
    let stale = store
        .list_stale(None, 10)
        .expect("Failed to list stale benchmark memories");

    let stale_precision = if stale.is_empty() {
        1.0
    } else {
        stale
            .iter()
            .filter(|memory| memory.linked_files.iter().any(|file| file == changed_file))
            .count() as f64
            / stale.len() as f64
    };

    let payload = json!({
        "recalled": recalled,
        "stale": stale,
    });
    let payload_bytes = serde_json::to_vec(&payload).unwrap().len();

    WorkflowResult {
        tool: WorkflowTool::MemoryRecall,
        payload_bytes,
        estimated_tokens: estimate_tokens(payload_bytes),
        calls_saved: 2usize.saturating_sub(DEFAULT_WORKFLOW_CALLS),
        top3_hit: recalled.iter().take(3).any(|memory| memory.id == recall_id),
        target_hit: recalled.iter().any(|memory| memory.id == recall_id),
        stale_precision: Some(stale_precision),
    }
}

#[test]
fn workflow_guardrail_certificate_noise_case() {
    let graph = build_certificate_guardrail_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "models/certificates.py".to_string(),
        "models/account.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
        "tests/test_security_hardening.py".to_string(),
    ]);

    let diagnosis = diagnose_failure(
        &graph,
        "tests/test_certificate_tenant_isolation.py:362: AssertionError: expected 404, got 403\nrouters/certificates.py:84: HTTPException(status_code=403)\nrouters/certificates.py:380: _verify_org_access(...)",
        Some("test"),
        &rules,
        BundleMode::Compact,
    );
    assert_eq!(
        diagnosis.suspects.first().map(|item| item.file.as_str()),
        Some("routers/certificates.py")
    );
    assert!(
        diagnosis
            .suspects
            .iter()
            .any(|item| item.symbol == "upsert_renewal_policy"),
        "expected endpoint suspect from second line reference: {:?}",
        diagnosis
            .suspects
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
    assert!(
        diagnosis
            .tests
            .iter()
            .take(3)
            .any(|item| item.file == "tests/test_cert_renewal.py"),
        "expected renewal regression test near the top of diagnosis suggestions: {:?}",
        diagnosis
            .tests
            .iter()
            .map(|item| format!("{}:{}", item.file, item.confidence))
            .collect::<Vec<_>>()
    );
    assert!(
        diagnosis
            .tests
            .iter()
            .all(|item| item.file != "tests/test_security_hardening.py"),
        "expected generic security hardening test to stay out of diagnosis suggestions: {:?}",
        diagnosis
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );

    let tests = find_relevant_tests(
        &graph,
        &["routers/certificates.py".to_string()],
        &[
            "_verify_org_access".to_string(),
            "upsert_renewal_policy".to_string(),
            "RenewalPolicyRequest".to_string(),
        ],
        None,
        &rules,
        6,
    );
    assert!(
        tests
            .tests
            .iter()
            .take(3)
            .any(|item| item.file == "tests/test_certificate_tenant_isolation.py"),
        "expected certificate isolation test near the top of relevant tests: {:?}",
        tests
            .tests
            .iter()
            .map(|item| format!("{}:{}", item.file, item.confidence))
            .collect::<Vec<_>>()
    );
    assert!(
        tests
            .tests
            .iter()
            .take(3)
            .any(|item| item.file == "tests/test_cert_renewal.py"),
        "expected certificate renewal test near the top of relevant tests: {:?}",
        tests
            .tests
            .iter()
            .map(|item| format!("{}:{}", item.file, item.confidence))
            .collect::<Vec<_>>()
    );
    assert!(
        tests
            .tests
            .iter()
            .all(|item| item.file != "tests/test_security_hardening.py"),
        "expected generic security hardening test to stay out of relevant tests: {:?}",
        tests
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );

    let bundle = prepare_change(
        &graph,
        &ContextCapsule {
            query: "Return 404 instead of 403 for cross-org organization access in certificate renewal policy endpoints".to_string(),
            intent: QueryIntent::FixBug,
            pivots: vec![],
            context: vec![],
            memories: vec![],
            stats: CapsuleStats {
                tokens_used: 0,
                tokens_saved: 0,
                nodes_evaluated: 0,
                nodes_included: 0,
                engine_version: "test".to_string(),
                seed_count: 0,
                seed_symbols: vec![],
            },
        },
        &["routers/certificates.py".to_string()],
        &["_verify_org_access".to_string(), "upsert_renewal_policy".to_string()],
        &rules,
        BundleMode::Compact,
    );
    assert_eq!(
        bundle.primary_files.first().map(|item| item.file.as_str()),
        Some("routers/certificates.py")
    );
    let cert_model_index = bundle
        .primary_files
        .iter()
        .position(|item| item.file == "models/certificates.py");
    let account_model_index = bundle
        .primary_files
        .iter()
        .position(|item| item.file == "models/account.py");
    assert!(
        cert_model_index.zip(account_model_index).map(|(cert, account)| cert < account).unwrap_or(true),
        "expected certificate-local model to outrank broad account model: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| format!("{}:{}", item.file, item.score))
            .collect::<Vec<_>>()
    );
}

#[test]
#[ignore]
fn workflow_bench_scorecard() {
    let graph = match get_benchmark_graph() {
        Some(graph) => graph,
        None => {
            eprintln!("Skipping workflow benchmark: repo root not found");
            return;
        }
    };

    let results = vec![
        bench_prepare_change(graph),
        bench_impact_from_diff(graph),
        bench_working_set(graph),
        bench_diagnose_failure(graph),
        bench_memory_recall(),
    ];

    eprintln!(
        "\n┌──────────────────────────┬──────────────┬────────────┬────────────┬──────────┬────────────┬────────────┐"
    );
    eprintln!(
        "│ Tool                     │ Payload (B)  │ Est Tokens │ Calls Saved│ Top3 Hit │ Target Hit │ Stale Prec │"
    );
    eprintln!(
        "├──────────────────────────┼──────────────┼────────────┼────────────┼──────────┼────────────┼────────────┤"
    );

    let mut top3_hits = 0usize;
    let mut target_hits = 0usize;
    let mut payload_total = 0usize;
    let mut token_total = 0usize;
    let mut calls_saved_total = 0usize;
    let mut stale_precision_total = 0.0f64;
    let mut stale_precision_count = 0usize;

    for result in &results {
        if result.top3_hit {
            top3_hits += 1;
        }
        if result.target_hit {
            target_hits += 1;
        }
        payload_total += result.payload_bytes;
        token_total += result.estimated_tokens;
        calls_saved_total += result.calls_saved;
        if let Some(stale_precision) = result.stale_precision {
            stale_precision_total += stale_precision;
            stale_precision_count += 1;
        }

        eprintln!(
            "│ {:<24} │ {:>12} │ {:>10} │ {:>10} │ {:<8} │ {:<10} │ {:>10} │",
            result.tool.as_str(),
            result.payload_bytes,
            result.estimated_tokens,
            result.calls_saved,
            if result.top3_hit { "PASS" } else { "FAIL" },
            if result.target_hit { "PASS" } else { "FAIL" },
            result
                .stale_precision
                .map(|value| format!("{:.0}%", value * 100.0))
                .unwrap_or_else(|| "--".to_string()),
        );
    }

    let top3_hit_rate = top3_hits as f64 / results.len() as f64;
    let target_hit_rate = target_hits as f64 / results.len() as f64;
    let average_payload = payload_total as f64 / results.len() as f64;
    let average_tokens = token_total as f64 / results.len() as f64;
    let average_calls_saved = calls_saved_total as f64 / results.len() as f64;
    let average_stale_precision = if stale_precision_count == 0 {
        1.0
    } else {
        stale_precision_total / stale_precision_count as f64
    };

    eprintln!(
        "├──────────────────────────┴──────────────┴────────────┴────────────┴──────────┴────────────┴────────────┤"
    );
    eprintln!(
        "  top3_hit_rate={:.0}% | target_hit_rate={:.0}% | avg_payload={:.0}B | avg_tokens={:.0} | avg_calls_saved={:.1} | stale_precision={:.0}%",
        top3_hit_rate * 100.0,
        target_hit_rate * 100.0,
        average_payload,
        average_tokens,
        average_calls_saved,
        average_stale_precision * 100.0,
    );

    assert!(
        top3_hit_rate >= 0.75,
        "top3 hit rate {:.0}% fell below 75%",
        top3_hit_rate * 100.0
    );
    assert!(
        target_hit_rate >= 0.75,
        "target hit rate {:.0}% fell below 75%",
        target_hit_rate * 100.0
    );
    assert!(
        average_stale_precision >= 0.80,
        "stale precision {:.0}% fell below 80%",
        average_stale_precision * 100.0
    );
    assert!(
        average_payload <= 2000.0,
        "average payload {:.0}B drifted above the ultra-compact target",
        average_payload
    );
    assert!(
        average_tokens <= 500.0,
        "average tokens {:.0} drifted above the ultra-compact target",
        average_tokens
    );
}
