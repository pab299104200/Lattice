use std::collections::HashMap;

use crate::graph::model::{CodeGraph, GraphNode};
use crate::storage::VectorStore;
use crate::symbols::SymbolId;

use super::capsule::{
    CapsuleStats, ContextCapsule, ContextNode, PivotNode, QueryIntent,
};
use super::intent::{detect_intent, IntentParams};

/// Token estimation: ~4 characters per token.
const CHARS_PER_TOKEN: usize = 4;

/// A candidate node with its computed score for ranking.
struct ScoredCandidate<'a> {
    node: &'a GraphNode,
    score: f64,
    semantic_sim: f64,
}

/// The query engine orchestrates intent detection, search, graph traversal,
/// ranking, and budget allocation to produce Context Capsules.
pub struct QueryEngine {
    graph: CodeGraph,
    vector_store: Option<VectorStore>,
    query_history: HashMap<String, usize>,
}

impl QueryEngine {
    /// Create a new QueryEngine with a code graph and optional vector store.
    pub fn new(graph: CodeGraph, vector_store: Option<VectorStore>) -> Self {
        Self {
            graph,
            vector_store,
            query_history: HashMap::new(),
        }
    }

    /// Execute a query and produce a Context Capsule.
    ///
    /// If `embedding` is provided and a vector store is available, semantic search
    /// is used. Otherwise, falls back to keyword matching on node names/signatures.
    pub fn query(&self, query_text: &str, embedding: Option<&[f32]>) -> ContextCapsule {
        // Step 1: Detect intent
        let intent = detect_intent(query_text);
        let params = IntentParams::for_intent(intent);

        // Step 2: Semantic search or keyword fallback
        let seed_hits = self.find_seed_hits(query_text, embedding, &params);

        // Step 3: Graph traversal — N hops from semantic hits
        let mut candidate_ids: HashMap<SymbolId, f64> = HashMap::new();
        for (id, sim) in &seed_hits {
            candidate_ids.insert(id.clone(), *sim);

            let neighbors = self.graph.n_hop_neighbors(id, params.hop_depth);
            for neighbor in neighbors {
                candidate_ids
                    .entry(neighbor.id.clone())
                    .or_insert(0.0);
            }
        }

        // Step 4: Rank candidates
        let mut candidates: Vec<ScoredCandidate> = Vec::new();
        let all_node_ids: Vec<&SymbolId> = self.graph.all_node_ids();
        let nodes_evaluated = candidate_ids.len();

        for (id, semantic_sim) in &candidate_ids {
            if let Some(node) = self.graph.get_node(id) {
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

                candidates.push(ScoredCandidate {
                    node,
                    score,
                    semantic_sim: *semantic_sim,
                });
            }
        }

        // Sort by descending score
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        // Step 5: Budget allocation
        let mut pivots = Vec::new();
        let mut context = Vec::new();
        let mut tokens_used: usize = 0;
        let budget = params.base_token_budget;

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
                            kind: format!("{:?}", candidate.node.kind),
                            file: candidate.node.file.clone(),
                            line: candidate.node.line,
                            skeleton: candidate.node.signature.clone(),
                            relationship: classify_relationship(candidate.semantic_sim),
                            score: candidate.score,
                        });
                        tokens_used += sig_tokens;
                    }
                    continue;
                }

                pivots.push(PivotNode {
                    symbol: candidate.node.name.clone(),
                    kind: format!("{:?}", candidate.node.kind),
                    file: candidate.node.file.clone(),
                    line: candidate.node.line,
                    source: candidate.node.body.clone(),
                    why: classify_why(candidate.semantic_sim, intent),
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
                    kind: format!("{:?}", candidate.node.kind),
                    file: candidate.node.file.clone(),
                    line: candidate.node.line,
                    skeleton: candidate.node.signature.clone(),
                    relationship: classify_relationship(candidate.semantic_sim),
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

        // Step 6: Assemble capsule
        ContextCapsule {
            query: query_text.to_string(),
            intent,
            pivots,
            context,
            memories: Vec::new(),
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

/// Classify why a node was selected as a pivot.
fn classify_why(semantic_sim: f64, intent: QueryIntent) -> String {
    let intent_str = match intent {
        QueryIntent::Explore => "exploration target",
        QueryIntent::FixBug => "likely bug location",
        QueryIntent::Refactor => "refactoring candidate",
        QueryIntent::AddFeature => "integration point",
        QueryIntent::Unknown => "relevant symbol",
    };

    if semantic_sim > 0.8 {
        format!("Direct semantic match — {}", intent_str)
    } else if semantic_sim > 0.5 {
        format!("High relevance — {}", intent_str)
    } else {
        format!("Graph-connected — {}", intent_str)
    }
}
