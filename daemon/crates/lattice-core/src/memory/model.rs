use serde::{Deserialize, Serialize};

/// The type of memory being stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    Observation,
    Decision,
    Exploration,
    Pattern,
    AntiPattern,
}

impl MemoryType {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryType::Observation => "observation",
            MemoryType::Decision => "decision",
            MemoryType::Exploration => "exploration",
            MemoryType::Pattern => "pattern",
            MemoryType::AntiPattern => "anti_pattern",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "observation" => MemoryType::Observation,
            "decision" => MemoryType::Decision,
            "exploration" => MemoryType::Exploration,
            "pattern" => MemoryType::Pattern,
            "anti_pattern" => MemoryType::AntiPattern,
            _ => MemoryType::Observation,
        }
    }
}

/// A session memory — an insight, decision, pattern, or observation recorded
/// during an AI coding session for later recall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub session_id: String,
    pub content: String,
    pub memory_type: MemoryType,
    pub confidence: f64,
    pub linked_symbols: Vec<String>,
    pub source_query: Option<String>,
    pub created_at: u64,
    pub last_accessed: u64,
    pub access_count: u32,
    pub is_stale: bool,
    pub stale_reason: Option<String>,
}
