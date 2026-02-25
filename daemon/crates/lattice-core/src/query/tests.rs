use crate::graph::model::{CodeGraph, EdgeKind};
use crate::symbols::{Language, SymbolId, SymbolKind};

use super::capsule::QueryIntent;
use super::engine::QueryEngine;
use super::intent::detect_intent;

// ─── Intent detection tests ─────────────────────────────────────────

#[test]
fn test_detect_explore_intent() {
    assert_eq!(detect_intent("How does auth work?"), QueryIntent::Explore);
}

#[test]
fn test_detect_fix_bug_intent() {
    assert_eq!(detect_intent("Fix the login bug"), QueryIntent::FixBug);
}

#[test]
fn test_detect_refactor_intent() {
    assert_eq!(detect_intent("Refactor the auth module"), QueryIntent::Refactor);
}

#[test]
fn test_detect_add_feature_intent() {
    assert_eq!(detect_intent("Add OAuth support"), QueryIntent::AddFeature);
}

// ─── Engine tests ───────────────────────────────────────────────────

fn make_id(file: &str, name: &str, offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset: offset,
    }
}

fn build_test_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let id_login = make_id("src/auth.ts", "loginUser", 0);
    let id_hash = make_id("src/crypto.ts", "hashPassword", 0);
    let id_validate = make_id("src/auth.ts", "validateToken", 100);
    let id_format = make_id("src/utils.ts", "formatDate", 0);

    graph.add_node(
        id_login.clone(),
        SymbolKind::Function,
        "loginUser".to_string(),
        "function loginUser(creds: Credentials): Promise<Session>".to_string(),
        "function loginUser(creds) { return hash(creds); }".to_string(),
        "src/auth.ts".to_string(),
        1, 5, true,
        Language::TypeScript,
    );

    graph.add_node(
        id_hash.clone(),
        SymbolKind::Function,
        "hashPassword".to_string(),
        "function hashPassword(plain: string): string".to_string(),
        "function hashPassword(plain) { return bcrypt.hash(plain); }".to_string(),
        "src/crypto.ts".to_string(),
        1, 3, true,
        Language::TypeScript,
    );

    graph.add_node(
        id_validate.clone(),
        SymbolKind::Function,
        "validateToken".to_string(),
        "function validateToken(token: string): boolean".to_string(),
        "function validateToken(token) { return jwt.verify(token); }".to_string(),
        "src/auth.ts".to_string(),
        10, 15, true,
        Language::TypeScript,
    );

    graph.add_node(
        id_format.clone(),
        SymbolKind::Function,
        "formatDate".to_string(),
        "function formatDate(d: Date): string".to_string(),
        "function formatDate(d) { return d.toISOString(); }".to_string(),
        "src/utils.ts".to_string(),
        1, 3, false,
        Language::TypeScript,
    );

    // loginUser -> hashPassword (calls)
    graph.add_edge(&id_login, &id_hash, EdgeKind::Calls);
    // loginUser -> validateToken (calls)
    graph.add_edge(&id_login, &id_validate, EdgeKind::Calls);

    graph
}

#[test]
fn test_query_engine_produces_capsule() {
    let graph = build_test_graph();
    let mut engine = QueryEngine::new(graph, None, None);

    let capsule = engine.query("How does loginUser work?", None);

    assert_eq!(capsule.intent, QueryIntent::Explore);
    assert!(!capsule.query.is_empty());

    // Should find loginUser and related nodes
    let all_symbols: Vec<&str> = capsule
        .pivots
        .iter()
        .map(|p| p.symbol.as_str())
        .chain(capsule.context.iter().map(|c| c.symbol.as_str()))
        .collect();

    // loginUser should be included (matches the keyword "loginuser")
    assert!(
        all_symbols.contains(&"loginUser"),
        "Expected loginUser in results, got: {:?}",
        all_symbols
    );

    // formatDate should NOT be included (unconnected and no keyword match)
    assert!(
        !all_symbols.contains(&"formatDate"),
        "formatDate should be excluded, got: {:?}",
        all_symbols
    );
}

#[test]
fn test_query_engine_token_budget() {
    // Build a graph with 20 nodes that have large bodies
    let mut graph = CodeGraph::new();
    let large_body = "x".repeat(2000); // 2000 chars = ~500 tokens each

    for i in 0..20 {
        let name = format!("func_{}", i);
        let id = make_id("src/big.ts", &name, i * 100);
        graph.add_node(
            id,
            SymbolKind::Function,
            name.clone(),
            format!("function {}(): void", name),
            large_body.clone(),
            "src/big.ts".to_string(),
            i + 1,
            i + 10,
            true,
            Language::TypeScript,
        );
    }

    // Add edges so they're all connected (chain)
    for i in 0..19 {
        let from_name = format!("func_{}", i);
        let to_name = format!("func_{}", i + 1);
        let from_id = make_id("src/big.ts", &from_name, i * 100);
        let to_id = make_id("src/big.ts", &to_name, (i + 1) * 100);
        graph.add_edge(&from_id, &to_id, EdgeKind::Calls);
    }

    let mut engine = QueryEngine::new(graph, None, None);
    let capsule = engine.query("func_0", None);

    // Token budget is 4000 — with ~500 tokens per body, we can't fit all 20
    assert!(
        capsule.stats.tokens_used <= 4000,
        "tokens_used ({}) should be within budget (4000)",
        capsule.stats.tokens_used
    );
}
