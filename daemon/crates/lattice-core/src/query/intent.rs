use crate::retrieval_v1::{classify_intent, IntentLabel};

use super::capsule::QueryIntent;

/// Per-intent tuning parameters.
#[derive(Debug, Clone)]
pub struct IntentParams {
    /// Number of top-k semantic search results.
    pub semantic_k: usize,
    /// Number of graph hops from semantic hits.
    pub hop_depth: usize,
    /// Weight for semantic similarity in ranking.
    pub w_semantic: f64,
    /// Weight for centrality in ranking.
    pub w_centrality: f64,
    /// Weight for recency in ranking.
    pub w_recency: f64,
    /// Weight for caller count in ranking.
    pub w_caller: f64,
    /// Base token budget for the capsule.
    pub base_token_budget: usize,
}

impl IntentParams {
    /// Get tuning parameters for a given intent.
    pub fn for_intent(intent: QueryIntent) -> Self {
        match intent {
            QueryIntent::Explore => IntentParams {
                semantic_k: 25,
                hop_depth: 2,
                w_semantic: 0.4,
                w_centrality: 0.3,
                w_recency: 0.1,
                w_caller: 0.2,
                base_token_budget: 4000,
            },
            QueryIntent::FixBug => IntentParams {
                semantic_k: 8,
                hop_depth: 3,
                w_semantic: 0.5,
                w_centrality: 0.2,
                w_recency: 0.2,
                w_caller: 0.1,
                base_token_budget: 4000,
            },
            QueryIntent::Refactor => IntentParams {
                semantic_k: 12,
                hop_depth: 2,
                w_semantic: 0.3,
                w_centrality: 0.4,
                w_recency: 0.1,
                w_caller: 0.2,
                base_token_budget: 4000,
            },
            QueryIntent::AddFeature => IntentParams {
                semantic_k: 10,
                hop_depth: 2,
                w_semantic: 0.4,
                w_centrality: 0.3,
                w_recency: 0.1,
                w_caller: 0.2,
                base_token_budget: 4000,
            },
            QueryIntent::Unknown => IntentParams {
                semantic_k: 8,
                hop_depth: 2,
                w_semantic: 0.4,
                w_centrality: 0.3,
                w_recency: 0.1,
                w_caller: 0.2,
                base_token_budget: 4000,
            },
        }
    }
}

/// Detect the intent of a query based on keyword matching.
/// Returns the intent with the highest keyword match count.
/// Falls back to Explore for keyword-heavy queries (4+ content words) since
/// a long descriptive phrase without action verbs indicates understanding intent.
/// Only returns Unknown for very short ambiguous queries.
pub fn detect_intent(query: &str) -> QueryIntent {
    let classification = classify_intent(query);
    map_label_to_query_intent(classification.primary_label)
}

fn map_label_to_query_intent(label: IntentLabel) -> QueryIntent {
    match label {
        IntentLabel::Debug
        | IntentLabel::AddTest
        | IntentLabel::Migration
        | IntentLabel::Performance => QueryIntent::FixBug,
        IntentLabel::Refactor | IntentLabel::Review => QueryIntent::Refactor,
        IntentLabel::Explain | IntentLabel::UpdateDocs => QueryIntent::Explore,
        IntentLabel::AddFeature | IntentLabel::ModifyFeature => QueryIntent::AddFeature,
        IntentLabel::Unknown => QueryIntent::Unknown,
    }
}
