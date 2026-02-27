use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
use crate::memory::MemoryStore;
use crate::storage::VectorStore;
use crate::symbols::SymbolId;

use super::capsule::{
    CapsuleStats, ContextCapsule, ContextNode, PivotNode,
};
use super::intent::{detect_intent, IntentParams};

/// Token estimation: ~4 characters per token.
const CHARS_PER_TOKEN: usize = 4;

/// A candidate node with its computed score for ranking.
struct ScoredCandidate<'a> {
    node: &'a GraphNode,
    score: f64,
    _semantic_sim: f64,
    /// How this node was reached (e.g., "semantic_match: 0.91" or "called_by: loginUser").
    relationship_detail: String,
}

/// The query engine orchestrates intent detection, search, graph traversal,
/// ranking, and budget allocation to produce Context Capsules.
pub struct QueryEngine {
    graph: CodeGraph,
    vector_store: Option<VectorStore>,
    memory_store: Option<Arc<Mutex<MemoryStore>>>,
    query_history: HashMap<String, usize>,
}

impl QueryEngine {
    /// Create a new QueryEngine with a code graph, optional vector store,
    /// and optional memory store.
    pub fn new(
        graph: CodeGraph,
        vector_store: Option<VectorStore>,
        memory_store: Option<Arc<Mutex<MemoryStore>>>,
    ) -> Self {
        Self {
            graph,
            vector_store,
            memory_store,
            query_history: HashMap::new(),
        }
    }

    /// Execute a query and produce a Context Capsule.
    ///
    /// If `embedding` is provided and a vector store is available, semantic search
    /// is used. Otherwise, falls back to keyword matching on node names/signatures.
    pub fn query(&mut self, query_text: &str, embedding: Option<&[f32]>) -> ContextCapsule {
        // Record the query for frequency tracking (adaptive budget)
        self.record_query(query_text);

        // Step 0: Parse query filters (repo:, file:, lang:)
        let (filter, clean_query) = parse_query_filters(query_text);

        // Step 1: Detect intent (use clean query without filter tokens)
        let intent = detect_intent(&clean_query);
        let params = IntentParams::for_intent(intent);

        // Step 2: Semantic search or keyword fallback (use clean query for matching)
        let seed_hits = self.find_seed_hits(&clean_query, embedding, &params);

        // Step 3: Graph traversal — N hops from semantic hits, tracking relationship paths
        let mut candidate_ids: HashMap<SymbolId, f64> = HashMap::new();
        let mut relationship_paths: HashMap<SymbolId, String> = HashMap::new();

        for (id, sim) in &seed_hits {
            candidate_ids.insert(id.clone(), *sim);
            relationship_paths.insert(
                id.clone(),
                format!("semantic_match: {:.2}", sim),
            );

            // Build path-aware traversal from this seed hit
            let seed_name = self.graph.get_node(id)
                .map(|n| n.name.clone())
                .unwrap_or_else(|| id.name.clone());

            // 1 hop: direct dependencies and dependents
            for (dep_node, edge_kind) in self.graph.get_dependencies(id) {
                let edge_label = format_edge_kind(edge_kind);
                candidate_ids.entry(dep_node.id.clone()).or_insert(0.0);
                relationship_paths.entry(dep_node.id.clone()).or_insert_with(|| {
                    format!("{}: {} (via {})", edge_label, seed_name, dep_node.name)
                });

                // 2 hops from dependencies
                if params.hop_depth >= 2 {
                    for (dep2_node, edge_kind2) in self.graph.get_dependencies(&dep_node.id) {
                        candidate_ids.entry(dep2_node.id.clone()).or_insert(0.0);
                        relationship_paths.entry(dep2_node.id.clone()).or_insert_with(|| {
                            format!("{} -> {} -> {} (via {:?})",
                                seed_name, dep_node.name, dep2_node.name, edge_kind2)
                        });
                    }
                }
            }

            for (caller_node, edge_kind) in self.graph.get_dependents(id) {
                let edge_label = format_edge_kind_reverse(edge_kind);
                candidate_ids.entry(caller_node.id.clone()).or_insert(0.0);
                relationship_paths.entry(caller_node.id.clone()).or_insert_with(|| {
                    format!("{}: {}", edge_label, seed_name)
                });

                // 2 hops from dependents
                if params.hop_depth >= 2 {
                    for (caller2_node, edge_kind2) in self.graph.get_dependents(&caller_node.id) {
                        candidate_ids.entry(caller2_node.id.clone()).or_insert(0.0);
                        relationship_paths.entry(caller2_node.id.clone()).or_insert_with(|| {
                            format!("{} -> {} -> {} (via {:?})",
                                seed_name, caller_node.name, caller2_node.name, edge_kind2)
                        });
                    }
                }
            }
        }

        // Step 4: Rank candidates (apply query filters)
        let mut candidates: Vec<ScoredCandidate> = Vec::new();
        let all_node_ids: Vec<&SymbolId> = self.graph.all_node_ids();
        let nodes_evaluated = candidate_ids.len();

        for (id, semantic_sim) in &candidate_ids {
            if let Some(node) = self.graph.get_node(id) {
                // Apply query filters — skip nodes that don't match
                if !filter.matches(node) {
                    continue;
                }

                let centrality = self.graph.centrality(id);

                // Recency: normalize last_modified to 0..1 range based on max
                let max_modified = all_node_ids
                    .iter()
                    .filter_map(|nid| self.graph.get_node(nid))
                    .map(|n| n.last_modified)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let recency = node.last_modified as f64 / max_modified as f64;

                // Caller count: number of incoming edges (dependents)
                let caller_count = self.graph.get_dependents(id).len() as f64;
                let max_callers = all_node_ids
                    .iter()
                    .map(|nid| self.graph.get_dependents(nid).len())
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let caller_norm = caller_count / max_callers as f64;

                let score = semantic_sim * params.w_semantic
                    + centrality * params.w_centrality
                    + recency * params.w_recency
                    + caller_norm * params.w_caller;

                let rel_detail = relationship_paths.get(id)
                    .cloned()
                    .unwrap_or_else(|| classify_relationship(*semantic_sim));

                candidates.push(ScoredCandidate {
                    node,
                    score,
                    _semantic_sim: *semantic_sim,
                    relationship_detail: rel_detail,
                });
            }
        }

        // Sort by descending score
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        // Step 5: Budget allocation (adaptive: repeated queries expand context)
        let repeat_count = self.query_history.get(query_text).copied().unwrap_or(0);
        let mut pivots = Vec::new();
        let mut context = Vec::new();
        let mut tokens_used: usize = 0;
        let budget = params.base_token_budget + (repeat_count * 500);

        for candidate in &candidates {
            if tokens_used >= budget {
                break;
            }

            if candidate.score > 0.7 {
                // Pivot: include full source
                let source_tokens = candidate.node.body.len() / CHARS_PER_TOKEN;
                if tokens_used + source_tokens > budget {
                    // Try to fit as context instead
                    let sig_tokens = candidate.node.signature.len() / CHARS_PER_TOKEN;
                    if tokens_used + sig_tokens <= budget {
                        context.push(ContextNode {
                            symbol: candidate.node.name.clone(),
                            kind: candidate.node.kind.short_code().to_string(),
                            file: candidate.node.file.clone(),
                            line: candidate.node.line,
                            skeleton: candidate.node.signature.clone(),
                            relationship: candidate.relationship_detail.clone(),
                            score: candidate.score,
                        });
                        tokens_used += sig_tokens;
                    }
                    continue;
                }

                pivots.push(PivotNode {
                    symbol: candidate.node.name.clone(),
                    kind: candidate.node.kind.short_code().to_string(),
                    file: candidate.node.file.clone(),
                    line: candidate.node.line,
                    source: candidate.node.body.clone(),
                    score: candidate.score,
                });
                tokens_used += source_tokens;
            } else if candidate.score > 0.3 {
                // Context: include skeleton (signature only)
                let sig_tokens = candidate.node.signature.len() / CHARS_PER_TOKEN;
                if tokens_used + sig_tokens > budget {
                    continue;
                }

                context.push(ContextNode {
                    symbol: candidate.node.name.clone(),
                    kind: candidate.node.kind.short_code().to_string(),
                    file: candidate.node.file.clone(),
                    line: candidate.node.line,
                    skeleton: candidate.node.signature.clone(),
                    relationship: candidate.relationship_detail.clone(),
                    score: candidate.score,
                });
                tokens_used += sig_tokens;
            }
            // Scores <= 0.3 are excluded
        }

        // Calculate tokens saved
        let total_tokens_if_all: usize = candidates
            .iter()
            .map(|c| c.node.body.len() / CHARS_PER_TOKEN)
            .sum();
        let tokens_saved = total_tokens_if_all.saturating_sub(tokens_used);
        let nodes_included = pivots.len() + context.len();

        // Step 6: Retrieve relevant memories
        let memories = if let Some(ref ms) = self.memory_store {
            let store = ms.lock().unwrap();
            let results = store.search_by_keyword(query_text).unwrap_or_default();
            results.into_iter().take(5).map(|m| {
                serde_json::json!({
                    "content": m.content,
                    "type": m.memory_type.as_str(),
                })
            }).collect()
        } else {
            vec![]
        };

        // Step 7: Assemble capsule
        ContextCapsule {
            query: query_text.to_string(),
            intent,
            pivots,
            context,
            memories,
            stats: CapsuleStats {
                tokens_used,
                tokens_saved,
                nodes_evaluated,
                nodes_included,
            },
        }
    }

    /// Get the underlying graph for direct operations.
    pub fn graph(&self) -> &CodeGraph {
        &self.graph
    }

    /// Get a reference to the vector store (if available).
    pub fn vector_store(&self) -> &Option<VectorStore> {
        &self.vector_store
    }

    /// Find a symbol by name (searches all nodes).
    pub fn find_symbol(&self, name: &str) -> Option<&GraphNode> {
        self.graph.all_nodes().into_iter()
            .find(|n| n.name == name)
    }

    /// Find all symbols in a file.
    pub fn file_symbols(&self, file: &str) -> Vec<&GraphNode> {
        self.graph.all_nodes().into_iter()
            .filter(|n| n.file == file)
            .collect()
    }

    /// Replace the code graph with a new one.
    pub fn update_graph(&mut self, graph: CodeGraph) {
        self.graph = graph;
    }

    /// Record a query for frequency tracking.
    pub fn record_query(&mut self, query: &str) {
        *self.query_history.entry(query.to_string()).or_insert(0) += 1;
    }

    /// Find seed hits using semantic search (vector store) or keyword fallback.
    fn find_seed_hits(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        params: &IntentParams,
    ) -> Vec<(SymbolId, f64)> {
        // Try semantic search first
        if let (Some(emb), Some(vs)) = (embedding, &self.vector_store) {
            if let Ok(results) = vs.search(emb, params.semantic_k) {
                let mut hits = Vec::new();
                for (name, file, byte_offset, similarity) in results {
                    let id = SymbolId {
                        file,
                        name,
                        byte_offset,
                    };
                    hits.push((id, similarity as f64));
                }
                if !hits.is_empty() {
                    return hits;
                }
            }
        }

        // Fallback: keyword matching on node names and signatures
        self.keyword_fallback(query_text, params.semantic_k)
    }

    /// Keyword-based fallback when no vector store or embedding is available.
    /// Matches query words against node names and signatures.
    fn keyword_fallback(&self, query_text: &str, top_k: usize) -> Vec<(SymbolId, f64)> {
        let query_lower = query_text.to_lowercase();
        // Strip punctuation and split into words
        let cleaned: String = query_lower
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { ' ' })
            .collect();
        let query_words: Vec<&str> = cleaned
            .split_whitespace()
            .filter(|w| w.len() > 2) // skip very short words like "a", "is", etc.
            .collect();

        if query_words.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(SymbolId, f64)> = Vec::new();

        for node in self.graph.all_nodes() {
            let name_lower = node.name.to_lowercase();
            let sig_lower = node.signature.to_lowercase();

            let mut match_score: f64 = 0.0;

            for word in &query_words {
                if name_lower.contains(word) {
                    match_score += 0.6;
                }
                if sig_lower.contains(word) {
                    match_score += 0.4;
                }
            }

            if match_score > 0.0 {
                // Cap at 1.0
                let normalized = match_score.min(1.0);
                scored.push((node.id.clone(), normalized));
            }
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
    }
}

/// Parsed query filters extracted from prefix tokens like `repo:name`, `file:pattern`, `lang:language`.
#[derive(Default, Debug, Clone)]
pub struct QueryFilter {
    pub repo: Option<String>,
    pub file_pattern: Option<String>,
    pub language: Option<String>,
}

impl QueryFilter {
    /// Returns true if the given graph node passes all active filters.
    pub fn matches(&self, node: &GraphNode) -> bool {
        if let Some(ref repo) = self.repo {
            // Match repo name against the beginning of the file path
            if !node.file.starts_with(repo) && !node.file.contains(&format!("/{}/", repo)) {
                return false;
            }
        }
        if let Some(ref pattern) = self.file_pattern {
            if !node.file.contains(pattern) {
                return false;
            }
        }
        if let Some(ref lang) = self.language {
            let lang_lower = lang.to_lowercase();
            let node_lang = format!("{:?}", node.language).to_lowercase();
            if node_lang != lang_lower {
                return false;
            }
        }
        true
    }
}

/// Parse query filters like `repo:frontend how does auth work?`
/// Returns (filter, clean_query) where clean_query has the filter tokens removed.
pub fn parse_query_filters(query: &str) -> (QueryFilter, String) {
    let mut filter = QueryFilter::default();
    let mut clean_parts = Vec::new();

    for word in query.split_whitespace() {
        if let Some(repo) = word.strip_prefix("repo:") {
            filter.repo = Some(repo.to_string());
        } else if let Some(file) = word.strip_prefix("file:") {
            filter.file_pattern = Some(file.to_string());
        } else if let Some(lang) = word.strip_prefix("lang:") {
            filter.language = Some(lang.to_string());
        } else {
            clean_parts.push(word);
        }
    }

    (filter, clean_parts.join(" "))
}

/// Format an edge kind for display in dependency direction (outgoing).
/// e.g., Calls -> "calls", Imports -> "imports"
fn format_edge_kind(kind: EdgeKind) -> String {
    match kind {
        EdgeKind::Calls => "calls".to_string(),
        EdgeKind::Imports => "imports".to_string(),
        EdgeKind::Implements => "implements".to_string(),
        EdgeKind::Extends => "extends".to_string(),
        EdgeKind::TypeRef => "type_ref_of".to_string(),
        EdgeKind::Contains => "contains".to_string(),
        EdgeKind::CoChanges => "co_changes_with".to_string(),
    }
}

/// Format an edge kind for display in dependent direction (incoming / reverse).
/// e.g., Calls -> "called_by", Imports -> "imported_by"
fn format_edge_kind_reverse(kind: EdgeKind) -> String {
    match kind {
        EdgeKind::Calls => "called_by".to_string(),
        EdgeKind::Imports => "imported_by".to_string(),
        EdgeKind::Implements => "implemented_by".to_string(),
        EdgeKind::Extends => "extended_by".to_string(),
        EdgeKind::TypeRef => "type_referenced_by".to_string(),
        EdgeKind::Contains => "contained_in".to_string(),
        EdgeKind::CoChanges => "co_changes_with".to_string(),
    }
}

/// Classify the relationship of a node based on its semantic similarity.
fn classify_relationship(semantic_sim: f64) -> String {
    if semantic_sim > 0.8 {
        "direct match".to_string()
    } else if semantic_sim > 0.5 {
        "closely related".to_string()
    } else if semantic_sim > 0.2 {
        "related via graph".to_string()
    } else {
        "transitive dependency".to_string()
    }
}

// classify_why was replaced by the detailed score/path-based why_included format in Fix 18.
