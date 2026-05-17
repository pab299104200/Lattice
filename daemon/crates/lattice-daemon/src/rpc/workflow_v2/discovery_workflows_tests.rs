use super::context_capsule;
use super::docs_capsule;
use super::impact_from_diff;
use super::relevant_tests;
use super::{WorkflowBundle, WorkflowRequest};
use lattice_core::graph::model::{CodeGraph, EdgeKind};
use lattice_core::intelligence::{self, BundleMode, RulesDetector};
use lattice_core::query::{CapsuleStats, ContextCapsule, ContextNode, PivotNode, QueryIntent};
use lattice_core::symbols::{Language, SymbolId, SymbolKind};

const WORKSPACE: &str = "workspace-main";

#[test]
fn serde_round_trips_each_discovery_workflow_response() {
    let fixture = Fixture::new();
    for bundle in fixture.all_bundles() {
        let encoded = serde_json::to_string(&bundle).expect("bundle serializes");
        let decoded: WorkflowBundle = serde_json::from_str(&encoded).expect("bundle deserializes");
        assert_eq!(decoded.overview, bundle.overview);
    }
}

#[test]
fn default_render_mode_is_compact_and_bundle_is_bounded() {
    let fixture = Fixture::new();
    let bundle = fixture.context_bundle();
    assert_eq!(bundle.render_choice.mode, "compact");
    assert!(bundle.ranked_pivots.len() <= 8);
}

#[test]
fn context_capsule_returns_expansion_handle_that_expand_context_can_use() {
    let fixture = Fixture::new();
    let bundle = fixture.context_bundle();
    let seed = super::build_expand_seed(&bundle);
    let report = intelligence::expand_context(&fixture.graph, &seed, "symbol:loginUser", 800);
    assert!(!report.symbols.is_empty() || !report.files.is_empty());
}

#[test]
fn relevant_tests_recovers_the_golden_test_target() {
    let fixture = Fixture::new();
    let bundle = fixture.tests_bundle();
    assert!(bundle
        .ranked_pivots
        .iter()
        .any(|pivot| pivot.file.as_deref() == Some("tests/auth_test.rs")));
    assert!(bundle
        .verification_commands
        .iter()
        .any(|command| command.contains("auth_test.rs")));
}

#[test]
fn impact_from_diff_stays_bounded_and_surfaces_affected_docs() {
    let fixture = Fixture::new();
    let bundle = fixture.impact_bundle();
    let affected_docs = bundle
        .relevant_context
        .iter()
        .filter(|item| item.kind == "affected_doc")
        .count();
    assert!(affected_docs >= 1);
    let report = &fixture.impact;
    assert!(report.affected_symbols.len() <= 2);
}

#[test]
fn stale_memories_are_labeled_when_present() {
    let fixture = Fixture::new();
    let bundle = fixture.context_bundle();
    let stale = bundle
        .memory_highlights
        .iter()
        .find(|memory| memory.verification_status == "stale")
        .expect("fixture includes stale memory");
    assert!(stale.stale_label.as_deref().unwrap_or("").contains("stale"));
}

struct Fixture {
    graph: CodeGraph,
    request: WorkflowRequest,
    capsule: ContextCapsule,
    docs: intelligence::DocsCapsule,
    tests: intelligence::TestSelectionReport,
    impact: intelligence::DiffImpactReport,
}

impl Fixture {
    fn new() -> Self {
        let graph = build_graph();
        let request = WorkflowRequest {
            input: "Investigate login workflow".to_string(),
            entry_files: vec!["src/auth.ts".to_string()],
            entry_symbols: vec!["loginUser".to_string()],
            render_mode: "compact".to_string(),
        };
        let capsule = build_capsule();
        let rules = RulesDetector::new().detect_rules(&[
            "src/auth.ts".to_string(),
            "src/session.ts".to_string(),
            "src/routes/auth.ts".to_string(),
            "tests/auth_test.rs".to_string(),
            "docs/auth.md".to_string(),
        ]);
        let docs = intelligence::get_docs_capsule(
            &graph,
            "login flow",
            &request.entry_files,
            &request.entry_symbols,
            6,
        );
        let tests = intelligence::find_relevant_tests(
            &graph,
            &request.entry_files,
            &request.entry_symbols,
            None,
            &rules,
            8,
        );
        let impact = intelligence::impact_from_diff(
            &graph,
            "diff --git a/src/auth.ts b/src/auth.ts\n--- a/src/auth.ts\n+++ b/src/auth.ts\n@@ -10,1 +10,1 @@\n-old\n+new\n",
            &[],
            &[],
            &rules,
            BundleMode::Compact,
            1,
        );
        Self {
            graph,
            request,
            capsule,
            docs,
            tests,
            impact,
        }
    }

    fn all_bundles(&self) -> Vec<WorkflowBundle> {
        vec![
            self.context_bundle(),
            self.docs_bundle(),
            self.tests_bundle(),
            self.impact_bundle(),
        ]
    }

    fn context_bundle(&self) -> WorkflowBundle {
        context_capsule::build_bundle(
            &self.graph,
            WORKSPACE,
            &self.request,
            &self.capsule,
            super::WorkflowRenderChoice::Compact,
        )
    }

    fn docs_bundle(&self) -> WorkflowBundle {
        docs_capsule::build_bundle(
            WORKSPACE,
            &self.request,
            &self.docs,
            super::WorkflowRenderChoice::Compact,
        )
    }

    fn tests_bundle(&self) -> WorkflowBundle {
        relevant_tests::build_bundle(
            WORKSPACE,
            &self.request,
            &self.tests,
            super::WorkflowRenderChoice::Compact,
        )
    }

    fn impact_bundle(&self) -> WorkflowBundle {
        impact_from_diff::build_bundle(
            &self.graph,
            WORKSPACE,
            &self.request,
            &self.impact,
            super::WorkflowRenderChoice::Compact,
        )
    }
}

fn build_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    let login = id("src/auth.ts", "loginUser", 0);
    let session = id("src/session.ts", "createSession", 10);
    let route = id("src/routes/auth.ts", "loginRoute", 20);
    let test = id("tests/auth_test.rs", "login_user_rejects_timeout", 30);
    let doc = id("docs/auth.md", "Login Flow", 40);
    add_node(
        &mut graph,
        login.clone(),
        SymbolKind::Function,
        "loginUser",
        "src/auth.ts",
        10,
        Language::Rust,
    );
    add_node(
        &mut graph,
        session.clone(),
        SymbolKind::Function,
        "createSession",
        "src/session.ts",
        30,
        Language::Rust,
    );
    add_node(
        &mut graph,
        route.clone(),
        SymbolKind::Function,
        "loginRoute",
        "src/routes/auth.ts",
        6,
        Language::Rust,
    );
    add_node(
        &mut graph,
        test.clone(),
        SymbolKind::Function,
        "login_user_rejects_timeout",
        "tests/auth_test.rs",
        4,
        Language::Rust,
    );
    add_node(
        &mut graph,
        doc.clone(),
        SymbolKind::Module,
        "Login Flow",
        "docs/auth.md",
        1,
        Language::Markdown,
    );
    graph.add_edge(&route, &login, EdgeKind::Calls);
    graph.add_edge(&login, &session, EdgeKind::Calls);
    graph.add_edge(&test, &route, EdgeKind::Calls);
    graph.add_edge(&doc, &login, EdgeKind::Mentions);
    graph
}

fn add_node(
    graph: &mut CodeGraph,
    id: SymbolId,
    kind: SymbolKind,
    name: &str,
    file: &str,
    line: usize,
    language: Language,
) {
    graph.add_node(
        id,
        kind,
        name.to_string(),
        format!("fn {name}()"),
        format!("fn {name}() {{}}"),
        file.to_string(),
        line,
        line + 8,
        true,
        language,
    );
}

fn id(file: &str, name: &str, byte_offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset,
    }
}

fn build_capsule() -> ContextCapsule {
    ContextCapsule {
        query: "Investigate login workflow".to_string(),
        intent: QueryIntent::Explore,
        pivots: vec![PivotNode {
            symbol: "loginUser".to_string(),
            kind: "fn".to_string(),
            file: "src/auth.ts".to_string(),
            line: 10,
            source: "fn loginUser() {}".to_string(),
            score: 0.95,
            reason: "direct query match".to_string(),
        }],
        context: vec![ContextNode {
            symbol: "loginRoute".to_string(),
            kind: "fn".to_string(),
            file: "src/routes/auth.ts".to_string(),
            line: 6,
            skeleton: "fn loginRoute()".to_string(),
            relationship: "caller".to_string(),
            score: 0.73,
        }],
        memories: vec![serde_json::json!({
            "id": "mem-stale",
            "content": "Previous login timeout regressions centered on session creation.",
            "memory_type": "observation",
            "scope": "repo",
            "verification_status": "stale",
            "is_stale": true
        })],
        stats: CapsuleStats {
            tokens_used: 120,
            tokens_saved: 80,
            nodes_evaluated: 8,
            nodes_included: 2,
            engine_version: "test".to_string(),
            seed_count: 1,
            seed_symbols: vec!["loginUser".to_string()],
        },
    }
}
