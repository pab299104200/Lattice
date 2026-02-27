use super::capsule::QueryIntent;

/// Keywords that indicate a FixBug intent.
const FIX_BUG_KEYWORDS: &[&str] = &[
    "fix", "bug", "error", "crash", "fail", "broken", "debug", "issue", "wrong", "exception",
];

/// Keywords that indicate a Refactor intent.
const REFACTOR_KEYWORDS: &[&str] = &[
    "refactor", "clean up", "cleanup", "restructure", "reorganize", "simplify", "improve",
    "optimize",
];

/// Keywords that indicate an AddFeature intent.
const ADD_FEATURE_KEYWORDS: &[&str] = &[
    "add", "implement", "create", "build", "new", "introduce", "support for",
];

/// Keywords that indicate an Explore intent.
/// Includes both question words ("how", "what") and architectural nouns
/// ("flow", "pipeline") that signal understanding-oriented queries.
const EXPLORE_KEYWORDS: &[&str] = &[
    "how", "what", "explain", "describe", "show", "where", "understand", "overview",
    "flow", "pipeline", "architecture", "trace", "lifecycle", "walkthrough",
];

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
    let lower = query.to_lowercase();

    let scores = [
        (QueryIntent::FixBug, count_keyword_matches(&lower, FIX_BUG_KEYWORDS)),
        (QueryIntent::Refactor, count_keyword_matches(&lower, REFACTOR_KEYWORDS)),
        (QueryIntent::AddFeature, count_keyword_matches(&lower, ADD_FEATURE_KEYWORDS)),
        (QueryIntent::Explore, count_keyword_matches(&lower, EXPLORE_KEYWORDS)),
    ];

    if let Some((intent, _)) = scores
        .iter()
        .filter(|(_, count)| *count > 0)
        .max_by_key(|(_, count)| *count)
    {
        return *intent;
    }

    // Fallback: keyword-heavy queries without action verbs are Explore.
    // "authentication system JWT login token generation" = understanding intent.
    let content_words = lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3)
        .count();
    if content_words >= 4 {
        return QueryIntent::Explore;
    }

    QueryIntent::Unknown
}

/// Count how many keywords from the list appear in the query text.
fn count_keyword_matches(text: &str, keywords: &[&str]) -> usize {
    keywords.iter().filter(|kw| text.contains(*kw)).count()
}
