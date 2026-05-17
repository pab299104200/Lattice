use std::collections::BTreeSet;

use crate::events::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, EventEnvelope, EventKind,
    EventPayload, PayloadHash, PayloadLocation, SessionId, StableRef, WorkflowSucceededPayload,
};
use crate::graph::{CodeGraph, EdgeKind};
use crate::identity::{EventId, FileId, Identity, SymbolId};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use crate::retrieval_v1::{
    classify_intent, retrieve_candidates, AnchorKind, AnchorResolution, CandidateSource,
    ResolvedAnchor, RetrievalBudget, RetrievalContext, SourceSpan,
};
use crate::storage::vector_store::VectorStore;
use crate::symbols::{Language, SymbolId as LegacySymbolId, SymbolKind};
use crate::{DateTime, Utc};

const WORKSPACE: &str = "workspace-main";

#[tokio::test]
async fn each_spec_named_source_produces_candidates() {
    let graph = sample_graph();
    let memory_store = sample_memory_store();
    let vector_store = sample_vector_store();
    let events = vec![
        event(
            "event-1",
            EventKind::AssistantTaskStarted,
            "debug auth workflow",
            Vec::new(),
        ),
        event(
            "event-2",
            EventKind::WorkflowSucceeded,
            "prepare_change auth workflow",
            Vec::new(),
        ),
    ];
    let working = vec![Identity::File(file_id("src/recent.rs"))];
    let anchors = vec![resolved_symbol_anchor("src/auth.rs", "login")];
    let intent = classify_intent("debug auth workflow login");
    let budget = RetrievalBudget::default();
    let ctx = RetrievalContext {
        workspace_id: WORKSPACE,
        graph: Some(&graph),
        memory_store: Some(&memory_store),
        vector_index: Some(&vector_store),
        query_embedding: Some(&[1.0, 0.0, 0.0]),
        event_candidates: &events,
        recent_working_memory: &working,
    };

    let candidates = retrieve_candidates(&intent, &anchors, &budget, &ctx).await;
    let sources = candidate_sources(&candidates);

    assert!(sources.contains(&CandidateSource::ExactPathSymbolLookup));
    assert!(sources.contains(&CandidateSource::CodeGraphTraversal));
    assert!(sources.contains(&CandidateSource::DocBacklinksOutgoingLinks));
    assert!(sources.contains(&CandidateSource::Fts));
    assert!(sources.contains(&CandidateSource::Embeddings));
    assert!(sources.contains(&CandidateSource::EventSimilarity));
    assert!(sources.contains(&CandidateSource::MemoryLinks));
    assert!(sources.contains(&CandidateSource::WorkflowSimilarity));
    assert!(sources.contains(&CandidateSource::RecentActiveWorkingMemory));
}

#[tokio::test]
async fn budgets_are_enforced_for_large_graphs() {
    let graph = large_graph();
    let anchors = vec![resolved_symbol_anchor("src/root.rs", "root")];
    let intent = classify_intent("refactor root");
    let budget = RetrievalBudget {
        max_candidates_per_source: 3,
        max_graph_hops: 2,
        ..RetrievalBudget::default()
    };
    let ctx = empty_context().with_graph(&graph);

    let candidates = retrieve_candidates(&intent, &anchors, &budget, &ctx).await;
    let graph_count = candidates
        .iter()
        .filter(|candidate| candidate.source == CandidateSource::CodeGraphTraversal)
        .count();

    assert_eq!(graph_count, 3);
}

#[tokio::test]
async fn intent_gating_skips_disqualified_event_sources() {
    let events = vec![event(
        "event-1",
        EventKind::WorkflowSucceeded,
        "update docs workflow",
        Vec::new(),
    )];
    let intent = classify_intent("update docs for auth");
    let budget = RetrievalBudget::default();
    let ctx = empty_context().with_events(&events);

    let candidates = retrieve_candidates(&intent, &[], &budget, &ctx).await;
    let sources = candidate_sources(&candidates);

    assert!(!sources.contains(&CandidateSource::EventSimilarity));
    assert!(!sources.contains(&CandidateSource::WorkflowSimilarity));
}

#[tokio::test]
async fn bounded_expansion_records_traversal_paths() {
    let graph = sample_graph();
    let anchors = vec![resolved_symbol_anchor("src/auth.rs", "login")];
    let intent = classify_intent("explain login flow");
    let budget = RetrievalBudget::default();
    let ctx = empty_context().with_graph(&graph);

    let candidates = retrieve_candidates(&intent, &anchors, &budget, &ctx).await;
    let graph_candidate = candidates
        .iter()
        .find(|candidate| candidate.source == CandidateSource::CodeGraphTraversal)
        .expect("graph candidate");

    assert!(!graph_candidate.traversal_path.is_empty());
    assert!(graph_candidate.expansion_handle_hint.is_some());
}

#[tokio::test]
async fn fts_embeddings_and_event_similarity_participate_when_present() {
    let memory_store = sample_memory_store();
    let vector_store = sample_vector_store();
    let events = vec![event(
        "event-1",
        EventKind::AssistantTaskStarted,
        "debug auth failure",
        Vec::new(),
    )];
    let intent = classify_intent("debug auth failure");
    let budget = RetrievalBudget::default();
    let ctx = empty_context()
        .with_memory_store(&memory_store)
        .with_vector_index(&vector_store)
        .with_embedding(&[1.0, 0.0, 0.0])
        .with_events(&events);

    let candidates = retrieve_candidates(&intent, &[], &budget, &ctx).await;
    let sources = candidate_sources(&candidates);

    assert!(sources.contains(&CandidateSource::Fts));
    assert!(sources.contains(&CandidateSource::Embeddings));
    assert!(sources.contains(&CandidateSource::EventSimilarity));
}

#[tokio::test]
async fn working_memory_window_returns_most_recent_items() {
    let working = vec![
        Identity::File(file_id("src/old.rs")),
        Identity::File(file_id("src/middle.rs")),
        Identity::File(file_id("src/new.rs")),
    ];
    let budget = RetrievalBudget {
        working_memory_window: 2,
        ..RetrievalBudget::default()
    };
    let intent = classify_intent("explain recent context");
    let ctx = empty_context().with_working_memory(&working);

    let candidates = retrieve_candidates(&intent, &[], &budget, &ctx).await;
    let working_candidates = candidates
        .iter()
        .filter(|candidate| candidate.source == CandidateSource::RecentActiveWorkingMemory)
        .collect::<Vec<_>>();

    assert_eq!(working_candidates.len(), 2);
    assert_eq!(working_candidates[0].identity, working[2]);
    assert_eq!(working_candidates[1].identity, working[1]);
}

#[tokio::test]
async fn empty_anchor_list_still_yields_events_and_working_memory() {
    let events = vec![event(
        "event-1",
        EventKind::AssistantTaskStarted,
        "debug auth failure",
        Vec::new(),
    )];
    let working = vec![Identity::File(file_id("src/recent.rs"))];
    let intent = classify_intent("debug auth failure");
    let budget = RetrievalBudget::default();
    let ctx = empty_context()
        .with_events(&events)
        .with_working_memory(&working);

    let candidates = retrieve_candidates(&intent, &[], &budget, &ctx).await;
    let sources = candidate_sources(&candidates);

    assert!(sources.contains(&CandidateSource::EventSimilarity));
    assert!(sources.contains(&CandidateSource::RecentActiveWorkingMemory));
}

fn candidate_sources(candidates: &[crate::retrieval_v1::Candidate]) -> BTreeSet<CandidateSource> {
    candidates
        .iter()
        .map(|candidate| candidate.source)
        .collect()
}

fn sample_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    add_node(&mut graph, "src/auth.rs", "login", SymbolKind::Function);
    add_node(
        &mut graph,
        "src/session.rs",
        "create_session",
        SymbolKind::Function,
    );
    add_node(
        &mut graph,
        "docs/auth.md",
        "Authentication",
        SymbolKind::Section,
    );
    graph.add_edge(
        &legacy_symbol("src/auth.rs", "login"),
        &legacy_symbol("src/session.rs", "create_session"),
        EdgeKind::Calls,
    );
    graph.add_edge(
        &legacy_symbol("docs/auth.md", "Authentication"),
        &legacy_symbol("src/auth.rs", "login"),
        EdgeKind::Mentions,
    );
    graph
}

fn large_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    add_node(&mut graph, "src/root.rs", "root", SymbolKind::Function);
    for index in 0..20 {
        let name = format!("child_{index}");
        add_node(&mut graph, "src/root.rs", &name, SymbolKind::Function);
        graph.add_edge(
            &legacy_symbol("src/root.rs", "root"),
            &legacy_symbol("src/root.rs", &name),
            EdgeKind::Calls,
        );
    }
    graph
}

fn add_node(graph: &mut CodeGraph, file: &str, name: &str, kind: SymbolKind) {
    graph.add_node(
        legacy_symbol(file, name),
        kind,
        name.to_string(),
        format!("{name}()"),
        format!("fn {name}() {{}}"),
        file.to_string(),
        1,
        1,
        true,
        if file.ends_with(".md") {
            Language::Markdown
        } else {
            Language::Rust
        },
    );
}

fn sample_memory_store() -> MemoryStore {
    let store = MemoryStore::open_in_memory().expect("memory store");
    store
        .store(memory(
            "memory-fts",
            "debug auth failure workflow login",
            &[],
            &[],
        ))
        .expect("store fts memory");
    store
        .store(memory(
            "memory-link",
            "login helper depends on session setup",
            &["login"],
            &["src/auth.rs"],
        ))
        .expect("store linked memory");
    store
}

fn sample_vector_store() -> VectorStore {
    let store = VectorStore::open_in_memory().expect("vector store");
    store.initialize(3).expect("vector schema");
    store
        .upsert_vector("src/vector.rs", "semantic_login", 7, &[1.0, 0.0, 0.0])
        .expect("vector upsert");
    store
}

fn memory(id: &str, content: &str, symbols: &[&str], files: &[&str]) -> Memory {
    Memory {
        id: id.to_string(),
        session_id: "session-main".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 1.0,
        linked_symbols: symbols.iter().map(|value| value.to_string()).collect(),
        linked_files: files.iter().map(|value| value.to_string()).collect(),
        workspace_id: Some(WORKSPACE.to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

fn event(ulid: &str, kind: EventKind, summary: &str, references: Vec<StableRef>) -> EventEnvelope {
    let payload = match kind {
        EventKind::WorkflowSucceeded => EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
            workflow_name: "prepare_change".to_string(),
            terminal_event_id: None,
            output_context_handle_id: None,
            memory_ids: Vec::new(),
            result_summary: summary.to_string(),
        }),
        _ => EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
            context_handle_id: None,
            seed_event_ids: Vec::new(),
            initial_memory_ids: Vec::new(),
            objective: summary.to_string(),
        }),
    };
    EventEnvelope {
        event_id: EventId {
            workspace_id: WORKSPACE.to_string(),
            ulid: ulid.to_string(),
        },
        workspace_id: WORKSPACE.to_string(),
        branch: BranchRef {
            name: "main".to_string(),
        },
        session_id: SessionId {
            value: "session-main".to_string(),
        },
        task_id: None,
        actor: Actor::Assistant {
            model: "test".to_string(),
        },
        timestamp: DateTime::<Utc>::from_unix_seconds(1),
        kind,
        references,
        payload_hash: PayloadHash::new([1; 32]),
        summary: CompactSummary::new(summary).expect("summary"),
        payload_location: PayloadLocation::Inline { bytes_len: 2 },
        payload,
    }
}

fn resolved_symbol_anchor(file: &str, name: &str) -> ResolvedAnchor {
    ResolvedAnchor {
        kind: AnchorKind::Symbol,
        anchor_text: name.to_string(),
        source_span: SourceSpan { start: 0, end: 1 },
        resolution: AnchorResolution::Resolved(Identity::Symbol(SymbolId {
            file: file_id(file),
            qualified_name: name.to_string(),
            byte_offset: 0,
            kind: "function".to_string(),
        })),
    }
}

fn file_id(path: &str) -> FileId {
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

fn empty_context<'a>() -> TestContext<'a> {
    TestContext {
        inner: RetrievalContext {
            workspace_id: WORKSPACE,
            graph: None,
            memory_store: None,
            vector_index: None,
            query_embedding: None,
            event_candidates: &[],
            recent_working_memory: &[],
        },
    }
}

struct TestContext<'a> {
    inner: RetrievalContext<'a>,
}

impl<'a> TestContext<'a> {
    fn with_graph(mut self, graph: &'a CodeGraph) -> Self {
        self.inner.graph = Some(graph);
        self
    }

    fn with_memory_store(mut self, store: &'a MemoryStore) -> Self {
        self.inner.memory_store = Some(store);
        self
    }

    fn with_vector_index(
        mut self,
        index: &'a dyn crate::storage::vector_index::VectorIndex,
    ) -> Self {
        self.inner.vector_index = Some(index);
        self
    }

    fn with_embedding(mut self, embedding: &'a [f32]) -> Self {
        self.inner.query_embedding = Some(embedding);
        self
    }

    fn with_events(mut self, events: &'a [EventEnvelope]) -> Self {
        self.inner.event_candidates = events;
        self
    }

    fn with_working_memory(mut self, working_memory: &'a [Identity]) -> Self {
        self.inner.recent_working_memory = working_memory;
        self
    }
}

impl<'a> std::ops::Deref for TestContext<'a> {
    type Target = RetrievalContext<'a>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
