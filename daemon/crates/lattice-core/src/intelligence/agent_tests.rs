use serde_json::json;

use crate::graph::model::{CodeGraph, EdgeKind};
use crate::intelligence::{
    diagnose_failure, expand_context, find_relevant_tests, get_repo_playbook,
    get_working_set_context, impact_from_diff, prepare_change, summarize_subsystem, trace_scenario,
    BundleMode, ExpandContextSeed, RulesDetector,
};
use crate::query::{CapsuleStats, ContextCapsule, ContextNode, PivotNode, QueryIntent};
use crate::symbols::{Language, SymbolId, SymbolKind};

fn make_id(file: &str, name: &str, offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset: offset,
    }
}

fn build_agent_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let login_id = make_id("src/auth.ts", "loginUser", 0);
    let session_id = make_id("src/session.ts", "createSession", 0);
    let route_id = make_id("src/routes/auth.ts", "loginRoute", 0);
    let service_id = make_id("src/api/auth_service.ts", "issueToken", 0);
    let auth_test_id = make_id("tests/auth.test.ts", "loginUserTest", 0);
    let session_test_id = make_id("tests/session.test.ts", "createSessionTest", 0);

    graph.add_node(
        login_id.clone(),
        SymbolKind::Function,
        "loginUser".to_string(),
        "function loginUser(input: Credentials): Promise<Session>".to_string(),
        "function loginUser(input) { return createSession(input.user); }".to_string(),
        "src/auth.ts".to_string(),
        10,
        25,
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
        12,
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
        18,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        service_id.clone(),
        SymbolKind::Function,
        "issueToken".to_string(),
        "function issueToken(user: User): string".to_string(),
        "function issueToken(user) { return 'token'; }".to_string(),
        "src/api/auth_service.ts".to_string(),
        7,
        16,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        auth_test_id.clone(),
        SymbolKind::Function,
        "loginUserTest".to_string(),
        "test('loginUser returns a session', async () => void)".to_string(),
        "test('loginUser returns a session', async () => { await loginRoute(req); });".to_string(),
        "tests/auth.test.ts".to_string(),
        4,
        14,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        session_test_id.clone(),
        SymbolKind::Function,
        "createSessionTest".to_string(),
        "test('createSession stores user data', () => void)".to_string(),
        "test('createSession stores user data', () => { createSession(user); });".to_string(),
        "tests/session.test.ts".to_string(),
        4,
        12,
        false,
        Language::TypeScript,
    );

    graph.add_edge(&login_id, &session_id, EdgeKind::Calls);
    graph.add_edge(&route_id, &login_id, EdgeKind::Calls);
    graph.add_edge(&service_id, &login_id, EdgeKind::Calls);
    graph.add_edge(&auth_test_id, &route_id, EdgeKind::Calls);
    graph.add_edge(&session_test_id, &session_id, EdgeKind::Calls);

    graph
}

fn build_trace_scenario_regression_graph() -> CodeGraph {
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
        "function loginRoute(req) { return refreshSession(req.body.user, req.body.refreshToken); }"
            .to_string(),
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
        "function refreshSession(user, token) { if (!verifyRefreshToken(token)) { return rejectRefresh(); } return createSession(user); }".to_string(),
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
        "function createSession(user) { persistSession(user); return { user }; }".to_string(),
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

fn build_capsule() -> ContextCapsule {
    ContextCapsule {
        query: "Fix the login timeout in auth flow".to_string(),
        intent: QueryIntent::FixBug,
        pivots: vec![PivotNode {
            symbol: "loginUser".to_string(),
            kind: "fn".to_string(),
            file: "src/auth.ts".to_string(),
            line: 10,
            source: "function loginUser(input) { return createSession(input.user); }".to_string(),
            score: 0.93,
            reason: "direct keyword hit".to_string(),
        }],
        context: vec![ContextNode {
            symbol: "createSession".to_string(),
            kind: "fn".to_string(),
            file: "src/session.ts".to_string(),
            line: 3,
            skeleton: "function createSession(user: User): Session".to_string(),
            relationship: "calls".to_string(),
            score: 0.62,
        }],
        memories: vec![json!({
            "content": "Auth bugs often need route and session coverage",
            "type": "pattern"
        })],
        stats: CapsuleStats {
            tokens_used: 100,
            tokens_saved: 200,
            nodes_evaluated: 4,
            nodes_included: 2,
            engine_version: "test".to_string(),
            seed_count: 2,
            seed_symbols: vec!["auth.ts:loginUser".to_string()],
        },
    }
}

fn build_worktree_poison_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let bills_id = make_id("react-web-ui/src/billing/Bills.tsx", "Bills", 0);
    let real_test_id = make_id(
        "react-web-ui/src/billing/__tests__/Bills.spec.tsx",
        "BillsSpec",
        0,
    );
    let artifact_test_id = make_id(
        "react-web-ui/.claude/worktrees/agent-ac9bb11a/e2e/navigation.spec.ts",
        "navigationSpec",
        0,
    );

    graph.add_node(
        bills_id.clone(),
        SymbolKind::Function,
        "Bills".to_string(),
        "function Bills(): JSX.Element".to_string(),
        "export function Bills() { return <div />; }".to_string(),
        "react-web-ui/src/billing/Bills.tsx".to_string(),
        1,
        12,
        true,
        Language::TypeScript,
    );
    graph.add_node(
        real_test_id.clone(),
        SymbolKind::Function,
        "BillsSpec".to_string(),
        "test('renders Bills', () => void)".to_string(),
        "test('renders Bills', () => { Bills(); });".to_string(),
        "react-web-ui/src/billing/__tests__/Bills.spec.tsx".to_string(),
        1,
        12,
        false,
        Language::TypeScript,
    );
    graph.add_node(
        artifact_test_id.clone(),
        SymbolKind::Function,
        "navigationSpec".to_string(),
        "test('navigates', async () => void)".to_string(),
        "test('navigates', async () => { await page.goto('/'); });".to_string(),
        "react-web-ui/.claude/worktrees/agent-ac9bb11a/e2e/navigation.spec.ts".to_string(),
        1,
        12,
        false,
        Language::TypeScript,
    );

    graph.add_edge(&real_test_id, &bills_id, EdgeKind::Calls);

    graph
}

fn build_failure_specificity_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let class_id = make_id("backend/os_account.py", "OSAccountProvider", 0);
    let method_id = make_id("backend/os_account.py", "rotate_password", 1);
    let test_id = make_id(
        "tests/test_rotation_providers.py",
        "test_rotate_password",
        0,
    );

    graph.add_node(
        class_id.clone(),
        SymbolKind::Class,
        "OSAccountProvider".to_string(),
        "class OSAccountProvider".to_string(),
        "class OSAccountProvider:\n    def rotate_password(self):\n        return True".to_string(),
        "backend/os_account.py".to_string(),
        1,
        120,
        true,
        Language::Python,
    );
    graph.add_node(
        method_id.clone(),
        SymbolKind::Method,
        "rotate_password".to_string(),
        "def rotate_password(self) -> bool".to_string(),
        "def rotate_password(self):\n    raise RuntimeError('bad password')".to_string(),
        "backend/os_account.py".to_string(),
        52,
        56,
        false,
        Language::Python,
    );
    graph.add_node(
        test_id.clone(),
        SymbolKind::Function,
        "test_rotate_password".to_string(),
        "def test_rotate_password() -> None".to_string(),
        "def test_rotate_password():\n    rotate_password()".to_string(),
        "tests/test_rotation_providers.py".to_string(),
        10,
        20,
        false,
        Language::Python,
    );

    graph.add_edge(&class_id, &method_id, EdgeKind::Contains);
    graph.add_edge(&test_id, &method_id, EdgeKind::Calls);

    graph
}

fn build_duplicate_symbol_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let cert_id = make_id("routers/certificates.py", "_verify_org_access", 0);
    let shared_id = make_id(
        "routers/compliance_mgmt/_shared.py",
        "_verify_org_access",
        0,
    );
    let policy_id = make_id("routers/certificates.py", "upsert_renewal_policy", 1);
    let cert_test_id = make_id(
        "tests/test_certificate_tenant_isolation.py",
        "TestCrossAccountRenewalPolicy.test_put_policy_foreign_org_returns_404",
        0,
    );
    let renewal_test_id = make_id("tests/test_cert_renewal.py", "test_put_renewal_policy", 0);

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
        cert_test_id.clone(),
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

    graph.add_edge(&policy_id, &cert_id, EdgeKind::Calls);
    graph.add_edge(&cert_test_id, &policy_id, EdgeKind::Calls);
    graph.add_edge(&renewal_test_id, &policy_id, EdgeKind::Calls);

    graph
}

fn build_duplicate_symbol_capsule() -> ContextCapsule {
    ContextCapsule {
        query: "Fix certificate access checks".to_string(),
        intent: QueryIntent::FixBug,
        pivots: vec![PivotNode {
            symbol: "_verify_org_access".to_string(),
            kind: "fn".to_string(),
            file: "routers/certificates.py".to_string(),
            line: 66,
            source: "def _verify_org_access(...): raise HTTPException(status_code=403)".to_string(),
            score: 0.91,
            reason: "entry file match".to_string(),
        }],
        context: vec![ContextNode {
            symbol: "_verify_org_access".to_string(),
            kind: "fn".to_string(),
            file: "routers/compliance_mgmt/_shared.py".to_string(),
            line: 14,
            skeleton: "def _verify_org_access(perms, org_id)".to_string(),
            relationship: "same_name".to_string(),
            score: 0.57,
        }],
        memories: vec![],
        stats: CapsuleStats {
            tokens_used: 80,
            tokens_saved: 120,
            nodes_evaluated: 2,
            nodes_included: 2,
            engine_version: "test".to_string(),
            seed_count: 2,
            seed_symbols: vec![
                "routers/certificates.py:_verify_org_access".to_string(),
                "routers/compliance_mgmt/_shared.py:_verify_org_access".to_string(),
            ],
        },
    }
}

fn build_prepare_change_noise_graph() -> CodeGraph {
    let mut graph = build_duplicate_symbol_graph();

    let cert_id = make_id("routers/certificates.py", "_verify_org_access", 0);
    let policy_id = make_id("routers/certificates.py", "upsert_renewal_policy", 1);
    let org_id = make_id("models/account.py", "Organization", 0);
    let policy_model_id = make_id("models/certificates.py", "CertificateRenewalPolicy", 0);
    let noisy_test_id = make_id(
        "tests/test_patch_engine_logic.py",
        "test_patch_engine_logic",
        0,
    );

    graph.add_node(
        org_id.clone(),
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
        policy_model_id.clone(),
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
        noisy_test_id.clone(),
        SymbolKind::Function,
        "test_patch_engine_logic".to_string(),
        "def test_patch_engine_logic()".to_string(),
        "def test_patch_engine_logic(): pass".to_string(),
        "tests/test_patch_engine_logic.py".to_string(),
        10,
        24,
        false,
        Language::Python,
    );

    graph.add_edge(&cert_id, &org_id, EdgeKind::Calls);
    graph.add_edge(&policy_id, &org_id, EdgeKind::Calls);
    graph.add_edge(&policy_id, &policy_model_id, EdgeKind::Calls);
    graph.add_edge(&noisy_test_id, &org_id, EdgeKind::Calls);

    graph
}

fn build_find_relevant_test_noise_graph() -> CodeGraph {
    let mut graph = build_duplicate_symbol_graph();
    let noisy_test_id = make_id(
        "tests/test_security_hardening.py",
        "test_verify_access_policy",
        0,
    );

    graph.add_node(
        noisy_test_id,
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

    graph
}

#[test]
fn test_prepare_change_prioritizes_primary_files_and_tests() {
    let graph = build_agent_graph();
    let files = vec![
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ];
    let rules = RulesDetector::new().detect_rules(&files);

    let bundle = prepare_change(
        &graph,
        &build_capsule(),
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        &rules,
        BundleMode::Compact,
    );

    assert!(
        bundle
            .primary_files
            .iter()
            .any(|item| item.file == "src/auth.ts"),
        "expected src/auth.ts in primary files: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .tests
            .iter()
            .any(|item| item.file == "tests/auth.test.ts"),
        "expected auth test in suggestions: {:?}",
        bundle
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        bundle.risks.iter().any(|risk| risk.symbol == "loginUser"),
        "expected loginUser risk in bundle: {:?}",
        bundle
            .risks
            .iter()
            .map(|risk| risk.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(bundle.memories.is_empty());
    assert_eq!(bundle.memory_highlights.len(), 1);
    assert!(!bundle.overview.is_empty());
    assert!(
        bundle.overview.contains("prior session pattern"),
        "expected compact overview to keep only a short memory reference: {}",
        bundle.overview
    );
    assert!(
        bundle
            .primary_files
            .iter()
            .all(|item| !item.reasons.is_empty() && !item.evidence.is_empty()),
        "expected compact primary files to keep bounded ranking evidence: {:?}",
        bundle.primary_files
    );
    assert!(
        bundle
            .rationale
            .iter()
            .any(|item| item.contains("Use this instead of rg")),
        "expected compact prepare_change to explain when it beats rg: {:?}",
        bundle.rationale
    );
}

#[test]
fn test_prepare_change_uses_repo_name_query_as_scope_hint() {
    let mut graph = CodeGraph::new();
    graph.add_node(
        make_id("lattice/AGENTS.md", "Lattice Context Engine", 0),
        SymbolKind::Section,
        "Lattice Context Engine".to_string(),
        "section Lattice Context Engine".to_string(),
        "Lattice workflow tool guidance".to_string(),
        "lattice/AGENTS.md".to_string(),
        1,
        5,
        false,
        Language::Markdown,
    );
    graph.add_node(
        make_id(
            "lattice/daemon/crates/lattice-core/src/workflow/tools.rs",
            "prepare_change_workflow_tools",
            1,
        ),
        SymbolKind::Function,
        "prepare_change_workflow_tools".to_string(),
        "fn prepare_change_workflow_tools()".to_string(),
        "fn prepare_change_workflow_tools() {}".to_string(),
        "lattice/daemon/crates/lattice-core/src/workflow/tools.rs".to_string(),
        10,
        20,
        true,
        Language::Rust,
    );
    graph.add_node(
        make_id("keystone/AGENTS.md", "Keystone Context Engine", 0),
        SymbolKind::Section,
        "Keystone Context Engine".to_string(),
        "section Keystone Context Engine".to_string(),
        "Keystone workflow guidance".to_string(),
        "keystone/AGENTS.md".to_string(),
        1,
        5,
        false,
        Language::Markdown,
    );
    graph.add_node(
        make_id(
            "keystone/.codex-home/.tmp/plugins/plugin-eval/src/core/workflow-guide.js",
            "workflowLabel",
            1,
        ),
        SymbolKind::Function,
        "workflowLabel".to_string(),
        "function workflowLabel()".to_string(),
        "function workflowLabel() {}".to_string(),
        "keystone/.codex-home/.tmp/plugins/plugin-eval/src/core/workflow-guide.js".to_string(),
        10,
        20,
        true,
        Language::JavaScript,
    );

    let capsule = ContextCapsule {
        query: "Improve Lattice workflow tools".to_string(),
        intent: QueryIntent::Explore,
        pivots: vec![PivotNode {
            symbol: "workflowLabel".to_string(),
            kind: "fn".to_string(),
            file: "keystone/.codex-home/.tmp/plugins/plugin-eval/src/core/workflow-guide.js"
                .to_string(),
            line: 10,
            source: "function workflowLabel() {}".to_string(),
            score: 1.0,
            reason: "keyword match".to_string(),
        }],
        context: vec![],
        memories: vec![],
        stats: CapsuleStats {
            tokens_used: 10,
            tokens_saved: 10,
            nodes_evaluated: 0,
            nodes_included: 0,
            engine_version: "test".to_string(),
            seed_count: 1,
            seed_symbols: vec![],
        },
    };
    let rules = RulesDetector::new().detect_rules(&[
        "lattice/daemon/crates/lattice-core/src/workflow/tools.rs".to_string(),
        "keystone/.codex-home/.tmp/plugins/plugin-eval/src/core/workflow-guide.js".to_string(),
    ]);

    let bundle = prepare_change(&graph, &capsule, &[], &[], &rules, BundleMode::Compact);

    assert_eq!(
        bundle.primary_files.first().map(|item| item.file.as_str()),
        Some("lattice/daemon/crates/lattice-core/src/workflow/tools.rs"),
        "expected repo name in query to scope workflow matches to lattice before sibling repos: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| format!("{}:{:?}", item.file, item.reasons))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_prepare_change_promotes_specific_file_stem_over_broad_path_match() {
    let mut graph = CodeGraph::new();
    graph.add_node(
        make_id("agent/pkg/agent/loops.go", "runAgentLoop", 0),
        SymbolKind::Function,
        "runAgentLoop".to_string(),
        "func runAgentLoop()".to_string(),
        "func runAgentLoop() { pollJobs() }".to_string(),
        "agent/pkg/agent/loops.go".to_string(),
        12,
        30,
        true,
        Language::Go,
    );
    graph.add_node(
        make_id(
            "backend/core/agent_version_drift.py",
            "check_agent_version_drift",
            0,
        ),
        SymbolKind::Function,
        "check_agent_version_drift".to_string(),
        "def check_agent_version_drift(db):".to_string(),
        "def check_agent_version_drift(db):\n    return compare_hosts_to_latest_publication(db)"
            .to_string(),
        "backend/core/agent_version_drift.py".to_string(),
        20,
        80,
        true,
        Language::Python,
    );
    graph.add_node(
        make_id(
            "backend/core/agent_rollout_engine.py",
            "active_rollout_for_version",
            0,
        ),
        SymbolKind::Function,
        "active_rollout_for_version".to_string(),
        "def active_rollout_for_version(db, version):".to_string(),
        "def active_rollout_for_version(db, version):\n    return db.query(Rollout).first()"
            .to_string(),
        "backend/core/agent_rollout_engine.py".to_string(),
        15,
        40,
        true,
        Language::Python,
    );
    graph.add_node(
        make_id(
            "backend/tests/test_agent_version_drift.py",
            "test_drift_suppressed_during_active_rollout",
            0,
        ),
        SymbolKind::Function,
        "test_drift_suppressed_during_active_rollout".to_string(),
        "def test_drift_suppressed_during_active_rollout(db):".to_string(),
        "def test_drift_suppressed_during_active_rollout(db):\n    check_agent_version_drift(db)"
            .to_string(),
        "backend/tests/test_agent_version_drift.py".to_string(),
        100,
        125,
        false,
        Language::Python,
    );

    let capsule = ContextCapsule {
        query: "agent version drift alerting should ignore active rollouts".to_string(),
        intent: QueryIntent::FixBug,
        pivots: vec![PivotNode {
            symbol: "runAgentLoop".to_string(),
            kind: "fn".to_string(),
            file: "agent/pkg/agent/loops.go".to_string(),
            line: 12,
            source: "func runAgentLoop() { pollJobs() }".to_string(),
            score: 9.0,
            reason: "broad path match".to_string(),
        }],
        context: vec![],
        memories: vec![],
        stats: CapsuleStats {
            tokens_used: 10,
            tokens_saved: 10,
            nodes_evaluated: 4,
            nodes_included: 1,
            engine_version: "test".to_string(),
            seed_count: 1,
            seed_symbols: vec!["runAgentLoop".to_string()],
        },
    };
    let rules = RulesDetector::new().detect_rules(&[
        "agent/pkg/agent/loops.go".to_string(),
        "backend/core/agent_version_drift.py".to_string(),
        "backend/core/agent_rollout_engine.py".to_string(),
        "backend/tests/test_agent_version_drift.py".to_string(),
    ]);

    let bundle = prepare_change(&graph, &capsule, &[], &[], &rules, BundleMode::Compact);

    assert_eq!(
        bundle.primary_files.first().map(|item| item.file.as_str()),
        Some("backend/core/agent_version_drift.py"),
        "expected exact file-stem/test-owner match to outrank broad agent runtime path: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| format!("{}:{:?}", item.file, item.reasons))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .primary_files
            .first()
            .is_some_and(|item| item.evidence.iter().any(|tag| tag == "stem")),
        "expected stem evidence on promoted file: {:?}",
        bundle.primary_files
    );
}

#[test]
fn test_find_relevant_tests_uses_diff_paths() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);
    let report = find_relevant_tests(
        &graph,
        &[],
        &["loginUser".to_string()],
        Some("diff --git a/src/auth.ts b/src/auth.ts\n--- a/src/auth.ts\n+++ b/src/auth.ts\n"),
        &rules,
        5,
    );

    assert!(
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/auth.test.ts"),
        "expected auth test in report: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report.gaps.is_empty(),
        "expected no gaps for matched test report: {:?}",
        report.gaps
    );
}

#[test]
fn test_find_relevant_tests_ignores_assistant_worktrees_and_prefers_graph_links() {
    let graph = build_worktree_poison_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "react-web-ui/src/billing/Bills.tsx".to_string(),
        "react-web-ui/src/billing/__tests__/Bills.spec.tsx".to_string(),
        "react-web-ui/.claude/worktrees/agent-ac9bb11a/e2e/navigation.spec.ts".to_string(),
    ]);

    let report = find_relevant_tests(
        &graph,
        &["react-web-ui/src/billing/Bills.tsx".to_string()],
        &["Bills".to_string()],
        None,
        &rules,
        5,
    );

    assert_eq!(
        report.tests.first().map(|item| item.file.as_str()),
        Some("react-web-ui/src/billing/__tests__/Bills.spec.tsx"),
        "expected real test to rank first: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .tests
            .iter()
            .all(|item| !item.file.contains("/.claude/worktrees/")),
        "assistant worktree tests should be filtered out: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .tests
            .first()
            .map(|item| item.confidence >= 6.0)
            .unwrap_or(false),
        "expected graph-linked test to have a strong confidence score: {:?}",
        report.tests
    );
}

#[test]
fn test_find_relevant_tests_matches_certificate_domain_tokens() {
    let graph = build_duplicate_symbol_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
    ]);

    let report = find_relevant_tests(
        &graph,
        &["routers/certificates.py".to_string()],
        &[
            "_verify_org_access".to_string(),
            "upsert_renewal_policy".to_string(),
        ],
        None,
        &rules,
        5,
    );

    assert!(
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/test_certificate_tenant_isolation.py"),
        "expected certificate tenant isolation test in report: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/test_cert_renewal.py"),
        "expected cert renewal test in report: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_find_relevant_tests_prefers_supplied_file_for_duplicate_symbols() {
    let graph = build_duplicate_symbol_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
    ]);

    let report = find_relevant_tests(
        &graph,
        &["routers/certificates.py".to_string()],
        &["_verify_org_access".to_string()],
        None,
        &rules,
        5,
    );

    assert!(
        report
            .source_files
            .iter()
            .all(|file| file != "routers/compliance_mgmt/_shared.py"),
        "expected duplicate helper file to stay out of source file anchors: {:?}",
        report.source_files
    );
}

#[test]
fn test_prepare_change_suggested_expand_uses_stable_handle_for_duplicate_symbols() {
    let graph = build_duplicate_symbol_graph();
    let files = vec![
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
    ];
    let rules = RulesDetector::new().detect_rules(&files);

    let first_bundle = prepare_change(
        &graph,
        &build_duplicate_symbol_capsule(),
        &["routers/certificates.py".to_string()],
        &["_verify_org_access".to_string()],
        &rules,
        BundleMode::Compact,
    );
    let second_bundle = prepare_change(
        &graph,
        &build_duplicate_symbol_capsule(),
        &["routers/certificates.py".to_string()],
        &["_verify_org_access".to_string()],
        &rules,
        BundleMode::Compact,
    );

    let first_suggested = first_bundle
        .suggested_expand
        .as_ref()
        .expect("expected prepare_change to suggest a follow-up target");
    let second_suggested = second_bundle
        .suggested_expand
        .as_ref()
        .expect("expected prepare_change to suggest a follow-up target");

    assert_eq!(
        first_suggested.focus, second_suggested.focus,
        "expected stable follow-up target to be deterministic across repeated planning calls"
    );
    assert_ne!(
        first_suggested.focus, "symbol:_verify_org_access",
        "expected duplicate symbol follow-up target to be stable rather than bare-name ambiguous"
    );

    let seed = ExpandContextSeed {
        query: Some(first_bundle.query.clone()),
        files: first_bundle
            .primary_files
            .iter()
            .chain(first_bundle.secondary_files.iter())
            .map(|item| item.file.clone())
            .collect(),
        symbols: first_bundle
            .symbols
            .iter()
            .map(|item| item.symbol.clone())
            .collect(),
        tests: first_bundle
            .tests
            .iter()
            .map(|item| item.file.clone())
            .collect(),
        memories: first_bundle.memories.clone(),
    };
    let report = expand_context(&graph, &seed, &first_suggested.focus, 800);

    assert_eq!(report.focus_type, "symbol");
    assert_eq!(
        report.symbols.first().map(|item| item.symbol.as_str()),
        Some("_verify_org_access"),
        "expected stable target to resolve to the intended duplicate symbol"
    );
    assert_eq!(
        report.symbols.first().map(|item| item.file.as_str()),
        Some("routers/certificates.py"),
        "expected stable target to resolve to the certificate router symbol rather than the same-name helper"
    );
    assert!(
        report
            .files
            .iter()
            .any(|item| item.file == "routers/certificates.py"),
        "expected expanded file context to include the intended file: {:?}",
        report
            .files
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_expand_context_keeps_legacy_symbol_name_fallback_for_duplicate_symbols() {
    let graph = build_duplicate_symbol_graph();
    let seed = ExpandContextSeed {
        query: Some("Fix certificate access checks".to_string()),
        files: vec![
            "routers/certificates.py".to_string(),
            "routers/compliance_mgmt/_shared.py".to_string(),
        ],
        symbols: vec![
            "_verify_org_access".to_string(),
            "upsert_renewal_policy".to_string(),
        ],
        tests: vec![
            "tests/test_certificate_tenant_isolation.py".to_string(),
            "tests/test_cert_renewal.py".to_string(),
        ],
        memories: vec![],
    };

    let report = expand_context(&graph, &seed, "symbol:_verify_org_access", 800);

    let expanded = report
        .symbols
        .first()
        .expect("expected legacy bare-name expansion to still resolve a duplicate symbol");
    assert_eq!(report.focus_type, "symbol");
    assert_eq!(expanded.symbol, "_verify_org_access");
    assert!(
        matches!(
            expanded.file.as_str(),
            "routers/certificates.py" | "routers/compliance_mgmt/_shared.py"
        ),
        "expected fallback expansion to resolve to one of the duplicate files, got {:?}",
        expanded.file
    );
    assert!(
        report.files.iter().any(|item| item.file == expanded.file),
        "expected file context to track the resolved duplicate symbol"
    );
}

#[test]
fn test_find_relevant_tests_filters_generic_symbol_overlap_noise() {
    let graph = build_find_relevant_test_noise_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
        "tests/test_security_hardening.py".to_string(),
    ]);

    let report = find_relevant_tests(
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
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/test_cert_renewal.py"),
        "expected cert renewal test to stay in the report: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .tests
            .iter()
            .all(|item| item.file != "tests/test_security_hardening.py"),
        "expected generic access/policy token overlap alone not to surface unrelated security test: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    let top_test = report
        .tests
        .iter()
        .find(|item| item.file == "tests/test_certificate_tenant_isolation.py")
        .expect("expected certificate tenant isolation test");
    assert!(
        top_test
            .evidence
            .iter()
            .any(|item| item == "domain" || item == "path"),
        "expected calibrated evidence tags on relevant tests: {:?}",
        top_test
    );
    assert!(
        top_test.confidence_band == "high" || top_test.confidence_band == "medium",
        "expected anchored test to avoid low-confidence band: {:?}",
        top_test
    );
}

#[test]
fn test_get_working_set_context_collects_symbols_tests_and_memories() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);
    let memories = vec![json!({
        "id": "mem-1",
        "content": "Auth fixes usually need both route and session checks",
        "scope": "repo"
    })];

    let report = get_working_set_context(
        &graph,
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        Some("login timeout auth"),
        &memories,
        &rules,
        BundleMode::Compact,
    );

    assert!(
        report.files.iter().any(|item| item.file == "src/auth.ts"),
        "expected src/auth.ts in working-set files: {:?}",
        report
            .files
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .active_symbols
            .iter()
            .any(|item| item.symbol == "loginUser"),
        "expected loginUser in active symbols: {:?}",
        report
            .active_symbols
            .iter()
            .map(|item| item.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .nearby_symbols
            .iter()
            .any(|item| item.symbol == "createSession" || item.symbol == "loginRoute"),
        "expected nearby auth symbols in working set: {:?}",
        report
            .nearby_symbols
            .iter()
            .map(|item| item.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/auth.test.ts"),
        "expected auth test suggestion in working set: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(report.memories.is_empty());
    assert_eq!(report.memory_highlights.len(), 1);
    assert!(!report.overview.is_empty());
    assert!(
        report.overview.contains("prior repo observation"),
        "expected compact overview to keep only a short memory reference: {}",
        report.overview
    );
}

#[test]
fn test_get_working_set_context_prefers_verified_structured_memories() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);
    let memories = vec![
        json!({
            "id": "mem-contradicted",
            "content": "Auth fixes should inspect fallbackLogin before touching loginUser.",
            "type": "observation",
            "scope": "repo",
            "verification_status": "verified",
            "contradicted_by_memory_ids": ["mem-verified"],
            "confidence_reason": "Older route note before refresh flow changes",
            "freshness_policy": "manual_review"
        }),
        json!({
            "id": "mem-verified",
            "content": "Auth fixes should inspect loginUser before createSession.",
            "type": "pattern",
            "assertion_type": "workflow_outcome",
            "scope": "repo",
            "verification_status": "verified",
            "confidence_reason": "Validated by recent auth regression coverage",
            "freshness_policy": "repo_scoped",
            "freshness_policy_detail": "Refresh after auth flow changes",
            "provenance": [{
                "source": "test",
                "reference": "tests/auth.test.ts"
            }],
            "evidence": [{
                "kind": "test",
                "reference": "tests/auth.test.ts",
                "detail": "Covers login route and session path"
            }]
        }),
        json!({
            "id": "mem-superseded",
            "content": "Old auth timeout workaround retried createSession from the route.",
            "type": "pattern",
            "scope": "repo",
            "superseded_by_memory_id": "mem-verified",
            "confidence_reason": "Superseded during auth refactor"
        }),
    ];

    let report = get_working_set_context(
        &graph,
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        Some("login timeout auth"),
        &memories,
        &rules,
        BundleMode::Full,
    );

    assert_eq!(
        report
            .memory_highlights
            .first()
            .and_then(|item| item.verification_status.as_deref()),
        Some("verified")
    );
    assert_eq!(
        report
            .memory_highlights
            .first()
            .and_then(|item| item.assertion_type.as_deref()),
        Some("workflow_outcome")
    );
    assert_eq!(
        report
            .memory_highlights
            .first()
            .and_then(|item| item.freshness_policy.as_deref()),
        Some("repo_scoped")
    );
    assert!(
        report
            .memory_highlights
            .first()
            .map(|item| item.content.contains("loginUser before createSession"))
            .unwrap_or(false),
        "expected verified structured memory to lead highlights: {:?}",
        report.memory_highlights
    );
    assert!(
        report
            .overview
            .contains("reuse verified repo workflow outcome"),
        "expected overview to reflect structured trust signal: {}",
        report.overview
    );
    assert_eq!(
        report
            .memories
            .first()
            .and_then(|item| item.get("id"))
            .and_then(|item| item.as_str()),
        Some("mem-verified")
    );
    assert_eq!(
        report
            .memories
            .first()
            .and_then(|item| item.get("assertion_type"))
            .and_then(|item| item.as_str()),
        Some("workflow_outcome")
    );
    assert_eq!(
        report
            .memories
            .first()
            .and_then(|item| item.get("verification_status"))
            .and_then(|item| item.as_str()),
        Some("verified")
    );
    assert!(
        report
            .memories
            .first()
            .and_then(|item| item.get("provenance"))
            .and_then(|item| item.as_array())
            .map(|items| !items.is_empty())
            .unwrap_or(false),
        "expected compact full-memory payload to retain provenance: {:?}",
        report.memories
    );
    assert!(
        report
            .memories
            .first()
            .and_then(|item| item.get("evidence"))
            .and_then(|item| item.as_array())
            .map(|items| !items.is_empty())
            .unwrap_or(false),
        "expected compact full-memory payload to retain evidence: {:?}",
        report.memories
    );
}

#[test]
fn test_summarize_subsystem_compresses_key_files_symbols_and_tests() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "src/api/auth_service.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);
    let memories = vec![json!({
        "content": "Auth routes call loginUser before session creation.",
        "type": "pattern",
        "scope": "repo",
        "is_stale": false
    })];

    let report = summarize_subsystem(
        &graph,
        "auth login flow",
        &["src/routes/auth.ts".to_string()],
        &["loginUser".to_string()],
        &memories,
        &rules,
        BundleMode::Compact,
    );

    assert!(!report.overview.is_empty());
    assert!(
        report
            .key_files
            .iter()
            .any(|item| item.file == "src/routes/auth.ts" || item.file == "src/auth.ts"),
        "expected auth files in subsystem summary: {:?}",
        report.key_files
    );
    assert!(
        report
            .key_symbols
            .iter()
            .any(|item| item.symbol == "loginUser"),
        "expected loginUser in subsystem summary: {:?}",
        report.key_symbols
    );
    assert!(
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/auth.test.ts"),
        "expected auth test in subsystem summary: {:?}",
        report.tests
    );
    assert_eq!(report.memories.len(), 1);
}

#[test]
fn test_summarize_subsystem_ignores_markdown_only_explicit_files_for_code_query() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "src/api/auth_service.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
        "README.md".to_string(),
        "AGENTS.md".to_string(),
        "CLAUDE.example.md".to_string(),
    ]);

    let report = summarize_subsystem(
        &graph,
        "auth login flow",
        &[
            "README.md".to_string(),
            "AGENTS.md".to_string(),
            "CLAUDE.example.md".to_string(),
        ],
        &[],
        &[],
        &rules,
        BundleMode::Compact,
    );

    assert!(
        report
            .key_files
            .iter()
            .all(|item| !item.file.ends_with(".md")),
        "expected code files to displace markdown-only explicit anchors: {:?}",
        report.key_files
    );
    assert!(
        report
            .key_files
            .iter()
            .any(|item| item.file == "src/routes/auth.ts" || item.file == "src/auth.ts"),
        "expected auth code files in subsystem summary after dropping markdown anchors: {:?}",
        report.key_files
    );
}

#[test]
fn test_summarize_subsystem_skips_test_helpers_for_code_review_query() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "src/api/auth_service.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);

    let report = summarize_subsystem(
        &graph,
        "auth login route workflow",
        &[
            "src/auth.ts".to_string(),
            "src/routes/auth.ts".to_string(),
            "src/api/auth_service.ts".to_string(),
        ],
        &[],
        &[],
        &rules,
        BundleMode::Compact,
    );

    assert!(
        report
            .key_symbols
            .iter()
            .all(|item| !item.file.starts_with("tests/")),
        "expected code review summary to avoid test helper symbols: {:?}",
        report
            .key_symbols
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .key_symbols
            .iter()
            .all(|item| report.key_files.iter().any(|file| file.file == item.file)),
        "expected key symbols to stay within the visible key files: key_files={:?} key_symbols={:?}",
        report
            .key_files
            .iter()
            .map(|item| item.file.clone())
            .collect::<Vec<_>>(),
        report
            .key_symbols
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_get_repo_playbook_surfaces_key_files_symbols_and_patterns() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "src/api/auth_service.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);
    let memories = vec![json!({
        "content": "Auth entrypoints live under src/routes before delegating to src/auth.ts.",
        "type": "pattern",
        "scope": "repo",
        "is_stale": false
    })];

    let report = get_repo_playbook(&graph, &memories, &rules, BundleMode::Compact);

    assert!(!report.overview.is_empty());
    assert!(!report.architecture.is_empty());
    assert!(!report.key_files.is_empty());
    assert!(
        report
            .notable_symbols
            .iter()
            .any(|item| item.symbol == "loginUser"),
        "expected loginUser in repo playbook symbols: {:?}",
        report.notable_symbols
    );
    assert!(
        report
            .durable_patterns
            .iter()
            .any(|item| item.content.contains("Auth entrypoints")),
        "expected durable memory pattern in playbook: {:?}",
        report.durable_patterns
    );
}

#[test]
fn test_diagnose_failure_maps_files_symbols_and_tests() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);

    let report = diagnose_failure(
        &graph,
        "src/auth.ts:12:5 - error TS2345: loginUser timed out\n    at src/routes/auth.ts:8:2",
        None,
        &rules,
        BundleMode::Compact,
    );

    assert_eq!(report.kind, "compiler");
    assert!(
        report
            .extracted_files
            .iter()
            .any(|file| file == "src/auth.ts"),
        "expected src/auth.ts in extracted files: {:?}",
        report.extracted_files
    );
    assert!(
        report
            .extracted_symbols
            .iter()
            .any(|symbol| symbol == "loginUser"),
        "expected loginUser in extracted symbols: {:?}",
        report.extracted_symbols
    );
    assert!(
        report
            .suspects
            .iter()
            .any(|item| item.symbol == "loginUser"),
        "expected loginUser in suspects: {:?}",
        report
            .suspects
            .iter()
            .map(|item| item.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/auth.test.ts"),
        "expected auth test in diagnosis: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(report.memory_highlights.is_empty());
    assert!(!report.overview.is_empty());
    assert!(!report.next_steps.is_empty());
}

#[test]
fn test_diagnose_failure_prefers_most_specific_line_match() {
    let graph = build_failure_specificity_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "backend/os_account.py".to_string(),
        "tests/test_rotation_providers.py".to_string(),
    ]);

    let report = diagnose_failure(
        &graph,
        "tests/test_rotation_providers.py:18: AssertionError\nbackend/os_account.py:54: RuntimeError: bad password",
        Some("test"),
        &rules,
        BundleMode::Compact,
    );

    assert_eq!(
        report.suspects.first().map(|item| item.symbol.as_str()),
        Some("rotate_password"),
        "expected method-level suspect ahead of class-level container: {:?}",
        report
            .suspects
            .iter()
            .map(|item| item.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .likely_causes
            .iter()
            .any(|item| item.contains("backend/os_account.py:52-56")),
        "expected explicit line-span mapping in likely causes: {:?}",
        report.likely_causes
    );
    assert_eq!(
        report.tests.first().map(|item| item.file.as_str()),
        Some("tests/test_rotation_providers.py"),
        "expected the directly referenced failing test to rank first: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_diagnose_failure_prefers_matching_file_for_duplicate_symbol_names() {
    let graph = build_duplicate_symbol_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
    ]);

    let report = diagnose_failure(
        &graph,
        "tests/test_certificate_tenant_isolation.py:362: AssertionError: expected 404, got 403\nrouters/certificates.py:84: HTTPException(status_code=403)\nrouters/certificates.py:380: _verify_org_access(...)",
        Some("test"),
        &rules,
        BundleMode::Compact,
    );

    assert_eq!(
        report.suspects.first().map(|item| item.file.as_str()),
        Some("routers/certificates.py"),
        "expected certificate router helper to outrank duplicate shared helper: {:?}",
        report
            .suspects
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .suspects
            .iter()
            .all(|item| item.file != "routers/compliance_mgmt/_shared.py" || item.symbol != "_verify_org_access"),
        "expected duplicate helper from another router to be filtered when failing file is known: {:?}",
        report
            .suspects
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_diagnose_failure_keeps_multiple_line_refs_for_same_file() {
    let graph = build_duplicate_symbol_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
    ]);

    let report = diagnose_failure(
        &graph,
        "tests/test_certificate_tenant_isolation.py:362: AssertionError: expected 404, got 403\nrouters/certificates.py:84: HTTPException(status_code=403)\nrouters/certificates.py:380: _verify_org_access(...)",
        Some("test"),
        &rules,
        BundleMode::Compact,
    );

    assert!(
        report
            .likely_causes
            .iter()
            .any(|item| item.contains("routers/certificates.py:365-430")),
        "expected endpoint line reference from the same file to survive alongside helper line reference: {:?}",
        report.likely_causes
    );
    assert!(
        report
            .suspects
            .iter()
            .any(|item| item.file == "routers/certificates.py" && item.symbol == "upsert_renewal_policy"),
        "expected endpoint function from the second file line reference to appear in suspects: {:?}",
        report
            .suspects
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
    let top_suspect = report.suspects.first().expect("expected suspects");
    assert_eq!(top_suspect.confidence_band, "high");
    assert!(
        top_suspect
            .evidence
            .iter()
            .any(|item| item == "line" || item == "direct"),
        "expected line/direct evidence on top suspect: {:?}",
        top_suspect
    );
}

#[test]
fn test_diagnose_failure_downgrades_graph_only_related_symbols() {
    let mut graph = build_duplicate_symbol_graph();
    let policy_id = make_id("routers/certificates.py", "upsert_renewal_policy", 1);
    let org_id = make_id("models/account.py", "Organization", 0);

    graph.add_node(
        org_id.clone(),
        SymbolKind::Class,
        "Organization".to_string(),
        "class Organization".to_string(),
        "class Organization: pass".to_string(),
        "models/account.py".to_string(),
        10,
        42,
        true,
        Language::Python,
    );
    graph.add_edge(&policy_id, &org_id, EdgeKind::TypeRef);

    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
        "models/account.py".to_string(),
    ]);

    let report = diagnose_failure(
        &graph,
        "tests/test_certificate_tenant_isolation.py:362: AssertionError: expected 404, got 403\nrouters/certificates.py:84: HTTPException(status_code=403)\nrouters/certificates.py:380: _verify_org_access(...)",
        Some("test"),
        &rules,
        BundleMode::Compact,
    );

    let organization = report
        .related_symbols
        .iter()
        .find(|item| item.file == "models/account.py" && item.symbol == "Organization")
        .expect("expected graph-only Organization related symbol");
    assert_eq!(organization.confidence_band, "low");
    assert_eq!(organization.evidence, vec!["graph".to_string()]);
}

#[test]
fn test_prepare_change_promotes_entry_file_over_duplicate_symbol_helpers() {
    let graph = build_duplicate_symbol_graph();
    let capsule = ContextCapsule {
        query: "Return 404 instead of 403 for cross-org renewal policy org access".to_string(),
        intent: QueryIntent::FixBug,
        pivots: vec![],
        context: vec![],
        memories: vec![],
        stats: CapsuleStats {
            tokens_used: 10,
            tokens_saved: 10,
            nodes_evaluated: 0,
            nodes_included: 0,
            engine_version: "test".to_string(),
            seed_count: 0,
            seed_symbols: vec![],
        },
    };
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "routers/compliance_mgmt/_shared.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
    ]);

    let bundle = prepare_change(
        &graph,
        &capsule,
        &["routers/certificates.py".to_string()],
        &[
            "_verify_org_access".to_string(),
            "upsert_renewal_policy".to_string(),
        ],
        &rules,
        BundleMode::Compact,
    );

    assert_eq!(
        bundle.primary_files.first().map(|item| item.file.as_str()),
        Some("routers/certificates.py"),
        "expected explicit entry file to be first primary file: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .primary_files
            .iter()
            .all(|item| item.file != "routers/compliance_mgmt/_shared.py"),
        "expected duplicate helper file to stay out of primary files: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_prepare_change_test_selection_stays_anchored_to_entry_files() {
    let graph = build_prepare_change_noise_graph();
    let capsule = ContextCapsule {
        query: "Return 404 instead of 403 for cross-org organization access in certificate renewal policy endpoints".to_string(),
        intent: QueryIntent::FixBug,
        pivots: vec![],
        context: vec![],
        memories: vec![],
        stats: CapsuleStats {
            tokens_used: 10,
            tokens_saved: 10,
            nodes_evaluated: 0,
            nodes_included: 0,
            engine_version: "test".to_string(),
            seed_count: 0,
            seed_symbols: vec![],
        },
    };
    let rules = RulesDetector::new().detect_rules(&[
        "routers/certificates.py".to_string(),
        "models/account.py".to_string(),
        "models/certificates.py".to_string(),
        "tests/test_certificate_tenant_isolation.py".to_string(),
        "tests/test_cert_renewal.py".to_string(),
        "tests/test_patch_engine_logic.py".to_string(),
    ]);

    let bundle = prepare_change(
        &graph,
        &capsule,
        &["routers/certificates.py".to_string()],
        &[
            "_verify_org_access".to_string(),
            "upsert_renewal_policy".to_string(),
        ],
        &rules,
        BundleMode::Compact,
    );

    assert!(
        bundle
            .tests
            .iter()
            .take(2)
            .any(|item| item.file == "tests/test_certificate_tenant_isolation.py"),
        "expected certificate-focused tests to stay at the top of the bundle: {:?}",
        bundle
            .tests
            .iter()
            .map(|item| format!("{}:{}", item.file, item.confidence))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .tests
            .iter()
            .take(2)
            .all(|item| item.file != "tests/test_patch_engine_logic.py"),
        "expected broad patch test not to outrank certificate-local tests: {:?}",
        bundle
            .tests
            .iter()
            .map(|item| format!("{}:{}", item.file, item.confidence))
            .collect::<Vec<_>>()
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
        "expected certificate-local model file to outrank broad account model when entry file is certificates.py: {:?}",
        bundle
            .primary_files
            .iter()
            .map(|item| format!("{}:{}", item.file, item.score))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .risks
            .iter()
            .all(|item| item.file != "models/account.py"),
        "expected broad account-model risks to be filtered out of certificate-local bundle: {:?}",
        bundle
            .risks
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_expand_context_returns_delta_for_cached_symbol_focus() {
    let graph = build_agent_graph();
    let seed = ExpandContextSeed {
        query: Some("Fix login timeout".to_string()),
        files: vec!["src/auth.ts".to_string(), "src/routes/auth.ts".to_string()],
        symbols: vec!["loginUser".to_string(), "loginRoute".to_string()],
        tests: vec!["tests/auth.test.ts".to_string()],
        memories: vec![json!({
            "content": "Auth fixes usually need route coverage",
            "type": "pattern"
        })],
    };

    let report = expand_context(&graph, &seed, "symbol:loginUser", 800);

    assert_eq!(report.focus_type, "symbol");
    assert!(
        report.symbols.iter().any(|item| item.symbol == "loginUser"),
        "expected loginUser in expanded symbols: {:?}",
        report
            .symbols
            .iter()
            .map(|item| item.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .symbols
            .first()
            .map(|item| item.dependents.iter().any(|dep| dep.symbol == "loginRoute"))
            .unwrap_or(false),
        "expected loginRoute dependent in expansion: {:?}",
        report
            .symbols
            .first()
            .map(|item| {
                item.dependents
                    .iter()
                    .map(|dep| dep.symbol.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    );
    assert!(
        report.files.iter().any(|item| item.file == "src/auth.ts"),
        "expected src/auth.ts file context: {:?}",
        report
            .files
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(report.memories.len(), 1);
}

#[test]
fn test_expand_context_memory_focus_prefers_verified_match() {
    let graph = build_agent_graph();
    let seed = ExpandContextSeed {
        query: Some("Fix auth timeout".to_string()),
        files: vec!["src/auth.ts".to_string()],
        symbols: vec!["loginUser".to_string()],
        tests: vec!["tests/auth.test.ts".to_string()],
        memories: vec![
            json!({
                "id": "mem-contradicted",
                "content": "Auth timeout fixes should start with fallbackLogin.",
                "type": "observation",
                "scope": "repo",
                "contradicted_by_memory_ids": ["mem-verified"]
            }),
            json!({
                "id": "mem-verified",
                "content": "Auth timeout fixes should start with loginUser and session coverage.",
                "type": "pattern",
                "assertion_type": "workflow_outcome",
                "scope": "repo",
                "verification_status": "verified"
            }),
        ],
    };

    let report = expand_context(&graph, &seed, "Auth timeout fixes", 800);

    assert_eq!(report.focus_type, "memory");
    assert_eq!(
        report
            .memories
            .first()
            .and_then(|item| item.get("id"))
            .and_then(|item| item.as_str()),
        Some("mem-verified")
    );
}

#[test]
fn test_impact_from_diff_maps_changed_symbols_and_dependents() {
    let graph = build_agent_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "src/routes/auth.ts".to_string(),
        "tests/auth.test.ts".to_string(),
        "tests/session.test.ts".to_string(),
    ]);
    let report = impact_from_diff(
        &graph,
        "diff --git a/src/auth.ts b/src/auth.ts\n--- a/src/auth.ts\n+++ b/src/auth.ts\n@@ -10,3 +10,4 @@\n-const before = 1;\n+const after = 2;\n",
        &[],
        &[],
        &rules,
        BundleMode::Compact,
        2,
    );

    assert!(
        report
            .changed_files
            .iter()
            .any(|item| item.file == "src/auth.ts"),
        "expected changed file in report: {:?}",
        report
            .changed_files
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .changed_symbols
            .iter()
            .any(|item| item.symbol == "loginUser"),
        "expected loginUser in changed symbols: {:?}",
        report
            .changed_symbols
            .iter()
            .map(|item| item.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .affected_symbols
            .iter()
            .any(|item| item.symbol == "loginRoute"),
        "expected loginRoute in affected symbols: {:?}",
        report
            .affected_symbols
            .iter()
            .map(|item| item.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .tests
            .iter()
            .any(|item| item.file == "tests/auth.test.ts"),
        "expected auth test in impact report: {:?}",
        report
            .tests
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .review_checklist
            .iter()
            .any(|item| item.message.contains("callers") || item.message.contains("dependents")),
        "expected caller/dependent checklist item: {:?}",
        report
            .review_checklist
            .iter()
            .map(|item| item.message.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_trace_scenario_separates_likely_and_plausible_entrypoints_and_paths() {
    let graph = build_trace_scenario_regression_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/routes/auth.ts".to_string(),
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "tests/auth_refresh.test.ts".to_string(),
        "tests/auth_login.test.ts".to_string(),
        "docs/auth.md".to_string(),
    ]);

    let bundle = trace_scenario(
        &graph,
        "Why does login fail after refresh token verification?",
        &["src/routes/auth.ts".to_string()],
        &["loginRoute".to_string()],
        &rules,
        BundleMode::Full,
    );

    assert!(
        !bundle.likely_entrypoints.is_empty(),
        "expected likely scenario entrypoints: {:?}",
        bundle
            .likely_entrypoints
            .iter()
            .map(|item| format!("{}:{}:{}", item.file, item.symbol, item.confidence_band))
            .collect::<Vec<_>>()
    );
    assert!(
        !bundle.plausible_entrypoints.is_empty(),
        "expected plausible scenario entrypoints for alternate branches: {:?}",
        bundle
            .likely_entrypoints
            .iter()
            .chain(bundle.plausible_entrypoints.iter())
            .map(|item| format!("{}:{}:{}", item.file, item.symbol, item.confidence_band))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .likely_entrypoints
            .iter()
            .all(|item| item.confidence_band == "high" || item.confidence_band == "medium"),
        "expected likely entrypoints to keep high/medium confidence: {:?}",
        bundle
            .likely_entrypoints
            .iter()
            .map(|item| format!("{}:{}", item.symbol, item.confidence_band))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .plausible_entrypoints
            .iter()
            .all(|item| item.confidence_band == "low"),
        "expected plausible entrypoints to stay in low-confidence band: {:?}",
        bundle
            .plausible_entrypoints
            .iter()
            .map(|item| format!("{}:{}", item.symbol, item.confidence_band))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle.plausible_entrypoints.iter().all(|item| {
            bundle
                .likely_entrypoints
                .iter()
                .all(|likely| likely.file != item.file || likely.symbol != item.symbol)
        }),
        "expected plausible entrypoints to remain distinct from likely entrypoints: likely={:?} plausible={:?}",
        bundle
            .likely_entrypoints
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>(),
        bundle
            .plausible_entrypoints
            .iter()
            .map(|item| format!("{}::{}", item.file, item.symbol))
            .collect::<Vec<_>>()
    );

    assert!(
        !bundle.execution_path.is_empty(),
        "expected traced execution path segments: {:?}",
        bundle
            .execution_path
            .iter()
            .map(|item| format!(
                "{}->{} ({})",
                item.from_symbol, item.to_symbol, item.confidence_band
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        !bundle.plausible_paths.is_empty(),
        "expected plausible alternate path segments: {:?}",
        bundle
            .execution_path
            .iter()
            .chain(bundle.plausible_paths.iter())
            .map(|item| format!(
                "{}->{} ({})",
                item.from_symbol, item.to_symbol, item.confidence_band
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .execution_path
            .iter()
            .all(|item| item.confidence_band == "high" || item.confidence_band == "medium"),
        "expected execution path confidence to be high/medium: {:?}",
        bundle
            .execution_path
            .iter()
            .map(|item| format!(
                "{}->{}:{}",
                item.from_symbol, item.to_symbol, item.confidence_band
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .plausible_paths
            .iter()
            .all(|item| item.confidence_band == "low"),
        "expected plausible path confidence to stay low: {:?}",
        bundle
            .plausible_paths
            .iter()
            .map(|item| format!(
                "{}->{}:{}",
                item.from_symbol, item.to_symbol, item.confidence_band
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .execution_path
            .iter()
            .any(|item| item.from_symbol == "loginRoute" && item.to_symbol == "refreshSession"),
        "expected refresh path in execution segments: {:?}",
        bundle
            .execution_path
            .iter()
            .map(|item| format!("{}->{}", item.from_symbol, item.to_symbol))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .plausible_paths
            .iter()
            .any(|item| item.from_symbol == "fallbackLogin" || item.to_symbol == "fallbackLogin"),
        "expected fallback-related branch in plausible segments: {:?}",
        bundle
            .plausible_paths
            .iter()
            .map(|item| format!("{}->{}", item.from_symbol, item.to_symbol))
            .collect::<Vec<_>>()
    );
    let top_execution_score = bundle
        .execution_path
        .iter()
        .map(|item| item.score)
        .fold(0.0, f64::max);
    let top_plausible_score = bundle
        .plausible_paths
        .iter()
        .map(|item| item.score)
        .fold(0.0, f64::max);
    assert!(
        top_execution_score > top_plausible_score,
        "expected higher-confidence execution path score than plausible alternatives: execution={} plausible={}",
        top_execution_score,
        top_plausible_score
    );
}

#[test]
fn test_trace_scenario_surfaces_guard_failure_and_relevant_tests_docs() {
    let graph = build_trace_scenario_regression_graph();
    let rules = RulesDetector::new().detect_rules(&[
        "src/routes/auth.ts".to_string(),
        "src/auth.ts".to_string(),
        "src/session.ts".to_string(),
        "tests/auth_refresh.test.ts".to_string(),
        "tests/auth_login.test.ts".to_string(),
        "docs/auth.md".to_string(),
    ]);

    let bundle = trace_scenario(
        &graph,
        "Debug login refresh timeout where token verification fails and unauthorized responses are returned.",
        &["src/routes/auth.ts".to_string(), "src/auth.ts".to_string()],
        &["loginRoute".to_string(), "refreshSession".to_string()],
        &rules,
        BundleMode::Full,
    );

    assert!(
        bundle
            .guards
            .iter()
            .any(|item| item.symbol == "verifyRefreshToken"),
        "expected guard signal for token verification: {:?}",
        bundle
            .guards
            .iter()
            .map(|item| format!("{}:{}", item.symbol, item.summary))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .failure_branches
            .iter()
            .any(|item| item.symbol == "rejectRefresh"),
        "expected failure-branch signal for reject path: {:?}",
        bundle
            .failure_branches
            .iter()
            .map(|item| format!("{}:{}", item.symbol, item.summary))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .relevant_docs
            .iter()
            .any(|item| item.file == "docs/auth.md"),
        "expected docs/auth.md recommendation in scenario trace: {:?}",
        bundle
            .relevant_docs
            .iter()
            .map(|item| format!("{}:{}", item.file, item.line))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .tests
            .iter()
            .take(3)
            .any(|item| item.file == "tests/auth_refresh.test.ts"),
        "expected refresh test to be recommended near the top: {:?}",
        bundle
            .tests
            .iter()
            .map(|item| format!(
                "{}:{:.2}:{}",
                item.file, item.confidence, item.confidence_band
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        bundle
            .tests
            .iter()
            .take(4)
            .any(|item| item.file == "tests/auth_login.test.ts"),
        "expected fallback/login test to remain recommended: {:?}",
        bundle
            .tests
            .iter()
            .map(|item| format!(
                "{}:{:.2}:{}",
                item.file, item.confidence, item.confidence_band
            ))
            .collect::<Vec<_>>()
    );

    let refresh_confidence = bundle
        .tests
        .iter()
        .find(|item| item.file == "tests/auth_refresh.test.ts")
        .map(|item| item.confidence)
        .unwrap_or(0.0);
    let login_confidence = bundle
        .tests
        .iter()
        .find(|item| item.file == "tests/auth_login.test.ts")
        .map(|item| item.confidence)
        .unwrap_or(0.0);
    assert!(
        refresh_confidence > login_confidence,
        "expected refresh failure test to outrank fallback login test for this scenario: refresh={} login={}",
        refresh_confidence,
        login_confidence
    );
}
