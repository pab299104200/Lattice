use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::events::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, EventEnvelope, EventKind,
    EventPayload, PayloadHash, PayloadLocation, SessionId, StableRef, WorkflowSucceededPayload,
};
use crate::graph::{CodeGraph, EdgeKind};
use crate::identity::{
    DocId, EventId, FileId, Identity, IdentityResolver, MemoryId, ResolveOutcome, SymbolId,
};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus};
use crate::parser::parse_file;
use crate::retrieval_v1::{
    classify_intent, extract_anchors, resolve_anchors, retrieve_candidates, score_candidates,
    shape_retrieval_bundle, AnchorKind, DiagnosticMode, EventScoringMetadata, RankedCandidate,
    RankingDiagnostics, ResolvedAnchor, RetrievalBudget, RetrievalContext, ScoringContext,
    ScoringWeights, ShaperBudget, ShaperContext,
};
use crate::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use crate::storage::vector_store::VectorStore;
use crate::symbols::{ParsedFile, SymbolId as LegacySymbolId};
use crate::{DateTime, Utc};

pub(super) const WORKSPACE: &str = "workspace-main";
const BRANCH: &str = "main";
const SESSION_ID: &str = "session-main";
pub(super) const MEMORY_AUTH_VERIFIED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAA";
pub(super) const MEMORY_AUTH_STALE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAB";
pub(super) const MEMORY_SESSION_VERIFIED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAC";
pub(super) const MEMORY_CLI_VERIFIED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAD";
pub(super) const MEMORY_DOCS_VERIFIED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAE";
pub(super) const MEMORY_CONFIG_VERIFIED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAF";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ExpectedIdentity {
    File(&'static str),
    Symbol {
        file: &'static str,
        qualified_name: &'static str,
    },
    Section {
        path: &'static str,
        heading: &'static str,
    },
    Memory(&'static str),
}

impl ExpectedIdentity {
    pub(super) fn matches(&self, identity: &Identity) -> bool {
        match (self, identity) {
            (Self::File(path), Identity::File(file)) => file.repo_relative_path == *path,
            (
                Self::Symbol {
                    file,
                    qualified_name,
                },
                Identity::Symbol(symbol),
            ) => {
                symbol.file.repo_relative_path == *file && symbol.qualified_name == *qualified_name
            }
            (Self::Section { path, heading }, Identity::Section(section)) => {
                section.doc.repo_relative_path == *path
                    && section.heading_path.last().map(String::as_str) == Some(*heading)
            }
            (Self::Memory(ulid), Identity::Memory(memory)) => memory.ulid == *ulid,
            _ => false,
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            Self::File(path) => (*path).to_string(),
            Self::Symbol {
                file,
                qualified_name,
            } => format!("{file}::{qualified_name}"),
            Self::Section { path, heading } => format!("{path}#{heading}"),
            Self::Memory(ulid) => format!("memory {ulid}"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct GoldenCase {
    pub name: &'static str,
    pub task: &'static str,
    pub anchor_kind: AnchorKind,
    pub expected_anchor_text: &'static str,
    pub expected_resolved_identity: ExpectedIdentity,
    pub expected_top_identity: ExpectedIdentity,
    pub dominant_signal: &'static str,
    pub shaper_token_budget: usize,
    pub query_embedding: [f32; 3],
}

pub(super) struct GoldenFixture {
    pub resolver: IdentityResolver<'static>,
    pub graph: &'static CodeGraph,
    pub memory_store: MemoryStore,
    pub vector_store: VectorStore,
    pub events: Vec<EventEnvelope>,
    pub working_memory: Vec<Identity>,
    pub scoring_context: ScoringContext,
}

pub(super) struct GoldenRun {
    pub anchors: Vec<ResolvedAnchor>,
    pub ranked: Vec<RankedCandidate>,
    pub diagnostics: RankingDiagnostics,
    pub bundle: crate::retrieval_v1::schema::RetrievalBundle,
    pub latency_ms: f64,
}

pub(super) fn golden_case(name: &str) -> GoldenCase {
    golden_cases()
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("missing golden case `{name}`"))
}

pub(super) fn golden_cases() -> Vec<GoldenCase> {
    vec![
        case(
            "path_auth_file_anchor_returns_auth_file",
            "Review `src/auth.rs` before changing the login flow",
            AnchorKind::Path,
            "src/auth.rs",
            ExpectedIdentity::File("src/auth.rs"),
            ExpectedIdentity::File("src/auth.rs"),
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "path_session_file_anchor_returns_session_file",
            "Trace `src/session.rs` because the session cache broke",
            AnchorKind::Path,
            "src/session.rs",
            ExpectedIdentity::File("src/session.rs"),
            ExpectedIdentity::File("src/session.rs"),
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "path_section_anchor_returns_retrieval_engine_section",
            "Update `docs/guide.md#RetrievalEngine` for the ranking notes",
            AnchorKind::Path,
            "docs/guide.md#RetrievalEngine",
            ExpectedIdentity::Section {
                path: "docs/guide.md",
                heading: "RetrievalEngine",
            },
            ExpectedIdentity::Section {
                path: "docs/guide.md",
                heading: "RetrievalEngine",
            },
            [0.0, 1.0, 0.0],
            110,
        ),
        case(
            "symbol_login_user_anchor_returns_login_user",
            "Explain login_user before changing authentication",
            AnchorKind::Symbol,
            "login_user",
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "login_user",
            },
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "login_user",
            },
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "symbol_refresh_session_anchor_returns_refresh_session",
            "Refactor refresh_session without regressing the session cache",
            AnchorKind::Symbol,
            "refresh_session",
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "refresh_session",
            },
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "refresh_session",
            },
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "symbol_search_logic_flow_anchor_returns_search_logic_flow",
            "Explain search_logic_flow before wiring the CLI",
            AnchorKind::Symbol,
            "search_logic_flow",
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "search_logic_flow",
            },
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "search_logic_flow",
            },
            [0.0, 1.0, 0.0],
            110,
        ),
        case(
            "error_panic_path_anchor_returns_auth_file",
            "thread 'main' panicked at src/auth.rs:12:5",
            AnchorKind::Error,
            "thread 'main' panicked at src/auth.rs:12:5",
            ExpectedIdentity::File("src/auth.rs"),
            ExpectedIdentity::Memory(MEMORY_AUTH_VERIFIED),
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "error_missing_symbol_anchor_returns_login_user",
            "error[E0425]: cannot find value `login_user` in this scope",
            AnchorKind::Error,
            "error[E0425]: cannot find value `login_user` in this scope",
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "login_user",
            },
            ExpectedIdentity::Memory(MEMORY_AUTH_VERIFIED),
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "error_traceback_anchor_returns_cli_file",
            "Traceback (most recent call last): diagnose_failure failed in src/cli.rs:22:3",
            AnchorKind::Error,
            "Traceback (most recent call last): diagnose_failure failed in src/cli.rs:22:3",
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "diagnose_failure",
            },
            ExpectedIdentity::Memory(MEMORY_CLI_VERIFIED),
            [0.0, 1.0, 0.0],
            110,
        ),
        case(
            "command_rg_anchor_returns_login_user",
            "Run `rg login_user src/auth.rs` before editing the login flow",
            AnchorKind::Command,
            "rg login_user src/auth.rs",
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "login_user",
            },
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "login_user",
            },
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "command_cargo_test_anchor_returns_session_file",
            "Use `cargo test src/session.rs` to reproduce the session failure",
            AnchorKind::Command,
            "cargo test src/session.rs",
            ExpectedIdentity::File("src/session.rs"),
            ExpectedIdentity::File("src/session.rs"),
            [1.0, 0.0, 0.0],
            110,
        ),
        case(
            "command_python_anchor_returns_cli_file",
            "Try `python src/cli.rs prepare_change` to inspect the wrapper",
            AnchorKind::Command,
            "python src/cli.rs prepare_change",
            ExpectedIdentity::File("src/cli.rs"),
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "prepare_change",
            },
            [0.0, 1.0, 0.0],
            110,
        ),
        case(
            "api_prepare_change_anchor_returns_prepare_change",
            "Call the MCP tool prepare_change for the auth workspace",
            AnchorKind::Api,
            "prepare_change",
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "prepare_change",
            },
            ExpectedIdentity::Memory(MEMORY_CLI_VERIFIED),
            [0.0, 1.0, 0.0],
            110,
        ),
        case(
            "api_diagnose_failure_anchor_returns_diagnose_failure",
            "Call the MCP tool diagnose_failure for the latest traceback",
            AnchorKind::Api,
            "diagnose_failure",
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "diagnose_failure",
            },
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "diagnose_failure",
            },
            [0.0, 1.0, 0.0],
            110,
        ),
        case(
            "api_search_logic_flow_anchor_returns_search_logic_flow",
            "Call the MCP tool search_logic_flow before touching the CLI",
            AnchorKind::Api,
            "search_logic_flow",
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "search_logic_flow",
            },
            ExpectedIdentity::Symbol {
                file: "src/cli.rs",
                qualified_name: "search_logic_flow",
            },
            [0.0, 1.0, 0.0],
            110,
        ),
        case(
            "config_index_root_anchor_returns_index_root_symbol",
            "Check LATTICE_INDEX_ROOT before indexing authentication data",
            AnchorKind::ConfigKey,
            "LATTICE_INDEX_ROOT",
            ExpectedIdentity::Symbol {
                file: "src/config.rs",
                qualified_name: "LATTICE_INDEX_ROOT",
            },
            ExpectedIdentity::Symbol {
                file: "src/config.rs",
                qualified_name: "LATTICE_INDEX_ROOT",
            },
            [0.0, 0.0, 1.0],
            110,
        ),
        case(
            "config_event_log_anchor_returns_event_log_symbol",
            "Verify LATTICE_EVENT_LOG when replaying workflow failures",
            AnchorKind::ConfigKey,
            "LATTICE_EVENT_LOG",
            ExpectedIdentity::Symbol {
                file: "src/config.rs",
                qualified_name: "LATTICE_EVENT_LOG",
            },
            ExpectedIdentity::Symbol {
                file: "src/config.rs",
                qualified_name: "LATTICE_EVENT_LOG",
            },
            [0.0, 0.0, 1.0],
            110,
        ),
        case(
            "config_workflow_cache_ttl_anchor_returns_workflow_cache_ttl",
            "Tune WORKFLOW_CACHE_TTL before replaying cached workflows",
            AnchorKind::ConfigKey,
            "WORKFLOW_CACHE_TTL",
            ExpectedIdentity::Symbol {
                file: "src/config.rs",
                qualified_name: "WORKFLOW_CACHE_TTL",
            },
            ExpectedIdentity::Symbol {
                file: "src/config.rs",
                qualified_name: "WORKFLOW_CACHE_TTL",
            },
            [0.0, 0.0, 1.0],
            110,
        ),
        case(
            "budget_login_user_anchor_keeps_pinned_symbol_under_truncation",
            "Explain login_user with all supporting context for an oversized bundle",
            AnchorKind::Symbol,
            "login_user",
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "login_user",
            },
            ExpectedIdentity::Symbol {
                file: "src/auth.rs",
                qualified_name: "login_user",
            },
            [1.0, 0.0, 0.0],
            65,
        ),
    ]
}

pub(super) fn build_fixture() -> GoldenFixture {
    let files = [
        fixture_file(
            "src/auth.rs",
            "pub fn login_user() {\n    create_session();\n    refresh_session();\n}\n\n\
             pub fn refresh_session() {\n    create_session();\n}\n",
        ),
        fixture_file(
            "src/session.rs",
            "pub fn create_session() {}\n\npub fn session_cache() {}\n",
        ),
        fixture_file(
            "src/cli.rs",
            "pub fn prepare_change() {\n    search_logic_flow();\n}\n\n\
             pub fn diagnose_failure() {\n    search_logic_flow();\n}\n\n\
             pub fn search_logic_flow() {}\n",
        ),
        fixture_file(
            "src/config.rs",
            "pub const LATTICE_INDEX_ROOT: &str = \"/tmp/index\";\n\
             pub const LATTICE_EVENT_LOG: &str = \"/tmp/events\";\n\
             pub const WORKFLOW_CACHE_TTL: u64 = 300;\n",
        ),
        fixture_file(
            "docs/guide.md",
            "# Guide\n\n## RetrievalEngine\nUse prepare_change and search_logic_flow to debug login_user.\n\n\
             ## Metrics Reporting\nRank exact identifier matches before stale memory.\n",
        ),
        fixture_file(
            "docs/ops.md",
            "# Operations\n\n## Recovery Steps\nUse diagnose_failure and check LATTICE_EVENT_LOG.\n",
        ),
    ];

    let mut graph = CodeGraph::new();
    let mut file_index = HashMap::new();
    let mut parsed_files = HashMap::new();
    let mut content_hashes = HashMap::new();

    for file in files {
        let stored_path = file.stored_path();
        let parsed_file = parse_file(&stored_path, file.source).expect("fixture parses");
        add_symbols(&mut graph, &parsed_file);
        content_hashes.insert(file.path.to_string(), content_hash(file.source));
        parsed_files.insert(stored_path.clone(), parsed_file);
        file_index.insert(
            stored_path.clone(),
            file_index_entry(&stored_path, file.source),
        );
    }

    add_edge(
        &mut graph,
        "src/auth.rs",
        "login_user",
        "src/session.rs",
        "create_session",
    );
    add_edge(
        &mut graph,
        "src/auth.rs",
        "refresh_session",
        "src/session.rs",
        "create_session",
    );
    add_edge(
        &mut graph,
        "src/cli.rs",
        "prepare_change",
        "src/cli.rs",
        "search_logic_flow",
    );
    add_edge(
        &mut graph,
        "src/cli.rs",
        "diagnose_failure",
        "src/cli.rs",
        "search_logic_flow",
    );
    add_edge(
        &mut graph,
        "docs/guide.md",
        "RetrievalEngine",
        "src/cli.rs",
        "prepare_change",
    );
    add_edge(
        &mut graph,
        "docs/guide.md",
        "RetrievalEngine",
        "src/cli.rs",
        "search_logic_flow",
    );
    add_edge(
        &mut graph,
        "docs/guide.md",
        "RetrievalEngine",
        "src/auth.rs",
        "login_user",
    );
    add_edge(
        &mut graph,
        "docs/ops.md",
        "Recovery Steps",
        "src/cli.rs",
        "diagnose_failure",
    );
    add_edge(
        &mut graph,
        "docs/ops.md",
        "Recovery Steps",
        "src/config.rs",
        "LATTICE_EVENT_LOG",
    );

    let graph = Box::leak(Box::new(graph));
    let file_index = Box::leak(Box::new(file_index));
    let parsed_files = Box::leak(Box::new(parsed_files));
    let resolver = IdentityResolver::new(
        graph,
        file_index,
        parsed_files,
        WORKSPACE.to_string(),
        Vec::new(),
    );

    let memory_store = memory_store();
    let vector_store = vector_store();
    let working_memory = vec![
        zero_file("src/auth.rs"),
        zero_file("src/session.rs"),
        zero_file("src/cli.rs"),
        zero_file("docs/guide.md"),
    ];
    let events = vec![
        workflow_event(
            "01ARZ3NDEKTSV4RRFFQ69G5FB0",
            "prepare_change auth workflow",
            vec![StableRef::SymbolRef(zero_symbol(
                "src/auth.rs",
                "login_user",
            ))],
        ),
        workflow_event(
            "01ARZ3NDEKTSV4RRFFQ69G5FB1",
            "diagnose_failure replay workflow",
            vec![StableRef::FileRef(zero_file_id("src/cli.rs"))],
        ),
        started_event(
            "01ARZ3NDEKTSV4RRFFQ69G5FB2",
            "replay LATTICE_EVENT_LOG after auth failure",
            vec![StableRef::SymbolRef(zero_symbol(
                "src/config.rs",
                "LATTICE_EVENT_LOG",
            ))],
        ),
    ];

    let scoring_context = scoring_context(&content_hashes, &resolver);

    GoldenFixture {
        resolver,
        graph,
        memory_store,
        vector_store,
        events,
        working_memory,
        scoring_context,
    }
}

pub(super) async fn execute_case(case: &GoldenCase, fixture: &GoldenFixture) -> GoldenRun {
    let intent = classify_intent(case.task);
    let anchors = resolve_anchors(extract_anchors(case.task, &intent), &fixture.resolver);
    let budget = RetrievalBudget {
        max_candidates_per_source: 8,
        max_graph_hops: 2,
        max_fts_rows: 8,
        embedding_k: 6,
        event_window: 6,
        working_memory_window: 4,
    };
    let ctx = RetrievalContext {
        workspace_id: WORKSPACE,
        graph: Some(fixture.graph),
        memory_store: Some(&fixture.memory_store),
        vector_index: Some(&fixture.vector_store),
        query_embedding: Some(&case.query_embedding),
        event_candidates: &fixture.events,
        recent_working_memory: &fixture.working_memory,
    };

    let started = Instant::now();
    let candidates = retrieve_candidates(&intent, &anchors, &budget, &ctx).await;
    let weights = ScoringWeights::default();
    let ranked = score_candidates(
        candidates,
        &intent,
        &anchors,
        &weights,
        DiagnosticMode::Diagnostic,
        &fixture.scoring_context,
    );
    let diagnostics =
        RankingDiagnostics::new(intent.clone(), anchors.clone(), ranked.clone(), weights);
    let pins = HashSet::from([pin_identity_for_case(&case.expected_top_identity, &ranked)]);
    let shaping_budget = if case.shaper_token_budget < 100 {
        case.shaper_token_budget
    } else {
        1_000
    };
    let bundle = shape_retrieval_bundle(
        case.name,
        format!("{:?}", intent.primary_label),
        anchors
            .iter()
            .map(|anchor| anchor.anchor_text.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        ranked.clone(),
        DiagnosticMode::Diagnostic,
        Some(diagnostics.clone()),
        &ShaperContext {
            pins,
            budget: ShaperBudget {
                max_tokens: shaping_budget,
            },
        },
    );

    GoldenRun {
        anchors,
        ranked,
        diagnostics,
        bundle,
        latency_ms: started.elapsed().as_secs_f64() * 1_000.0,
    }
}

pub(super) fn stale_memory_rank(run: &GoldenRun) -> Option<usize> {
    run.ranked
        .iter()
        .position(|candidate| matches_memory(&candidate.candidate.identity, MEMORY_AUTH_STALE))
        .map(|index| index + 1)
}

pub(super) fn verified_memory_rank(run: &GoldenRun, ulid: &str) -> Option<usize> {
    run.ranked
        .iter()
        .position(|candidate| matches_memory(&candidate.candidate.identity, ulid))
        .map(|index| index + 1)
}

pub(super) fn is_stale_memory(identity: &Identity) -> bool {
    matches_memory(identity, MEMORY_AUTH_STALE)
}

pub(super) fn top_identity_matches(case: &GoldenCase, run: &GoldenRun) -> bool {
    run.ranked
        .first()
        .map(|candidate| {
            case.expected_top_identity
                .matches(&candidate.candidate.identity)
        })
        .unwrap_or(false)
}

fn case(
    name: &'static str,
    task: &'static str,
    anchor_kind: AnchorKind,
    expected_anchor_text: &'static str,
    expected_resolved_identity: ExpectedIdentity,
    expected_top_identity: ExpectedIdentity,
    query_embedding: [f32; 3],
    shaper_token_budget: usize,
) -> GoldenCase {
    GoldenCase {
        name,
        task,
        anchor_kind,
        expected_anchor_text,
        expected_resolved_identity,
        expected_top_identity,
        dominant_signal: "exact id",
        shaper_token_budget,
        query_embedding,
    }
}

fn pin_identity_for_case(expected: &ExpectedIdentity, ranked: &[RankedCandidate]) -> Identity {
    ranked
        .iter()
        .find(|candidate| expected.matches(&candidate.candidate.identity))
        .or_else(|| ranked.first())
        .map(|candidate| candidate.candidate.identity.clone())
        .unwrap_or_else(|| panic!("missing ranked output for {}", expected.label()))
}

fn matches_memory(identity: &Identity, ulid: &str) -> bool {
    matches!(identity, Identity::Memory(memory) if memory.ulid == ulid)
}

fn memory_store() -> MemoryStore {
    let store = MemoryStore::open_in_memory().expect("memory store");
    for memory in [
        memory(
            MEMORY_AUTH_VERIFIED,
            "login_user hands off to create_session before issuing a session token",
            &["login_user", "create_session"],
            &["src/auth.rs", "src/session.rs"],
            false,
        ),
        memory(
            MEMORY_AUTH_STALE,
            "login_user writes the session inline without create_session",
            &["login_user"],
            &["src/auth.rs"],
            true,
        ),
        memory(
            MEMORY_SESSION_VERIFIED,
            "refresh_session eventually relies on create_session and session_cache",
            &["refresh_session", "create_session"],
            &["src/session.rs"],
            false,
        ),
        memory(
            MEMORY_CLI_VERIFIED,
            "prepare_change and diagnose_failure both route through search_logic_flow",
            &["prepare_change", "diagnose_failure", "search_logic_flow"],
            &["src/cli.rs"],
            false,
        ),
        memory(
            MEMORY_DOCS_VERIFIED,
            "docs/guide.md RetrievalEngine explains prepare_change and exact identifier ranking",
            &["prepare_change", "search_logic_flow"],
            &["docs/guide.md"],
            false,
        ),
        memory(
            MEMORY_CONFIG_VERIFIED,
            "LATTICE_INDEX_ROOT and LATTICE_EVENT_LOG are the config entry points for retrieval replay",
            &["LATTICE_INDEX_ROOT", "LATTICE_EVENT_LOG", "WORKFLOW_CACHE_TTL"],
            &["src/config.rs"],
            false,
        ),
    ] {
        store.store(memory).expect("store fixture memory");
    }
    store
}

fn vector_store() -> VectorStore {
    let store = VectorStore::open_in_memory().expect("vector store");
    store.initialize(3).expect("vector schema");
    for (file, symbol, embedding) in [
        ("src/auth.rs", "login_user", [1.0, 0.0, 0.0]),
        ("src/auth.rs", "refresh_session", [1.0, 0.0, 0.0]),
        ("src/cli.rs", "prepare_change", [0.0, 1.0, 0.0]),
        ("src/cli.rs", "diagnose_failure", [0.0, 1.0, 0.0]),
        ("src/cli.rs", "search_logic_flow", [0.0, 1.0, 0.0]),
        ("src/config.rs", "LATTICE_INDEX_ROOT", [0.0, 0.0, 1.0]),
        ("src/config.rs", "LATTICE_EVENT_LOG", [0.0, 0.0, 1.0]),
        ("src/config.rs", "WORKFLOW_CACHE_TTL", [0.0, 0.0, 1.0]),
    ] {
        store
            .upsert_vector(file, symbol, 0, &embedding)
            .expect("vector upsert");
    }
    store
}

fn scoring_context(
    content_hashes: &HashMap<String, String>,
    resolver: &IdentityResolver<'_>,
) -> ScoringContext {
    let mut ctx = ScoringContext {
        now_unix_seconds: Some(2_000),
        ..ScoringContext::default()
    };

    for (ulid, status, scope, confidence, evidence_count, created_at) in [
        (
            MEMORY_AUTH_VERIFIED,
            MemoryVerificationStatus::Verified,
            MemoryScope::Branch,
            0.55,
            1usize,
            1_980u64,
        ),
        (
            MEMORY_AUTH_STALE,
            MemoryVerificationStatus::Stale,
            MemoryScope::Repo,
            0.20,
            1usize,
            1_400u64,
        ),
        (
            MEMORY_SESSION_VERIFIED,
            MemoryVerificationStatus::Verified,
            MemoryScope::Branch,
            0.50,
            1usize,
            1_975u64,
        ),
        (
            MEMORY_CLI_VERIFIED,
            MemoryVerificationStatus::Verified,
            MemoryScope::Branch,
            0.48,
            1usize,
            1_990u64,
        ),
        (
            MEMORY_DOCS_VERIFIED,
            MemoryVerificationStatus::Verified,
            MemoryScope::Branch,
            0.46,
            1usize,
            1_995u64,
        ),
        (
            MEMORY_CONFIG_VERIFIED,
            MemoryVerificationStatus::Verified,
            MemoryScope::Branch,
            0.47,
            1usize,
            1_992u64,
        ),
    ] {
        let is_stale = status == MemoryVerificationStatus::Stale;
        let identity = Identity::Memory(MemoryId {
            workspace_id: WORKSPACE.to_string(),
            ulid: ulid.to_string(),
        });
        ctx.memory_metadata.insert(
            identity.to_string(),
            crate::retrieval_v1::MemoryScoringMetadata {
                verification_status: status,
                scope,
                confidence,
                evidence_count,
                created_at: Some(created_at),
                last_accessed: Some(1_998),
                access_count: 5,
                is_stale,
                superseded_by_memory_id: None,
                contradicted_by_memory_ids: Vec::new(),
            },
        );
        ctx.event_history.insert(
            identity.to_string(),
            EventScoringMetadata {
                past_usefulness_count: if ulid == MEMORY_AUTH_STALE { 0 } else { 1 },
                recent_successful_reuse_count: 0,
                occurred_at: Some(created_at),
            },
        );
        ctx.token_estimates.insert(
            identity.to_string(),
            if ulid == MEMORY_AUTH_STALE {
                1_800
            } else {
                1_200
            },
        );
    }

    for (path, hash) in content_hashes {
        let file_identity = resolved_file_identity(resolver, path, hash);
        ctx.semantic_similarity
            .insert(file_identity.to_string(), 0.82);
        ctx.token_estimates.insert(file_identity.to_string(), 180);
        ctx.event_history.insert(
            file_identity.to_string(),
            EventScoringMetadata {
                past_usefulness_count: 2,
                recent_successful_reuse_count: 1,
                occurred_at: Some(1_997),
            },
        );
    }

    for (file, name, score, tokens) in [
        ("src/auth.rs", "login_user", 0.99, 120u32),
        ("src/auth.rs", "refresh_session", 0.95, 140u32),
        ("src/session.rs", "create_session", 0.86, 120u32),
        ("src/cli.rs", "prepare_change", 0.97, 130u32),
        ("src/cli.rs", "diagnose_failure", 0.96, 135u32),
        ("src/cli.rs", "search_logic_flow", 0.94, 145u32),
        ("src/config.rs", "LATTICE_INDEX_ROOT", 0.98, 80u32),
        ("src/config.rs", "LATTICE_EVENT_LOG", 0.97, 85u32),
        ("src/config.rs", "WORKFLOW_CACHE_TTL", 0.96, 90u32),
    ] {
        let symbol = zero_symbol(file, name);
        ctx.semantic_similarity.insert(symbol.to_string(), score);
        ctx.event_history.insert(
            symbol.to_string(),
            EventScoringMetadata {
                past_usefulness_count: 3,
                recent_successful_reuse_count: 2,
                occurred_at: Some(1_999),
            },
        );
        ctx.token_estimates.insert(symbol.to_string(), tokens);
        let resolved_symbol = resolved_symbol_identity(resolver, name);
        ctx.semantic_similarity
            .insert(resolved_symbol.to_string(), score);
        ctx.event_history.insert(
            resolved_symbol.to_string(),
            EventScoringMetadata {
                past_usefulness_count: 3,
                recent_successful_reuse_count: 2,
                occurred_at: Some(1_999),
            },
        );
        ctx.token_estimates
            .insert(resolved_symbol.to_string(), tokens);
    }

    let section_identity = resolved_section_identity(resolver, "docs/guide.md", "RetrievalEngine");
    ctx.semantic_similarity
        .insert(section_identity.to_string(), 0.93);
    ctx.event_history.insert(
        section_identity.to_string(),
        EventScoringMetadata {
            past_usefulness_count: 2,
            recent_successful_reuse_count: 1,
            occurred_at: Some(1_999),
        },
    );
    ctx.token_estimates
        .insert(section_identity.to_string(), 150);

    ctx
}

fn resolved_file_identity(
    resolver: &IdentityResolver<'_>,
    path: &str,
    fallback_hash: &str,
) -> Identity {
    Identity::File(
        resolver
            .resolve_path(resolver.default_workspace_id(), path)
            .unwrap_or_else(|_| FileId {
                workspace_id: WORKSPACE.to_string(),
                repo_relative_path: path.to_string(),
                content_hash: fallback_hash.to_string(),
            }),
    )
}

fn resolved_symbol_identity(resolver: &IdentityResolver<'_>, symbol: &str) -> Identity {
    match resolver.resolve_legacy_symbol_name(symbol) {
        ResolveOutcome::Unique(identity) => Identity::Symbol(identity),
        other => panic!("missing resolved symbol `{symbol}`: {other:?}"),
    }
}

fn resolved_section_identity(
    resolver: &IdentityResolver<'_>,
    path: &str,
    heading: &str,
) -> Identity {
    let file = resolver
        .resolve_path(resolver.default_workspace_id(), path)
        .expect("resolved section file");
    let doc = DocId {
        workspace_id: file.workspace_id.clone(),
        repo_relative_path: file.repo_relative_path.clone(),
        content_hash: file.content_hash.clone(),
    };
    match resolver.resolve_section(resolver.default_workspace_id(), &doc, heading) {
        ResolveOutcome::Unique(identity) => Identity::Section(identity),
        other => panic!("missing resolved section `{path}#{heading}`: {other:?}"),
    }
}

fn memory(id: &str, content: &str, symbols: &[&str], files: &[&str], is_stale: bool) -> Memory {
    Memory {
        id: id.to_string(),
        session_id: SESSION_ID.to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: if is_stale { 0.35 } else { 0.96 },
        linked_symbols: symbols.iter().map(|value| (*value).to_string()).collect(),
        linked_files: files.iter().map(|value| (*value).to_string()).collect(),
        workspace_id: Some(WORKSPACE.to_string()),
        branch: Some(BRANCH.to_string()),
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: if is_stale { 1_400 } else { 1_980 },
        last_accessed: 1_998,
        access_count: if is_stale { 0 } else { 4 },
        is_stale,
        stale_reason: is_stale.then(|| "superseded by verified workflow".to_string()),
        verification_status: if is_stale {
            MemoryVerificationStatus::Stale
        } else {
            MemoryVerificationStatus::Verified
        },
    }
}

fn workflow_event(ulid: &str, summary: &str, references: Vec<StableRef>) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId {
            workspace_id: WORKSPACE.to_string(),
            ulid: ulid.to_string(),
        },
        workspace_id: WORKSPACE.to_string(),
        branch: BranchRef {
            name: BRANCH.to_string(),
        },
        session_id: SessionId {
            value: SESSION_ID.to_string(),
        },
        task_id: None,
        actor: Actor::Assistant {
            model: "test".to_string(),
        },
        timestamp: DateTime::<Utc>::from_unix_seconds(1_999),
        kind: EventKind::WorkflowSucceeded,
        references,
        payload_hash: PayloadHash::new([1; 32]),
        summary: CompactSummary::new(summary).expect("summary"),
        payload_location: PayloadLocation::Inline { bytes_len: 2 },
        payload: EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
            workflow_name: "prepare_change".to_string(),
            terminal_event_id: None,
            output_context_handle_id: None,
            memory_ids: Vec::new(),
            result_summary: summary.to_string(),
        }),
    }
}

fn started_event(ulid: &str, summary: &str, references: Vec<StableRef>) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId {
            workspace_id: WORKSPACE.to_string(),
            ulid: ulid.to_string(),
        },
        workspace_id: WORKSPACE.to_string(),
        branch: BranchRef {
            name: BRANCH.to_string(),
        },
        session_id: SessionId {
            value: SESSION_ID.to_string(),
        },
        task_id: None,
        actor: Actor::Assistant {
            model: "test".to_string(),
        },
        timestamp: DateTime::<Utc>::from_unix_seconds(1_998),
        kind: EventKind::AssistantTaskStarted,
        references,
        payload_hash: PayloadHash::new([2; 32]),
        summary: CompactSummary::new(summary).expect("summary"),
        payload_location: PayloadLocation::Inline { bytes_len: 2 },
        payload: EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
            context_handle_id: None,
            seed_event_ids: Vec::new(),
            initial_memory_ids: Vec::new(),
            objective: summary.to_string(),
        }),
    }
}

fn add_symbols(graph: &mut CodeGraph, parsed_file: &ParsedFile) {
    for symbol in &parsed_file.symbols {
        graph.add_node(
            symbol.id.clone(),
            symbol.kind,
            symbol.name.clone(),
            symbol.signature.clone(),
            symbol.body.clone(),
            symbol.file.clone(),
            symbol.line,
            symbol.end_line,
            symbol.is_exported,
            symbol.language,
        );
    }
}

fn add_edge(graph: &mut CodeGraph, from_file: &str, from: &str, to_file: &str, to: &str) {
    let from_id = graph_symbol_id(graph, from_file, from);
    let to_id = graph_symbol_id(graph, to_file, to);
    graph.add_edge(
        &from_id,
        &to_id,
        if from_file.ends_with(".md") {
            EdgeKind::Mentions
        } else {
            EdgeKind::Calls
        },
    );
}

fn graph_symbol_id(graph: &CodeGraph, file: &str, name: &str) -> LegacySymbolId {
    graph
        .all_nodes()
        .into_iter()
        .find(|node| node.file == file && node.name == name)
        .map(|node| node.id.clone())
        .unwrap_or_else(|| legacy_symbol(file, name))
}

fn zero_symbol(file: &str, name: &str) -> SymbolId {
    SymbolId {
        file: zero_file_id(file),
        qualified_name: name.to_string(),
        byte_offset: 0,
        kind: "symbol".to_string(),
    }
}

fn zero_file(path: &str) -> Identity {
    Identity::File(zero_file_id(path))
}

fn zero_file_id(path: &str) -> FileId {
    FileId {
        workspace_id: WORKSPACE.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "00000000".to_string(),
    }
}

fn legacy_symbol(file: &str, name: &str) -> LegacySymbolId {
    LegacySymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset: 0,
    }
}

fn fixture_file<'a>(path: &'a str, source: &'a str) -> FixtureFile<'a> {
    FixtureFile { path, source }
}

fn file_index_entry(stored_path: &str, source: &str) -> FileIndexEntry {
    FileIndexEntry {
        file: stored_path.to_string(),
        content_hash: content_hash(source),
        mtime_ns: 0,
        size_bytes: source.len() as i64,
        parser_version: FILE_INDEX_PARSER_VERSION,
        schema_version: FILE_INDEX_SCHEMA_VERSION,
        last_indexed_at: 0,
    }
}

fn content_hash(source: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in source.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

struct FixtureFile<'a> {
    path: &'a str,
    source: &'a str,
}

impl FixtureFile<'_> {
    fn stored_path(&self) -> String {
        format!("{WORKSPACE}/{}", self.path)
    }
}
