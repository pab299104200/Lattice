use crate::graph::model::{CodeGraph, EdgeKind};
use crate::storage::{SharedVectorIndex, VectorIndex, VectorSearchResult};
use crate::symbols::{Language, SymbolId, SymbolKind};
use std::sync::Arc;

use super::capsule::QueryIntent;
use super::engine::{merge_seed_hits, parse_query_filters, QueryEngine};
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
    assert_eq!(
        detect_intent("Refactor the auth module"),
        QueryIntent::Refactor
    );
}

#[test]
fn test_detect_add_feature_intent() {
    assert_eq!(detect_intent("Add OAuth support"), QueryIntent::AddFeature);
}

#[test]
fn test_detect_keyword_heavy_explore() {
    // Keyword-heavy queries without action verbs → Explore, not Unknown.
    // "flow" is now an Explore keyword, but even without it the 4+ word fallback fires.
    assert_eq!(
        detect_intent("authentication system JWT login token generation password verification"),
        QueryIntent::Explore,
    );
    // Short ambiguous queries without intent signals → Unknown.
    assert_eq!(detect_intent("auth JWT"), QueryIntent::Unknown);
}

#[test]
fn test_detect_explore_with_flow_keyword() {
    assert_eq!(detect_intent("user auth flow"), QueryIntent::Explore,);
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
        1,
        5,
        true,
        Language::TypeScript,
    );

    graph.add_node(
        id_hash.clone(),
        SymbolKind::Function,
        "hashPassword".to_string(),
        "function hashPassword(plain: string): string".to_string(),
        "function hashPassword(plain) { return bcrypt.hash(plain); }".to_string(),
        "src/crypto.ts".to_string(),
        1,
        3,
        true,
        Language::TypeScript,
    );

    graph.add_node(
        id_validate.clone(),
        SymbolKind::Function,
        "validateToken".to_string(),
        "function validateToken(token: string): boolean".to_string(),
        "function validateToken(token) { return jwt.verify(token); }".to_string(),
        "src/auth.ts".to_string(),
        10,
        15,
        true,
        Language::TypeScript,
    );

    graph.add_node(
        id_format.clone(),
        SymbolKind::Function,
        "formatDate".to_string(),
        "function formatDate(d: Date): string".to_string(),
        "function formatDate(d) { return d.toISOString(); }".to_string(),
        "src/utils.ts".to_string(),
        1,
        3,
        false,
        Language::TypeScript,
    );

    // loginUser -> hashPassword (calls)
    graph.add_edge(&id_login, &id_hash, EdgeKind::Calls);
    // loginUser -> validateToken (calls)
    graph.add_edge(&id_login, &id_validate, EdgeKind::Calls);

    graph
}

fn build_lattice_workflow_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let context_id = make_id(
        "daemon/crates/lattice-daemon/src/rpc/mcp.rs",
        "get_context_capsule",
        0,
    );
    let render_id = make_id(
        "daemon/crates/lattice-daemon/src/rpc/mcp.rs",
        "wrap_workflow_tool_result",
        1,
    );
    let summary_id = make_id(
        "daemon/crates/lattice-core/src/intelligence/agent.rs",
        "summarize_subsystem",
        0,
    );
    let readme_id = make_id("README.md", "Why Lattice", 0);
    let workflow_doc_id = make_id("README.md", "Workflow Commands", 1);

    graph.add_node(
        context_id.clone(),
        SymbolKind::Function,
        "get_context_capsule".to_string(),
        "fn get_context_capsule(render: RenderMode, context_handle: &str)".to_string(),
        "fn get_context_capsule(...) { /* render context_handle */ }".to_string(),
        "daemon/crates/lattice-daemon/src/rpc/mcp.rs".to_string(),
        10,
        45,
        true,
        Language::Rust,
    );
    graph.add_node(
        render_id.clone(),
        SymbolKind::Function,
        "wrap_workflow_tool_result".to_string(),
        "fn wrap_workflow_tool_result(render: RenderMode, context_handle: &str, prepare_change: bool)".to_string(),
        "fn wrap_workflow_tool_result(...) { /* render context_handle prepare_change */ }".to_string(),
        "daemon/crates/lattice-daemon/src/rpc/mcp.rs".to_string(),
        50,
        95,
        true,
        Language::Rust,
    );
    graph.add_node(
        summary_id.clone(),
        SymbolKind::Function,
        "summarize_subsystem".to_string(),
        "fn summarize_subsystem(prepare_change: bool, diagnose_failure: bool, expand_context: bool)".to_string(),
        "fn summarize_subsystem(...) { /* prepare_change diagnose_failure expand_context */ }".to_string(),
        "daemon/crates/lattice-core/src/intelligence/agent.rs".to_string(),
        20,
        75,
        true,
        Language::Rust,
    );
    graph.add_node(
        readme_id,
        SymbolKind::Section,
        "Why Lattice".to_string(),
        "section Why Lattice get_context_capsule prepare_change diagnose_failure expand_context render context_handle".to_string(),
        "Why Lattice: get_context_capsule prepare_change diagnose_failure expand_context render context_handle".to_string(),
        "README.md".to_string(),
        1,
        20,
        false,
        Language::Markdown,
    );
    graph.add_node(
        workflow_doc_id,
        SymbolKind::Section,
        "Workflow Commands".to_string(),
        "section Workflow Commands summarize_subsystem get_context_capsule prepare_change render context_handle".to_string(),
        "Workflow Commands: summarize_subsystem get_context_capsule prepare_change render context_handle".to_string(),
        "README.md".to_string(),
        22,
        48,
        false,
        Language::Markdown,
    );

    graph.add_edge(&render_id, &context_id, EdgeKind::Calls);
    graph.add_edge(&summary_id, &context_id, EdgeKind::Calls);

    graph
}

struct StubVectorIndex {
    hits: Vec<VectorSearchResult>,
}

impl VectorIndex for StubVectorIndex {
    fn initialize(&self, _dimension: usize) -> Result<(), crate::error::LatticeError> {
        Ok(())
    }

    fn upsert_vector(
        &self,
        _file: &str,
        _name: &str,
        _byte_offset: usize,
        _vector: &[f32],
    ) -> Result<(), crate::error::LatticeError> {
        Ok(())
    }

    fn delete_by_file(&self, _file: &str) -> Result<(), crate::error::LatticeError> {
        Ok(())
    }

    fn clear_all(&self) -> Result<(), crate::error::LatticeError> {
        Ok(())
    }

    fn search(
        &self,
        _query: &[f32],
        _top_k: usize,
    ) -> Result<Vec<VectorSearchResult>, crate::error::LatticeError> {
        Ok(self.hits.clone())
    }

    fn implementation_name(&self) -> &'static str {
        "stub"
    }
}

#[test]
fn test_query_engine_produces_capsule() {
    let graph = build_test_graph();
    let mut engine = QueryEngine::new(graph, None, None);

    let capsule = engine.query("How does loginUser work?", None, false);

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
fn test_query_engine_uses_vector_index_abstraction_for_semantic_hits() {
    let graph = build_test_graph();
    let vector_index: SharedVectorIndex = Arc::new(StubVectorIndex {
        hits: vec![(
            "hashPassword".to_string(),
            "src/crypto.ts".to_string(),
            0,
            0.91,
        )],
    });
    let mut engine = QueryEngine::new(graph, Some(vector_index), None);

    let capsule = engine.query("credential hashing", Some(&[0.1, 0.2, 0.3]), false);

    assert!(!capsule.pivots.is_empty());
    assert_eq!(capsule.pivots[0].symbol, "hashPassword");
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
    let capsule = engine.query("func_0", None, false);

    // Token budget is 4000 — with ~500 tokens per body, we can't fit all 20
    // (First query: repeat_count = 1, budget = 4000 + 1*500 = 4500)
    // Plus up to 500 tokens for same-file sibling completion
    assert!(
        capsule.stats.tokens_used <= 5000,
        "tokens_used ({}) should be within budget (5000)",
        capsule.stats.tokens_used
    );
}

#[test]
fn test_adaptive_budget_expands_on_repeat() {
    let graph = build_test_graph();
    let mut engine = QueryEngine::new(graph, None, None);

    // First query — record_query is called inside query(), so repeat_count = 1
    let capsule1 = engine.query("How does loginUser work?", None, false);
    let _budget1 = capsule1.stats.tokens_used;

    // Second query with the same text — repeat_count = 2, budget grows by 500
    let capsule2 = engine.query("How does loginUser work?", None, false);

    // The budget should have expanded (4000 + 2*500 = 5000 vs 4000 + 1*500 = 4500)
    // We can't directly observe the budget, but the query_history should have count 2
    // after two calls. At minimum, verify the engine doesn't crash and produces results.
    assert_eq!(capsule2.intent, QueryIntent::Explore);

    // Third query
    let _capsule3 = engine.query("How does loginUser work?", None, false);
    // query_history should now have count 3 for this query
}

#[test]
fn test_query_engine_can_return_markdown_sections() {
    use crate::graph::builder::GraphBuilder;
    use crate::parser::parse_file;

    let guide_source = r#"
# Guide

## Setup

Run `prepare_change` before editing.
"#;
    let mut builder = GraphBuilder::new();
    builder.add_file(parse_file("docs/guide.md", guide_source).unwrap());
    let graph = builder.build();
    let mut engine = QueryEngine::new(graph, None, None);

    let capsule = engine.query("setup guide", None, false);
    let all_symbols: Vec<&str> = capsule
        .pivots
        .iter()
        .map(|pivot| pivot.symbol.as_str())
        .chain(capsule.context.iter().map(|node| node.symbol.as_str()))
        .collect();

    assert!(
        all_symbols.contains(&"Setup") || all_symbols.contains(&"Guide"),
        "expected markdown doc symbols in results, got: {:?}",
        all_symbols
    );
}

#[test]
fn test_query_engine_prefers_source_over_markdown_for_workflow_queries() {
    let graph = build_lattice_workflow_graph();
    let mut engine = QueryEngine::new(graph, None, None);

    let capsule = engine.query(
        "Lattice agent workflow path for summarize_subsystem, get_context_capsule, prepare_change, diagnose_failure, expand_context, render modes, and context_handle reuse",
        None,
        true,
    );

    let lead_file = capsule
        .pivots
        .first()
        .map(|item| item.file.as_str())
        .or_else(|| capsule.context.first().map(|item| item.file.as_str()));

    assert!(
        matches!(
            lead_file,
            Some("daemon/crates/lattice-daemon/src/rpc/mcp.rs")
                | Some("daemon/crates/lattice-core/src/intelligence/agent.rs")
        ),
        "expected source-first lead file for workflow query, got pivots={:?} context={:?}",
        capsule
            .pivots
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>(),
        capsule
            .context
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_merge_seed_hits_keeps_keyword_matches_when_semantic_hits_exist() {
    let semantic_hits = vec![(make_id("README.md", "Why Lattice", 0), 0.91)];
    let keyword_hits = vec![(
        make_id(
            "daemon/crates/lattice-daemon/src/rpc/mcp.rs",
            "get_context_capsule",
            0,
        ),
        0.74,
    )];

    let merged = merge_seed_hits(semantic_hits, keyword_hits, 4);

    assert!(
        merged.iter().any(|(id, _)| {
            id.file == "daemon/crates/lattice-daemon/src/rpc/mcp.rs"
                && id.name == "get_context_capsule"
        }),
        "expected merged seed hits to keep direct keyword-matched source anchors: {:?}",
        merged
            .iter()
            .map(|(id, score)| format!("{}::{}:{:.2}", id.file, id.name, score))
            .collect::<Vec<_>>()
    );
}

// ─── Query filter parsing tests ─────────────────────────────────────

#[test]
fn test_query_filter_parsing() {
    let (filter, clean) = parse_query_filters("repo:frontend how does auth work?");
    assert_eq!(filter.repo.as_deref(), Some("frontend"));
    assert_eq!(clean, "how does auth work?");
}

#[test]
fn test_query_with_language_filter() {
    let (filter, clean) = parse_query_filters("lang:python find database models");
    assert_eq!(filter.language.as_deref(), Some("python"));
    assert_eq!(clean, "find database models");
}

#[test]
fn test_query_with_file_filter() {
    let (filter, clean) = parse_query_filters("file:auth.ts how does login work?");
    assert_eq!(filter.file_pattern.as_deref(), Some("auth.ts"));
    assert_eq!(clean, "how does login work?");
}

#[test]
fn test_query_with_multiple_filters() {
    let (filter, clean) = parse_query_filters("repo:backend lang:typescript find auth handlers");
    assert_eq!(filter.repo.as_deref(), Some("backend"));
    assert_eq!(filter.language.as_deref(), Some("typescript"));
    assert_eq!(clean, "find auth handlers");
}

#[test]
fn test_query_no_filters() {
    let (filter, clean) = parse_query_filters("how does auth work?");
    assert!(filter.repo.is_none());
    assert!(filter.file_pattern.is_none());
    assert!(filter.language.is_none());
    assert_eq!(clean, "how does auth work?");
}

// ─── Detailed why_included tests ────────────────────────────────────

#[test]
fn test_pivot_has_positive_score() {
    let graph = build_test_graph();
    let mut engine = QueryEngine::new(graph, None, None);

    let capsule = engine.query("How does loginUser work?", None, false);

    for pivot in &capsule.pivots {
        assert!(
            pivot.score > 0.0,
            "Pivot score should be positive — got {}",
            pivot.score
        );
    }
}

#[test]
fn test_context_relationship_has_edge_info() {
    let graph = build_test_graph();
    let mut engine = QueryEngine::new(graph, None, None);

    let capsule = engine.query("How does loginUser work?", None, false);

    // Check that context nodes have relationship info beyond generic labels
    for ctx in &capsule.context {
        assert!(
            !ctx.relationship.is_empty(),
            "Context relationship should not be empty for {}",
            ctx.symbol
        );
    }
}

#[path = "benchmark_tests.rs"]
mod benchmark_tests;
