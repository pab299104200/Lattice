//! Hybrid candidate retrieval for Retrieval V1.
//!
//! The source list is intentionally limited to the nine candidates named in
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## 7. Retrieval Engine`.

use std::collections::BTreeSet;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::events::{EventEnvelope, EventKind, StableRef};
use crate::graph::{CodeGraph, GraphTraversalPath};
use crate::identity::{DocId, FileId, Identity, MemoryId, SectionId, SymbolId};
use crate::memory::MemoryStore;
use crate::storage::vector_index::{VectorIndex, VectorScope};
use crate::symbols::{SymbolId as LegacySymbolId, SymbolKind};
use crate::verification::ScopeFilter;

use super::{AnchorResolution, IntentClassification, IntentLabel, ResolvedAnchor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CandidateSource {
    ExactPathSymbolLookup,
    CodeGraphTraversal,
    DocBacklinksOutgoingLinks,
    Fts,
    Embeddings,
    EventSimilarity,
    MemoryLinks,
    WorkflowSimilarity,
    RecentActiveWorkingMemory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub identity: Identity,
    pub source: CandidateSource,
    pub seed_anchor: Option<Identity>,
    pub raw_score: f64,
    pub preliminary_inclusion_reason: String,
    pub expansion_handle_hint: Option<String>,
    pub traversal_path: Vec<Identity>,
    pub budget_exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalBudget {
    pub max_candidates_per_source: usize,
    pub max_graph_hops: usize,
    pub max_fts_rows: usize,
    pub embedding_k: usize,
    pub event_window: usize,
    pub working_memory_window: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalBudgetConfig {
    pub max_candidates_per_source: Option<usize>,
    pub max_graph_hops: Option<usize>,
    pub max_fts_rows: Option<usize>,
    pub embedding_k: Option<usize>,
    pub event_window: Option<usize>,
    pub working_memory_window: Option<usize>,
}

pub struct RetrievalContext<'a> {
    pub workspace_id: &'a str,
    pub graph: Option<&'a CodeGraph>,
    pub memory_store: Option<&'a MemoryStore>,
    pub vector_index: Option<&'a dyn VectorIndex>,
    pub query_embedding: Option<&'a [f32]>,
    pub event_candidates: &'a [EventEnvelope],
    pub recent_working_memory: &'a [Identity],
}

impl Default for RetrievalBudgetConfig {
    fn default() -> Self {
        Self {
            max_candidates_per_source: Some(24),
            max_graph_hops: Some(2),
            max_fts_rows: Some(24),
            embedding_k: Some(12),
            event_window: Some(32),
            working_memory_window: Some(12),
        }
    }
}

impl RetrievalBudget {
    pub fn from_config(config: &RetrievalBudgetConfig) -> Self {
        let defaults = RetrievalBudgetConfig::default();
        Self {
            max_candidates_per_source: config
                .max_candidates_per_source
                .or(defaults.max_candidates_per_source)
                .unwrap(),
            max_graph_hops: config.max_graph_hops.or(defaults.max_graph_hops).unwrap(),
            max_fts_rows: config.max_fts_rows.or(defaults.max_fts_rows).unwrap(),
            embedding_k: config.embedding_k.or(defaults.embedding_k).unwrap(),
            event_window: config.event_window.or(defaults.event_window).unwrap(),
            working_memory_window: config
                .working_memory_window
                .or(defaults.working_memory_window)
                .unwrap(),
        }
    }
}

impl Default for RetrievalBudget {
    fn default() -> Self {
        Self::from_config(&RetrievalBudgetConfig::default())
    }
}

pub async fn retrieve_candidates(
    intent: &IntentClassification,
    anchors: &[ResolvedAnchor],
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    let started = Instant::now();
    let exact = retrieve_exact_lookup(anchors, budget);
    let graph = retrieve_graph_traversal(intent, anchors, budget, ctx);
    let docs = retrieve_doc_links(anchors, budget, ctx);
    let fts = retrieve_fts(intent, budget, ctx);
    let embeddings = retrieve_embeddings(budget, ctx);
    let events = retrieve_event_similarity(intent, anchors, budget, ctx);
    let memory = retrieve_memory_links(anchors, budget, ctx);
    let workflow = retrieve_workflow_similarity(intent, budget, ctx);
    let working = retrieve_working_memory(budget, ctx);

    let (exact, graph, docs, fts, embeddings, events, memory, workflow, working) =
        tokio::join!(exact, graph, docs, fts, embeddings, events, memory, workflow, working);

    let grouped = [
        exact, graph, docs, fts, embeddings, events, memory, workflow, working,
    ];
    let total = grouped.iter().map(Vec::len).sum::<usize>();
    debug!(
        candidate_sources = grouped.len(),
        candidate_total = total,
        elapsed_ms = started.elapsed().as_millis(),
        "retrieval_v1 candidate retrieval completed"
    );
    grouped.into_iter().flatten().collect()
}

async fn retrieve_exact_lookup(
    anchors: &[ResolvedAnchor],
    budget: &RetrievalBudget,
) -> Vec<Candidate> {
    let candidates = anchors.iter().filter_map(candidate_for_resolved_anchor);
    enforce_budget(
        candidates.collect(),
        budget.max_candidates_per_source,
        CandidateSource::ExactPathSymbolLookup,
    )
}

async fn retrieve_graph_traversal(
    intent: &IntentClassification,
    anchors: &[ResolvedAnchor],
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    if !source_enabled(intent, CandidateSource::CodeGraphTraversal) {
        return Vec::new();
    }
    let Some(graph) = ctx.graph else {
        return Vec::new();
    };
    let hops = intent_graph_hops(intent).min(budget.max_graph_hops);
    let mut candidates = Vec::new();
    for (seed, symbol) in resolved_symbols(anchors) {
        for path in graph.n_hop_neighbor_paths(&symbol, hops) {
            candidates.push(candidate_for_graph_path(ctx, &seed, path));
            if candidates.len() >= budget.max_candidates_per_source {
                break;
            }
        }
        if candidates.len() >= budget.max_candidates_per_source {
            break;
        }
    }
    enforce_budget(
        candidates,
        budget.max_candidates_per_source,
        CandidateSource::CodeGraphTraversal,
    )
}

async fn retrieve_doc_links(
    anchors: &[ResolvedAnchor],
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    let Some(graph) = ctx.graph else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for (seed, symbol) in resolved_symbols(anchors) {
        for (node, edge) in graph.get_dependencies(&symbol) {
            if matches!(node.kind, SymbolKind::Document | SymbolKind::Section) {
                candidates.push(doc_candidate(ctx, &seed, node, edge.short_code()));
            }
        }
        for (node, edge) in graph.get_dependents(&symbol) {
            if matches!(node.kind, SymbolKind::Document | SymbolKind::Section) {
                candidates.push(doc_candidate(ctx, &seed, node, edge.short_code()));
            }
        }
        if candidates.len() >= budget.max_candidates_per_source {
            break;
        }
    }
    enforce_budget(
        candidates,
        budget.max_candidates_per_source,
        CandidateSource::DocBacklinksOutgoingLinks,
    )
}

async fn retrieve_fts(
    intent: &IntentClassification,
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    if !source_enabled(intent, CandidateSource::Fts) {
        return Vec::new();
    }
    let Some(store) = ctx.memory_store else {
        return Vec::new();
    };
    let scope = workspace_scope(ctx);
    let Ok(memories) = store.query(Some(&intent.inspected_text), budget.max_fts_rows, &scope)
    else {
        return Vec::new();
    };
    let candidates = memories
        .into_iter()
        .map(|memory| memory_candidate(ctx, memory.id, CandidateSource::Fts));
    enforce_budget(
        candidates.collect(),
        budget.max_candidates_per_source,
        CandidateSource::Fts,
    )
}

async fn retrieve_embeddings(
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    let (Some(index), Some(query)) = (ctx.vector_index, ctx.query_embedding) else {
        return Vec::new();
    };
    let Ok(results) = index.search_in_scope(query, budget.embedding_k, VectorScope::All) else {
        return Vec::new();
    };
    let candidates = results
        .into_iter()
        .map(|(name, file, byte_offset, score)| Candidate {
            identity: symbol_identity(ctx, &legacy_symbol(&file, &name, byte_offset)),
            source: CandidateSource::Embeddings,
            seed_anchor: None,
            raw_score: f64::from(score),
            preliminary_inclusion_reason: "embedding nearest-neighbour result".to_string(),
            expansion_handle_hint: Some(format!("embedding:{file}:{byte_offset}")),
            traversal_path: Vec::new(),
            budget_exhausted: false,
        });
    enforce_budget(
        candidates.collect(),
        budget.max_candidates_per_source,
        CandidateSource::Embeddings,
    )
}

async fn retrieve_event_similarity(
    intent: &IntentClassification,
    anchors: &[ResolvedAnchor],
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    if !source_enabled(intent, CandidateSource::EventSimilarity) {
        return Vec::new();
    }
    let anchors = anchor_identity_set(anchors);
    let mut candidates = Vec::new();
    for event in ctx.event_candidates.iter().rev().take(budget.event_window) {
        if event_matches_intent(event, intent) || event_matches_anchors(event, &anchors) {
            candidates.push(event_candidate(event, CandidateSource::EventSimilarity));
        }
    }
    enforce_budget(
        candidates,
        budget.max_candidates_per_source,
        CandidateSource::EventSimilarity,
    )
}

async fn retrieve_memory_links(
    anchors: &[ResolvedAnchor],
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    let Some(store) = ctx.memory_store else {
        return Vec::new();
    };
    let scope = workspace_scope(ctx);
    let Ok(memories) = store.query(None, budget.max_candidates_per_source, &scope) else {
        return Vec::new();
    };
    let anchor_terms = anchor_link_terms(anchors);
    let candidates = memories
        .into_iter()
        .filter(|memory| memory_has_anchor_link(memory, &anchor_terms))
        .map(|memory| memory_candidate(ctx, memory.id, CandidateSource::MemoryLinks));
    enforce_budget(
        candidates.collect(),
        budget.max_candidates_per_source,
        CandidateSource::MemoryLinks,
    )
}

async fn retrieve_workflow_similarity(
    intent: &IntentClassification,
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    if !source_enabled(intent, CandidateSource::WorkflowSimilarity) {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for event in ctx.event_candidates.iter().rev().take(budget.event_window) {
        if matches!(
            event.kind,
            EventKind::WorkflowSucceeded | EventKind::WorkflowFailed
        ) && event_matches_intent(event, intent)
        {
            candidates.push(event_candidate(event, CandidateSource::WorkflowSimilarity));
        }
    }
    enforce_budget(
        candidates,
        budget.max_candidates_per_source,
        CandidateSource::WorkflowSimilarity,
    )
}

async fn retrieve_working_memory(
    budget: &RetrievalBudget,
    ctx: &RetrievalContext<'_>,
) -> Vec<Candidate> {
    let candidates = ctx
        .recent_working_memory
        .iter()
        .rev()
        .take(budget.working_memory_window)
        .cloned()
        .map(|identity| Candidate {
            identity,
            source: CandidateSource::RecentActiveWorkingMemory,
            seed_anchor: None,
            raw_score: 0.55,
            preliminary_inclusion_reason: "recent active working-memory item".to_string(),
            expansion_handle_hint: Some("working-memory:recent".to_string()),
            traversal_path: Vec::new(),
            budget_exhausted: false,
        });
    enforce_budget(
        candidates.collect(),
        budget.max_candidates_per_source,
        CandidateSource::RecentActiveWorkingMemory,
    )
}

fn candidate_for_resolved_anchor(anchor: &ResolvedAnchor) -> Option<Candidate> {
    let AnchorResolution::Resolved(identity) = &anchor.resolution else {
        return None;
    };
    Some(Candidate {
        identity: identity.clone(),
        source: CandidateSource::ExactPathSymbolLookup,
        seed_anchor: Some(identity.clone()),
        raw_score: 1.0,
        preliminary_inclusion_reason: format!("exact anchor `{}` resolved", anchor.anchor_text),
        expansion_handle_hint: Some(format!("anchor:{}", anchor.anchor_text)),
        traversal_path: Vec::new(),
        budget_exhausted: false,
    })
}

fn candidate_for_graph_path(
    ctx: &RetrievalContext<'_>,
    seed: &Identity,
    path: GraphTraversalPath,
) -> Candidate {
    let traversal_path = path
        .steps
        .iter()
        .map(|step| symbol_identity(ctx, &step.node.id))
        .collect::<Vec<_>>();
    Candidate {
        identity: symbol_identity(ctx, &path.target.id),
        source: CandidateSource::CodeGraphTraversal,
        seed_anchor: Some(seed.clone()),
        raw_score: 0.75,
        preliminary_inclusion_reason: "bounded code graph traversal from resolved anchor"
            .to_string(),
        expansion_handle_hint: Some("graph-neighborhood".to_string()),
        traversal_path,
        budget_exhausted: false,
    }
}

fn doc_candidate(
    ctx: &RetrievalContext<'_>,
    seed: &Identity,
    node: &crate::graph::GraphNode,
    edge_code: &str,
) -> Candidate {
    Candidate {
        identity: doc_identity(ctx, node),
        source: CandidateSource::DocBacklinksOutgoingLinks,
        seed_anchor: Some(seed.clone()),
        raw_score: 0.7,
        preliminary_inclusion_reason: format!("doc graph edge `{edge_code}` connected to anchor"),
        expansion_handle_hint: Some(format!("doc-link:{}", node.file)),
        traversal_path: vec![doc_identity(ctx, node)],
        budget_exhausted: false,
    }
}

fn memory_candidate(ctx: &RetrievalContext<'_>, id: String, source: CandidateSource) -> Candidate {
    Candidate {
        identity: Identity::Memory(MemoryId {
            workspace_id: ctx.workspace_id.to_string(),
            ulid: id.clone(),
        }),
        source,
        seed_anchor: None,
        raw_score: 0.65,
        preliminary_inclusion_reason: "memory graph or FTS retrieval result".to_string(),
        expansion_handle_hint: Some(format!("memory:{id}")),
        traversal_path: Vec::new(),
        budget_exhausted: false,
    }
}

/// Retrieval V1 has no branch, session, or organization authority in its context.
/// Restrict memory sources to the active workspace's repository scope rather than
/// using an administrative query and reconstructing a workspace identity from it.
fn workspace_scope(ctx: &RetrievalContext<'_>) -> ScopeFilter {
    ScopeFilter::new(ctx.workspace_id, None, None)
}

fn event_candidate(event: &EventEnvelope, source: CandidateSource) -> Candidate {
    Candidate {
        identity: Identity::Event(event.event_id.clone()),
        source,
        seed_anchor: None,
        raw_score: 0.6,
        preliminary_inclusion_reason: format!("related event `{}`", event.kind.as_str()),
        expansion_handle_hint: Some(format!("event:{}", event.event_id.ulid)),
        traversal_path: event
            .references
            .iter()
            .filter_map(identity_from_ref)
            .collect(),
        budget_exhausted: false,
    }
}

fn enforce_budget(
    mut candidates: Vec<Candidate>,
    cap: usize,
    source: CandidateSource,
) -> Vec<Candidate> {
    debug_assert!(
        candidates.len() <= cap,
        "{source:?} retriever exceeded candidate cap {cap}"
    );
    let exhausted = candidates.len() > cap;
    candidates.truncate(cap);
    if exhausted {
        for candidate in &mut candidates {
            candidate.budget_exhausted = true;
        }
    }
    debug!(source = ?source, count = candidates.len(), exhausted, "retriever candidates");
    candidates
}

fn resolved_symbols(anchors: &[ResolvedAnchor]) -> Vec<(Identity, LegacySymbolId)> {
    anchors
        .iter()
        .filter_map(|anchor| match &anchor.resolution {
            AnchorResolution::Resolved(Identity::Symbol(symbol)) => {
                Some((Identity::Symbol(symbol.clone()), legacy_symbol_id(symbol)))
            }
            _ => None,
        })
        .collect()
}

fn source_enabled(intent: &IntentClassification, source: CandidateSource) -> bool {
    if intent.primary_label == IntentLabel::UpdateDocs {
        return !matches!(
            source,
            CandidateSource::EventSimilarity | CandidateSource::WorkflowSimilarity
        );
    }
    true
}

fn intent_graph_hops(intent: &IntentClassification) -> usize {
    match intent.primary_label {
        IntentLabel::Debug | IntentLabel::Refactor | IntentLabel::Review => 3,
        IntentLabel::UpdateDocs | IntentLabel::Explain => 2,
        _ => 1,
    }
}

fn anchor_identity_set(anchors: &[ResolvedAnchor]) -> BTreeSet<String> {
    anchors
        .iter()
        .filter_map(|anchor| match &anchor.resolution {
            AnchorResolution::Resolved(identity) => Some(identity.to_string()),
            _ => None,
        })
        .collect()
}

fn anchor_link_terms(anchors: &[ResolvedAnchor]) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    for anchor in anchors {
        if let AnchorResolution::Resolved(identity) = &anchor.resolution {
            add_identity_terms(identity, &mut terms);
        }
        terms.insert(anchor.anchor_text.clone());
    }
    terms
}

fn add_identity_terms(identity: &Identity, terms: &mut BTreeSet<String>) {
    match identity {
        Identity::File(file) => {
            terms.insert(file.repo_relative_path.clone());
        }
        Identity::Symbol(symbol) => {
            terms.insert(symbol.qualified_name.clone());
            terms.insert(symbol.file.repo_relative_path.clone());
        }
        Identity::Doc(doc) => {
            terms.insert(doc.repo_relative_path.clone());
        }
        Identity::Section(section) => {
            terms.insert(section.doc.repo_relative_path.clone());
            terms.extend(section.heading_path.iter().cloned());
        }
        _ => {}
    }
}

fn memory_has_anchor_link(memory: &crate::memory::Memory, terms: &BTreeSet<String>) -> bool {
    memory
        .linked_files
        .iter()
        .chain(memory.linked_symbols.iter())
        .any(|value| terms.contains(value))
}

fn event_matches_intent(event: &EventEnvelope, intent: &IntentClassification) -> bool {
    let haystack = event.summary.as_str().to_lowercase();
    intent
        .inspected_text
        .split_whitespace()
        .take(8)
        .any(|token| token.len() > 3 && haystack.contains(&token.to_lowercase()))
}

fn event_matches_anchors(event: &EventEnvelope, anchors: &BTreeSet<String>) -> bool {
    event
        .references
        .iter()
        .filter_map(identity_from_ref)
        .any(|identity| anchors.contains(&identity.to_string()))
}

fn identity_from_ref(reference: &StableRef) -> Option<Identity> {
    match reference {
        StableRef::FileRef(id) => Some(Identity::File(id.clone())),
        StableRef::SymbolRef(id) => Some(Identity::Symbol(id.clone())),
        StableRef::EventRef(id) => Some(Identity::Event(id.clone())),
        StableRef::MemoryRef(id) => Some(Identity::Memory(id.clone())),
        StableRef::ContextHandleRef(id) => Some(Identity::ContextHandle(id.clone())),
        StableRef::DocSectionRef(id) => Some(Identity::Section(id.clone())),
    }
}

fn symbol_identity(ctx: &RetrievalContext<'_>, id: &LegacySymbolId) -> Identity {
    Identity::Symbol(SymbolId {
        file: FileId {
            workspace_id: ctx.workspace_id.to_string(),
            repo_relative_path: id.file.clone(),
            content_hash: "00000000".to_string(),
        },
        qualified_name: id.name.clone(),
        byte_offset: id.byte_offset,
        kind: "symbol".to_string(),
    })
}

fn doc_identity(ctx: &RetrievalContext<'_>, node: &crate::graph::GraphNode) -> Identity {
    if node.kind == SymbolKind::Document {
        return Identity::Doc(DocId {
            workspace_id: ctx.workspace_id.to_string(),
            repo_relative_path: node.file.clone(),
            content_hash: "00000000".to_string(),
        });
    }
    Identity::Section(SectionId {
        doc: DocId {
            workspace_id: ctx.workspace_id.to_string(),
            repo_relative_path: node.file.clone(),
            content_hash: "00000000".to_string(),
        },
        heading_path: vec![node.name.clone()],
        byte_offset: node.id.byte_offset,
    })
}

fn legacy_symbol_id(symbol: &SymbolId) -> LegacySymbolId {
    legacy_symbol(
        &symbol.file.repo_relative_path,
        &symbol.qualified_name,
        symbol.byte_offset,
    )
}

fn legacy_symbol(file: &str, name: &str, byte_offset: usize) -> LegacySymbolId {
    LegacySymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset,
    }
}
