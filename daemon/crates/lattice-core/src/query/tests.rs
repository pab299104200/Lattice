use crate::graph::model::{CodeGraph, EdgeKind};
use crate::storage::{SharedVectorIndex, VectorIndex, VectorScope, VectorSearchResult};
use crate::symbols::{Language, SymbolId, SymbolKind};
use std::sync::Arc;

use super::capsule::QueryIntent;
use super::engine::{
    merge_seed_hits, parse_query_filters, query_prefers_markdown_results, QueryAdmission,
    QueryAdmissionError, QueryEngine,
};
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

// ─── Query isolation and bounded admission tests ───────────────────

#[test]
fn query_snapshot_keeps_one_graph_generation_after_live_publication() {
    let mut live = QueryEngine::new(build_test_graph(), None, None);
    let snapshot = live.snapshot();
    let graph_snapshot = live.graph_snapshot();

    let mut replacement = CodeGraph::new();
    replacement.add_node(
        make_id("src/replacement.ts", "replacement", 0),
        SymbolKind::Function,
        "replacement".to_string(),
        "function replacement(): void".to_string(),
        "function replacement() {}".to_string(),
        "src/replacement.ts".to_string(),
        1,
        1,
        true,
        Language::TypeScript,
    );
    live.update_graph(replacement);

    assert_eq!(live.graph().stats().node_count, 1);
    assert_eq!(snapshot.graph().stats().node_count, 4);
    assert_eq!(graph_snapshot.stats().node_count, 4);
    assert!(snapshot.find_symbol("loginUser").is_some());
    assert!(snapshot.find_symbol("replacement").is_none());
}

#[test]
fn query_snapshot_shares_adaptive_history_without_sharing_graph_publication() {
    let mut live = QueryEngine::new(build_test_graph(), None, None);
    let mut snapshot = live.snapshot();

    live.query("How does loginUser work?", None, false);
    snapshot.query("How does loginUser work?", None, false);

    assert_eq!(live.recorded_query_count("How does loginUser work?"), 2);
}

#[test]
fn query_admission_rejects_at_fixed_capacity_without_queueing() {
    let admission = QueryAdmission::new(2).expect("valid capacity");
    let first = admission.try_acquire().expect("first slot");
    let second = admission.try_acquire().expect("second slot");

    assert_eq!(admission.active_jobs(), 2);
    assert!(matches!(
        admission.try_acquire(),
        Err(QueryAdmissionError::CapacityExhausted { capacity: 2 })
    ));

    drop(first);
    assert_eq!(admission.active_jobs(), 1);
    let third = admission.try_acquire().expect("released slot is reusable");
    assert_eq!(admission.active_jobs(), 2);

    drop(second);
    drop(third);
    assert_eq!(admission.active_jobs(), 0);
}

#[test]
fn query_admission_rejects_zero_capacity() {
    assert!(matches!(
        QueryAdmission::new(0),
        Err(QueryAdmissionError::InvalidCapacity)
    ));
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

fn build_semantic_identifier_rerank_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let target = make_id("src/auth/session_manager.ts", "rotateSessionToken", 10);
    let target_helper = make_id("src/auth/session_manager.ts", "authorizeSessionContext", 60);
    let decoy = make_id("src/cache/reconcile.ts", "reconcileCacheEntries", 20);
    let decoy_helper = make_id("src/cache/reconcile.ts", "pruneCacheShard", 70);

    graph.add_node(
        target.clone(),
        SymbolKind::Function,
        "rotateSessionToken".to_string(),
        "fn rotateSessionToken(ctx: AuthCtx) -> SessionToken".to_string(),
        "fn rotateSessionToken(ctx) { return issue_token(ctx.user_id); }".to_string(),
        "src/auth/session_manager.ts".to_string(),
        10,
        28,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        target_helper.clone(),
        SymbolKind::Function,
        "authorizeSessionContext".to_string(),
        "fn authorizeSessionContext(ctx: AuthCtx) -> bool".to_string(),
        "fn authorizeSessionContext(ctx) { return ctx.org_id > 0; }".to_string(),
        "src/auth/session_manager.ts".to_string(),
        60,
        76,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        decoy.clone(),
        SymbolKind::Function,
        "reconcileCacheEntries".to_string(),
        "fn reconcileCacheEntries(cache: Cache) -> usize".to_string(),
        "fn reconcileCacheEntries(cache) { return cache.compact(); }".to_string(),
        "src/cache/reconcile.ts".to_string(),
        20,
        42,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        decoy_helper.clone(),
        SymbolKind::Function,
        "pruneCacheShard".to_string(),
        "fn pruneCacheShard(shard: usize) -> usize".to_string(),
        "fn pruneCacheShard(shard) { return shard + 1; }".to_string(),
        "src/cache/reconcile.ts".to_string(),
        70,
        83,
        false,
        Language::TypeScript,
    );

    graph.add_edge(&target, &target_helper, EdgeKind::Calls);
    graph.add_edge(&decoy, &decoy_helper, EdgeKind::Calls);

    graph
}

fn build_semantic_graph_rerank_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let anchor = make_id("routers/certificates.py", "validateTenantAccess", 10);
    let candidate = make_id("routers/certificates.py", "enforceOrgBoundary", 70);
    let decoy = make_id("services/inventory.py", "syncInventoryCache", 20);
    let decoy_helper = make_id("services/inventory.py", "flushInventoryShard", 60);

    graph.add_node(
        anchor.clone(),
        SymbolKind::Function,
        "validateTenantAccess".to_string(),
        "def validateTenantAccess(org_id):".to_string(),
        "def validateTenantAccess(org_id):\n    return org_id > 0".to_string(),
        "routers/certificates.py".to_string(),
        10,
        28,
        true,
        Language::Python,
    );
    graph.add_node(
        candidate.clone(),
        SymbolKind::Function,
        "enforceOrgBoundary".to_string(),
        "def enforceOrgBoundary(ctx):".to_string(),
        "def enforceOrgBoundary(ctx):\n    raise HTTPException(status_code=403)".to_string(),
        "routers/certificates.py".to_string(),
        70,
        92,
        true,
        Language::Python,
    );
    graph.add_node(
        decoy.clone(),
        SymbolKind::Function,
        "syncInventoryCache".to_string(),
        "def syncInventoryCache():".to_string(),
        "def syncInventoryCache():\n    return 1".to_string(),
        "services/inventory.py".to_string(),
        20,
        35,
        true,
        Language::Python,
    );
    graph.add_node(
        decoy_helper.clone(),
        SymbolKind::Function,
        "flushInventoryShard".to_string(),
        "def flushInventoryShard(shard):".to_string(),
        "def flushInventoryShard(shard):\n    return shard".to_string(),
        "services/inventory.py".to_string(),
        60,
        72,
        false,
        Language::Python,
    );

    graph.add_edge(&anchor, &candidate, EdgeKind::Calls);
    graph.add_edge(&decoy, &decoy_helper, EdgeKind::Calls);

    graph
}

fn build_file_summary_influence_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let guard = make_id("src/policy/guardrails.rs", "enforce_org_boundary", 10);
    let scope = make_id("src/policy/guardrails.rs", "check_access_scope", 60);
    let decoy = make_id("src/cache/reconcile.rs", "reconcile_cache_entries", 20);

    graph.add_node(
        guard.clone(),
        SymbolKind::Function,
        "enforce_org_boundary".to_string(),
        "fn enforce_org_boundary(actor: &Actor, target: OrgId) -> Result<()>".to_string(),
        "fn enforce_org_boundary(actor, target) { if actor.org_id != target { return Err(anyhow!(\"org boundary\")); } Ok(()) }".to_string(),
        "src/policy/guardrails.rs".to_string(),
        10,
        34,
        true,
        Language::Rust,
    );
    graph.add_node(
        scope.clone(),
        SymbolKind::Function,
        "check_access_scope".to_string(),
        "fn check_access_scope(actor: &Actor, scope: Scope) -> bool".to_string(),
        "fn check_access_scope(actor, scope) { actor.scope.contains(scope) }".to_string(),
        "src/policy/guardrails.rs".to_string(),
        60,
        82,
        true,
        Language::Rust,
    );
    graph.add_node(
        decoy.clone(),
        SymbolKind::Function,
        "reconcile_cache_entries".to_string(),
        "fn reconcile_cache_entries(cache: &mut Cache) -> usize".to_string(),
        "fn reconcile_cache_entries(cache) { cache.compact() }".to_string(),
        "src/cache/reconcile.rs".to_string(),
        20,
        47,
        true,
        Language::Rust,
    );

    graph.add_edge(&guard, &scope, EdgeKind::Calls);
    graph.add_edge(&decoy, &guard, EdgeKind::Calls);

    graph
}

struct StubVectorIndex {
    symbol_hits: Vec<VectorSearchResult>,
    file_summary_hits: Vec<VectorSearchResult>,
}

impl StubVectorIndex {
    fn with_symbol_hits(symbol_hits: Vec<VectorSearchResult>) -> Self {
        Self {
            symbol_hits,
            file_summary_hits: Vec::new(),
        }
    }
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
        Ok(self.symbol_hits.clone())
    }

    fn search_in_scope(
        &self,
        _query: &[f32],
        top_k: usize,
        scope: VectorScope,
    ) -> Result<Vec<VectorSearchResult>, crate::error::LatticeError> {
        let mut results = match scope {
            VectorScope::Symbol => self.symbol_hits.clone(),
            VectorScope::FileSummary => self.file_summary_hits.clone(),
            VectorScope::All => {
                let mut combined = self.symbol_hits.clone();
                combined.extend(self.file_summary_hits.clone());
                combined
            }
        };
        results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
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
    let vector_index: SharedVectorIndex = Arc::new(StubVectorIndex::with_symbol_hits(vec![(
        "hashPassword".to_string(),
        "src/crypto.ts".to_string(),
        0,
        0.91,
    )]));
    let mut engine = QueryEngine::new(graph, Some(vector_index), None);

    let capsule = engine.query("credential hashing", Some(&[0.1, 0.2, 0.3]), false);

    assert!(!capsule.pivots.is_empty());
    assert_eq!(capsule.pivots[0].symbol, "hashPassword");
}

#[test]
fn test_query_engine_reranks_semantic_hits_using_identifier_and_intent_signals() {
    let graph = build_semantic_identifier_rerank_graph();
    let vector_index: SharedVectorIndex = Arc::new(StubVectorIndex::with_symbol_hits(vec![
        (
            "reconcileCacheEntries".to_string(),
            "src/cache/reconcile.ts".to_string(),
            20,
            0.93,
        ),
        (
            "rotateSessionToken".to_string(),
            "src/auth/session_manager.ts".to_string(),
            10,
            0.79,
        ),
    ]));
    let mut engine = QueryEngine::new(graph, Some(vector_index), None);

    let capsule = engine.query(
        "Fix AuthService::RefreshSession bug",
        Some(&[0.1, 0.2, 0.3]),
        false,
    );
    let target_seed = "session_manager.ts:rotateSessionToken".to_string();
    let decoy_seed = "reconcile.ts:reconcileCacheEntries".to_string();
    let target_idx = capsule
        .stats
        .seed_symbols
        .iter()
        .position(|item| item == &target_seed);
    let decoy_idx = capsule
        .stats
        .seed_symbols
        .iter()
        .position(|item| item == &decoy_seed);

    assert!(
        matches!((target_idx, decoy_idx), (Some(a), Some(b)) if a < b),
        "expected identifier + intent rerank to promote rotateSessionToken over higher raw semantic cache symbol, got seeds: {:?}",
        capsule.stats.seed_symbols
    );
}

#[test]
fn test_query_engine_reranks_semantic_hits_using_graph_proximity() {
    let graph = build_semantic_graph_rerank_graph();
    let vector_index: SharedVectorIndex = Arc::new(StubVectorIndex::with_symbol_hits(vec![
        (
            "syncInventoryCache".to_string(),
            "services/inventory.py".to_string(),
            20,
            0.92,
        ),
        (
            "enforceOrgBoundary".to_string(),
            "routers/certificates.py".to_string(),
            70,
            0.84,
        ),
    ]));
    let mut engine = QueryEngine::new(graph, Some(vector_index), None);

    let capsule = engine.query("tenant access flow analysis", Some(&[0.2, 0.1, 0.4]), false);
    let enforce_seed = "certificates.py:enforceOrgBoundary".to_string();
    let decoy_seed = "inventory.py:syncInventoryCache".to_string();
    let enforce_idx = capsule
        .stats
        .seed_symbols
        .iter()
        .position(|item| item == &enforce_seed);
    let decoy_idx = capsule
        .stats
        .seed_symbols
        .iter()
        .position(|item| item == &decoy_seed);

    assert!(
        matches!((enforce_idx, decoy_idx), (Some(a), Some(b)) if a < b),
        "expected graph-aware rerank to place enforceOrgBoundary ahead of unrelated cache symbol, got seeds: {:?}",
        capsule.stats.seed_symbols
    );
}

#[test]
fn test_query_engine_uses_file_summary_vectors_to_seed_file_symbols() {
    let graph = build_file_summary_influence_graph();
    let vector_index: SharedVectorIndex = Arc::new(StubVectorIndex {
        symbol_hits: vec![(
            "reconcile_cache_entries".to_string(),
            "src/cache/reconcile.rs".to_string(),
            20,
            0.91,
        )],
        file_summary_hits: vec![(
            "file_summary".to_string(),
            "src/policy/guardrails.rs".to_string(),
            usize::MAX,
            0.86,
        )],
    });
    let mut engine = QueryEngine::new(graph, Some(vector_index), None);

    let capsule = engine.query(
        "cross-org certificate renewal returned 403",
        Some(&[0.3, 0.2]),
        false,
    );

    assert!(
        capsule
            .stats
            .seed_symbols
            .iter()
            .any(|item| item.ends_with(":enforce_org_boundary")),
        "expected file-summary semantic hit to seed at least one guardrails symbol, got seeds: {:?}",
        capsule.stats.seed_symbols
    );
    let target_seed = "guardrails.rs:enforce_org_boundary".to_string();
    let decoy_seed = "reconcile.rs:reconcile_cache_entries".to_string();
    let target_idx = capsule
        .stats
        .seed_symbols
        .iter()
        .position(|item| item == &target_seed);
    let decoy_idx = capsule
        .stats
        .seed_symbols
        .iter()
        .position(|item| item == &decoy_seed);
    assert!(
        matches!((target_idx, decoy_idx), (Some(a), Some(b)) if a < b),
        "expected file-summary seeded guardrails symbol to outrank decoy symbol seed, got seeds: {:?}",
        capsule.stats.seed_symbols
    );
}

#[test]
fn test_query_engine_identifier_queries_still_prefer_keyword_hits_over_scoped_semantic() {
    let graph = build_test_graph();
    let vector_index: SharedVectorIndex = Arc::new(StubVectorIndex {
        symbol_hits: vec![(
            "hashPassword".to_string(),
            "src/crypto.ts".to_string(),
            0,
            0.99,
        )],
        file_summary_hits: vec![(
            "file_summary".to_string(),
            "src/crypto.ts".to_string(),
            usize::MAX,
            0.97,
        )],
    });
    let mut engine = QueryEngine::new(graph, Some(vector_index), None);

    let capsule = engine.query("fix login_user timeout", Some(&[0.5, 0.4, 0.3]), false);
    let lead = capsule
        .pivots
        .first()
        .map(|item| item.symbol.as_str())
        .or_else(|| capsule.context.first().map(|item| item.symbol.as_str()));
    assert_eq!(
        lead,
        Some("loginUser"),
        "identifier query should still anchor on keyword-derived loginUser even when scoped semantic hits point elsewhere"
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
fn test_query_prefers_markdown_results_only_for_explicit_doc_terms() {
    assert!(query_prefers_markdown_results("docs for auth flow"));
    assert!(query_prefers_markdown_results("README.md render notes"));
    assert!(!query_prefers_markdown_results(
        "get_docs_capsule render modes"
    ));
    assert!(!query_prefers_markdown_results(
        "intersection observer state"
    ));
}

#[test]
fn test_query_engine_keeps_tool_identifiers_source_first_when_name_contains_docs() {
    let mut graph = build_lattice_workflow_graph();
    let code_id = make_id(
        "daemon/crates/lattice-daemon/src/rpc/mcp.rs",
        "get_docs_capsule",
        2,
    );
    let doc_id = make_id("README.md", "Docs Tools", 2);

    graph.add_node(
        code_id,
        SymbolKind::Function,
        "get_docs_capsule".to_string(),
        "fn get_docs_capsule(render: RenderMode, query: &str)".to_string(),
        "fn get_docs_capsule(...) { /* source implementation for tool lookup */ }".to_string(),
        "daemon/crates/lattice-daemon/src/rpc/mcp.rs".to_string(),
        96,
        140,
        true,
        Language::Rust,
    );
    graph.add_node(
        doc_id,
        SymbolKind::Section,
        "Docs Tools".to_string(),
        "section Docs Tools get_docs_capsule render modes".to_string(),
        "Docs Tools: get_docs_capsule render modes".to_string(),
        "README.md".to_string(),
        49,
        65,
        false,
        Language::Markdown,
    );

    let mut engine = QueryEngine::new(graph, None, None);
    let capsule = engine.query("get_docs_capsule render modes", None, false);
    let lead_file = capsule
        .pivots
        .first()
        .map(|item| item.file.as_str())
        .or_else(|| capsule.context.first().map(|item| item.file.as_str()));

    assert_eq!(
        lead_file,
        Some("daemon/crates/lattice-daemon/src/rpc/mcp.rs"),
        "expected tool identifier query to stay source-first, got pivots={:?} context={:?}",
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
