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

/// The durability scope of a memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryScope {
    Session,
    Branch,
    Repo,
}

impl MemoryScope {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryScope::Session => "session",
            MemoryScope::Branch => "branch",
            MemoryScope::Repo => "repo",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "branch" => MemoryScope::Branch,
            "repo" => MemoryScope::Repo,
            _ => MemoryScope::Session,
        }
    }
}

/// The assertion shape represented by a memory row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryAssertionType {
    Observation,
    Decision,
    Exploration,
    Pattern,
    AntiPattern,
    WorkflowOutcome,
    Constraint,
}

impl MemoryAssertionType {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryAssertionType::Observation => "observation",
            MemoryAssertionType::Decision => "decision",
            MemoryAssertionType::Exploration => "exploration",
            MemoryAssertionType::Pattern => "pattern",
            MemoryAssertionType::AntiPattern => "anti_pattern",
            MemoryAssertionType::WorkflowOutcome => "workflow_outcome",
            MemoryAssertionType::Constraint => "constraint",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "decision" => MemoryAssertionType::Decision,
            "exploration" => MemoryAssertionType::Exploration,
            "pattern" => MemoryAssertionType::Pattern,
            "anti_pattern" => MemoryAssertionType::AntiPattern,
            "workflow_outcome" => MemoryAssertionType::WorkflowOutcome,
            "constraint" => MemoryAssertionType::Constraint,
            _ => MemoryAssertionType::Observation,
        }
    }

    pub fn from_memory_type(memory_type: &MemoryType) -> Self {
        match memory_type {
            MemoryType::Observation => MemoryAssertionType::Observation,
            MemoryType::Decision => MemoryAssertionType::Decision,
            MemoryType::Exploration => MemoryAssertionType::Exploration,
            MemoryType::Pattern => MemoryAssertionType::Pattern,
            MemoryType::AntiPattern => MemoryAssertionType::AntiPattern,
        }
    }
}

/// Verification lifecycle state for a memory assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryVerificationStatus {
    Unverified,
    InReview,
    Verified,
    Stale,
    Contradicted,
    Superseded,
}

impl MemoryVerificationStatus {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryVerificationStatus::Unverified => "unverified",
            MemoryVerificationStatus::InReview => "in_review",
            MemoryVerificationStatus::Verified => "verified",
            MemoryVerificationStatus::Stale => "stale",
            MemoryVerificationStatus::Contradicted => "contradicted",
            MemoryVerificationStatus::Superseded => "superseded",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "in_review" => MemoryVerificationStatus::InReview,
            "verified" => MemoryVerificationStatus::Verified,
            "stale" => MemoryVerificationStatus::Stale,
            "contradicted" => MemoryVerificationStatus::Contradicted,
            "superseded" => MemoryVerificationStatus::Superseded,
            _ => MemoryVerificationStatus::Unverified,
        }
    }
}

/// How a memory should be revalidated over time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryFreshnessPolicy {
    SessionScoped,
    BranchScoped,
    RepoScoped,
    TimeBound,
    ManualReview,
}

impl MemoryFreshnessPolicy {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryFreshnessPolicy::SessionScoped => "session_scoped",
            MemoryFreshnessPolicy::BranchScoped => "branch_scoped",
            MemoryFreshnessPolicy::RepoScoped => "repo_scoped",
            MemoryFreshnessPolicy::TimeBound => "time_bound",
            MemoryFreshnessPolicy::ManualReview => "manual_review",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "branch_scoped" => MemoryFreshnessPolicy::BranchScoped,
            "repo_scoped" => MemoryFreshnessPolicy::RepoScoped,
            "time_bound" => MemoryFreshnessPolicy::TimeBound,
            "manual_review" => MemoryFreshnessPolicy::ManualReview,
            _ => MemoryFreshnessPolicy::SessionScoped,
        }
    }

    pub fn from_scope(scope: &MemoryScope) -> Self {
        match scope {
            MemoryScope::Session => MemoryFreshnessPolicy::SessionScoped,
            MemoryScope::Branch => MemoryFreshnessPolicy::BranchScoped,
            MemoryScope::Repo => MemoryFreshnessPolicy::RepoScoped,
        }
    }

    pub fn is_scope_derived(&self) -> bool {
        matches!(
            self,
            MemoryFreshnessPolicy::SessionScoped
                | MemoryFreshnessPolicy::BranchScoped
                | MemoryFreshnessPolicy::RepoScoped
        )
    }
}

/// Evidence attached to a structured memory assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub kind: String,
    pub reference: Option<String>,
    pub detail: Option<String>,
    pub captured_at: Option<u64>,
}

/// Provenance entry describing where a structured assertion came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProvenance {
    pub source: String,
    pub reference: Option<String>,
    pub captured_at: Option<u64>,
    pub note: Option<String>,
}

/// Structured assertion metadata persisted alongside each memory row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryStructuredFields {
    pub assertion_type: MemoryAssertionType,
    pub verification_status: MemoryVerificationStatus,
    pub confidence_reason: Option<String>,
    pub supersedes_memory_id: Option<String>,
    pub superseded_by_memory_id: Option<String>,
    pub contradicts_memory_ids: Vec<String>,
    pub contradicted_by_memory_ids: Vec<String>,
    pub freshness_policy: MemoryFreshnessPolicy,
    pub freshness_policy_detail: Option<String>,
    pub provenance: Vec<MemoryProvenance>,
    pub evidence: Vec<MemoryEvidence>,
}

impl Default for MemoryStructuredFields {
    fn default() -> Self {
        Self {
            assertion_type: MemoryAssertionType::Observation,
            verification_status: MemoryVerificationStatus::Unverified,
            confidence_reason: None,
            supersedes_memory_id: None,
            superseded_by_memory_id: None,
            contradicts_memory_ids: Vec::new(),
            contradicted_by_memory_ids: Vec::new(),
            freshness_policy: MemoryFreshnessPolicy::SessionScoped,
            freshness_policy_detail: None,
            provenance: Vec::new(),
            evidence: Vec::new(),
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
    pub scope: MemoryScope,
    pub confidence: f64,
    pub linked_symbols: Vec<String>,
    pub linked_files: Vec<String>,
    pub workspace_id: Option<String>,
    pub branch: Option<String>,
    pub refresh_key: Option<String>,
    pub source_query: Option<String>,
    pub created_at: u64,
    pub last_accessed: u64,
    pub access_count: u32,
    pub is_stale: bool,
    pub stale_reason: Option<String>,
}
