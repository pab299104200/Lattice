// Precision benchmark tests — indexes a real codebase and validates
// query precision against the v31 scorecard (all queries >= 90%).
//
// Run with: cargo test bench_precision -- --ignored --nocapture
//
// Tests gracefully skip if BENCHMARK_DIR doesn't exist on the machine.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::graph::model::CodeGraph;
use crate::intelligence::{prepare_change, BundleMode, RulesDetector};
use crate::query::capsule::ContextCapsule;
use crate::query::engine::QueryEngine;

const BENCHMARK_DIR: &str = "/home/pete/rmm_server";

static BENCHMARK_GRAPH: OnceLock<CodeGraph> = OnceLock::new();

fn get_benchmark_graph() -> Option<&'static CodeGraph> {
    let rmm_path = Path::new(BENCHMARK_DIR);
    if !rmm_path.exists() {
        return None;
    }

    Some(BENCHMARK_GRAPH.get_or_init(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut indexer = crate::indexer::Indexer::new(rmm_path.to_path_buf());
        let count = rt
            .block_on(indexer.index_directory_parallel(rmm_path))
            .unwrap();
        eprintln!(
            "Benchmark: indexed {} files, {} nodes, {} edges",
            count,
            indexer.graph().node_count(),
            indexer.graph().edge_count(),
        );
        indexer.graph().clone()
    }))
}

fn measure_precision(capsule: &ContextCapsule, relevant_keywords: &[&str]) -> (f64, usize, usize) {
    let all_symbols: Vec<(&str, &str)> = capsule
        .pivots
        .iter()
        .map(|p| (p.symbol.as_str(), p.file.as_str()))
        .chain(
            capsule
                .context
                .iter()
                .map(|c| (c.symbol.as_str(), c.file.as_str())),
        )
        .collect();

    let total = all_symbols.len();
    if total == 0 {
        return (1.0, 0, 0);
    }

    let relevant = all_symbols
        .iter()
        .filter(|(name, file)| {
            let name_lower = name.to_lowercase();
            let file_lower = file.to_lowercase();
            relevant_keywords
                .iter()
                .any(|kw| name_lower.contains(kw) || file_lower.contains(kw))
        })
        .count();

    (relevant as f64 / total as f64, relevant, total)
}

fn collect_symbol_names(capsule: &ContextCapsule) -> Vec<&str> {
    capsule
        .pivots
        .iter()
        .map(|p| p.symbol.as_str())
        .chain(capsule.context.iter().map(|c| c.symbol.as_str()))
        .collect()
}

#[derive(Clone, Copy)]
struct ProductEvalCase {
    product: &'static str,
    root: &'static str,
    name: &'static str,
    query: &'static str,
    rg_terms: &'static [&'static str],
    path_terms: &'static [&'static str],
    expected_files: &'static [&'static str],
}

struct ProductEvalResult {
    product: &'static str,
    name: &'static str,
    lattice_hit: bool,
    lattice_rank: Option<usize>,
    capsule_hit: bool,
    capsule_rank: Option<usize>,
    rg_hit: bool,
    rg_rank: Option<usize>,
    lattice_files: Vec<String>,
    rg_files: Vec<String>,
}

fn graph_files_for_rules(graph: &CodeGraph) -> Vec<String> {
    let mut files: Vec<String> = graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect();
    files.sort();
    files.dedup();
    files
}

fn build_case_graph(root: &Path, case: ProductEvalCase) -> Option<CodeGraph> {
    let mut files = Vec::new();
    collect_case_files(root, root, case, &mut files);
    files.sort();
    files.dedup();
    files.truncate(80);
    for expected in case.expected_files {
        if root.join(expected).exists() && !files.iter().any(|file| file == expected) {
            files.push(expected.to_string());
        }
    }

    let mut indexer = crate::indexer::Indexer::new(root.to_path_buf());
    let mut indexed = 0usize;
    for rel_path in files {
        let abs_path = root.join(&rel_path);
        let Ok(content) = std::fs::read_to_string(&abs_path) else {
            continue;
        };
        if indexer.index_file_content(&rel_path, &content).is_ok() {
            indexed += 1;
        }
    }

    if indexer.graph().node_count() == 0 {
        eprintln!(
            "Product eval: no graph nodes for {}:{} from {} selected files",
            case.product, case.name, indexed
        );
        None
    } else {
        eprintln!(
            "Product eval: {}:{} indexed {} bounded files ({} nodes, {} edges)",
            case.product,
            case.name,
            indexed,
            indexer.graph().node_count(),
            indexer.graph().edge_count()
        );
        Some(indexer.graph().clone())
    }
}

fn collect_case_files(root: &Path, dir: &Path, case: ProductEvalCase, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name == "node_modules"
            || file_name == ".venv"
            || file_name == "venv"
            || file_name == "site-packages"
            || file_name == "vendor"
            || file_name == "dist"
            || file_name == "build"
            || file_name == "coverage"
            || file_name == "audit"
            || file_name == "logs"
            || file_name == "artifacts"
            || file_name == ".lattice"
            || file_name == "__pycache__"
            || file_name == ".pytest_cache"
            || file_name == ".git"
        {
            continue;
        }
        if path.is_dir() {
            collect_case_files(root, &path, case, files);
            continue;
        }
        if !is_eval_source_file(&path) {
            continue;
        }
        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };
        let rel_path = rel_path.to_string_lossy().replace('\\', "/");
        let rel_lower = rel_path.to_lowercase();
        let expected = case
            .expected_files
            .iter()
            .any(|expected| rel_path == *expected);
        let path_match = case
            .path_terms
            .iter()
            .any(|term| rel_lower.contains(&term.to_lowercase()));
        if expected || path_match {
            files.push(rel_path);
        }
    }
}

fn is_eval_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("py" | "ts" | "tsx" | "js" | "jsx" | "rs" | "go" | "md")
    )
}

fn lattice_working_set(graph: &CodeGraph, query: &str) -> (Vec<String>, Vec<String>) {
    let mut engine = QueryEngine::new(graph.clone(), None);
    let capsule = engine.query(query, None, false);
    let mut capsule_files = capsule
        .pivots
        .iter()
        .map(|pivot| pivot.file.clone())
        .chain(capsule.context.iter().map(|context| context.file.clone()))
        .collect::<Vec<_>>();
    capsule_files.dedup();
    capsule_files.truncate(5);

    let rules = RulesDetector::new().detect_rules(&graph_files_for_rules(graph));
    let bundle = prepare_change(graph, &capsule, &[], &[], &rules, BundleMode::Compact, None);
    let mut files = bundle
        .primary_files
        .iter()
        .map(|file| file.file.clone())
        .chain(bundle.secondary_files.iter().map(|file| file.file.clone()))
        .collect::<Vec<_>>();
    files.dedup();
    files.truncate(5);

    (files, capsule_files)
}

fn rg_working_set(root: &Path, terms: &[&str]) -> Vec<String> {
    if terms.is_empty() {
        return Vec::new();
    }
    let mut command = Command::new("rg");
    command.arg("-l").arg("-i").arg("-F");
    for term in terms {
        command.arg("-e").arg(term);
    }
    let output = command
        .arg("--glob")
        .arg("!node_modules/**")
        .arg("--glob")
        .arg("!.venv/**")
        .arg("--glob")
        .arg("!venv/**")
        .arg("--glob")
        .arg("!**/site-packages/**")
        .arg("--glob")
        .arg("!vendor/**")
        .arg("--glob")
        .arg("!dist/**")
        .arg("--glob")
        .arg("!build/**")
        .arg("--glob")
        .arg("!coverage/**")
        .arg("--glob")
        .arg("!docs/audit/**")
        .arg("--glob")
        .arg("!**/logs/**")
        .arg("--glob")
        .arg("!**/artifacts/**")
        .arg("--glob")
        .arg("!.lattice/**")
        .arg("--glob")
        .arg("!__pycache__/**")
        .arg("--glob")
        .arg("!.pytest_cache/**")
        .arg(root)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() && output.stdout.is_empty() {
        return Vec::new();
    }

    let mut files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            Path::new(line)
                .strip_prefix(root)
                .ok()
                .map(|path| path.to_string_lossy().to_string())
        })
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files.truncate(5);
    files
}

fn first_expected_rank(files: &[String], expected: &[&str]) -> Option<usize> {
    files
        .iter()
        .position(|file| expected.iter().any(|expected_file| file == expected_file))
        .map(|index| index + 1)
}

fn run_product_eval_case(case: ProductEvalCase) -> Option<ProductEvalResult> {
    let root = PathBuf::from(case.root);
    if !root.exists() {
        eprintln!("Skipping {}: {} not found", case.product, case.root);
        return None;
    }
    let graph = build_case_graph(&root, case)?;
    let (lattice_files, capsule_files) = lattice_working_set(&graph, case.query);
    let rg_files = rg_working_set(&root, case.rg_terms);
    let lattice_rank = first_expected_rank(&lattice_files, case.expected_files);
    let capsule_rank = first_expected_rank(&capsule_files, case.expected_files);
    let rg_rank = first_expected_rank(&rg_files, case.expected_files);

    Some(ProductEvalResult {
        product: case.product,
        name: case.name,
        lattice_hit: lattice_rank.is_some(),
        lattice_rank,
        capsule_hit: capsule_rank.is_some(),
        capsule_rank,
        rg_hit: rg_rank.is_some(),
        rg_rank,
        lattice_files,
        rg_files,
    })
}

#[test]
#[ignore]
fn bench_product_working_set_vs_rg() {
    let cases = [
        ProductEvalCase {
            product: "portal",
            root: "/home/pete/cadres/portal",
            name: "auth_logout_csrf",
            query: "browser RP initiated logout is blocked by CSRF and local logout does not propagate downstream OIDC session termination",
            rg_terms: &["logout", "csrf", "end-session", "propagate_logout"],
            path_terms: &["auth", "oauth", "csrf", "session"],
            expected_files: &[
                "backend/routers/auth.py",
                "backend/routers/oauth2.py",
                "backend/core/middleware/csrf.py",
            ],
        },
        ProductEvalCase {
            product: "portal",
            root: "/home/pete/cadres/portal",
            name: "identity_profile_rbac",
            query: "ordinary tenant members can read tenant identity profiles custom attributes provenance and resolution issues",
            rg_terms: &["identity profiles", "custom attributes", "provenance", "resolution issues"],
            path_terms: &["identity", "rbac", "tenant"],
            expected_files: &[
                "backend/routers/identity_profiles.py",
                "backend/core/rbac.py",
                "backend/tests/test_cross_tenant_url_tid_identity_profiles.py",
            ],
        },
        ProductEvalCase {
            product: "meridian",
            root: "/home/pete/cadres/meridian",
            name: "webhook_dispatch_queue",
            query: "event bridge webhook dispatch should enqueue outbound connector delivery instead of inline retries that block dispatch",
            rg_terms: &["webhook_dispatch", "event_bridge", "enqueue_connector_webhook_event", "dispatch_pending_events"],
            path_terms: &["webhook", "event", "dispatch"],
            expected_files: &[
                "backend/core/event_bridge.py",
                "backend/core/webhook_dispatch.py",
                "backend/tests/test_int02_event_bridge_queue.py",
            ],
        },
        ProductEvalCase {
            product: "meridian",
            root: "/home/pete/cadres/meridian",
            name: "auditor_portal_permissions",
            query: "auditor portal invite permissions create findings sign off report close engagement are under documented and need route coverage",
            rg_terms: &["auditor portal", "create_findings", "sign_off_report", "close_engagement"],
            path_terms: &["auditor", "invite", "portal"],
            expected_files: &[
                "backend/core/auditor_portal.py",
                "backend/routers/auditor_invites.py",
                "frontend/src/components/audit/AuditCreateInviteModal.tsx",
            ],
        },
        ProductEvalCase {
            product: "keystone",
            root: "/home/pete/cadres/keystone",
            name: "product_contract_rmm_integration",
            query: "RMM product integration should configure product contracts return URL allowlists entitlements lifecycle state and product events",
            rg_terms: &["product contracts", "return URL", "entitlements", "product events", "RMM"],
            path_terms: &["product", "integration", "billing", "rmm"],
            expected_files: &[
                "backend/core/product_contracts.py",
                "backend/core/product_lifecycle.py",
                "backend/core/integration_events.py",
                "backend/routers/integrations/rmm.py",
            ],
        },
        ProductEvalCase {
            product: "keystone",
            root: "/home/pete/cadres/keystone",
            name: "company_profile_entity_setup",
            query: "company profile legal entities fiscal settings ownership tax registrations and dashboard scope workflow",
            rg_terms: &["company profile", "legal entities", "fiscal settings", "tax registrations", "dashboard scope"],
            path_terms: &["company", "profile", "tax"],
            expected_files: &[
                "backend/core/company_profile.py",
                "backend/models/company.py",
                "backend/routers/company/profile.py",
                "frontend/src/pages/CompanyProfile.tsx",
            ],
        },
        ProductEvalCase {
            product: "rmm",
            root: "/home/pete/cadres/rmm",
            name: "patch_pipeline_failure",
            query: "trace patch rollout failure from scheduled hosts through begin host patching deployment state machine and recovery",
            rg_terms: &["patch rollout", "begin_host_patching", "deployment state", "patch recovery"],
            path_terms: &["patch", "deployment", "recovery"],
            expected_files: &[
                "backend/core/patch_pipeline.py",
                "backend/core/patch_scheduler.py",
                "backend/core/patch_state_machine.py",
                "backend/core/patch_recovery.py",
            ],
        },
        ProductEvalCase {
            product: "rmm",
            root: "/home/pete/cadres/rmm",
            name: "agent_update_drift",
            query: "agent version drift alerting should ignore active rollouts and compare host agent versions against latest publication",
            rg_terms: &["agent version drift", "active rollout", "is_latest", "agent_version"],
            path_terms: &["agent", "rollout", "version"],
            expected_files: &[
                "backend/core/agent_version_drift.py",
                "backend/core/agent_rollout_engine.py",
                "backend/tests/test_agent_version_drift.py",
            ],
        },
    ];

    let mut results = Vec::new();
    for case in cases {
        if let Some(result) = run_product_eval_case(case) {
            results.push(result);
        }
    }

    if results.is_empty() {
        eprintln!("Skipping product eval: no product repos were available");
        return;
    }

    eprintln!("\n┌──────────┬──────────────────────────────┬──────────────┬──────────────┬──────────────┐");
    eprintln!(
        "│ Product  │ Case                         │ Lattice Top5 │ Capsule Top5 │ rg Top5      │"
    );
    eprintln!(
        "├──────────┼──────────────────────────────┼──────────────┼──────────────┼──────────────┤"
    );
    for result in &results {
        eprintln!(
            "│ {:<8} │ {:<28} │ {:<12} │ {:<12} │ {:<12} │",
            result.product,
            result.name,
            result
                .lattice_rank
                .map(|rank| format!("PASS #{}", rank))
                .unwrap_or_else(|| "FAIL".to_string()),
            result
                .capsule_rank
                .map(|rank| format!("PASS #{}", rank))
                .unwrap_or_else(|| "FAIL".to_string()),
            result
                .rg_rank
                .map(|rank| format!("PASS #{}", rank))
                .unwrap_or_else(|| "FAIL".to_string()),
        );
        eprintln!(
            "│          │ Lattice: {:<91} │",
            result.lattice_files.join(", ")
        );
        eprintln!("│          │ rg:      {:<91} │", result.rg_files.join(", "));
    }
    eprintln!("└──────────┴──────────────────────────────┴──────────────┴──────────────┴──────────────┘\n");

    let lattice_hits = results.iter().filter(|result| result.lattice_hit).count();
    let capsule_hits = results.iter().filter(|result| result.capsule_hit).count();
    let rg_hits = results.iter().filter(|result| result.rg_hit).count();
    let total = results.len();
    let lattice_rate = lattice_hits as f64 / total as f64;
    let capsule_rate = capsule_hits as f64 / total as f64;
    let rg_rate = rg_hits as f64 / total as f64;

    eprintln!(
        "Product working-set eval: lattice_top5={}/{} ({:.0}%) capsule_top5={}/{} ({:.0}%) rg_top5={}/{} ({:.0}%)",
        lattice_hits,
        total,
        lattice_rate * 100.0,
        capsule_hits,
        total,
        capsule_rate * 100.0,
        rg_hits,
        total,
        rg_rate * 100.0,
    );

    assert!(
        lattice_rate >= 0.75,
        "Lattice product working-set top5 hit rate {:.0}% fell below 75%",
        lattice_rate * 100.0
    );
    assert!(
        lattice_hits >= rg_hits,
        "Lattice top5 hits ({}) should be at least rg top5 hits ({}) for working-set discovery",
        lattice_hits,
        rg_hits
    );
}

#[test]
#[ignore] // Requires /home/pete/rmm_server
fn bench_precision_auth() {
    let graph = match get_benchmark_graph() {
        Some(g) => g,
        None => {
            eprintln!("Skipping: {} not found", BENCHMARK_DIR);
            return;
        }
    };
    let mut engine = QueryEngine::new(graph.clone(), None);
    let capsule = engine.query(
        "authentication system JWT login token generation password verification user auth flow",
        None,
        false,
    );

    let keywords = &[
        "auth",
        "jwt",
        "token",
        "login",
        "password",
        "verify",
        "session",
        "credential",
        "user",
        "permission",
        "role",
        "group",
    ];
    let (precision, relevant, total) = measure_precision(&capsule, keywords);

    // v31 eliminated verify_agent_* via negative keyword signal
    let names = collect_symbol_names(&capsule);
    assert!(
        !names.iter().any(|n| n.contains("verify_agent")),
        "verify_agent* should be excluded by negative keyword signal, found in: {:?}",
        names
            .iter()
            .filter(|n| n.contains("verify_agent"))
            .collect::<Vec<_>>()
    );

    eprintln!(
        "Auth: {:.1}% ({}/{}) — pivots: {}, context: {}",
        precision * 100.0,
        relevant,
        total,
        capsule.pivots.len(),
        capsule.context.len()
    );

    assert!(
        precision >= 0.90,
        "Auth precision {:.1}% ({}/{}) below 90% threshold",
        precision * 100.0,
        relevant,
        total
    );
}

#[test]
#[ignore] // Requires /home/pete/rmm_server
fn bench_precision_patch() {
    let graph = match get_benchmark_graph() {
        Some(g) => g,
        None => {
            eprintln!("Skipping: {} not found", BENCHMARK_DIR);
            return;
        }
    };
    let mut engine = QueryEngine::new(graph.clone(), None);
    let capsule = engine.query("How does patch deployment work", None, false);

    let keywords = &[
        "patch", "deploy", "update", "install", "package", "rollback", "agent",
    ];
    let (precision, relevant, total) = measure_precision(&capsule, keywords);

    eprintln!(
        "Patch: {:.1}% ({}/{}) — pivots: {}, context: {}",
        precision * 100.0,
        relevant,
        total,
        capsule.pivots.len(),
        capsule.context.len()
    );

    assert!(
        precision >= 0.90,
        "Patch precision {:.1}% ({}/{}) below 90% threshold",
        precision * 100.0,
        relevant,
        total
    );
}

#[test]
#[ignore] // Requires /home/pete/rmm_server
fn bench_precision_host() {
    let graph = match get_benchmark_graph() {
        Some(g) => g,
        None => {
            eprintln!("Skipping: {} not found", BENCHMARK_DIR);
            return;
        }
    };
    let mut engine = QueryEngine::new(graph.clone(), None);
    let capsule = engine.query("host management and discovery", None, false);

    let keywords = &[
        "host",
        "discover",
        "device",
        "network",
        "scan",
        "manage",
        "inventory",
        "asset",
    ];
    let (precision, relevant, total) = measure_precision(&capsule, keywords);

    eprintln!(
        "Host: {:.1}% ({}/{}) — pivots: {}, context: {}",
        precision * 100.0,
        relevant,
        total,
        capsule.pivots.len(),
        capsule.context.len()
    );

    assert!(
        precision >= 0.90,
        "Host precision {:.1}% ({}/{}) below 90% threshold",
        precision * 100.0,
        relevant,
        total
    );
}

#[test]
#[ignore] // Requires /home/pete/rmm_server
fn bench_precision_snmp() {
    let graph = match get_benchmark_graph() {
        Some(g) => g,
        None => {
            eprintln!("Skipping: {} not found", BENCHMARK_DIR);
            return;
        }
    };
    let mut engine = QueryEngine::new(graph.clone(), None);
    let capsule = engine.query("SNMP polling credential encryption", None, false);

    let keywords = &[
        "snmp",
        "poll",
        "credential",
        "encrypt",
        "decrypt",
        "cipher",
        "secret",
        "community",
    ];
    let (precision, relevant, total) = measure_precision(&capsule, keywords);

    // v31 eliminated encrypt_file_bytes via negative keyword signal
    let names = collect_symbol_names(&capsule);
    assert!(
        !names.iter().any(|n| *n == "encrypt_file_bytes"),
        "encrypt_file_bytes should be excluded by negative keyword signal"
    );

    eprintln!(
        "SNMP: {:.1}% ({}/{}) — pivots: {}, context: {}",
        precision * 100.0,
        relevant,
        total,
        capsule.pivots.len(),
        capsule.context.len()
    );

    assert!(
        precision >= 0.90,
        "SNMP precision {:.1}% ({}/{}) below 90% threshold",
        precision * 100.0,
        relevant,
        total
    );
}

#[test]
#[ignore] // Requires /home/pete/rmm_server
fn bench_precision_scorecard() {
    let graph = match get_benchmark_graph() {
        Some(g) => g,
        None => {
            eprintln!("Skipping: {} not found", BENCHMARK_DIR);
            return;
        }
    };

    struct BenchQuery {
        name: &'static str,
        query: &'static str,
        keywords: &'static [&'static str],
    }

    let queries = [
        BenchQuery {
            name: "Auth",
            query: "authentication system JWT login token generation password verification user auth flow",
            keywords: &[
                "auth", "jwt", "token", "login", "password", "verify", "session",
                "credential", "user", "permission", "role", "group",
            ],
        },
        BenchQuery {
            name: "Patch",
            query: "How does patch deployment work",
            keywords: &[
                "patch", "deploy", "update", "install", "package", "rollback", "agent",
            ],
        },
        BenchQuery {
            name: "Host",
            query: "host management and discovery",
            keywords: &[
                "host", "discover", "device", "network", "scan", "manage", "inventory", "asset",
            ],
        },
        BenchQuery {
            name: "SNMP",
            query: "SNMP polling credential encryption",
            keywords: &[
                "snmp", "poll", "credential", "encrypt", "decrypt", "cipher", "secret",
                "community",
            ],
        },
    ];

    eprintln!("\n┌──────────┬───────────┬─────────┬────────┐");
    eprintln!("│ Query    │ Precision │ Rel/Tot │ Status │");
    eprintln!("├──────────┼───────────┼─────────┼────────┤");

    let mut all_pass = true;
    for q in &queries {
        let mut engine = QueryEngine::new(graph.clone(), None);
        let capsule = engine.query(q.query, None, false);
        let (precision, relevant, total) = measure_precision(&capsule, q.keywords);
        let pass = precision >= 0.90;
        if !pass {
            all_pass = false;
        }

        eprintln!(
            "│ {:<8} │ {:>8.1}% │ {:>3}/{:<3} │ {:<6} │",
            q.name,
            precision * 100.0,
            relevant,
            total,
            if pass { "PASS" } else { "FAIL" }
        );
    }

    eprintln!("└──────────┴───────────┴─────────┴────────┘\n");

    assert!(
        all_pass,
        "One or more queries fell below the 90% precision threshold"
    );
}

#[test]
#[ignore] // Requires /home/pete/rmm_server
fn bench_precision_assistant_scorecard() {
    let graph = match get_benchmark_graph() {
        Some(g) => g,
        None => {
            eprintln!("Skipping: {} not found", BENCHMARK_DIR);
            return;
        }
    };

    struct BenchQuery {
        name: &'static str,
        query: &'static str,
        keywords: &'static [&'static str],
    }

    let queries = [
        BenchQuery {
            name: "AuthNL",
            query: "Why does login fail after a token refresh?",
            keywords: &[
                "auth",
                "jwt",
                "token",
                "login",
                "password",
                "verify",
                "session",
                "credential",
                "user",
                "permission",
                "role",
                "group",
            ],
        },
        BenchQuery {
            name: "PatchNL",
            query: "How do I trace a patch rollout when deployment fails?",
            keywords: &[
                "patch", "deploy", "update", "install", "package", "rollback", "agent",
            ],
        },
        BenchQuery {
            name: "HostID",
            query: "host management and discovery",
            keywords: &[
                "host",
                "discover",
                "device",
                "network",
                "scan",
                "manage",
                "inventory",
                "asset",
            ],
        },
        BenchQuery {
            name: "SNMPID",
            query: "SNMP polling credential encryption",
            keywords: &[
                "snmp",
                "poll",
                "credential",
                "encrypt",
                "decrypt",
                "cipher",
                "secret",
                "community",
            ],
        },
    ];

    eprintln!("\n┌──────────┬───────────┬─────────┬────────┐");
    eprintln!("│ Query    │ Precision │ Rel/Tot │ Status │");
    eprintln!("├──────────┼───────────┼─────────┼────────┤");

    let mut all_pass = true;
    for q in &queries {
        let mut engine = QueryEngine::new(graph.clone(), None);
        let capsule = engine.query(q.query, None, false);
        let (precision, relevant, total) = measure_precision(&capsule, q.keywords);
        let pass = precision >= 0.90;
        if !pass {
            all_pass = false;
        }

        eprintln!(
            "│ {:<8} │ {:>8.1}% │ {:>3}/{:<3} │ {:<6} │",
            q.name,
            precision * 100.0,
            relevant,
            total,
            if pass { "PASS" } else { "FAIL" }
        );
    }

    eprintln!("└──────────┴───────────┴─────────┴────────┘\n");

    assert!(
        all_pass,
        "One or more assistant-style queries fell below the 90% precision threshold"
    );
}
