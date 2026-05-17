// Agent-workflow benchmark tests — index the current Lattice repo and score the
// workflow tools against realistic maintenance tasks.
//
// Run with:
// cargo test workflow_bench -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::graph::model::{CodeGraph, EdgeKind};
use crate::intelligence::{
    diagnose_failure, find_relevant_tests, get_docs_capsule, get_working_set_context,
    impact_from_diff, plan_edit, prepare_change, BundleMode, RulesDetector,
};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use crate::query::{CapsuleStats, ContextCapsule, QueryEngine, QueryIntent};
use crate::symbols::{Language, SymbolId, SymbolKind};
use serde::Serialize;
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

#[derive(Clone, Copy)]
enum PlanEditCase {
    BugFix,
    Refactor,
    FeatureAdd,
}

impl PlanEditCase {
    fn as_str(self) -> &'static str {
        match self {
            Self::BugFix => "bug_fix",
            Self::Refactor => "refactor",
            Self::FeatureAdd => "feature_add",
        }
    }
}

#[derive(Serialize)]
struct PlanEditGoldenPayload {
    case: &'static str,
    query: String,
    intent: QueryIntent,
    edit_files: Vec<String>,
    edit_symbols: Vec<String>,
    candidate_spans: Vec<String>,
    affected_symbols: Vec<String>,
    docs: Vec<String>,
    tests: Vec<String>,
}

struct PlanEditGoldenResult {
    case: PlanEditCase,
    payload_bytes: usize,
    edit_file_hit: bool,
    symbol_hit: bool,
    span_hit: bool,
    caller_hit: bool,
    doc_hit: bool,
    test_hit: bool,
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
    let shared_id = make_id(
        "routers/compliance_mgmt/_shared.py",
        "_verify_org_access",
        0,
    );
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

fn build_plan_edit_bug_fix_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let auth_id = make_id("src/auth.ts", "loginUser", 10);
    let route_id = make_id("src/routes/auth.ts", "loginRoute", 5);
    let session_id = make_id("src/session.ts", "createSession", 3);
    let test_id = make_id("tests/auth.test.ts", "loginUserTest", 4);
    let doc_id = make_id("docs/auth.md", "Login Flow", 1);

    graph.add_node(
        auth_id.clone(),
        SymbolKind::Function,
        "loginUser".to_string(),
        "function loginUser(input: Credentials): Promise<Session>".to_string(),
        "function loginUser(input) { return createSession(input.user); }".to_string(),
        "src/auth.ts".to_string(),
        10,
        28,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        route_id.clone(),
        SymbolKind::Function,
        "loginRoute".to_string(),
        "function loginRoute(req: Request): Promise<Response>".to_string(),
        "function loginRoute(req) { return loginUser(req.body); }".to_string(),
        "src/routes/auth.ts".to_string(),
        5,
        20,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        session_id.clone(),
        SymbolKind::Function,
        "createSession".to_string(),
        "function createSession(user: User): Session".to_string(),
        "function createSession(user) { return { user }; }".to_string(),
        "src/session.ts".to_string(),
        3,
        16,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        test_id.clone(),
        SymbolKind::Function,
        "loginUserTest".to_string(),
        "test('loginUser returns a session', () => void)".to_string(),
        "test('loginUser returns a session', () => { loginRoute(req); });".to_string(),
        "tests/auth.test.ts".to_string(),
        4,
        14,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        doc_id.clone(),
        SymbolKind::Section,
        "Login Flow".to_string(),
        "section Login Flow loginUser loginRoute".to_string(),
        "Use loginUser from the login route.".to_string(),
        "docs/auth.md".to_string(),
        1,
        12,
        false,
        Language::Markdown,
    );

    graph.add_edge(&route_id, &auth_id, EdgeKind::Calls);
    graph.add_edge(&auth_id, &session_id, EdgeKind::Calls);
    graph.add_edge(&test_id, &route_id, EdgeKind::Calls);
    graph.add_edge(&doc_id, &auth_id, EdgeKind::Mentions);
    graph.add_edge(&doc_id, &route_id, EdgeKind::Mentions);

    graph
}

fn build_plan_edit_refactor_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let reconcile_id = make_id("src/cache/reconcile.ts", "reconcileCacheEntries", 12);
    let backend_id = make_id("src/cache/backend.ts", "CacheBackend", 4);
    let cli_id = make_id("src/cache/cli.ts", "refreshCaches", 9);
    let test_id = make_id("tests/cache.test.ts", "test_cache_reconcile", 3);
    let doc_id = make_id("docs/cache.md", "Cache Reconcile", 1);

    graph.add_node(
        reconcile_id.clone(),
        SymbolKind::Function,
        "reconcileCacheEntries".to_string(),
        "function reconcileCacheEntries(cache: CacheStore): void".to_string(),
        "function reconcileCacheEntries(cache) { return cache.compact(); }".to_string(),
        "src/cache/reconcile.ts".to_string(),
        12,
        34,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        backend_id.clone(),
        SymbolKind::Interface,
        "CacheBackend".to_string(),
        "interface CacheBackend".to_string(),
        "interface CacheBackend { reconcile(): void }".to_string(),
        "src/cache/backend.ts".to_string(),
        4,
        18,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        cli_id.clone(),
        SymbolKind::Function,
        "refreshCaches".to_string(),
        "function refreshCaches(opts: CacheOptions): void".to_string(),
        "function refreshCaches(opts) { return reconcileCacheEntries(opts.cache); }".to_string(),
        "src/cache/cli.ts".to_string(),
        9,
        22,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        test_id.clone(),
        SymbolKind::Function,
        "test_cache_reconcile".to_string(),
        "test('reconcile cache entries', () => void)".to_string(),
        "test('reconcile cache entries', () => { refreshCaches(opts); });".to_string(),
        "tests/cache.test.ts".to_string(),
        3,
        13,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        doc_id.clone(),
        SymbolKind::Section,
        "Cache Reconcile".to_string(),
        "section Cache Reconcile reconcileCacheEntries CacheBackend".to_string(),
        "Refactor the cache reconciliation flow.".to_string(),
        "docs/cache.md".to_string(),
        1,
        10,
        false,
        Language::Markdown,
    );

    graph.add_edge(&cli_id, &reconcile_id, EdgeKind::Calls);
    graph.add_edge(&reconcile_id, &backend_id, EdgeKind::Implements);
    graph.add_edge(&test_id, &reconcile_id, EdgeKind::Calls);
    graph.add_edge(&doc_id, &reconcile_id, EdgeKind::Mentions);
    graph.add_edge(&doc_id, &backend_id, EdgeKind::Mentions);

    graph
}

fn build_plan_edit_feature_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let checkout_id = make_id("src/billing/checkout.ts", "createCheckoutSession", 20);
    let pricing_id = make_id("src/billing/pricing.ts", "applyDiscountCode", 8);
    let controller_id = make_id("src/billing/api.ts", "billingController", 11);
    let types_id = make_id("src/billing/types.ts", "CheckoutRequest", 2);
    let test_id = make_id("tests/billing_feature.test.ts", "test_checkout_feature", 5);
    let doc_id = make_id("docs/billing.md", "Checkout Feature", 1);

    graph.add_node(
        checkout_id.clone(),
        SymbolKind::Function,
        "createCheckoutSession".to_string(),
        "function createCheckoutSession(req: CheckoutRequest): Promise<Session>".to_string(),
        "function createCheckoutSession(req) { return applyDiscountCode(req); }".to_string(),
        "src/billing/checkout.ts".to_string(),
        20,
        44,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        pricing_id.clone(),
        SymbolKind::Function,
        "applyDiscountCode".to_string(),
        "function applyDiscountCode(req: CheckoutRequest): CheckoutQuote".to_string(),
        "function applyDiscountCode(req) { return { total: 1 }; }".to_string(),
        "src/billing/pricing.ts".to_string(),
        8,
        26,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        controller_id.clone(),
        SymbolKind::Function,
        "billingController".to_string(),
        "function billingController(req: Request): Response".to_string(),
        "function billingController(req) { return createCheckoutSession(req.body); }".to_string(),
        "src/billing/api.ts".to_string(),
        11,
        28,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        types_id.clone(),
        SymbolKind::Interface,
        "CheckoutRequest".to_string(),
        "interface CheckoutRequest".to_string(),
        "interface CheckoutRequest { plan: string }".to_string(),
        "src/billing/types.ts".to_string(),
        2,
        18,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        test_id.clone(),
        SymbolKind::Function,
        "test_checkout_feature".to_string(),
        "test('checkout feature returns quote', () => void)".to_string(),
        "test('checkout feature returns quote', () => { billingController(req); });".to_string(),
        "tests/billing_feature.test.ts".to_string(),
        5,
        15,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        doc_id.clone(),
        SymbolKind::Section,
        "Checkout Feature".to_string(),
        "section Checkout Feature createCheckoutSession CheckoutRequest".to_string(),
        "Document the new billing checkout flow.".to_string(),
        "docs/billing.md".to_string(),
        1,
        11,
        false,
        Language::Markdown,
    );

    graph.add_edge(&controller_id, &checkout_id, EdgeKind::Calls);
    graph.add_edge(&checkout_id, &pricing_id, EdgeKind::Calls);
    graph.add_edge(&checkout_id, &types_id, EdgeKind::TypeRef);
    graph.add_edge(&test_id, &checkout_id, EdgeKind::Calls);
    graph.add_edge(&doc_id, &checkout_id, EdgeKind::Mentions);
    graph.add_edge(&doc_id, &types_id, EdgeKind::Mentions);

    graph
}

fn build_trace_scenario_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let login_route_id = make_id("src/routes/auth.ts", "loginRoute", 0);
    let refresh_session_id = make_id("src/auth.ts", "refreshSession", 1);
    let verify_refresh_token_id = make_id("src/auth.ts", "verifyRefreshToken", 2);
    let create_session_id = make_id("src/session.ts", "createSession", 3);
    let fallback_login_id = make_id("src/auth.ts", "fallbackLogin", 4);
    let reject_refresh_id = make_id("src/auth.ts", "rejectRefresh", 5);
    let refresh_test_id = make_id(
        "tests/auth_refresh.test.ts",
        "test_refresh_session_rejects_expired_token",
        0,
    );
    let login_test_id = make_id(
        "tests/auth_login.test.ts",
        "test_login_route_falls_back_to_password_login",
        0,
    );
    let docs_refresh_id = make_id("docs/auth.md", "Refresh Flow", 1);
    let docs_login_id = make_id("docs/auth.md", "Login Flow", 20);

    graph.add_node(
        login_route_id.clone(),
        SymbolKind::Function,
        "loginRoute".to_string(),
        "function loginRoute(req: Request): Promise<Response>".to_string(),
        "function loginRoute(req) { return refreshSession(req.body); }".to_string(),
        "src/routes/auth.ts".to_string(),
        4,
        18,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        refresh_session_id.clone(),
        SymbolKind::Function,
        "refreshSession".to_string(),
        "function refreshSession(user: User, token: Token): Promise<Session>".to_string(),
        "function refreshSession(user, token) { return verifyRefreshToken(token) ? createSession(user) : rejectRefresh(); }".to_string(),
        "src/auth.ts".to_string(),
        12,
        30,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        verify_refresh_token_id.clone(),
        SymbolKind::Function,
        "verifyRefreshToken".to_string(),
        "function verifyRefreshToken(token: Token): bool".to_string(),
        "function verifyRefreshToken(token) { return token !== ''; }".to_string(),
        "src/auth.ts".to_string(),
        32,
        46,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        create_session_id.clone(),
        SymbolKind::Function,
        "createSession".to_string(),
        "function createSession(user: User): Session".to_string(),
        "function createSession(user) { return { user }; }".to_string(),
        "src/session.ts".to_string(),
        3,
        14,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        fallback_login_id.clone(),
        SymbolKind::Function,
        "fallbackLogin".to_string(),
        "function fallbackLogin(req: Request): Promise<Response>".to_string(),
        "function fallbackLogin(req) { return createSession(req.body.user); }".to_string(),
        "src/auth.ts".to_string(),
        48,
        62,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        reject_refresh_id.clone(),
        SymbolKind::Function,
        "rejectRefresh".to_string(),
        "function rejectRefresh(): Response".to_string(),
        "function rejectRefresh() { return new Response(401); }".to_string(),
        "src/auth.ts".to_string(),
        64,
        72,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        refresh_test_id.clone(),
        SymbolKind::Function,
        "test_refresh_session_rejects_expired_token".to_string(),
        "test('refresh session rejects expired token', () => void)".to_string(),
        "test('refresh session rejects expired token', () => { loginRoute(req); });".to_string(),
        "tests/auth_refresh.test.ts".to_string(),
        6,
        18,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        login_test_id.clone(),
        SymbolKind::Function,
        "test_login_route_falls_back_to_password_login".to_string(),
        "test('login route falls back to password login', () => void)".to_string(),
        "test('login route falls back to password login', () => { fallbackLogin(req); });"
            .to_string(),
        "tests/auth_login.test.ts".to_string(),
        6,
        18,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        docs_refresh_id.clone(),
        SymbolKind::Section,
        "Refresh Flow".to_string(),
        "section Refresh Flow loginRoute refreshSession verifyRefreshToken rejectRefresh"
            .to_string(),
        "Use refreshSession after loginRoute and verifyRefreshToken before creating a session."
            .to_string(),
        "docs/auth.md".to_string(),
        1,
        16,
        false,
        Language::Markdown,
    );
    graph.add_node(
        docs_login_id.clone(),
        SymbolKind::Section,
        "Login Flow".to_string(),
        "section Login Flow loginRoute fallbackLogin createSession".to_string(),
        "Fallback login remains available when refresh fails.".to_string(),
        "docs/auth.md".to_string(),
        20,
        34,
        false,
        Language::Markdown,
    );

    graph.add_edge(&login_route_id, &refresh_session_id, EdgeKind::Calls);
    graph.add_edge(&login_route_id, &fallback_login_id, EdgeKind::Calls);
    graph.add_edge(
        &refresh_session_id,
        &verify_refresh_token_id,
        EdgeKind::Calls,
    );
    graph.add_edge(&refresh_session_id, &create_session_id, EdgeKind::Calls);
    graph.add_edge(&refresh_session_id, &reject_refresh_id, EdgeKind::Calls);
    graph.add_edge(
        &verify_refresh_token_id,
        &create_session_id,
        EdgeKind::Calls,
    );
    graph.add_edge(&fallback_login_id, &create_session_id, EdgeKind::Calls);
    graph.add_edge(&refresh_test_id, &login_route_id, EdgeKind::Calls);
    graph.add_edge(&login_test_id, &fallback_login_id, EdgeKind::Calls);
    graph.add_edge(&docs_refresh_id, &login_route_id, EdgeKind::Mentions);
    graph.add_edge(&docs_refresh_id, &refresh_session_id, EdgeKind::Mentions);
    graph.add_edge(
        &docs_refresh_id,
        &verify_refresh_token_id,
        EdgeKind::Mentions,
    );
    graph.add_edge(&docs_refresh_id, &reject_refresh_id, EdgeKind::Mentions);
    graph.add_edge(&docs_login_id, &login_route_id, EdgeKind::Mentions);
    graph.add_edge(&docs_login_id, &fallback_login_id, EdgeKind::Mentions);
    graph.add_edge(&docs_login_id, &create_session_id, EdgeKind::Mentions);

    graph
}

fn empty_plan_edit_capsule(query: &str, intent: QueryIntent) -> ContextCapsule {
    ContextCapsule {
        query: query.to_string(),
        intent,
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
    }
}

fn build_plan_edit_payload(
    case: PlanEditCase,
    bundle: &crate::intelligence::PlanEditBundle,
) -> PlanEditGoldenPayload {
    let mut edit_files = bundle
        .edit_files
        .iter()
        .chain(bundle.supporting_files.iter())
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let mut edit_symbols = bundle
        .symbols
        .iter()
        .map(|item| item.symbol.clone())
        .collect::<Vec<_>>();
    edit_symbols.extend(
        bundle
            .affected_callers
            .iter()
            .map(|item| item.symbol.clone()),
    );
    edit_symbols.extend(
        bundle
            .affected_dependencies
            .iter()
            .map(|item| item.symbol.clone()),
    );
    let candidate_spans = bundle
        .candidate_spans
        .iter()
        .map(|span| format!("{}:{}-{}", span.file, span.start_line, span.end_line))
        .collect::<Vec<_>>();
    let affected_symbols = bundle
        .affected_callers
        .iter()
        .chain(bundle.affected_dependencies.iter())
        .map(|item| format!("{}:{}", item.file, item.symbol))
        .collect::<Vec<_>>();
    let docs = bundle
        .relevant_docs
        .iter()
        .map(|item| format!("{}:{}", item.file, item.line))
        .collect::<Vec<_>>();
    let tests = bundle
        .tests
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();

    edit_files.sort();
    edit_files.dedup();
    edit_symbols.sort();
    edit_symbols.dedup();

    PlanEditGoldenPayload {
        case: case.as_str(),
        query: bundle.query.clone(),
        intent: bundle.intent,
        edit_files,
        edit_symbols,
        candidate_spans,
        affected_symbols,
        docs,
        tests,
    }
}

fn plan_edit_case(
    case: PlanEditCase,
    graph: &CodeGraph,
    query: &str,
    intent: QueryIntent,
    entry_files: &[&str],
    entry_symbols: &[&str],
    _diff: &str,
    _docs_files: &[&str],
    _docs_symbols: &[&str],
    expected_files: &[&str],
    expected_symbols: &[&str],
    expected_caller: &str,
    expected_doc: &str,
    expected_test: &str,
    expected_span: &str,
) -> PlanEditGoldenResult {
    let rules = project_rules(graph);
    let capsule = empty_plan_edit_capsule(query, intent);
    let entry_files_vec: Vec<String> = entry_files
        .iter()
        .map(|value| (*value).to_string())
        .collect();
    let entry_symbols_vec: Vec<String> = entry_symbols
        .iter()
        .map(|value| (*value).to_string())
        .collect();

    let bundle = plan_edit(
        graph,
        &capsule,
        &entry_files_vec,
        &entry_symbols_vec,
        &rules,
        BundleMode::Compact,
    );
    let payload = build_plan_edit_payload(case, &bundle);
    let payload_bytes = serde_json::to_vec(&payload).unwrap().len();

    let edit_file_hit = expected_files
        .iter()
        .all(|expected| payload.edit_files.iter().any(|item| item == expected));
    let symbol_hit = expected_symbols.iter().all(|expected| {
        payload.edit_symbols.iter().any(|item| {
            item == expected
                || item.ends_with(expected)
                || item.ends_with(&format!(".{}", expected))
        })
    });
    let span_hit = parse_span_signature(expected_span).is_some_and(|expected| {
        payload
            .candidate_spans
            .iter()
            .filter_map(|item| parse_span_signature(item))
            .any(|candidate| {
                candidate.file == expected.file
                    && ranges_overlap(
                        candidate.start_line,
                        candidate.end_line,
                        expected.start_line,
                        expected.end_line,
                    )
            })
    });
    let caller_hit = payload.affected_symbols.iter().any(|item| {
        item == expected_caller
            || item.ends_with(&format!(":{}", expected_caller))
            || item.ends_with(&format!(".{}", expected_caller))
    });
    let doc_hit = payload
        .docs
        .iter()
        .any(|item| item == expected_doc || item.starts_with(&format!("{}:", expected_doc)));
    let test_hit = payload.tests.iter().any(|item| item == expected_test);

    PlanEditGoldenResult {
        case,
        payload_bytes,
        edit_file_hit,
        symbol_hit,
        span_hit,
        caller_hit,
        doc_hit,
        test_hit,
    }
}

struct ParsedSpanSignature {
    file: String,
    start_line: usize,
    end_line: usize,
}

fn parse_span_signature(value: &str) -> Option<ParsedSpanSignature> {
    let (file, range) = value.rsplit_once(':')?;
    let (start, end) = match range.split_once('-') {
        Some((start, end)) => (start, end),
        None => (range, range),
    };
    let start_line = start.trim().parse::<usize>().ok()?;
    let end_line = end.trim().parse::<usize>().ok()?;
    Some(ParsedSpanSignature {
        file: file.to_string(),
        start_line,
        end_line: end_line.max(start_line),
    })
}

fn ranges_overlap(start_a: usize, end_a: usize, start_b: usize, end_b: usize) -> bool {
    start_a <= end_b && start_b <= end_a
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
        linked_symbols: linked_symbols
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        linked_files: linked_files
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        workspace_id: Some("lattice-benchmark".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
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
    let failure =
        format!("{file}:{line}:9 error: search_across_sessions failed during session recall");
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
        .search_across_sessions(
            "previous session recall new session",
            Some("session-current"),
            5,
        )
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

fn bench_prepare_change_certificate_assistant(graph: &CodeGraph) -> WorkflowResult {
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query(
        "Why do cross-org certificate renewal requests return 403 instead of 404?",
        None,
        false,
    );
    let rules = project_rules(graph);
    let report = prepare_change(
        graph,
        &capsule,
        &["routers/certificates.py".to_string()],
        &[
            "_verify_org_access".to_string(),
            "upsert_renewal_policy".to_string(),
        ],
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
            .any(|item| item.file == "routers/certificates.py"),
        target_hit: report.symbols.iter().any(|item| {
            item.symbol == "_verify_org_access"
                || item.symbol.ends_with("._verify_org_access")
                || item.symbol == "upsert_renewal_policy"
                || item.symbol.ends_with(".upsert_renewal_policy")
        }),
        stale_precision: None,
    }
}

fn bench_working_set_certificate_assistant(graph: &CodeGraph) -> WorkflowResult {
    let rules = project_rules(graph);
    let report = get_working_set_context(
        graph,
        &["routers/certificates.py".to_string()],
        &[
            "_verify_org_access".to_string(),
            "upsert_renewal_policy".to_string(),
        ],
        Some("Why do cross-org certificate renewals fail after login refresh?"),
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
            .any(|item| item.file == "routers/certificates.py"),
        target_hit: report
            .active_symbols
            .iter()
            .chain(report.nearby_symbols.iter())
            .any(|item| {
                item.symbol == "_verify_org_access"
                    || item.symbol.ends_with("._verify_org_access")
                    || item.symbol == "upsert_renewal_policy"
                    || item.symbol.ends_with(".upsert_renewal_policy")
            }),
        stale_precision: None,
    }
}

fn bench_diagnose_failure_certificate_assistant(graph: &CodeGraph) -> WorkflowResult {
    let rules = project_rules(graph);
    let report = diagnose_failure(
        graph,
        "tests/test_certificate_tenant_isolation.py:362: AssertionError: expected 404, got 403\nrouters/certificates.py:84: HTTPException(status_code=403)\nrouters/certificates.py:380: _verify_org_access(...)",
        Some("test"),
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
            .any(|item| item.file == "routers/certificates.py"),
        target_hit: report.suspects.iter().any(|item| {
            item.symbol == "_verify_org_access"
                || item.symbol.ends_with("._verify_org_access")
                || item.symbol == "upsert_renewal_policy"
                || item.symbol.ends_with(".upsert_renewal_policy")
        }),
        stale_precision: None,
    }
}

fn trace_query_tokens(query: &str) -> std::collections::HashSet<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(|part| part.trim().to_lowercase())
        .filter(|part| part.len() >= 2)
        .collect()
}

fn trace_path_signature(path: &[&crate::graph::model::GraphNode]) -> Vec<String> {
    path.iter().map(|node| node.name.clone()).collect()
}

fn trace_path_score(
    query_tokens: &std::collections::HashSet<String>,
    path: &[&crate::graph::model::GraphNode],
) -> f64 {
    let mut score = 0.0;
    for node in path {
        let node_tokens = trace_query_tokens(&node.name);
        score += query_tokens.intersection(&node_tokens).count() as f64;

        let lowered = node.name.to_lowercase();
        if lowered.contains("refresh") && query_tokens.contains("refresh") {
            score += 0.5;
        }
        if lowered.contains("verify") && query_tokens.contains("fail") {
            score += 0.5;
        }
        if lowered.contains("fallback") {
            score -= 0.25;
        }
    }

    score
}

fn trace_entrypoint_candidate(
    graph: &CodeGraph,
    query_tokens: &std::collections::HashSet<String>,
) -> Option<String> {
    graph
        .all_nodes()
        .into_iter()
        .filter(|node| node.is_exported || node.file.contains("/routes/"))
        .map(|node| {
            let mut score = 0.0;
            let lowered_name = node.name.to_lowercase();
            let lowered_file = node.file.to_lowercase();
            if lowered_file.contains("/routes/") {
                score += 2.0;
            }
            if lowered_name.contains("route") {
                score += 2.0;
            }
            score += query_tokens
                .intersection(&trace_query_tokens(&node.name))
                .count() as f64;
            if lowered_name.contains("fallback") {
                score -= 0.5;
            }
            if lowered_name.contains("refresh") {
                score += 0.5;
            }

            (score, node.name.clone())
        })
        .max_by(|(score_a, name_a), (score_b, name_b)| {
            score_a
                .partial_cmp(score_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| name_a.cmp(name_b))
        })
        .map(|(_, name)| name)
}

fn bench_trace_scenario_login_refresh_case() -> (usize, bool, bool, bool, bool, bool, bool) {
    let graph = build_trace_scenario_graph();
    let rules = project_rules(&graph);
    let query = "Why does login fail after refresh?";
    let query_tokens = trace_query_tokens(query);

    let login_route_id = make_id("src/routes/auth.ts", "loginRoute", 0);
    let create_session_id = make_id("src/session.ts", "createSession", 3);
    let reject_refresh_id = make_id("src/auth.ts", "rejectRefresh", 5);
    let paths = graph.find_call_paths(&login_route_id, &create_session_id, 4, 8);
    let path_signatures: Vec<Vec<String>> = paths
        .iter()
        .map(|path| trace_path_signature(path))
        .collect();

    let strong_path = path_signatures.iter().find(|path| {
        path.contains(&"refreshSession".to_string())
            && path.contains(&"verifyRefreshToken".to_string())
    });
    let alternative_path = path_signatures
        .iter()
        .find(|path| path.contains(&"fallbackLogin".to_string()));

    let strong_path_nodes = strong_path
        .map(|path| {
            path.iter()
                .filter_map(|name| {
                    graph
                        .all_nodes()
                        .into_iter()
                        .find(|node| &node.name == name)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let alternative_path_nodes = alternative_path
        .map(|path| {
            path.iter()
                .filter_map(|name| {
                    graph
                        .all_nodes()
                        .into_iter()
                        .find(|node| &node.name == name)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let strong_score = trace_path_score(&query_tokens, &strong_path_nodes);
    let alternative_score = trace_path_score(&query_tokens, &alternative_path_nodes);

    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query(query, None, false);
    let entrypoint_hit =
        trace_entrypoint_candidate(&graph, &query_tokens).as_deref() == Some("loginRoute");

    let failure = diagnose_failure(
        &graph,
        "tests/auth_refresh.test.ts:42: AssertionError: expected 200, got 401\nsrc/auth.ts:12: refreshSession(user, token)\nsrc/auth.ts:36: verifyRefreshToken(token)\nsrc/auth.ts:64: rejectRefresh()",
        Some("test"),
        &rules,
        BundleMode::Compact,
    );
    let guard_hit = strong_path
        .as_ref()
        .is_some_and(|path| path.iter().any(|name| name == "verifyRefreshToken"));
    let failure_branch_hit = graph
        .find_call_paths(&login_route_id, &reject_refresh_id, 3, 8)
        .iter()
        .any(|path| path.iter().any(|node| node.name == "rejectRefresh"));

    let tests = find_relevant_tests(
        &graph,
        &["src/routes/auth.ts".to_string(), "src/auth.ts".to_string()],
        &[
            "loginRoute".to_string(),
            "refreshSession".to_string(),
            "verifyRefreshToken".to_string(),
        ],
        None,
        &rules,
        4,
    );
    let tests_hit = tests
        .tests
        .iter()
        .take(2)
        .any(|item| item.file == "tests/auth_refresh.test.ts")
        && tests
            .tests
            .iter()
            .take(3)
            .any(|item| item.file == "tests/auth_login.test.ts");
    let confidence_separation_hit = tests
        .tests
        .iter()
        .find(|item| item.file == "tests/auth_refresh.test.ts")
        .zip(
            tests
                .tests
                .iter()
                .find(|item| item.file == "tests/auth_login.test.ts"),
        )
        .map(|(refresh, login)| refresh.confidence > login.confidence)
        .unwrap_or(false);

    let docs = get_docs_capsule(
        &graph,
        query,
        &["src/routes/auth.ts".to_string(), "src/auth.ts".to_string()],
        &[
            "loginRoute".to_string(),
            "refreshSession".to_string(),
            "verifyRefreshToken".to_string(),
        ],
        4,
    );
    let docs_hit = docs
        .docs
        .iter()
        .take(2)
        .any(|item| item.symbol == "Refresh Flow")
        && docs
            .related_symbols
            .iter()
            .any(|item| item.symbol == "verifyRefreshToken");

    let payload = json!({
        "case": "login_refresh_failure",
        "query": query,
        "entrypoint": trace_entrypoint_candidate(&graph, &query_tokens),
        "capsule_pivot": capsule.pivots.first().map(|pivot| pivot.symbol.clone()),
        "strong_path": strong_path.cloned().unwrap_or_default(),
        "alternative_path": alternative_path.cloned().unwrap_or_default(),
        "strong_score": strong_score,
        "alternative_score": alternative_score,
        "failure_suspects": failure
            .suspects
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>(),
        "tests": tests
            .tests
            .iter()
            .map(|item| format!("{}:{:.2}", item.file, item.confidence))
            .collect::<Vec<_>>(),
        "docs": docs
            .docs
            .iter()
            .map(|item| format!("{}:{}", item.file, item.symbol))
            .collect::<Vec<_>>(),
    });
    let payload_bytes = serde_json::to_vec(&payload).unwrap().len();

    (
        payload_bytes,
        entrypoint_hit,
        strong_path.is_some_and(|path| path.first().is_some_and(|node| node == "loginRoute"))
            && alternative_path
                .is_some_and(|path| path.first().is_some_and(|node| node == "loginRoute"))
            && strong_score > alternative_score,
        guard_hit && failure_branch_hit,
        docs_hit,
        tests_hit,
        confidence_separation_hit && strong_score > alternative_score,
    )
}

fn bench_plan_edit_bug_fix_case() -> PlanEditGoldenResult {
    let graph = build_plan_edit_bug_fix_graph();
    let diff = "diff --git a/src/auth.ts b/src/auth.ts\n--- a/src/auth.ts\n+++ b/src/auth.ts\n@@ -10,7 +10,7 @@\n-    return oldSession(input);\n+    return createSession(input.user);\n";

    plan_edit_case(
        PlanEditCase::BugFix,
        &graph,
        "Fix login timeout after refresh",
        QueryIntent::FixBug,
        &["src/auth.ts", "src/routes/auth.ts"],
        &["loginUser", "loginRoute"],
        diff,
        &["src/auth.ts", "docs/auth.md"],
        &["loginUser", "loginRoute"],
        &["src/auth.ts", "src/routes/auth.ts"],
        &["loginUser", "loginRoute"],
        "loginRoute",
        "docs/auth.md",
        "tests/auth.test.ts",
        "src/auth.ts:10-16",
    )
}

fn bench_plan_edit_refactor_case() -> PlanEditGoldenResult {
    let graph = build_plan_edit_refactor_graph();
    let diff = "diff --git a/src/cache/reconcile.ts b/src/cache/reconcile.ts\n--- a/src/cache/reconcile.ts\n+++ b/src/cache/reconcile.ts\n@@ -12,9 +12,9 @@\n-    return cache.fold();\n+    return cache.compact();\n";

    plan_edit_case(
        PlanEditCase::Refactor,
        &graph,
        "Refactor cache reconciliation flow",
        QueryIntent::Refactor,
        &["src/cache/reconcile.ts"],
        &["reconcileCacheEntries"],
        diff,
        &["src/cache/reconcile.ts", "docs/cache.md"],
        &["reconcileCacheEntries", "CacheBackend"],
        &["src/cache/reconcile.ts", "src/cache/backend.ts"],
        &["reconcileCacheEntries", "refreshCaches"],
        "refreshCaches",
        "docs/cache.md",
        "tests/cache.test.ts",
        "src/cache/reconcile.ts:12-20",
    )
}

fn bench_plan_edit_feature_add_case() -> PlanEditGoldenResult {
    let graph = build_plan_edit_feature_graph();
    let diff = "diff --git a/src/billing/checkout.ts b/src/billing/checkout.ts\n--- a/src/billing/checkout.ts\n+++ b/src/billing/checkout.ts\n@@ -20,9 +20,9 @@\n-    return createLegacySession(req);\n+    return createCheckoutSession(req);\n";

    plan_edit_case(
        PlanEditCase::FeatureAdd,
        &graph,
        "Add checkout preview for billing discounts",
        QueryIntent::AddFeature,
        &["src/billing/checkout.ts", "src/billing/types.ts"],
        &["createCheckoutSession"],
        diff,
        &["src/billing/checkout.ts", "docs/billing.md"],
        &["createCheckoutSession", "CheckoutRequest"],
        &["src/billing/checkout.ts", "src/billing/types.ts"],
        &["createCheckoutSession", "billingController"],
        "billingController",
        "docs/billing.md",
        "tests/billing_feature.test.ts",
        "src/billing/checkout.ts:20-28",
    )
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
        cert_model_index
            .zip(account_model_index)
            .map(|(cert, account)| cert < account)
            .unwrap_or(true),
        "expected certificate-local model to outrank broad account model: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| format!("{}:{}", item.file, item.score))
            .collect::<Vec<_>>()
    );
}

#[test]
fn workflow_guardrail_structured_memory_trust_case() {
    let graph = build_certificate_guardrail_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "models/certificates.py".to_string(),
        "models/account.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
    ]);

    let bundle = prepare_change(
        &graph,
        &ContextCapsule {
            query: "Return 404 instead of 403 for cross-org organization access in certificate renewal policy endpoints".to_string(),
            intent: QueryIntent::FixBug,
            pivots: vec![],
            context: vec![],
            memories: vec![
                json!({
                    "id": "mem-contradicted",
                    "content": "Certificate org access regressions should start in the shared compliance helper.",
                    "type": "observation",
                    "scope": "repo",
                    "contradicted_by_memory_ids": ["mem-verified"],
                    "confidence_reason": "Older routing note before endpoint isolation fix"
                }),
                json!({
                    "id": "mem-verified",
                    "content": "Certificate org access regressions should start in routers/certificates.py and verify the 404 tenant-isolation path.",
                    "type": "pattern",
                    "assertion_type": "workflow_outcome",
                    "scope": "repo",
                    "verification_status": "verified",
                    "confidence_reason": "Confirmed by certificate tenant-isolation regression",
                    "provenance": [{
                        "source": "test",
                        "reference": "tests/test_certificate_tenant_isolation.py"
                    }],
                    "evidence": [{
                        "kind": "test",
                        "reference": "tests/test_certificate_tenant_isolation.py",
                        "detail": "Exercises foreign-org policy update failure"
                    }]
                }),
            ],
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
        bundle
            .memory_highlights
            .first()
            .and_then(|item| item.verification_status.as_deref()),
        Some("verified")
    );
    assert_eq!(
        bundle
            .memory_highlights
            .first()
            .and_then(|item| item.assertion_type.as_deref()),
        Some("workflow_outcome")
    );
    assert!(
        bundle
            .memory_highlights
            .first()
            .map(|item| item.content.contains("routers/certificates"))
            .unwrap_or(false),
        "expected compact bundle to surface the verified structured memory first: {:?}",
        bundle.memory_highlights
    );
    assert!(
        bundle
            .overview
            .contains("reuse verified repo workflow outcome"),
        "expected compact workflow overview to carry structured trust phrasing: {}",
        bundle.overview
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

#[test]
#[ignore]
fn workflow_bench_assistant_scorecard() {
    let graph = build_certificate_guardrail_graph();

    let results = vec![
        bench_prepare_change_certificate_assistant(&graph),
        bench_working_set_certificate_assistant(&graph),
        bench_diagnose_failure_certificate_assistant(&graph),
    ];

    eprintln!(
        "\n┌──────────────────────────┬──────────────┬────────────┬────────────┬──────────┬────────────┐"
    );
    eprintln!(
        "│ Tool                     │ Payload (B)  │ Est Tokens │ Calls Saved│ Top3 Hit │ Target Hit │"
    );
    eprintln!(
        "├──────────────────────────┼──────────────┼────────────┼────────────┼──────────┼────────────┤"
    );

    let mut top3_hits = 0usize;
    let mut target_hits = 0usize;
    let mut payload_total = 0usize;
    let mut token_total = 0usize;
    let mut calls_saved_total = 0usize;

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

        eprintln!(
            "│ {:<24} │ {:>12} │ {:>10} │ {:>10} │ {:<8} │ {:<10} │",
            result.tool.as_str(),
            result.payload_bytes,
            result.estimated_tokens,
            result.calls_saved,
            if result.top3_hit { "PASS" } else { "FAIL" },
            if result.target_hit { "PASS" } else { "FAIL" },
        );
    }

    let top3_hit_rate = top3_hits as f64 / results.len() as f64;
    let target_hit_rate = target_hits as f64 / results.len() as f64;
    let average_payload = payload_total as f64 / results.len() as f64;
    let average_tokens = token_total as f64 / results.len() as f64;
    let average_calls_saved = calls_saved_total as f64 / results.len() as f64;

    eprintln!(
        "├──────────────────────────┴──────────────┴────────────┴────────────┴──────────┴────────────┤"
    );
    eprintln!(
        "  top3_hit_rate={:.0}% | target_hit_rate={:.0}% | avg_payload={:.0}B | avg_tokens={:.0} | avg_calls_saved={:.1}",
        top3_hit_rate * 100.0,
        target_hit_rate * 100.0,
        average_payload,
        average_tokens,
        average_calls_saved,
    );

    assert!(
        top3_hit_rate >= 0.90,
        "assistant prompt top3 hit rate {:.0}% fell below 90%",
        top3_hit_rate * 100.0
    );
    assert!(
        target_hit_rate >= 0.90,
        "assistant prompt target hit rate {:.0}% fell below 90%",
        target_hit_rate * 100.0
    );
}

#[test]
#[ignore]
fn workflow_bench_plan_edit_scorecard() {
    // Scores the implemented plan_edit contract against bug-fix, refactor,
    // and feature-add scenarios using synthetic fixture graphs.
    let results = vec![
        bench_plan_edit_bug_fix_case(),
        bench_plan_edit_refactor_case(),
        bench_plan_edit_feature_add_case(),
    ];

    eprintln!(
        "\n┌──────────────────────────┬──────────────┬──────────┬──────────┬──────────┬────────┬────────┬────────┐"
    );
    eprintln!(
        "│ Case                     │ Payload (B)  │ EditFile │ Symbol   │ Span     │ Caller │ Docs   │ Tests  │"
    );
    eprintln!(
        "├──────────────────────────┼──────────────┼──────────┼──────────┼──────────┼────────┼────────┼────────┤"
    );

    let mut edit_file_hits = 0usize;
    let mut symbol_hits = 0usize;
    let mut span_hits = 0usize;
    let mut caller_hits = 0usize;
    let mut doc_hits = 0usize;
    let mut test_hits = 0usize;
    let mut payload_total = 0usize;

    for result in &results {
        if result.edit_file_hit {
            edit_file_hits += 1;
        }
        if result.symbol_hit {
            symbol_hits += 1;
        }
        if result.span_hit {
            span_hits += 1;
        }
        if result.caller_hit {
            caller_hits += 1;
        }
        if result.doc_hit {
            doc_hits += 1;
        }
        if result.test_hit {
            test_hits += 1;
        }
        payload_total += result.payload_bytes;

        eprintln!(
            "│ {:<24} │ {:>12} │ {:<8} │ {:<8} │ {:<8} │ {:<6} │ {:<6} │ {:<6} │",
            result.case.as_str(),
            result.payload_bytes,
            if result.edit_file_hit { "PASS" } else { "FAIL" },
            if result.symbol_hit { "PASS" } else { "FAIL" },
            if result.span_hit { "PASS" } else { "FAIL" },
            if result.caller_hit { "PASS" } else { "FAIL" },
            if result.doc_hit { "PASS" } else { "FAIL" },
            if result.test_hit { "PASS" } else { "FAIL" },
        );
    }

    let case_count = results.len() as f64;
    let average_payload = payload_total as f64 / case_count;

    eprintln!(
        "├──────────────────────────┴──────────────┴──────────┴──────────┴──────────┴────────┴────────┴────────┤"
    );
    eprintln!(
        "  edit_file_hit_rate={:.0}% | symbol_hit_rate={:.0}% | span_hit_rate={:.0}% | caller_hit_rate={:.0}% | doc_hit_rate={:.0}% | test_hit_rate={:.0}% | avg_payload={:.0}B",
        (edit_file_hits as f64 / case_count) * 100.0,
        (symbol_hits as f64 / case_count) * 100.0,
        (span_hits as f64 / case_count) * 100.0,
        (caller_hits as f64 / case_count) * 100.0,
        (doc_hits as f64 / case_count) * 100.0,
        (test_hits as f64 / case_count) * 100.0,
        average_payload,
    );

    assert!(
        edit_file_hits == results.len(),
        "plan_edit should surface the likely edit files for all cases"
    );
    assert!(
        symbol_hits == results.len(),
        "plan_edit should surface the likely edit symbols for all cases"
    );
    assert!(
        span_hits == results.len(),
        "plan_edit should surface candidate edit spans for all cases"
    );
    assert!(
        caller_hits == results.len(),
        "plan_edit should surface affected callers/interfaces for all cases"
    );
    assert!(
        doc_hits >= 2,
        "plan_edit should surface docs guidance in the bug-fix and feature-add cases"
    );
    assert!(
        test_hits == results.len(),
        "plan_edit should surface recommended tests for all cases"
    );
    assert!(
        average_payload <= 3000.0,
        "plan_edit payload should stay compact enough for direct edit planning"
    );
}

#[test]
#[ignore]
fn workflow_bench_trace_scenario_scorecard() {
    let (payload_bytes, entrypoint_hit, path_hit, guard_hit, docs_hit, tests_hit, confidence_hit) =
        bench_trace_scenario_login_refresh_case();

    eprintln!(
        "\n┌──────────────────────────┬──────────────┬──────────┬──────────┬──────────┬────────┬────────┐"
    );
    eprintln!(
        "│ Case                     │ Payload (B)  │ Entry    │ Path     │ Guards   │ Docs   │ Tests  │"
    );
    eprintln!(
        "├──────────────────────────┼──────────────┼──────────┼──────────┼──────────┼────────┼────────┤"
    );
    eprintln!(
        "│ {:<24} │ {:>12} │ {:<8} │ {:<8} │ {:<8} │ {:<6} │ {:<6} │",
        "login_refresh_failure",
        payload_bytes,
        if entrypoint_hit { "PASS" } else { "FAIL" },
        if path_hit { "PASS" } else { "FAIL" },
        if guard_hit { "PASS" } else { "FAIL" },
        if docs_hit { "PASS" } else { "FAIL" },
        if tests_hit { "PASS" } else { "FAIL" },
    );
    eprintln!(
        "├──────────────────────────┴──────────────┴──────────┴──────────┴──────────┴────────┴────────┤"
    );
    eprintln!(
        "  confidence_separation={}",
        if confidence_hit { "PASS" } else { "FAIL" }
    );

    assert!(
        entrypoint_hit,
        "expected loginRoute to surface as the entrypoint"
    );
    assert!(
        path_hit,
        "expected the traced execution path to prefer the refresh branch over the fallback branch"
    );
    assert!(
        guard_hit,
        "expected the guard and failure branch to surface in the diagnosis output"
    );
    assert!(docs_hit, "expected docs coverage for the behavior prompt");
    assert!(tests_hit, "expected relevant tests for the behavior prompt");
    assert!(
        confidence_hit,
        "expected the refresh path and refresh regression test to outrank the fallback alternatives"
    );
    assert!(
        payload_bytes <= 5000,
        "trace_scenario payload should stay compact enough for a single workflow response"
    );
}
