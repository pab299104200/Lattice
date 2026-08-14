use super::diagnose_failure;
use super::plan_edit;
use super::prepare_change;
use super::trace_scenario;
use super::{StableIdentity, VecEventSink, WorkflowBundle, WorkflowRequest};
use lattice_core::events::EventKind;
use lattice_core::graph::model::{CodeGraph, EdgeKind};
use lattice_core::intelligence::{self, BundleMode, RulesDetector};
use lattice_core::query::{CapsuleStats, ContextCapsule, ContextNode, PivotNode, QueryIntent};
use lattice_core::symbols::{Language, SymbolId, SymbolKind};
use serde_json::json;
use std::time::Instant;

const WORKSPACE: &str = "workspace-main";

#[test]
fn serde_round_trips_each_edit_workflow_response() {
    let fixture = Fixture::new();
    for bundle in fixture.all_bundles() {
        let encoded = serde_json::to_string(&bundle).expect("bundle serializes");
        let decoded: WorkflowBundle = serde_json::from_str(&encoded).expect("bundle deserializes");
        assert_eq!(decoded.overview, bundle.overview);
        assert!(!decoded.verification_commands.is_empty());
    }
}

#[test]
fn prepare_change_returns_complete_workflow_bundle() {
    let fixture = Fixture::new();
    let bundle = fixture.prepare_bundle();
    assert!(!bundle.ranked_pivots.is_empty());
    assert!(bundle.memory_empty_rationale.is_some() || !bundle.memory_highlights.is_empty());
    assert!(!bundle.risks.is_empty());
    assert!(!bundle.verification_commands.is_empty());
}

#[test]
fn stale_memories_are_explicitly_labeled() {
    let fixture = Fixture::new();
    let bundle = fixture.prepare_bundle();
    let stale = bundle
        .memory_highlights
        .iter()
        .find(|memory| memory.verification_status == "stale")
        .expect("fixture includes stale memory");
    assert!(stale.stale_label.as_deref().unwrap_or("").contains("stale"));
}

#[test]
fn unverified_memories_are_advisory_risks() {
    let fixture = Fixture::new();
    let bundle = fixture.prepare_bundle();
    let advisory = bundle
        .memory_highlights
        .iter()
        .find(|memory| memory.verification_status == "unverified")
        .expect("fixture includes unverified memory");
    assert_eq!(advisory.trust_status, "advisory");
    assert_eq!(advisory.trust_reason, "unverified");
    assert!(advisory
        .risk_domains
        .iter()
        .any(|domain| domain == "security"));
    assert!(advisory.requires_reverification);
    assert_eq!(advisory.reverification_reason, "high_risk_unverified");
    assert!(advisory
        .recheck_commands
        .iter()
        .any(|command| command.contains("auth")));
    assert!(advisory.evidence_links.iter().any(|link| {
        link["kind"].as_str() == Some("file")
            && link["reference"].as_str() == Some("src/auth.ts")
            && link["recheck_command"]
                .as_str()
                .is_some_and(|command| command.contains("src/auth.ts"))
    }));
    assert!(bundle.risks.iter().any(|risk| {
        risk.message
            .contains("Memory is advisory, not proof: unverified")
    }));
    assert!(bundle
        .verification_commands
        .iter()
        .any(|command| command.contains("auth")));
}

#[test]
fn artifact_conflicts_are_workflow_risks_even_when_memory_is_verified() {
    let fixture = Fixture::new();
    let bundle = fixture.prepare_bundle();
    let conflict = bundle
        .memory_highlights
        .iter()
        .find(|memory| memory.memory_id.ulid == "artifact-conflict-pattern")
        .expect("fixture includes artifact-conflict memory");
    assert_eq!(conflict.trust_status, "trusted");
    assert!(!conflict.artifact_conflicts.is_empty());
    assert!(bundle.risks.iter().any(|risk| {
        risk.identity.as_ref().is_some_and(|identity| {
            matches!(
                identity,
                StableIdentity::Memory(memory_id)
                    if memory_id.ulid == "artifact-conflict-pattern"
            )
        }) && risk
            .message
            .contains("Linked artifacts contain conflicting status claims")
    }));
}

#[test]
fn workflow_events_are_emitted_in_order_under_budget() {
    let fixture = Fixture::new();
    let mut sink = VecEventSink::default();
    let start = Instant::now();
    let _bundle = prepare_change::run(
        WORKSPACE,
        &fixture.request,
        &fixture.task,
        &fixture.capsule,
        &mut sink,
    );
    assert!(start.elapsed().as_millis() < 5);
    assert_eq!(
        sink.events,
        vec![
            EventKind::AssistantTaskStarted,
            EventKind::ToolCalled,
            EventKind::ContextBundleReturned,
            EventKind::MemoryRetrieved,
            EventKind::PlanCreated,
        ]
    );
}

#[test]
fn plan_edit_steps_reference_stable_identities() {
    let fixture = Fixture::new();
    let bundle = fixture.plan_bundle();
    let steps = bundle.structured_payload["ordered_edit_steps"]
        .as_array()
        .expect("ordered steps array");
    assert!(!steps.is_empty());
    assert!(steps.iter().all(|step| step.get("file_identity").is_some()));
}

#[test]
fn diagnose_failure_recovers_injected_failure_pattern() {
    let fixture = Fixture::new();
    let bundle = fixture.diagnose_bundle();
    assert!(bundle
        .memory_highlights
        .iter()
        .any(|memory| memory.memory_type == "pattern"));
    assert!(bundle
        .ranked_pivots
        .iter()
        .any(|pivot| pivot.symbol.as_deref() == Some("loginUser")));
}

struct Fixture {
    capsule: ContextCapsule,
    request: WorkflowRequest,
    task: intelligence::TaskBundle,
    plan: intelligence::PlanEditBundle,
    trace: intelligence::ScenarioTraceBundle,
    diagnosis: intelligence::FailureDiagnosis,
}

impl Fixture {
    fn new() -> Self {
        let graph = build_graph();
        let capsule = build_capsule();
        let files = graph
            .all_nodes()
            .iter()
            .map(|node| node.file.clone())
            .collect::<Vec<_>>();
        let rules = RulesDetector::new().detect_rules(&files);
        let request = WorkflowRequest {
            input: capsule.query.clone(),
            entry_files: vec!["src/auth.ts".to_string()],
            entry_symbols: vec!["loginUser".to_string()],
            render_mode: "compact".to_string(),
        };
        let task = intelligence::prepare_change(
            &graph,
            &capsule,
            &request.entry_files,
            &request.entry_symbols,
            &rules,
            BundleMode::Compact,
            None,
        );
        let plan = intelligence::plan_edit(
            &graph,
            &capsule,
            &request.entry_files,
            &request.entry_symbols,
            &rules,
            BundleMode::Compact,
            None,
        );
        let trace = intelligence::trace_scenario(
            &graph,
            "login route fails after token refresh",
            &request.entry_files,
            &request.entry_symbols,
            &rules,
            BundleMode::Compact,
        );
        let diagnosis = intelligence::diagnose_failure(
            &graph,
            "thread panicked at src/auth.ts:12: loginUser failed",
            Some("test"),
            &rules,
            BundleMode::Compact,
            None,
        );
        Self {
            capsule,
            request,
            task,
            plan,
            trace,
            diagnosis,
        }
    }

    fn all_bundles(&self) -> Vec<WorkflowBundle> {
        vec![
            self.prepare_bundle(),
            self.plan_bundle(),
            self.trace_bundle(),
            self.diagnose_bundle(),
        ]
    }

    fn prepare_bundle(&self) -> WorkflowBundle {
        let mut sink = VecEventSink::default();
        prepare_change::run(
            WORKSPACE,
            &self.request,
            &self.task,
            &self.capsule,
            &mut sink,
        )
    }

    fn plan_bundle(&self) -> WorkflowBundle {
        let mut sink = VecEventSink::default();
        plan_edit::run(
            WORKSPACE,
            &self.request,
            &self.plan,
            &self.capsule,
            &mut sink,
        )
    }

    fn trace_bundle(&self) -> WorkflowBundle {
        let mut sink = VecEventSink::default();
        trace_scenario::run(
            WORKSPACE,
            &self.request,
            &self.trace,
            &self.capsule.memories,
            &mut sink,
        )
    }

    fn diagnose_bundle(&self) -> WorkflowBundle {
        let mut sink = VecEventSink::default();
        diagnose_failure::run(
            WORKSPACE,
            &self.request,
            &self.diagnosis,
            &self.capsule.memories,
            &mut sink,
        )
    }
}

fn build_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    let login = id("src/auth.ts", "loginUser", 0);
    let session = id("src/session.ts", "createSession", 10);
    let route = id("src/routes/auth.ts", "loginRoute", 20);
    let test = id("tests/auth_test.rs", "login_user_rejects_timeout", 30);
    add_node(&mut graph, login.clone(), "loginUser", "src/auth.ts", 10);
    add_node(
        &mut graph,
        session.clone(),
        "createSession",
        "src/session.ts",
        30,
    );
    add_node(
        &mut graph,
        route.clone(),
        "loginRoute",
        "src/routes/auth.ts",
        6,
    );
    add_node(
        &mut graph,
        test.clone(),
        "login_user_rejects_timeout",
        "tests/auth_test.rs",
        4,
    );
    graph.add_edge(&route, &login, EdgeKind::Calls);
    graph.add_edge(&login, &session, EdgeKind::Calls);
    graph.add_edge(&test, &route, EdgeKind::Calls);
    graph
}

fn add_node(graph: &mut CodeGraph, id: SymbolId, name: &str, file: &str, line: usize) {
    graph.add_node(
        id,
        SymbolKind::Function,
        name.to_string(),
        format!("fn {name}()"),
        format!("fn {name}() {{}}"),
        file.to_string(),
        line,
        line + 8,
        true,
        Language::Rust,
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
        query: "Fix login timeout".to_string(),
        intent: QueryIntent::FixBug,
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
            symbol: "createSession".to_string(),
            kind: "fn".to_string(),
            file: "src/session.ts".to_string(),
            line: 30,
            skeleton: "fn createSession()".to_string(),
            relationship: "calls".to_string(),
            score: 0.7,
        }],
        memories: vec![
            json!({
                "id": "verified-pattern",
                "content": "Auth failures usually need route and session assertions.",
                "memory_type": "pattern",
                "scope": "repo",
                "confidence": 0.91,
                "verification_status": "verified",
                "evidence": [{"kind": "test", "reference": "tests/auth.test.ts"}],
                "inclusion_reason": "linked to loginUser",
            }),
            json!({
                "id": "artifact-conflict-pattern",
                "content": "Auth route split was resolved.",
                "memory_type": "workflow_outcome",
                "scope": "repo",
                "confidence": 0.92,
                "verification_status": "verified",
                "evidence": [{"kind": "test", "reference": "tests/auth.test.ts"}],
                "inclusion_reason": "linked to loginUser",
                "artifact_conflicts": [{
                    "key": "PX-0027-S01",
                    "positive_status": "resolved",
                    "negative_status": "blocked",
                    "positive_refs": ["docs/PX-0027-S01.md"],
                    "negative_refs": ["docs/verify-PX-0027-S01.md"],
                    "reason": "linked artifacts contain conflicting resolved/blocked style status terms"
                }]
            }),
            json!({
                "id": "unverified-pattern",
                "content": "Suspected auth issue may involve import-time settings pollution.",
                "memory_type": "observation",
                "scope": "repo",
                "confidence": 0.7,
                "verification_status": "unverified",
                "linked_files": ["src/auth.ts"],
                "inclusion_reason": "matched auth failure",
            }),
            json!({
                "id": "stale-pattern",
                "content": "Old auth flow used cookie-only sessions.",
                "memory_type": "pattern",
                "scope": "repo",
                "confidence": 0.2,
                "verification_status": "stale",
                "is_stale": true,
                "stale_reason": "superseded by token flow",
            }),
        ],
        stats: CapsuleStats {
            tokens_used: 50,
            tokens_saved: 100,
            nodes_evaluated: 4,
            nodes_included: 2,
            engine_version: "test".to_string(),
            seed_count: 1,
            seed_symbols: vec!["loginUser".to_string()],
        },
    }
}
