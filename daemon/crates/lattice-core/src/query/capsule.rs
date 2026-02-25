use serde::{Deserialize, Serialize};

/// The detected intent behind a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryIntent {
    Explore,
    FixBug,
    Refactor,
    AddFeature,
    Unknown,
}

/// A high-scoring node included with full source code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PivotNode {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub source: String,
    pub why: String,
    pub score: f64,
}

/// A supporting node included with a skeleton (signature only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextNode {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub skeleton: String,
    pub relationship: String,
    pub score: f64,
}

/// Statistics about capsule assembly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleStats {
    pub tokens_used: usize,
    pub tokens_saved: usize,
    pub nodes_evaluated: usize,
    pub nodes_included: usize,
}

/// A Context Capsule — the structured response from the query engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCapsule {
    pub query: String,
    pub intent: QueryIntent,
    pub pivots: Vec<PivotNode>,
    pub context: Vec<ContextNode>,
    pub memories: Vec<serde_json::Value>,
    pub stats: CapsuleStats,
}
