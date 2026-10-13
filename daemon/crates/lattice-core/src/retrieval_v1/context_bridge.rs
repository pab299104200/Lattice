//! Q2 bridge: run the principled `retrieval_v1` signal engine for the
//! agent-facing `context` tool and adapt its `RetrievalBundle` into the
//! existing `ContextCapsule` shape the daemon already serializes.
//!
//! This is the convergence seam. The agent tools historically ranked code
//! context with the heuristic `query::engine::QueryEngine`, while this
//! principled 13-signal ranker served only working memory. The bridge lets the
//! `context` tool opt into `retrieval_v1` behind a flag, returning the same
//! capsule schema so nothing downstream changes. The default remains the
//! heuristic engine until a fixture comparison justifies the switch (see
//! `docs/plans/2026-09-30-q2-r2-design.md`, Q2 Phase 2/3).

use std::collections::HashMap;

use crate::graph::CodeGraph;
use crate::identity::{decode_identity, Identity, IdentityResolver};
use crate::memory::MemoryStore;
use crate::query::capsule::{CapsuleStats, ContextCapsule, ContextNode, PivotNode, QueryIntent};
use crate::storage::graph_store::FileIndexEntry;
use crate::storage::vector_index::VectorIndex;
use crate::symbols::{ParsedFile, SymbolId as LegacySymbolId};

use super::{
    classify_intent, extract_anchors, resolve_anchors, retrieve_candidates, schema::BundleResult,
    score_candidates, shape_retrieval_bundle, DiagnosticMode, IntentLabel, RetrievalBudget,
    RetrievalContext, ScoringContext, ScoringWeights, ShaperBudget, ShaperContext,
};

/// Immutable resources the bridge borrows for one query. Mirrors what the
/// daemon's `context` handler already holds.
pub struct ContextBridgeResources<'a> {
    pub workspace_id: &'a str,
    pub graph: &'a CodeGraph,
    pub file_index: &'a HashMap<String, FileIndexEntry>,
    pub memory_store: Option<&'a MemoryStore>,
    pub vector_index: Option<&'a dyn VectorIndex>,
}

/// Run `retrieval_v1` for a free-text query and adapt the result into a
/// `ContextCapsule`. `query_embedding` enables the semantic candidate source
/// when a vector index is present; without it the ranker leans on exact-match
/// and graph proximity, which it does well.
pub fn context_capsule_via_retrieval_v1(
    query: &str,
    resources: &ContextBridgeResources<'_>,
    query_embedding: Option<&[f32]>,
    max_tokens: usize,
) -> ContextCapsule {
    let intent = classify_intent(query);
    let empty_parsed: HashMap<String, ParsedFile> = HashMap::new();
    let resolver = IdentityResolver::new(
        resources.graph,
        resources.file_index,
        &empty_parsed,
        resources.workspace_id.to_string(),
        Vec::new(),
    );
    let anchors = resolve_anchors(extract_anchors(query, &intent), &resolver);

    let budget = RetrievalBudget::default();
    let retrieval_ctx = RetrievalContext {
        workspace_id: resources.workspace_id,
        graph: Some(resources.graph),
        memory_store: resources.memory_store,
        vector_index: resources.vector_index,
        query_embedding,
        event_candidates: &[],
        recent_working_memory: &[],
    };

    // retrieve_candidates is async only to fan out its sources concurrently;
    // the context handler already runs inside a blocking query job, so drive it
    // on a bounded current-thread runtime rather than requiring an async caller.
    let candidates = run_blocking(retrieve_candidates(
        &intent,
        &anchors,
        &budget,
        &retrieval_ctx,
    ));

    let weights = ScoringWeights::default();
    let scoring_ctx = ScoringContext::default();
    let ranked = score_candidates(
        candidates,
        &intent,
        &anchors,
        &weights,
        DiagnosticMode::Compact,
        &scoring_ctx,
    );

    let shaper = ShaperContext {
        pins: Default::default(),
        budget: ShaperBudget { max_tokens },
    };
    let bundle = shape_retrieval_bundle(
        "context",
        format!("{:?}", intent.primary_label),
        summarize_anchors(query, &anchors),
        ranked,
        DiagnosticMode::Compact,
        None,
        &shaper,
    );

    adapt_bundle_to_capsule(resources.graph, query, intent.primary_label, bundle.results)
}

/// Drive a future to completion on a bounded current-thread runtime. The
/// retrieval sources are CPU/graph work wrapped in async for fan-out, not true
/// I/O, so a local runtime is sufficient and keeps the bridge callable from the
/// daemon's existing synchronous query-job closure.
fn run_blocking<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime for retrieval bridge")
        .block_on(future)
}

fn summarize_anchors(query: &str, anchors: &[super::ResolvedAnchor]) -> String {
    if anchors.is_empty() {
        query.chars().take(80).collect()
    } else {
        anchors
            .iter()
            .map(|anchor| anchor.anchor_text.clone())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn adapt_bundle_to_capsule(
    graph: &CodeGraph,
    query: &str,
    label: IntentLabel,
    results: Vec<BundleResult>,
) -> ContextCapsule {
    let evaluated = results.len();
    let mut pivots = Vec::new();
    let mut context = Vec::new();

    for result in &results {
        let (file, symbol, line) = locate(graph, &result.identity, &result.headline);
        // The top few results become pivots (full snippet); the rest become
        // context nodes (skeleton), matching the heuristic engine's split.
        if pivots.len() < 3 {
            pivots.push(PivotNode {
                symbol: symbol.clone(),
                kind: format!("{:?}", result.kind).to_lowercase(),
                file: file.clone(),
                line,
                source: result.snippet.clone(),
                score: f64::from(result.score),
                reason: result.inclusion_reason.clone(),
            });
        } else {
            context.push(ContextNode {
                symbol,
                kind: format!("{:?}", result.kind).to_lowercase(),
                file,
                line,
                skeleton: result.snippet.clone(),
                relationship: result.inclusion_reason.clone(),
                score: f64::from(result.score),
            });
        }
    }

    ContextCapsule {
        query: query.to_string(),
        intent: intent_to_query_intent(label),
        pivots,
        context,
        memories: Vec::new(),
        stats: CapsuleStats {
            tokens_used: 0,
            tokens_saved: 0,
            nodes_evaluated: evaluated,
            nodes_included: results.len(),
            engine_version: format!("retrieval_v1-{}", env!("CARGO_PKG_VERSION")),
            seed_count: evaluated,
            seed_symbols: Vec::new(),
        },
    }
}

/// File / symbol / line from a result identity. For symbols the real start
/// line is looked up from the graph node (keyed by the legacy symbol id), so
/// the capsule carries true `file:line` locations the way the heuristic engine
/// does — a hardcoded line would make an anchor-precision fixture comparison
/// mark every v1 result wrong on line even when the file/symbol is perfect.
/// Doc/Section lines come from the section's byte offset is not a line, so
/// those fall back to 1 (the heuristic engine treats docs the same way).
fn locate(graph: &CodeGraph, identity: &Identity, headline: &str) -> (String, String, usize) {
    match identity {
        Identity::Symbol(symbol) => {
            let legacy = LegacySymbolId {
                file: symbol.file.repo_relative_path.clone(),
                name: symbol.qualified_name.clone(),
                byte_offset: symbol.byte_offset,
            };
            let line = graph.get_node(&legacy).map(|node| node.line).unwrap_or(1);
            (
                symbol.file.repo_relative_path.clone(),
                symbol.qualified_name.clone(),
                line,
            )
        }
        Identity::File(file) => (file.repo_relative_path.clone(), headline.to_string(), 1),
        Identity::Doc(doc) => (doc.repo_relative_path.clone(), headline.to_string(), 1),
        Identity::Section(section) => {
            // A section node, when present in the graph, carries its heading
            // line; look it up by the section's offset-keyed legacy id.
            let legacy = LegacySymbolId {
                file: section.doc.repo_relative_path.clone(),
                name: section.heading_path.last().cloned().unwrap_or_default(),
                byte_offset: section.byte_offset,
            };
            let line = graph.get_node(&legacy).map(|node| node.line).unwrap_or(1);
            (
                section.doc.repo_relative_path.clone(),
                section.heading_path.join(" > "),
                line,
            )
        }
        _ => (String::new(), headline.to_string(), 1),
    }
}

fn intent_to_query_intent(label: IntentLabel) -> QueryIntent {
    match label {
        IntentLabel::Explain => QueryIntent::Explore,
        IntentLabel::Debug => QueryIntent::FixBug,
        IntentLabel::Refactor => QueryIntent::Refactor,
        IntentLabel::AddFeature | IntentLabel::ModifyFeature => QueryIntent::AddFeature,
        _ => QueryIntent::Unknown,
    }
}

/// Decode a stored identity string to its file component, for callers that only
/// have the encoded form. Public so the daemon can reuse one decode path.
pub fn identity_file_path(encoded: &str) -> Option<String> {
    match decode_identity(encoded).ok()? {
        Identity::Symbol(symbol) => Some(symbol.file.repo_relative_path),
        Identity::File(file) => Some(file.repo_relative_path),
        Identity::Doc(doc) => Some(doc.repo_relative_path),
        Identity::Section(section) => Some(section.doc.repo_relative_path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{Language, SymbolId as LegacySymbolId, SymbolKind};

    fn graph_with_login() -> CodeGraph {
        let mut graph = CodeGraph::new();
        let id = LegacySymbolId {
            file: "src/auth.rs".to_string(),
            name: "login".to_string(),
            byte_offset: 0,
        };
        graph.add_node(
            id,
            SymbolKind::Function,
            "login".to_string(),
            "fn login(user: &str) -> bool".to_string(),
            "fn login(user: &str) -> bool { verify(user) }".to_string(),
            "src/auth.rs".to_string(),
            1,
            3,
            true,
            Language::Rust,
        );
        graph
    }

    /// A graph whose `login` node starts at a distinctive, non-1 line, so a
    /// test can tell a real line lookup from the old hardcoded `1`.
    fn graph_with_login_at_line(line: usize) -> CodeGraph {
        let mut graph = CodeGraph::new();
        let id = LegacySymbolId {
            file: "src/auth.rs".to_string(),
            name: "login".to_string(),
            byte_offset: 0,
        };
        graph.add_node(
            id,
            SymbolKind::Function,
            "login".to_string(),
            "fn login(user: &str) -> bool".to_string(),
            "fn login(user: &str) -> bool { verify(user) }".to_string(),
            "src/auth.rs".to_string(),
            line,
            line + 2,
            true,
            Language::Rust,
        );
        graph
    }

    /// T2 input parity: when a vector index and a query embedding are supplied,
    /// the semantic candidate source must actually participate — proof the
    /// bridge is no longer running the ranker blind. Without this, flipping the
    /// Q2 flag would compare a blind v1 against an embedding-fed heuristic path
    /// and v1 would lose for a reason unrelated to ranking quality.
    #[test]
    fn embedding_and_vector_index_feed_the_ranker() {
        use crate::storage::vector_store::VectorStore;

        let graph = graph_with_login_at_line(10);
        let file_index: HashMap<String, FileIndexEntry> = HashMap::new();

        let vector_store = VectorStore::open_in_memory().expect("vector store");
        vector_store.initialize(3).expect("vector schema");
        vector_store
            .upsert_vector("src/semantic.rs", "semantic_hit", 5, &[1.0, 0.0, 0.0])
            .expect("vector upsert");

        let resources = ContextBridgeResources {
            workspace_id: "workspace-main",
            graph: &graph,
            file_index: &file_index,
            memory_store: None,
            vector_index: Some(&vector_store),
        };

        // With an embedding + vector index, the semantic hit must reach the
        // capsule. Compared against the None-embedding run on the same
        // resources, the embedding run must surface the semantic symbol.
        let with_embedding = context_capsule_via_retrieval_v1(
            "how does login work",
            &resources,
            Some(&[1.0, 0.0, 0.0]),
            320,
        );
        let names: Vec<String> = with_embedding
            .pivots
            .iter()
            .map(|p| p.symbol.clone())
            .chain(with_embedding.context.iter().map(|n| n.symbol.clone()))
            .collect();
        assert!(
            names.iter().any(|name| name == "semantic_hit"),
            "the embedding-fed semantic candidate must appear in the capsule, got {names:?}"
        );

        // And without an embedding, that semantic hit does not appear — proof
        // the embedding input, not some other path, is what surfaced it.
        let without_embedding =
            context_capsule_via_retrieval_v1("how does login work", &resources, None, 320);
        let blind_names: Vec<String> = without_embedding
            .pivots
            .iter()
            .map(|p| p.symbol.clone())
            .chain(without_embedding.context.iter().map(|n| n.symbol.clone()))
            .collect();
        assert!(
            !blind_names.iter().any(|name| name == "semantic_hit"),
            "without an embedding the semantic candidate must not appear, got {blind_names:?}"
        );
    }

    /// T2 (independent review): the bridge used to hardcode `line: 1` for every
    /// result, so an anchor-precision fixture comparison scored by `file:line`
    /// would mark every v1 result wrong on line even with a perfect file/symbol
    /// — a false-negative source that would make v1 look worse than it is. The
    /// capsule must now carry the real graph line.
    #[test]
    fn capsule_carries_real_graph_line_numbers_not_a_constant() {
        let graph = graph_with_login_at_line(42);
        let file_index: HashMap<String, FileIndexEntry> = HashMap::new();
        let resources = ContextBridgeResources {
            workspace_id: "workspace-main",
            graph: &graph,
            file_index: &file_index,
            memory_store: None,
            vector_index: None,
        };

        let capsule =
            context_capsule_via_retrieval_v1("how does login work", &resources, None, 320);

        let login_line = capsule
            .pivots
            .iter()
            .find(|pivot| pivot.symbol == "login")
            .map(|pivot| pivot.line)
            .or_else(|| {
                capsule
                    .context
                    .iter()
                    .find(|node| node.symbol == "login")
                    .map(|node| node.line)
            })
            .expect("the login symbol must appear in the capsule");

        assert_eq!(
            login_line, 42,
            "the capsule must carry the real graph line, not a hardcoded 1"
        );
    }

    #[test]
    fn bridge_produces_a_well_formed_capsule_from_retrieval_v1() {
        // Q2 Phase 1 characterization: the retrieval_v1 path yields a valid
        // ContextCapsule with the same schema the heuristic engine returns, so
        // the daemon can serve it behind the flag without any downstream change.
        let graph = graph_with_login();
        let file_index: HashMap<String, FileIndexEntry> = HashMap::new();
        let resources = ContextBridgeResources {
            workspace_id: "workspace-main",
            graph: &graph,
            file_index: &file_index,
            memory_store: None,
            vector_index: None,
        };

        let capsule =
            context_capsule_via_retrieval_v1("how does login work", &resources, None, 320);

        // Schema parity: the capsule is well-formed and reports the v1 engine.
        assert_eq!(capsule.query, "how does login work");
        assert!(capsule.stats.engine_version.starts_with("retrieval_v1-"));
        // Every emitted node has a usable shape (no panic, no empty-structure).
        for pivot in &capsule.pivots {
            assert!(!pivot.kind.is_empty());
            assert!(pivot.score.is_finite());
        }
        for node in &capsule.context {
            assert!(!node.kind.is_empty());
            assert!(node.score.is_finite());
        }
        // Running twice over the same resources yields identical results —
        // the bridge inherits retrieval_v1's determinism.
        let again = context_capsule_via_retrieval_v1("how does login work", &resources, None, 320);
        let names = |capsule: &ContextCapsule| -> Vec<String> {
            capsule
                .pivots
                .iter()
                .map(|pivot| pivot.symbol.clone())
                .chain(capsule.context.iter().map(|node| node.symbol.clone()))
                .collect()
        };
        assert_eq!(
            names(&capsule),
            names(&again),
            "bridge must be deterministic"
        );
    }
}
