use serde::{Deserialize, Serialize};

use crate::identity::FileId;

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

/// The primary memory class recorded by the cognitive workspace surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryClass {
    Observation,
    Decision,
    Constraint,
    Pattern,
    AntiPattern,
    WorkflowOutcome,
    FailurePattern,
    Procedure,
    Preference,
    ArchitectureInvariant,
    DocsContract,
    OpenQuestion,
    CounterMemory,
}

impl MemoryClass {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryClass::Observation => "observation",
            MemoryClass::Decision => "decision",
            MemoryClass::Constraint => "constraint",
            MemoryClass::Pattern => "pattern",
            MemoryClass::AntiPattern => "anti_pattern",
            MemoryClass::WorkflowOutcome => "workflow_outcome",
            MemoryClass::FailurePattern => "failure_pattern",
            MemoryClass::Procedure => "procedure",
            MemoryClass::Preference => "preference",
            MemoryClass::ArchitectureInvariant => "architecture_invariant",
            MemoryClass::DocsContract => "docs_contract",
            MemoryClass::OpenQuestion => "open_question",
            MemoryClass::CounterMemory => "counter_memory",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "decision" => MemoryClass::Decision,
            "constraint" => MemoryClass::Constraint,
            "pattern" => MemoryClass::Pattern,
            "anti_pattern" => MemoryClass::AntiPattern,
            "workflow_outcome" => MemoryClass::WorkflowOutcome,
            "failure_pattern" => MemoryClass::FailurePattern,
            "procedure" => MemoryClass::Procedure,
            "preference" => MemoryClass::Preference,
            "architecture_invariant" => MemoryClass::ArchitectureInvariant,
            "docs_contract" => MemoryClass::DocsContract,
            "open_question" => MemoryClass::OpenQuestion,
            "counter_memory" => MemoryClass::CounterMemory,
            _ => MemoryClass::Observation,
        }
    }

    pub fn from_memory_type(memory_type: &MemoryType) -> Self {
        match memory_type {
            MemoryType::Observation => MemoryClass::Observation,
            MemoryType::Decision => MemoryClass::Decision,
            MemoryType::Exploration => MemoryClass::OpenQuestion,
            MemoryType::Pattern => MemoryClass::Pattern,
            MemoryType::AntiPattern => MemoryClass::AntiPattern,
        }
    }
}

/// The durability scope of a memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryScope {
    Session,
    Branch,
    Repo,
    Organization,
}

impl MemoryScope {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryScope::Session => "session",
            MemoryScope::Branch => "branch",
            MemoryScope::Repo => "repo",
            MemoryScope::Organization => "organization",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "branch" => MemoryScope::Branch,
            "repo" => MemoryScope::Repo,
            "organization" => MemoryScope::Organization,
            _ => MemoryScope::Session,
        }
    }
}

/// The assertion shape represented by a memory row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAssertionType {
    Observation,
    Decision,
    Exploration,
    Pattern,
    AntiPattern,
    WorkflowOutcome,
    Constraint,
    Hypothesis,
    Procedure,
    Outcome,
    Preference,
    Question,
    Counter,
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
            MemoryAssertionType::Hypothesis => "hypothesis",
            MemoryAssertionType::Procedure => "procedure",
            MemoryAssertionType::Outcome => "outcome",
            MemoryAssertionType::Preference => "preference",
            MemoryAssertionType::Question => "question",
            MemoryAssertionType::Counter => "counter",
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
            "hypothesis" => MemoryAssertionType::Hypothesis,
            "procedure" => MemoryAssertionType::Procedure,
            "outcome" => MemoryAssertionType::Outcome,
            "preference" => MemoryAssertionType::Preference,
            "question" => MemoryAssertionType::Question,
            "counter" => MemoryAssertionType::Counter,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryVerificationStatus {
    Unverified,
    InReview,
    Verified,
    Stale,
    Contradicted,
    Superseded,
    Expired,
    Invalidated,
}

/// Whether the references supporting a memory still resolve in the current
/// checkout. This is deliberately independent from whether the claim was
/// behaviorally demonstrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFreshnessStatus {
    Unknown,
    Fresh,
    Stale,
    Invalidated,
}

/// Result of an observed behavioral check. A test file merely existing leaves
/// this value unverified; only a recorded result bound to the current revision
/// or graph generation can set it to passed or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BehavioralValidationStatus {
    NotRequired,
    Unverified,
    Passed,
    Failed,
}

/// Daemon-observed validation supplied to the verifier by a trusted runtime.
/// `command` is intentionally absent: verification consumes results and never
/// executes commands recovered from memory content. Repository and checkout
/// authority are mandatory, and consumers require both revision and graph
/// generation so a clean-commit identity cannot certify different dirty bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehavioralValidationRecord {
    pub repository_id: String,
    pub checkout_id: String,
    pub evidence_reference: String,
    pub status: BehavioralValidationStatus,
    pub revision: Option<String>,
    pub graph_generation: Option<u64>,
    pub observed_at: u64,
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
            MemoryVerificationStatus::Expired => "expired",
            MemoryVerificationStatus::Invalidated => "invalidated",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "in_review" => MemoryVerificationStatus::InReview,
            "verified" => MemoryVerificationStatus::Verified,
            "stale" => MemoryVerificationStatus::Stale,
            "contradicted" => MemoryVerificationStatus::Contradicted,
            "superseded" => MemoryVerificationStatus::Superseded,
            "expired" => MemoryVerificationStatus::Expired,
            "invalidated" => MemoryVerificationStatus::Invalidated,
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
            MemoryScope::Organization => MemoryFreshnessPolicy::RepoScoped,
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
pub struct EvidenceSpan {
    pub file_id: FileId,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line_start: u32,
    pub line_end: u32,
}

/// Evidence attached to a structured memory assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub kind: String,
    pub reference: Option<String>,
    pub detail: Option<String>,
    pub captured_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<EvidenceSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_content_hash: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScoreKind {
    UsefulnessPrior,
    RecentUsefulness,
    RetrievalAccuracy,
    RegressionRisk,
}

impl MemoryScoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UsefulnessPrior => "usefulness_prior",
            Self::RecentUsefulness => "recent_usefulness",
            Self::RetrievalAccuracy => "retrieval_accuracy",
            Self::RegressionRisk => "regression_risk",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "usefulness_prior" => Self::UsefulnessPrior,
            "recent_usefulness" => Self::RecentUsefulness,
            "retrieval_accuracy" => Self::RetrievalAccuracy,
            "regression_risk" => Self::RegressionRisk,
            _ => Self::UsefulnessPrior,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAccessRecord {
    pub access_id: String,
    pub accessed_at: u64,
    pub inclusion_reason: String,
    pub was_used: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryScoreRecord {
    pub score_kind: MemoryScoreKind,
    pub value: f32,
    pub computed_at: u64,
    pub computed_from_window_secs: u64,
    pub sample_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryLinkRecord {
    pub link_id: String,
    pub source_memory_id: String,
    pub target_memory_id: String,
    pub link_type: String,
    pub reason: String,
    pub created_at: u64,
    pub verification_status: String,
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
    pub memory_class: MemoryClass,
    pub assertion_type: MemoryAssertionType,
    pub verification_status: MemoryVerificationStatus,
    pub confidence_reason: Option<String>,
    pub supersedes_memory_id: Option<String>,
    pub superseded_by_memory_id: Option<String>,
    pub contradicts_memory_ids: Vec<String>,
    pub contradicted_by_memory_ids: Vec<String>,
    pub freshness_policy: MemoryFreshnessPolicy,
    pub freshness_policy_detail: Option<String>,
    pub validity_conditions: Vec<String>,
    pub invalidation_triggers: Vec<String>,
    pub provenance: Vec<MemoryProvenance>,
    pub evidence: Vec<MemoryEvidence>,
    pub linked_docs: Vec<String>,
    pub linked_tests: Vec<String>,
    pub linked_memories: Vec<String>,
}

impl Default for MemoryStructuredFields {
    fn default() -> Self {
        Self {
            memory_class: MemoryClass::Observation,
            assertion_type: MemoryAssertionType::Observation,
            verification_status: MemoryVerificationStatus::Unverified,
            confidence_reason: None,
            supersedes_memory_id: None,
            superseded_by_memory_id: None,
            contradicts_memory_ids: Vec::new(),
            contradicted_by_memory_ids: Vec::new(),
            freshness_policy: MemoryFreshnessPolicy::SessionScoped,
            freshness_policy_detail: None,
            validity_conditions: Vec::new(),
            invalidation_triggers: Vec::new(),
            provenance: Vec::new(),
            evidence: Vec::new(),
            linked_docs: Vec::new(),
            linked_tests: Vec::new(),
            linked_memories: Vec::new(),
        }
    }
}

/// A session memory — an insight, decision, pattern, or observation recorded
/// during an AI coding session for later recall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub scope_organization_id: Option<String>,
    pub refresh_key: Option<String>,
    pub source_query: Option<String>,
    pub created_at: u64,
    pub last_accessed: u64,
    pub access_count: u32,
    pub is_stale: bool,
    pub stale_reason: Option<String>,
    pub verification_status: MemoryVerificationStatus,
}
