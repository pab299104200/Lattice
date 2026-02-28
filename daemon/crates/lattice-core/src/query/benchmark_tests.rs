// Precision benchmark tests — indexes a real codebase and validates
// query precision against the v31 scorecard (all queries >= 90%).
//
// Run with: cargo test bench_precision -- --ignored --nocapture
//
// Tests gracefully skip if BENCHMARK_DIR doesn't exist on the machine.

use std::path::Path;
use std::sync::OnceLock;

use crate::graph::model::CodeGraph;
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
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query(
        "authentication system JWT login token generation password verification user auth flow",
        None,
        false,
    );

    let keywords = &[
        "auth", "jwt", "token", "login", "password", "verify", "session", "credential", "user",
        "permission", "role", "group",
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
    let mut engine = QueryEngine::new(graph.clone(), None, None);
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
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query("host management and discovery", None, false);

    let keywords = &[
        "host", "discover", "device", "network", "scan", "manage", "inventory", "asset",
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
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query("SNMP polling credential encryption", None, false);

    let keywords = &[
        "snmp", "poll", "credential", "encrypt", "decrypt", "cipher", "secret", "community",
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
        let mut engine = QueryEngine::new(graph.clone(), None, None);
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
